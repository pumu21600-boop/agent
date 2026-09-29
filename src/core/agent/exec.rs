use super::*;

/// 是否构成「重复调用死循环」（纯函数）：两步工具调用签名完全相同，且上一轮**不是**
/// 全部失败。失败后重试是正当行为——超时/后端重置后状态可能已变，模型重试一次
/// 理应放行（用户反馈：navigate 超时被当场判死循环，用户只看到天书横幅）；
/// 真正要防的是对成功调用的重复请求，那种由步数上限兜底。
pub(crate) fn is_repeat_loop(prev: &[String], cur: &[String], prev_all_failed: bool) -> bool {
    !prev.is_empty() && prev == cur && !prev_all_failed
}

/// P22-C：执行一个工具调用——`subagent_spawn` 在核心循环拦截交给派发器
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
