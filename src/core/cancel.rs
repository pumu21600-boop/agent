//! 取消模型 —— 施工单 001 / 任务 C；P16-D2 会话化；P16-N1 同会话并发拒登
//!
//! 一个在跑的任务 = 一个 [CancelFlag]（原子位，置位即停；网关与核心循环每个决策点查它）。
//! [CancelRegistry] 把这些标志按 `session_id` 登记：每轮任务开始 `try_begin()` 注册（标志
//! 天然是复位态），结束 `end()` 注销。
//!
//! 为什么不是单个全局标志（D2 修的病根）：多会话并发时，① 新会话任务开始时的
//! `clear()` 会把别的会话正在响应的「停止」清掉；② 一次「停止」会停掉所有在跑任务。
//! 会话化后停止按钮只作用于目标会话；命令行客户端不带 session 的 `/cancel`
//! 仍是显式的「停全部」。
//!
//! 同一会话同时只允许一轮（N1）：`try_begin` 在返回 None 时表示该会话已有在跑的一轮。
//! 旧实现是「换绑」——第二个并发请求顶掉第一轮的取消入口，且两轮双写同一会话文件；
//! 现在由 HTTP 入口据此拒 409，注册表本身也不再提供静默换绑的路径。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
pub struct CancelFlag {
    flag: AtomicBool,
    /// P26：异步等待用——工具执行（如卡在浏览器 CDP 调用里）只靠轮询 is_set
    /// 要等工具自己超时（用户点「结束回答」后新消息会白等几十秒进队列）。
    /// set() 时唤醒所有等待者，agent 循环用 select! 当场中断工具执行。
    notify: tokio::sync::Notify,
}

impl CancelFlag {
    pub fn new() -> Self {
        Self { flag: AtomicBool::new(false), notify: tokio::sync::Notify::new() }
    }

    /// 置位（停止按钮 / 审批卡的取消路径）。
    pub fn set(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// 网关每个决策点、每个流式分片前检查。
    pub fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// 异步等到取消置位（已置位则立即返回）。供 select! 中断阻塞中的工具执行。
    pub async fn cancelled(&self) {
        if self.is_set() {
            return;
        }
        self.notify.notified().await;
    }
}

/// 会话级取消注册表（D2）：`session_id` → 该会话当前在跑这一轮的取消标志。
#[derive(Default)]
pub struct CancelRegistry {
    inner: Mutex<HashMap<String, Arc<CancelFlag>>>,
}

impl CancelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 一轮任务开始：原子地「判定 + 登记」本会话的取消标志（新标志 = 已复位）。
    /// 同一会话已有在跑的一轮时返回 `None` —— HTTP 入口据此拒并发（N1）：
    /// 判定与登记在同一把锁内完成，两个并发请求不会都通过检查。
    pub fn try_begin(&self, session_id: &str) -> Option<Arc<CancelFlag>> {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if m.contains_key(session_id) {
            return None;
        }
        let flag = Arc::new(CancelFlag::new());
        m.insert(session_id.to_string(), flag.clone());
        Some(flag)
    }

    /// 一轮结束：仅当登记的还是自己的标志时注销——不误删同一会话后来的一轮。
    pub fn end(&self, session_id: &str, flag: &Arc<CancelFlag>) {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if m.get(session_id).map(|cur| Arc::ptr_eq(cur, flag)).unwrap_or(false) {
            m.remove(session_id);
        }
    }

    /// 停止指定会话：返回是否命中在跑的一轮（没在跑 = false，幂等无副作用）。
    pub fn cancel(&self, session_id: &str) -> bool {
        let flag = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned();
        match flag {
            Some(f) => {
                f.set();
                true
            }
            None => false,
        }
    }

    /// 停止全部在跑会话，返回命中数（命令行客户端与内置页的显式「全停」）。
    pub fn cancel_all(&self) -> usize {
        let flags: Vec<Arc<CancelFlag>> = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for f in &flags {
            f.set();
        }
        flags.len()
    }

