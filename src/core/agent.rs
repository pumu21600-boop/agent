//! 核心循环 v1（施工单 003）
//!
//! 任务消息 → 模型（带 tools）→ 若返回 tool_calls 则执行回填继续；纯文本则终止。
//! 硬约束：步数上限（cfg.max_steps）；每步开始与每个流式分片前查取消标志。
//! thought / tool_call / tool_result 全量落 JSONL；on_event 把结构化事件转给前端。

use std::{future::Future, pin::Pin, sync::Arc};

use serde_json::{Value, json};

use crate::config::{AgentConfig, ContextConfig, RecoveryConfig, SandboxConfig};
use crate::context;
use crate::core::cancel::CancelFlag;
use crate::gateway::{ChatOutcome, Gateway, extract_delta};
use crate::observability::{self, EventType};
use crate::recovery;
use crate::tools::{self, ExecConfig};

/// 循环向外发的结构化事件（SSE 转发用）。
pub enum AgentEvent {
    Content(String),
    Reasoning(String),
    ToolCall { name: String, args: String },
    ToolResult { name: String, ok: bool, preview: String },
    StepLimit(u32),
    /// 上下文占用与分类占比（每轮模型调用前更新；P5 圆环数据源）
    ContextUsage { tokens: u64, budget: u64, breakdown: context::Breakdown },
    /// 渠道返回的真实用量（P5：缓存命中率展示）
    ApiUsage { prompt_tokens: u64, cached_tokens: Option<u64> },
    /// 会话快照（P6）：当前全量消息（不含 system），server 收到即落盘
    Snapshot { messages: Vec<Value> },
    /// 提示性通知（P6：自动 HANDOFF 等），前端以系统消息展示
    Notice(String),
    /// 任务改动统计条（P6.1）：相对任务起点基线的文件数/增删行；「撤销」恢复到 id
    Checkpoint { id: String, files_changed: usize, additions: u64, deletions: u64 },
    /// P7 滚动摘要更新：server 收到即写进会话文件（跨任务延续）
    Summary(String),
}

/// P7 滚动摘要消息的前缀标记（注入/剥离/快照过滤都认它）。
pub const SUMMARY_PREFIX: &str = "【历史摘要】";

fn is_summary_msg(m: &Value) -> bool {
    m.get("content").and_then(Value::as_str).map(|c| c.starts_with(SUMMARY_PREFIX)).unwrap_or(false)
}

/// 快照用：去掉 system 头与摘要消息（摘要由会话文件 summary 字段单独持久化）。
fn snapshot_msgs(msgs: &[Value]) -> Vec<Value> {
    msgs.iter().skip(1).filter(|m| !is_summary_msg(m)).cloned().collect()
}

