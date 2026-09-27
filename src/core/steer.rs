//! 运行中收到的用户消息（steering / 排队，施工单 025+027）
//!
//! 同一会话已有一轮在跑时，新来的用户消息**不拒收**，按两种模式进本会话的队列：
//!
//! - **排队（immediate=false，默认）**：等这一轮结束再作为下一轮发出去（服务层收尾取走）。
//!   网页端默认走这条：消息先显示成「排队中」，本轮跑完自动发；想马上插话点「立即发送」。
//! - **立即/引导（immediate=true）**：核心循环在下一个决策点取走并入本轮对话（模型下一步
//!   就看到），原轮不被打断、不另起一轮——「不阻断对话，但引导模型的回答」。
//!
//! 两种情况都**不**产生并发双轮：同一会话同时仍然只有一轮在跑（会话文件单写者、
//! 取消入口不被顶掉），这条不变量由 [super::cancel::CancelRegistry] 保证。

use std::collections::HashMap;
use std::sync::Mutex;

/// 队列里的一条消息：`immediate=true` = 「立即发送」（并进正在跑的这一轮）。
#[derive(Debug, Clone)]
pub struct SteerMsg {
    pub text: String,
    pub immediate: bool,
}

#[derive(Default)]
pub struct SteerQueue {
    inner: Mutex<HashMap<String, Vec<SteerMsg>>>,
}

impl SteerQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一条消息（服务层在「同会话已在跑」时调用）。返回积压条数。
    pub fn push(&self, session_id: &str, text: String, immediate: bool) -> usize {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let v = m.entry(session_id.to_string()).or_default();
        v.push(SteerMsg { text, immediate });
        v.len()
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

    /// 取走「立即发送」的（核心循环每个决策点一次）；排队的留下等本轮结束。
    pub fn drain_immediate(&self, session_id: &str) -> Vec<String> {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(v) = m.get_mut(session_id) else { return Vec::new() };
        let mut now = Vec::new();
        let mut keep = Vec::new();
        for s in v.drain(..) {
            if s.immediate {
                now.push(s.text);
            } else {
                keep.push(s);
            }
        }
        if keep.is_empty() {
            m.remove(session_id);
        } else {
            *v = keep;
        }
        now
    }

    /// 取走本会话全部消息（服务层收尾：一轮结束后按先到先得顺序接着跑）。
    pub fn drain_all(&self, session_id: &str) -> Vec<String> {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        m.remove(session_id).unwrap_or_default().into_iter().map(|s| s.text).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_drain_per_session_and_order_kept() {
        let q = SteerQueue::new();
        assert_eq!(q.drain_all("a"), Vec::<String>::new(), "没登记过的会话取到空表");
        assert_eq!(q.push("a", "第一条".into(), false), 1);
        assert_eq!(q.push("a", "第二条".into(), true), 2);
        assert_eq!(q.push("b", "别人的".into(), true), 1);

        // 先到先得（模型按顺序看到），取走后本会话清空、其它会话不受影响
        assert_eq!(q.drain_all("a"), vec!["第一条".to_string(), "第二条".to_string()]);
        assert_eq!(q.drain_all("a"), Vec::<String>::new(), "取空后再取是空表，不报错");
        assert_eq!(q.drain_all("b"), vec!["别人的".to_string()], "别的会话的消息不得被顺带清掉");
    }

    #[test]
    fn immediate_drains_now_queued_waits_for_turn_end() {
        let q = SteerQueue::new();
        q.push("s", "排队一".into(), false);
        q.push("s", "立即一".into(), true);
        q.push("s", "排队二".into(), false);

        // 决策点只取「立即」的，且保持原有先后顺序；排队的留在队列里
        assert_eq!(q.drain_immediate("s"), vec!["立即一".to_string()]);
        assert_eq!(q.drain_immediate("s"), Vec::<String>::new(), "取完立即项后为空");
        // 收尾把剩下的全取走（先到先得：排队一 → 排队二）
        assert_eq!(q.drain_all("s"), vec!["排队一".to_string(), "排队二".to_string()]);
        assert_eq!(q.drain_all("s"), Vec::<String>::new());
    }
}