    /// 指定会话是否已被请求取消（没在跑 = false）。
    pub fn is_set(&self, session_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .map(|f| f.is_set())
            .unwrap_or(false)
    }

    /// 是否有任一在跑会话被请求取消（无会话参数的 /api/cancel 查询语义）。
    pub fn any_cancelled(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|f| f.is_set())
    }

    /// 指定会话当前是否有在跑的一轮。
    pub fn is_active(&self, session_id: &str) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).contains_key(session_id)
    }

    /// 在跑会话数（状态查询）。
    pub fn active_count(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_are_independent() {
        let reg = CancelRegistry::new();
        let a = reg.try_begin("sess-a").expect("首次登记");
        let b = reg.try_begin("sess-b").expect("另一个会话互不干扰");
        assert!(!a.is_set() && !b.is_set(), "新一轮标志应为复位态");
        assert!(reg.try_begin("sess-a").is_none(), "同会话已有在跑一轮：拒登（N1）");
        assert!(reg.try_begin("sess-b").is_none(), "并发请求不会双双通过检查");

        assert!(reg.cancel("sess-a"), "命中在跑会话");
        assert!(a.is_set(), "A 被停");
        assert!(!b.is_set(), "停 A 不得影响 B（旧全局标志的病根）");
        assert!(reg.is_set("sess-a") && !reg.is_set("sess-b"));

        reg.end("sess-a", &a);
        assert!(!reg.cancel("sess-a"), "已结束的会话不再命中");
        assert!(!reg.is_set("sess-a"), "注销后查询为未取消");
        assert_eq!(reg.active_count(), 1);
        assert!(!reg.any_cancelled(), "B 没被停，全局查询为否");

        reg.end("sess-b", &b);
        assert_eq!(reg.active_count(), 0);
        assert_eq!(reg.cancel_all(), 0, "没有在跑会话时全停是空操作");
    }

    #[test]
    fn new_turn_does_not_clear_others_and_old_end_is_safe() {
        let reg = CancelRegistry::new();
        let a1 = reg.try_begin("a").unwrap();
        reg.cancel("a");
        assert!(a1.is_set());

        // 旧轮结束前，同一会话的第二轮被拒（N1）——旧轮始终握着自己的取消入口
        assert!(reg.try_begin("a").is_none());
        assert!(reg.is_set("a"), "被拒的并发请求不得复位在跑一轮的标志");

        // 旧轮结束后重新开始：新标志是复位态，且旧轮 end 迟到也不注销新登记
        reg.end("a", &a1);
        let a2 = reg.try_begin("a").unwrap();
        assert!(!a2.is_set(), "新轮标志必须是复位态");
        assert!(a1.is_set(), "旧轮已收到的停止不得被新轮清掉（旧 clear() 的病根之一）");
        reg.end("a", &a1);
        assert_eq!(reg.active_count(), 1, "迟到的旧轮 end 不得注销新轮");
        assert!(reg.cancel("a"));
        assert!(a2.is_set());

        // 别的会话新一轮同样不影响 a
        let b = reg.try_begin("b").unwrap();
        assert!(a2.is_set() && !b.is_set(), "新会话不得复位他人已收到的停止");
    }

    #[test]
    fn cancel_all_hits_every_running_session() {
        let reg = CancelRegistry::new();
        let a = reg.try_begin("a").unwrap();
        let b = reg.try_begin("b").unwrap();
        let c = reg.try_begin("c").unwrap();
        assert_eq!(reg.cancel_all(), 3);
        assert!(a.is_set() && b.is_set() && c.is_set());
        assert!(reg.any_cancelled());
        // 幂等：再按一次不报错、命中数照常
        assert_eq!(reg.cancel_all(), 3);
        reg.end("b", &b);
        assert_eq!(reg.cancel_all(), 2);
    }
}
