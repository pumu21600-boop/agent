//! 核心循环 v1（施工单 003）
//!
//! 任务消息 → 模型（带 tools）→ 若返回 tool_calls 则执行回填继续；纯文本则终止。
//! 硬约束：步数上限（cfg.max_steps）；每步开始与每个流式分片前查取消标志。
//! thought / tool_call / tool_result 全量落 JSONL；on_event 把结构化事件转给前端。

use std::{future::Future, pin::Pin, sync::Arc};

use serde_json::{Value, json};

use crate::config::{AgentConfig, SandboxConfig};
use crate::core::cancel::CancelFlag;
use crate::gateway::{ChatOutcome, Gateway, extract_delta};
use crate::observability::{self, EventType};
use crate::tools::{self, ExecConfig};

/// 循环向外发的结构化事件（SSE 转发用）。
pub enum AgentEvent {
    Content(String),
    Reasoning(String),
    ToolCall { name: String, args: String },
    ToolResult { name: String, ok: bool, preview: String },
    StepLimit(u32),
}

/// 审批三档判定（施工单 005）：suggest=写/执行都要批；auto-edit=只批执行；full-auto=全自动。
/// local（本机直跑）的审批策略由 gate 按 unsandboxed_approval 配置决定。
pub fn needs_approval(mode: &str, danger_level: u8) -> bool {
    match mode {
        "full-auto" | "local" => false,
        "auto-edit" => danger_level >= 2,
        _ => danger_level >= 1, // suggest（默认兜底）
    }
}

