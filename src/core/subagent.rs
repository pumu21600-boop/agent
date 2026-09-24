//! P22-C 子代理调度：核心循环的派发接口（trait 只此一个，实现见 server/subagent.rs）
//!
//! 纪律（docs/subagent-draft.md）：子代理 = 独立会话，主会话「一会话一轮」不变量
//! 一根手指都不碰。核心循环只认这个 trait——派发、并发度、取消级联、成本归属的
//! 全部实现都在服务层，核心零感知。

use std::pin::Pin;
use std::future::Future;

/// 子代理派发入口（由服务层实现并挂进 `LoopChannels::subagents`）。
/// `parent_session` = 派发方会话 id（成本归属 / 取消级联 / 并发度都按它算）；
/// `args_json` = 模型给的工具参数。返回值即回喂给父模型的工具结果文本。
pub trait SubagentRunner: Send + Sync {
    fn run(
        &self,
        parent_session: &str,
        args_json: &str,
    ) -> Pin<Box<dyn Future<Output = String> + Send + '_>>;
}
