use super::*;

/// 执行一个工具调用——`subagent_spawn` 在核心循环拦截交给派发器
/// （子代理跑在独立会话，实现见 server/subagent.rs）；其余走工具注册表。
pub(crate) async fn exec_tool_call(
    cfg: &crate::tools::ExecConfig,
    name: &str,
    args: &str,
    subagents: Option<&Arc<dyn crate::core::subagent::SubagentRunner>>,
    parent_session: &str,
) -> Result<String, String> {
    if name == "subagent_spawn" {
        return match subagents {
            Some(runner) => Ok(runner.run(parent_session, args).await),
            None => Err("子代理派发未启用（本任务没有派发器）".to_string()),
        };
    }
    tools::execute(cfg, name, args).await
}

/// 坑 31 ① 收口：工具执行统一过取消闸——工具卡住（浏览器 CDP 调用超时 ~30s、
/// 长命令未返回等）时，用户点「结束回答」当场中断，不再干等工具返回（此前只靠
/// 下个决策点轮询 is_set，停止要等工具自己超时，期间服务端仍显示在跑、新消息
/// 被排进队列——用户实测实锤）。select! 丢掉工具 future 即返回；被丢的底层操作
/// （沙箱子进程、CDP 连接等）按自身超时自终，不会复活本轮。
pub(crate) async fn exec_with_cancel<E>(cancel: &CancelFlag, exec: E) -> Result<String, String>
where
    E: Future<Output = Result<String, String>>,
{
    tokio::select! {
        r = exec => r,
        () = cancel.cancelled() => Err("已取消：用户点了结束回答".to_string()),
    }
}

/// 工具参数压成一行短 reason（写文件等大参数只留头部）。
pub(crate) fn short_args(args: &str, max_chars: usize) -> String {
    let flat: String = args.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    flat.chars().take(max_chars).collect()
}

/// A3（施工单 012）：能力标记「工具调用」闭环的核心判定——tools_enabled=false 时不带
/// tools 参数。独立成纯函数便于单测。
pub(crate) fn tools_def_for(params: &ChatParams) -> Option<Value> {
    if params.tools_enabled {
        Some(Value::Array(tools::definitions()))
    } else {
        None
    }
}
