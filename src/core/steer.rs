//! 运行中追加消息（steering，施工单 025）
//!
//! 同一会话已有一轮在跑时，新来的用户消息**不拒收**：进本会话的追加队列，
//! 核心循环在下一个决策点取走并进对话（模型下一步就看到），原轮不被打断。
//! 一轮结束时还没被消费的（最后一轮决策点之后才到的），由服务层紧接着
//! 再起一轮（同一 SSE 流），保证「发出去的消息一定被处理」而不是被丢掉。
//!
//! 与 [super::cancel::CancelRegistry] 的分工：取消标志管「怎么停」，本队列管
//! 「运行中加话」；两者都按 `session_id` 归属，同一会话同时仍然只有一轮在跑
//! （不做并发双轮——会话文件是单写者，这条不变量由 CancelRegistry 保证）。

use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct SteerQueue {
    inner: Mutex<HashMap<String, Vec<String>>>,
}

impl SteerQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一条消息（服务层在「同会话已在跑」时调用）。返回积压条数。
    pub fn push(&self, session_id: &str, text: String) -> usize {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let v = m.entry(session_id.to_string()).or_default();
        v.push(text);
        v.len()
    }

    /// 取走本会话的全部追加消息（核心循环每个决策点一次；服务层收尾一次）。
    pub fn drain(&self, session_id: &str) -> Vec<String> {
        let mut m = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        m.remove(session_id).unwrap_or_default()
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_drain_per_session_and_order_kept() {
        let q = SteerQueue::new();
        assert_eq!(q.drain("a"), Vec::<String>::new(), "没登记过的会话取到空表");
        assert_eq!(q.push("a", "第一条".into()), 1);
        assert_eq!(q.push("a", "第二条".into()), 2);
        assert_eq!(q.push("b", "别人的".into()), 1);

        // 先到先得（模型按顺序看到），drain 后本会话清空、其它会话不受影响
        assert_eq!(q.drain("a"), vec!["第一条".to_string(), "第二条".to_string()]);
        assert_eq!(q.drain("a"), Vec::<String>::new(), "取空后再取是空表，不报错");
        assert_eq!(q.drain("b"), vec!["别人的".to_string()], "别的会话的追加消息不得被顺带清掉");
    }
}