/// P7 滚动摘要：折叠内容 + 旧摘要 → 模型压成 ≤300 字要点。失败返回 None（调用方降级）。
async fn summarize_folded(
    gateway: &Arc<Gateway>,
    base_url: &str,
    model: &str,
    api_key: &str,
    prior: &Option<String>,
    folded: &[Value],
) -> Option<String> {
    let mut text = String::new();
    if let Some(p) = prior {
        text.push_str(&format!("已有摘要：{p}\n\n"));
    }
    text.push_str("新增对话：\n");
    for m in folded {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("?");
        let body: String = match m.get("content") {
            Some(Value::String(s)) => s.chars().take(500).collect(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string().chars().take(500).collect(),
        };
        let calls = m
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.pointer("/function/name").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        text.push_str(&format!("{role}: {body}{}\n", if calls.is_empty() { String::new() } else { format!(" [调用工具: {calls}]") }));
        if text.chars().count() > 8000 {
            break;
        }
    }
    let req = json!([
        {"role": "system", "content": "你是对话历史压缩器。把给定对话压成不超过300字中文要点，必须保留：任务目标、关键决定、改过哪些文件、未完成事项。只输出要点正文，不要开场白。"},
        {"role": "user", "content": text},
    ]);
    let out = gateway.complete(base_url, model, api_key, &req, 400).await.ok()?;
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

/// 任务结束时的改动统计条：先补一次「结束快照」提交（让任务内新建文件也进影子仓库），
/// 再算相对基线的 numstat；无改动不发。撤销 = 影子仓库 reset --hard 回基线。
async fn emit_checkpoint_bar(
    baseline: Option<String>,
    on_event: &(dyn Fn(AgentEvent) + Send + Sync),
) {
    let Some(base) = baseline else { return };
    let ws = tools::workspace_root().to_path_buf();
    let res = tokio::task::spawn_blocking({
        let base = base.clone();
        move || -> anyhow::Result<(usize, u64, u64)> {
            recovery::checkpoint(&ws, "任务结束快照")?;
            recovery::diff_stat(&ws, &base)
        }
    })
    .await;
    match res {
        Ok(Ok((files, adds, dels))) if files > 0 => {
            observability::event(
                EventType::State,
                json!({ "kind": "checkpoint.summary", "baseline": base, "files": files, "additions": adds, "deletions": dels }),
            );
            on_event(AgentEvent::Checkpoint { id: base, files_changed: files, additions: adds, deletions: dels });
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => observability::event(
            EventType::State,
            json!({ "kind": "checkpoint.skipped", "reason": e.to_string() }),
        ),
        Err(e) => observability::event(
            EventType::State,
            json!({ "kind": "checkpoint.skipped", "reason": format!("任务失败: {e}") }),
        ),
    }
}

/// 审批三档判定（Codex 式，施工单 005/006）：
/// - read-only（只读）：查询类自动，写文件/执行命令要批
/// - auto（自动）：沙箱内全自动（等效 Codex workspace-write）
/// - full-access（完全访问）：脱沙箱全自动（审批策略由 gate 按 unsandboxed_approval 决定）
/// 旧档位名 suggest/auto-edit/full-auto/local 在 server 层归一化后映射进来。
pub fn needs_approval(mode: &str, danger_level: u8) -> bool {
    match mode {
        "auto" | "full-access" | "full-auto" | "local" => false,
        "auto-edit" => danger_level >= 2,
        _ => danger_level >= 1, // read-only / suggest（默认兜底）
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

/// 工具参数压成一行短 reason（写文件等大参数只留头部）。
fn short_args(args: &str, max_chars: usize) -> String {
    let flat: String = args.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    flat.chars().take(max_chars).collect()
}

/// 执行一个任务：messages 为前端带来的对话历史（不含 system）。
#[allow(clippy::too_many_arguments)]
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
    ctx_cfg: &ContextConfig,
    rec_cfg: &RecoveryConfig,
    mem: &crate::memory::Memory,
    prior_summary: Option<String>,
    max_output_tokens: Option<u64>,
    gate: &dyn ApprovalGate,
    on_event: impl Fn(AgentEvent) + Send + Sync,
) -> anyhow::Result<LoopOutcome> {
    let mut msgs: Vec<Value> = Vec::with_capacity(messages.len() + 1);
    msgs.push(json!({ "role": "system", "content": system_prompt }));
    msgs.extend(messages.iter().cloned());
    // P6 会话落盘：任务一开始就把用户消息存住（崩溃也丢不了这轮请求）
    on_event(AgentEvent::Snapshot { messages: snapshot_msgs(&msgs) });
    // P7 事件流水：任务开始
    mem.record(
        "task.start",
        json!({ "text": messages.iter().rev().find(|m| m.get("role").and_then(Value::as_str) == Some("user")).and_then(|m| m.get("content")).and_then(Value::as_str).map(|s| s.chars().take(200).collect::<String>()).unwrap_or_default() }),
    );
    // P7 滚动摘要：跨任务延续的当前摘要
    let mut running_summary = prior_summary.filter(|s| !s.trim().is_empty());
    // P6.1 改动统计条基线：任务起点当场拍一个快照（提交当前工作树状态），
    // 结束时的 diff 就只包含本任务的改动，不会把历史漂移算进来
    let baseline: Option<String> = if rec_cfg.checkpoint_enabled {
        let ws = tools::workspace_root().to_path_buf();
        tokio::task::spawn_blocking(move || -> Option<String> {
            let _ = recovery::checkpoint(&ws, "任务起点");
            recovery::head_id(&ws).ok().flatten()
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    // P6 自动交接：本任务内只触发一次
    let mut handoff_done = false;

    let tools_def = Value::Array(tools::definitions());
    let mut prev_tool_sigs: Vec<String> = Vec::new();

    for step in 0..cfg.max_steps {
        // 每轮模型决策前查取消标志（核心循环刚性检查点）
        if cancel.is_set() {
            observability::event(EventType::State, json!({ "kind": "cancel", "phase": "loop", "step": step }));
            on_event(AgentEvent::Snapshot { messages: snapshot_msgs(&msgs) });
            emit_checkpoint_bar(baseline, &on_event).await;
            return Ok(LoopOutcome::Cancelled);
        }
        // 上下文预算（P5）：超预算则归档旧工具结果/折叠旧轮次；占用推给前端圆环
        {
            // P7：摘要消息每步重新注入，先剥掉上一轮的（不参与裁剪统计）
            msgs.retain(|m| !is_summary_msg(m));
            let (mut compacted, stats) = context::compact_messages(&msgs, ctx_cfg);
            if stats.compacted {
                observability::event(
                    EventType::State,
                    json!({
                        "kind": "context.compacted",
                        "step": step,
                        "archived_tools": stats.archived_tools,
                        "folded_messages": stats.folded_messages,
                        "tokens_before": stats.tokens_before,
                        "tokens_after": stats.tokens_after,
                    }),
                );
                // P7 滚动摘要：有折叠 → 调模型把折叠内容压进摘要（失败保留占位符降级）
                if !stats.folded.is_empty() && mem.enabled {
                    match summarize_folded(gateway, base_url, model, api_key, &running_summary, &stats.folded).await {
                        Some(s) => {
                            running_summary = Some(s.clone());
                            observability::event(
                                EventType::State,
                                json!({ "kind": "memory.summarized", "step": step, "chars": s.chars().count() }),
                            );
                            on_event(AgentEvent::Summary(s));
                        }
                        None => observability::event(
                            EventType::State,
                            json!({ "kind": "memory.summary_skipped", "step": step, "note": "摘要调用失败，保留占位符" }),
                        ),
                    }
                }
            }
            // P7：把当前摘要作为 system 消息紧跟主 system 注入（不进快照、不参与折叠）
            if let Some(s) = &running_summary {
                compacted.insert(1, json!({ "role": "system", "content": format!("{SUMMARY_PREFIX}{s}") }));
            }
            msgs = compacted;
        }
        // 圆环数据源：分类占比推给前端（裁剪后取最终值，与预算口径一致）
        let breakdown = context::estimate_breakdown(&msgs);
        let used = breakdown.total();
        on_event(AgentEvent::ContextUsage {
            tokens: used,
            budget: ctx_cfg.budget_tokens,
            breakdown,
        });
        // P6 自动交接：占用过阈值 → 生成交接文档（每任务一次），下一会话可接续
        if !handoff_done && used > 0 {
            let ratio = rec_cfg.handoff_token_ratio.clamp(0.05, 1.0);
            let threshold = (ctx_cfg.budget_tokens as f32 * ratio) as u64;
            if threshold > 0 && used >= threshold {
                handoff_done = true;
                let pct = ((used as f32 / ctx_cfg.budget_tokens as f32) * 100.0).round() as u64;
                let ws = tools::workspace_root().to_path_buf();
                let session_id = observability::get().session_id().to_string();
                let trigger =
                    format!("自动：上下文占用 {pct}%（阈值 {}%）", (ratio * 100.0).round() as u64);
                let task_msgs = snapshot_msgs(&msgs);
                let res = tokio::task::spawn_blocking(move || {
                    recovery::write_handoff(
                        &ws,
                        &recovery::HandoffInput {
                            trigger,
                            session_id,
                            messages: Some(task_msgs),
                        },
                    )
                })
                .await;
                match res {
                    Ok(Ok(path)) => {
                        observability::event(
                            EventType::State,
                            json!({ "kind": "handoff.written", "path": path.display().to_string(), "trigger": "auto", "context_pct": pct }),
                        );
                        mem.record("handoff.written", json!({ "trigger": "auto", "context_pct": pct }));
                        on_event(AgentEvent::Notice(format!(
                            "上下文占用 {pct}%，已自动生成交接文档 {}",
                            path.display()
                        )));
                    }
                    Ok(Err(e)) => {
                        observability::event(EventType::Error, json!({ "message": format!("自动 HANDOFF 生成失败: {e}") }))
                    }
                    Err(e) => {
                        observability::event(EventType::Error, json!({ "message": format!("自动 HANDOFF 任务失败: {e}") }))
                    }
                }
            }
        }
        observability::debug(&format!("[loop] step {step} 调用模型 {model}"));

        let outcome = gateway
            .stream_chat(
                base_url,
                model,
                api_key,
                &Value::Array(msgs.clone()),
                Some(&tools_def),
                max_output_tokens,
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

        // 渠道真实用量（缓存命中率展示；渠道不支持时为空）
        if let Some(p) = turn.prompt_tokens {
            on_event(AgentEvent::ApiUsage { prompt_tokens: p, cached_tokens: turn.cached_tokens });
        }

        // 思考完整段落落 thought（分片级已记 model_chunk）
        if !turn.reasoning.is_empty() {
            observability::event(EventType::Thought, json!({ "step": step, "text": turn.reasoning }));
        }

        // 没有工具调用 → 纯文本即最终回答（已流式转发），终止
        if turn.tool_calls.is_empty() {
            // 兜底：弱模型/免费模型常只给 reasoning 不给 content，避免前端空回答
            let final_text = if turn.content.trim().is_empty() && !turn.reasoning.trim().is_empty() {
                observability::event(
                    EventType::Error,
                    json!({ "message": "模型未输出正文，回退展示思考内容" }),
                );
                on_event(AgentEvent::Content(turn.reasoning.clone()));
                turn.reasoning.clone()
            } else {
                turn.content.clone()
            };
            // P6：最终回答写入会话并落盘（与前端看到的一致）
            msgs.push(json!({ "role": "assistant", "content": final_text }));
            on_event(AgentEvent::Snapshot { messages: snapshot_msgs(&msgs) });
            emit_checkpoint_bar(baseline, &on_event).await;
            observability::event(
                EventType::State,
                json!({ "kind": "loop.finish", "step": step, "content_chars": turn.content.chars().count() }),
            );
            // P7 事件流水：任务完成
            mem.record("task.finish", json!({ "steps": step + 1, "chars": final_text.chars().count() }));
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
                on_event(AgentEvent::Snapshot { messages: snapshot_msgs(&msgs) });
                emit_checkpoint_bar(baseline, &on_event).await;
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
            let approved = if gate.needs_approval(danger) {
                gate.ask(&call.name, &call.arguments).await
            } else {
                true
            };
            let (content, ok) = if !approved {
                ("用户拒绝了该操作，工具未执行。请换别的做法或直接回答用户。".to_string(), true)
            } else {
                // P6：写类/执行类工具动手前先拍影子仓库检查点；失败只记日志，绝不阻断
                if rec_cfg.checkpoint_enabled && danger >= 1 {
                    let reason = format!("{} {}", call.name, short_args(&call.arguments, 100));
                    let ws = tools::workspace_root().to_path_buf();
                    match tokio::task::spawn_blocking(move || recovery::checkpoint(&ws, &reason)).await {
                        Ok(Ok(Some(cp))) => {
                            observability::event(
                                EventType::State,
                                json!({ "kind": "checkpoint.created", "id": cp.id, "ts": cp.ts, "reason": cp.reason }),
                            );
                            mem.record("checkpoint.created", json!({ "reason": cp.reason }));
                        }
                        Ok(Ok(None)) => {}
                        Ok(Err(e)) => observability::event(
                            EventType::State,
                            json!({ "kind": "checkpoint.skipped", "reason": e.to_string() }),
                        ),
                        Err(e) => observability::event(
                            EventType::State,
                            json!({ "kind": "checkpoint.skipped", "reason": format!("任务失败: {e}") }),
                        ),
                    }
                }
                match tools::execute(&exec_cfg, &call.name, &call.arguments).await {
                    Ok(s) => (s, true),
                    Err(e) => (format!("工具执行失败: {e}"), false),
                }
            };

            let preview: String = content.chars().take(300).collect();
            let truncated = content.contains(context::OMITTED_MARK);
            observability::event(
                EventType::ToolResult,
                json!({ "step": step, "id": call.id, "name": call.name, "ok": ok, "chars": content.chars().count(), "truncated": truncated, "preview": preview }),
            );
            on_event(AgentEvent::ToolResult { name: call.name.clone(), ok, preview });
            // P7 事件流水：工具调用结果
            mem.record("tool.call", json!({ "name": call.name, "danger": danger, "ok": ok }));
            msgs.push(json!({ "role": "tool", "tool_call_id": call.id, "content": content }));
        }
        // P6 会话落盘：本步工具回填完毕，状态常驻磁盘
        on_event(AgentEvent::Snapshot { messages: snapshot_msgs(&msgs) });
    }

    observability::event(EventType::Error, json!({ "message": format!("已达步数上限 {}，强制终止", cfg.max_steps) }));
    on_event(AgentEvent::StepLimit(cfg.max_steps));
    Ok(LoopOutcome::Done)
}
