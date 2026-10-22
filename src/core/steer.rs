//! 运行中收到的用户消息（steering / 排队 / 打断，施工单 025+027，2026-10-22 三级分家）
//!
//! 同一会话已有一轮在跑时，新来的用户消息**不拒收**，按三种模式进本会话的队列：
//!
//! - **排队（Queued，默认）**：等这一轮结束再作为下一轮发出去（服务层收尾取走）。
//!   网页端默认走这条：消息先显示成「排队中」，本轮跑完自动发。
//! - **引导（Steer）**：核心循环取走并入本轮对话（模型下一个决策点看到），
//!   原轮**不打断**、不另起一轮——「不阻断对话，但引导模型的回答」。取走点有两个：
//!   每步决策前，以及最终回答落地后（补流式输出期间到达的——决策点只在模型调用
//!   之前，不补取就会漏掉最后窗口、被收尾 drain_all 捡去当 followup 另起一轮，
//!   2026-09-27 用户实锤）。
//! - **打断（Interrupt，2026-10-22 用户定案「立即发送」）**：置位本会话的打断信号
//!   （一个 [CancelFlag]——审批等待/工具执行/流式输出三个等待点 select! 到它当场
//!   让位：挂起的审批按拒绝放行、跑着的工具按「已中断」回喂、流式回答半截落地），
//!   消息本身仍走引导路径并入本轮——打断不是停（停止才终止整轮），是「放下手里的、
//!   马上听我的」。信号在决策点取走打断消息时复位；若打断到达时没有任何在等的
//!   等待点（恰在缝隙里），信号置而复清，退化为一次引导，无害。
//!
//! 三种情况都**不**产生并发双轮：同一会话同时仍然只有一轮在跑（会话文件单写者、
//! 取消入口不被顶掉），这条不变量由 [super::cancel::CancelRegistry] 保证。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::cancel::CancelFlag;

/// 运行中消息的模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerMode {
    /// 排队：本轮结束后作为下一轮发出（服务层收尾 drain_all 取走）
    Queued,
    /// 引导：并入本轮，下一个决策点生效，不打断
    Steer,
    /// 打断（「立即发送」）：置打断信号让等待点让位，消息并入本轮
    Interrupt,
}

/// 队列里的一条消息。
#[derive(Debug, Clone)]
pub struct SteerMsg {
    pub text: String,
    pub mode: SteerMode,
}

#[derive(Default)]
pub struct SteerQueue {
    inner: Mutex<HashMap<String, Vec<SteerMsg>>>,
    /// 每会话的打断信号（「立即发送」置位；决策点取走打断消息时复位；收尾 drain_all 清除）。
    /// 与取消信号（CancelRegistry）平行传播：取消终止整轮，打断让位后本轮继续。
    interrupts: Mutex<HashMap<String, Arc<CancelFlag>>>,
}

