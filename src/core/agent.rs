//! 核心循环 v1（施工单 003）
//!
//! 任务消息 → 模型（带 tools）→ 若返回 tool_calls 则执行回填继续；纯文本则终止。
//! 硬约束：步数上限（cfg.max_steps）；每步开始与每个流式分片前查取消标志。
//! thought / tool_call / tool_result 全量落 JSONL；on_event 把结构化事件转给前端。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::config::AgentConfig;
use crate::core::cancel::CancelFlag;
use crate::gateway::{ChatOutcome, Gateway, extract_delta};
use crate::observability::{self, EventType};
use crate::tools;

/// 循环向外发的结构化事件（SSE 转发用）。
pub enum AgentEvent {
    Content(String),
    Reasoning(String),
    ToolCall { name: String, args: String },
    ToolResult { name: String, ok: bool, preview: String },
    StepLimit(u32),
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

            let (content, ok) = match tools::execute(&call.name, &call.arguments, cfg.tool_result_max_chars) {
                Ok(s) => (s, true),
                Err(e) => (format!("工具执行失败: {e}"), false),
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
