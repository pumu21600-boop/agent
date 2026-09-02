//! 全局取消标志 —— 施工单 001 / 任务 C
//!
//! 本单只做「壳」：标志 + 端点 + 按钮，能点、能置位、能查。
//! 核心循环（下一单实现）约定：每轮模型决策前调用 [CancelFlag::is_set]，
//! 已置位立即终止当前轮；新一轮开始时 clear()。

use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Default)]
pub struct CancelFlag(AtomicBool);

impl CancelFlag {
    pub fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// POST /cancel 置位。
    pub fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// 新一轮开始时复位（单会话、无历史：上一轮的取消不带到下一轮）。
    pub fn clear(&self) {
        self.0.store(false, Ordering::SeqCst);
    }

    /// 网关每个决策点、每个流式分片前检查。
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