impl SteerQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一条消息（服务层在「同会话已在跑」时调用）。返回积压条数。
    /// Interrupt 模式同时置位打断信号（三处等待点 select! 到它当场让位）。
    pub fn push(&self, session_id: &str, text: String, mode: SteerMode) -> usize {
        if mode == SteerMode::Interrupt {
            self.interrupt_flag(session_id).set();
        }
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let v = m.entry(session_id.to_string()).or_default();
        v.push(SteerMsg { text, mode });
        v.len()
    }

    /// 本会话的打断信号（取或建；等待方 select! 它，push(Interrupt) 置位它）。
    pub fn interrupt_flag(&self, session_id: &str) -> Arc<CancelFlag> {
        let mut m = self.interrupts.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(session_id.to_string()).or_insert_with(|| Arc::new(CancelFlag::new())).clone()
    }

    /// 本会话当前积压条数（防线用：超过上限说明客户端在循环重发，拒收新消息）。
    pub fn pending(&self, session_id: &str) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// 取走「引导/打断」的消息（核心循环每个决策点一次）；排队的留下等本轮结束。
    /// 取走打断消息时复位打断信号（指令已并入，让位使命完成）。
    /// 返回带 mode：消费方据此给消息打标记（引导=steered 静默，打断=interrupted 气泡）。
    pub fn drain_immediate(&self, session_id: &str) -> Vec<SteerMsg> {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(v) = m.get_mut(session_id) else { return Vec::new() };
        let mut now = Vec::new();
        let mut keep = Vec::new();
        let mut took_interrupt = false;
        for s in v.drain(..) {
            match s.mode {
                SteerMode::Queued => keep.push(s),
                SteerMode::Steer => now.push(s),
                SteerMode::Interrupt => {
                    now.push(s);
                    took_interrupt = true;
                }
            }
        }
        if keep.is_empty() {
            m.remove(session_id);
        } else {
            *v = keep;
        }
        drop(m);
        if took_interrupt {
            self.clear_interrupt(session_id);
        }
        now
    }

    /// 是否有「引导/打断」的消息在等（**不取走**）。审批等待期据此让位：用户插话 =
    /// 要改方向，挂着的审批按拒绝处理，消息仍留在队列里给决策点取。
    pub fn has_immediate(&self, session_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .map(|v| v.iter().any(|s| s.mode != SteerMode::Queued))
            .unwrap_or(false)
    }

    /// 复位打断信号（取走打断消息时调用；信号条目留给下次复用，drain_all 时清除）。
    fn clear_interrupt(&self, session_id: &str) {
        if let Some(f) = self
            .interrupts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
        {
            f.clear();
        }
    }

    /// 取走本会话全部消息（服务层收尾：一轮结束后按先到先得顺序接着跑），
    /// 并清掉打断信号条目（先复位再删——外部可能还持着旧 Arc，别让人看到置位残留）。
    /// 带 mode：followup 轮的消息按 引导=steered / 打断=interrupted 打标记（与
    /// 决策点并入同款），放回队列时也保真原 mode。
    pub fn drain_all(&self, session_id: &str) -> Vec<SteerMsg> {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let out = m.remove(session_id).unwrap_or_default();
        drop(m);
        let mut ints = self.interrupts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = ints.remove(session_id) {
            f.clear();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_drain_per_session_and_order_kept() {
        let q = SteerQueue::new();
        assert!(q.drain_all("a").is_empty(), "没登记过的会话取到空表");
        assert_eq!(q.push("a", "第一条".into(), SteerMode::Queued), 1);
        assert_eq!(q.push("a", "第二条".into(), SteerMode::Steer), 2);
        assert_eq!(q.push("b", "别人的".into(), SteerMode::Steer), 1);

        // 先到先得（模型按顺序看到），取走后本会话清空、其它会话不受影响；
        // drain_all 带 mode（followup 轮按它打标记）
        let took = q.drain_all("a");
        assert_eq!(took.len(), 2);
        assert_eq!((took[0].text.as_str(), took[0].mode), ("第一条", SteerMode::Queued));
        assert_eq!((took[1].text.as_str(), took[1].mode), ("第二条", SteerMode::Steer));
        assert!(q.drain_all("a").is_empty(), "取空后再取是空表，不报错");
        assert_eq!(q.drain_all("b")[0].text, "别人的", "别的会话的消息不得被顺带清掉");
    }

    #[test]
    fn immediate_drains_now_queued_waits_for_turn_end() {
        let q = SteerQueue::new();
        q.push("s", "排队一".into(), SteerMode::Queued);
        q.push("s", "立即一".into(), SteerMode::Steer);
        q.push("s", "排队二".into(), SteerMode::Queued);

        // 探针只看不动：有立即项 = true，且取不走队列
        assert!(q.has_immediate("s"));
        assert_eq!(q.pending("s"), 3, "has_immediate 不得消费消息");
        assert!(!q.has_immediate("别的会话"));
        q.drain_immediate("s");
        assert!(!q.has_immediate("s"), "立即项取走后探针为 false");
        assert_eq!(q.pending("s"), 2, "排队项不受影响");

        // 决策点只取「立即」的，且保持原有先后顺序；排队的留在队列里
        assert!(q.drain_immediate("s").is_empty(), "取完立即项后为空");
        // 收尾把剩下的全取走（先到先得：排队一 → 排队二）
        let left = q.drain_all("s");
        assert_eq!(left.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), vec!["排队一", "排队二"]);
        assert!(q.drain_all("s").is_empty());
    }

    /// 打断信号生命周期（2026-10-22「立即发送」）：push 置位 → 等待点 select! 让位 →
    /// 决策点取走打断消息时复位 → 复位后再 push 可再次置位 → 收尾清除。
    #[test]
    fn interrupt_signal_set_reset_and_reuse() {
        let q = SteerQueue::new();
        let flag = q.interrupt_flag("s");
        assert!(!flag.is_set(), "新建信号必须是复位态");

        q.push("s", "停下，听我说".into(), SteerMode::Interrupt);
        assert!(flag.is_set(), "打断式 push 必须置位信号");
        assert!(q.has_immediate("s"));

        // 决策点取走打断消息 → 信号复位（指令已并入本轮）
        let took = q.drain_immediate("s");
        assert_eq!(took.len(), 1);
        assert_eq!(took[0].text, "停下，听我说");
        assert_eq!(took[0].mode, SteerMode::Interrupt);
        assert!(!flag.is_set(), "取走打断消息必须复位信号");

        // 复用：同一条会话可以再次打断
        q.push("s", "又改主意了".into(), SteerMode::Interrupt);
        assert!(flag.is_set());
        q.drain_all("s");
        assert!(!flag.is_set(), "收尾 drain_all 清信号条目");

        // 打断消息只给本会话：别的会话置位不影响
        let f2 = q.interrupt_flag("other");
        q.push("s", "再打断".into(), SteerMode::Interrupt);
        assert!(!f2.is_set(), "别的会话的信号不得被连带置位");
    }
}