/// 审批门：由 server 层实现（SSE 发审批请求、挂 oneshot 等决定）。
pub trait ApprovalGate: Send + Sync {
    /// 当前档位下该危险等级是否要审批（档位可被侧栏即时切换，实现方读活值）
    fn needs_approval(&self, danger_level: u8) -> bool;
    /// 本档位是否还在沙箱里跑（local 档 = false，前提是 config 总开关已开）
    fn sandbox_enabled(&self) -> bool;
    /// 等待用户决定；true=允许。实现内部处理超时（视为拒绝）与取消。
    fn ask(&self, tool: &str, args: &str) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopOutcome {
    Done,
    Cancelled,
}

/// 执行一个任务：messages 为前端带来的对话历史（不含 system）。
pub async fn run_task(
    gateway: &Arc<Gateway>,
    base_url: &str,
    model: &str,
    api_key: &str,
    system_prompt: &str,
    messages: &[Value],
    cancel: &CancelFlag,
    cfg: &AgentConfig,
    sandbox_cfg: &SandboxConfig,
    gate: &dyn ApprovalGate,
    on_event: impl Fn(AgentEvent),
) -> anyhow::Result<LoopOutcome> {
    let mut msgs: Vec<Value> = Vec::with_capacity(messages.len() + 1);
    msgs.push(json!({ "role": "system", "content": system_prompt }));
    msgs.extend(messages.iter().cloned());

    let tools_def = Value::Array(tools::definitions());
    let mut prev_tool_sigs: Vec<String> = Vec::new();

    for step in 0..cfg.max_steps {
        // 每轮模型决策前查取消标志（核心循环刚性检查点）
        if cancel.is_set() {
            observability::event(EventType::State, json!({ "kind": "cancel", "phase": "loop", "step": step }));
            return Ok(LoopOutcome::Cancelled);
        }
        observability::debug(&format!("[loop] step {step} 调用模型 {model}"));

        let outcome = gateway
            .stream_chat(
                base_url,
                model,
                api_key,
                &Value::Array(msgs.clone()),
                Some(&tools_def),
                cancel,
                |chunk| {
                    let delta = extract_delta(chunk);
                    if !delta.content.is_empty() {
                        on_event(AgentEvent::Content(delta.content));
                    }
                    if let Some(r) = delta.reasoning {
                        if !r.is_empty() {
                            on_event(AgentEvent::Reasoning(r));
                        }
                    }
                },
            )
            .await?;

        let turn = match outcome {
            ChatOutcome::Cancelled => return Ok(LoopOutcome::Cancelled),
            ChatOutcome::Done(t) => t,
        };

        // 思考完整段落落 thought（分片级已记 model_chunk）
        if !turn.reasoning.is_empty() {
            observability::event(EventType::Thought, json!({ "step": step, "text": turn.reasoning }));
        }

        // 没有工具调用 → 纯文本即最终回答（已流式转发），终止
        if turn.tool_calls.is_empty() {
            // 兜底：弱模型/免费模型常只给 reasoning 不给 content，避免前端空回答
            if turn.content.trim().is_empty() && !turn.reasoning.trim().is_empty() {
                observability::event(
                    EventType::Error,
                    json!({ "message": "模型未输出正文，回退展示思考内容" }),
                );
                on_event(AgentEvent::Content(turn.reasoning.clone()));
            }
            observability::event(
                EventType::State,
                json!({ "kind": "loop.finish", "step": step, "content_chars": turn.content.chars().count() }),
            );
            return Ok(LoopOutcome::Done);
        }

        // 护栏：模型连续两步请求完全相同的工具调用 → 死循环，立即终止
        let sigs: Vec<String> =
            turn.tool_calls.iter().map(|c| format!("{}|{}", c.name, c.arguments)).collect();
        if prev_tool_sigs == sigs {
            observability::event(
                EventType::Error,
                json!({ "message": "模型重复请求相同工具调用，已终止循环" }),
            );
            return Err(anyhow::anyhow!("模型重复请求相同工具调用，已终止"));
        }
        prev_tool_sigs = sigs;

        // 回填 assistant 消息（含 tool_calls）
        let calls_json: Vec<Value> = turn
            .tool_calls
            .iter()
            .map(|c| {
                json!({
                    "id": c.id,
                    "type": "function",
                    "function": { "name": c.name, "arguments": c.arguments },
                })
            })
            .collect();
        msgs.push(json!({
            "role": "assistant",
            "content": if turn.content.is_empty() { Value::Null } else { json!(turn.content) },
            "tool_calls": calls_json,
        }));

        // 逐个执行工具
        let exec_cfg = ExecConfig {
            sandbox: sandbox_cfg.clone(),
            max_chars: cfg.tool_result_max_chars,
            unsandboxed: !gate.sandbox_enabled(),
        };
        for call in turn.tool_calls {
            if cancel.is_set() {
                observability::event(EventType::State, json!({ "kind": "cancel", "phase": "tool", "step": step }));
                return Ok(LoopOutcome::Cancelled);
            }
            observability::event(
                EventType::ToolCall,
                json!({ "step": step, "id": call.id, "name": call.name, "arguments": call.arguments }),
            );
            on_event(AgentEvent::ToolCall { name: call.name.clone(), args: call.arguments.clone() });
            observability::debug(&format!("[loop] 工具 {} 参数 {}", call.name, call.arguments));

            // 审批三档：按档位与工具危险等级决定是否等用户点头
            // （审批请求的 SSE/JSONL/等待全部由 gate 单点负责，避免双事件双 id）
            let danger = tools::find(&call.name).map(|s| s.danger_level).unwrap_or(2);
            let (content, ok) = if gate.needs_approval(danger) {
                let approved = gate.ask(&call.name, &call.arguments).await;
                if !approved {
                    ("用户拒绝了该操作，工具未执行。请换别的做法或直接回答用户。".to_string(), true)
                } else {
                    match tools::execute(&exec_cfg, &call.name, &call.arguments).await {
                        Ok(s) => (s, true),
                        Err(e) => (format!("工具执行失败: {e}"), false),
                    }
                }
            } else {
                match tools::execute(&exec_cfg, &call.name, &call.arguments).await {
                    Ok(s) => (s, true),
                    Err(e) => (format!("工具执行失败: {e}"), false),
                }
            };

            let preview: String = content.chars().take(300).collect();
            observability::event(
                EventType::ToolResult,
                json!({ "step": step, "id": call.id, "name": call.name, "ok": ok, "chars": content.chars().count(), "preview": preview }),
            );
            on_event(AgentEvent::ToolResult { name: call.name.clone(), ok, preview });
            msgs.push(json!({ "role": "tool", "tool_call_id": call.id, "content": content }));
        }
    }

    observability::event(EventType::Error, json!({ "message": format!("已达步数上限 {}，强制终止", cfg.max_steps) }));
    on_event(AgentEvent::StepLimit(cfg.max_steps));
    Ok(LoopOutcome::Done)
}
