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
use crate::gateway::{Channel, ChatOutcome, ChatParams, Gateway, UsageCtx, extract_delta};
use crate::observability::{self, EventType};
use crate::recovery;
use crate::tools::{self, ExecConfig};


mod checkpoint;
mod exec;
mod plan;
mod summary;

pub(crate) use checkpoint::*;
pub(crate) use exec::*;
pub use plan::*;
pub(crate) use summary::*;
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
    /// 本步作废重来（P15/022 降级链）：主渠道流到一半失败、换渠道重试时发出。
    /// 前端应把当前消息尾部各丢掉 `content` / `reasoning` 个 UTF-16 码元
    /// （与 JS String.length 同口径），否则半截回答会与重试后的完整回答叠在一起。
    StepReset { content: usize, reasoning: usize },
    /// P8 TITLE 角色：会话标题已生成（server 收到即改会话文件并转前端）
    Title(String),
    /// 任务改动统计条（P6.1）：相对任务起点基线的文件数/增删行；「撤销」恢复到 id
    Checkpoint { id: String, files_changed: usize, additions: u64, deletions: u64 },
    /// P7 滚动摘要更新：server 收到即写进会话文件（跨任务延续）
    Summary(String),
    /// P9 B3：Plan 档产出的结构化计划（模型最终回答中的 ```plan 块），前端渲染计划卡
    PlanProposal { goal: String, steps: Vec<String>, files: Vec<String> },
    /// N1/025 运行中追加消息（steering）：这些用户消息已并入本轮对话（模型下一步可见），
    /// server 收到即转前端补气泡 + 落盘。取走点有两个：每步决策前（take_steering）+
    /// 最终回答落地后（补流式输出期间到达的，见主循环 027 补口）。
    Steer { messages: Vec<Value> },
}

/// P9 B3：Plan 档解析出的结构化计划。
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub goal: String,
    pub steps: Vec<String>,
    pub files: Vec<String>,
}

/// P7 滚动摘要消息的前缀标记（注入/剥离/快照过滤都认它）。
pub const SUMMARY_PREFIX: &str = "【历史摘要】";

/// P9 B3：Plan 档 system prompt 增量。
/// 红线（坑 13/施工单 013 红线 9）：不注入模型身份/人设——这里是纯任务行为约束与计划格式约定。
pub const PLAN_PROMPT: &str = "\
[Plan 模式]\n\
当前处于 Plan（规划）档：只允许只读类操作（查看文件、搜索代码、只读命令）。写文件与改动类命令会被直接拒绝，请勿尝试。\n\
请先用只读工具完成调研，然后在最终回答里输出一份执行计划，必须使用如下 ```plan 围栏格式：\n\
```plan\n\
目标: <一句话目标>\n\
步骤:\n\
1. <步骤一>\n\
2. <步骤二>\n\
文件: <涉及文件路径，逗号分隔>\n\
```\n\
用户批准该计划后才会开始实际执行。";

/// P8 角色映射 + 降级链的接线件：主渠道 + 可选专用渠道 + 降级配置（按序）。
pub struct LoopChannels {
    /// 主对话渠道（base_url/model/key/参数已从 settings+预设组装）
    pub primary: Channel,
    /// SUMMARY 专用渠道（未配置=主渠道）
    pub summary: Option<Channel>,
    /// TITLE 专用渠道；None=不生成标题
    pub title: Option<Channel>,
    /// 降级链：主渠道当轮失败时按序替换重试
    pub fallbacks: Vec<Channel>,
    /// P21：任务超 token 预算后的降级渠道（[roles] budget；None=不降级，维持主渠道）
    pub downgrade: Option<Channel>,
    /// P21：与 server 共享的「本任务已超预算」标志（budget_exceeded 触发时置位，
    /// 本任务剩余步骤改走 downgrade）。未配 downgrade 时只影响 Notice，不换渠道。
    pub budget_hit: Arc<std::sync::atomic::AtomicBool>,
    /// P22-C 子代理派发器（None = 本任务不许派发子代理，如测试或无头简单场景）。
    /// 工具调用 subagent_spawn 由核心循环拦截交给它——实现见 server/subagent.rs。
    pub subagents: Option<Arc<dyn crate::core::subagent::SubagentRunner>>,
}

/// 审批判定**单点**：一律走 `ApprovalGate::needs_approval_for`（细粒度，见
/// `core::rules::evaluate_risk` + `should_ask_approval` 的档位映射）。
/// 旧的按工具静态 danger_level 判定的 `needs_approval` 已在 P9 施工单 013 A2 中删除——
/// 两套判定并存会造成语义分裂，此后再无第二处审批判定入口。
/// （工具静态 danger_level 现仅用于「是否拍影子仓库检查点」，不参与审批判定。）
///
/// 审批门：由 server 层实现（SSE 发审批请求、挂 oneshot 等决定）。
pub trait ApprovalGate: Send + Sync {
    /// P9 细粒度审批：结合工具名与具体参数进行风险研判
    fn needs_approval_for(&self, tool: &str, args_json: &str) -> bool;
    /// P9 B2（Plan 档）：返回 Some(理由) = 该工具在 Plan 档被**直接驳回**，
    /// 不进审批弹窗流程（写/执行类操作连「问用户」都不问，直接拦下回喂模型调整计划）。
    /// 默认不拦截（Plan 档之外恒 None）——判定单点原则与 A2 一致。
    fn plan_block_reason(&self, _tool: &str, _args: &str) -> Option<String> {
        None
    }
    /// 本档位是否还在沙箱里跑（local 档 = false，前提是 config 总开关已开）
    fn sandbox_enabled(&self) -> bool;
    /// 本档位文件工具是否允许越出工作区（auto / full-access = true；read-only / sandbox = false）。
    /// 与 sandbox_enabled 一样按决策步现读——运行中切档，下一步即生效。
    fn file_tools_unconfined(&self) -> bool {
        false
    }
    /// 档位硬拦截（P19 sandbox 档）：Some(理由) = 该工具被当前档位直接驳回，
    /// 不进审批弹窗（连「问用户」都不问）。默认不拦截。
    fn tier_block_reason(&self, _tool: &str, _args: &str) -> Option<String> {
        None
    }
    /// 当前对话会话 id：检查点归属用（切回该会话时改动条/撤销只认自己的基线）。
    /// 默认空串 = 不归属，行为与旧版一致。
    fn session_id(&self) -> &str {
        ""
    }

    /// P22-C：本会话的**父任务**会话 id（子代理会话专用）——台账按父任务聚合。
    /// 默认 None = 本会话就是主任务（行为与旧版一致）。
    fn usage_parent_session(&self) -> Option<&str> {
        None
    }
    /// 等待用户决定；true=允许。实现内部处理超时（视为拒绝）与取消。
    fn ask(&self, tool: &str, args: &str) -> Pin<Box<dyn Future<Output = bool> + Send + '_>>;
    /// N1/025「立即发送（引导）」的运行中消息：核心循环每个决策点取一次并入本轮；
    /// 默认无（测试与其它实现不受影响）。只取「立即」的，排队的等本轮结束再发。
    fn take_steering(&self) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopOutcome {
    Done,
    Cancelled,
}

/// 渠道链调用（P27 从 run_task 抽出，续写复用同一套）：主渠道当轮失败按序换 fallback
/// 重试同一步；取消置位立即停（不降级）。流式增量原样转发 on_event；换渠道时先发
/// StepReset（前端丢半截内容）+ 人话提示。返回 (结果, 提示文本)——提示已实时转发过，
/// 调用方负责 persist_notice 补进上下文落盘（重启后气泡还在）。
#[allow(clippy::too_many_arguments)]
async fn run_chain(
    gateway: &Arc<Gateway>,
    chain: &[&Channel],
    ctx: &[Value],
    tools_def: Option<&Value>,
    cancel: &CancelFlag,
    usage_ctx: &UsageCtx,
    step: u32,
    on_event: &(impl Fn(AgentEvent) + Send + Sync),
) -> (anyhow::Result<ChatOutcome>, Vec<String>) {
    let mut notices = Vec::new();
    let mut outcome = Err(anyhow::anyhow!("渠道链为空"));
    // 本步当前这次尝试已经推给前端的字符数（UTF-16 码元，与 JS String.length 同口径）。
    // 换渠道重试时前端要按这个数把半截内容丢掉——否则界面会出现「半截 + 完整」叠着显示。
    let streamed_content = std::sync::atomic::AtomicUsize::new(0);
    let streamed_reasoning = std::sync::atomic::AtomicUsize::new(0);
    for (i, ch) in chain.iter().enumerate() {
        if i > 0 {
            observability::event(
                EventType::State,
                json!({ "kind": "gateway.fallback", "step": step, "from": chain[0].model, "to": ch.model, "base_url": ch.base_url }),
            );
            let last_err = outcome.err().map(|e| e.to_string()).unwrap_or_default();
            on_event(AgentEvent::StepReset {
                content: streamed_content.swap(0, std::sync::atomic::Ordering::Relaxed),
                reasoning: streamed_reasoning.swap(0, std::sync::atomic::Ordering::Relaxed),
            });
            let note = format!("主渠道调用失败（{last_err}），已切换到降级渠道「{}」", ch.model);
            on_event(AgentEvent::Notice(note.clone()));
            notices.push(note);
        }
        outcome = gateway
            .stream_chat(usage_ctx, ch, &Value::Array(ctx.to_vec()), tools_def, cancel, |chunk| {
                let delta = extract_delta(chunk);
                if !delta.content.is_empty() {
                    streamed_content.fetch_add(
                        delta.content.encode_utf16().count(),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                    on_event(AgentEvent::Content(delta.content));
                }
                if let Some(r) = delta.reasoning
                    && !r.is_empty() {
                        streamed_reasoning.fetch_add(
                            r.encode_utf16().count(),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        on_event(AgentEvent::Reasoning(r));
                    }
            })
            .await;
        match &outcome {
            Ok(ChatOutcome::Done(_)) => break,
            Ok(ChatOutcome::Cancelled) => return (outcome, notices),
            Err(_) => continue, // 换下一个渠道
        }
    }
    (outcome, notices)
}

/// 通知持久化（P27）：通知以 display_only 消息进上下文——随 Snapshot 落盘，重启/切回
/// 会话后气泡还在；发送侧由 gateway replay 净化剥离（模型永远看不到）。
/// 提示已实时转发过 Notice 事件的调用方用这个（如渠道链降级提示）。
fn persist_notice(msgs: &mut Vec<Value>, on_event: &(impl Fn(AgentEvent) + Send + Sync), text: &str) {
    msgs.push(json!({ "role": "system", "content": format!("[通知] {text}"), "display_only": true }));
    on_event(AgentEvent::Snapshot { messages: snapshot_msgs(msgs) });
}

/// 实时转发 + 持久化（绝大多数通知走这个）。
fn emit_notice(msgs: &mut Vec<Value>, on_event: &(impl Fn(AgentEvent) + Send + Sync), text: String) {
    on_event(AgentEvent::Notice(text.clone()));
    persist_notice(msgs, on_event, &text);
}

/// 执行一个任务：messages 为前端带来的对话历史（不含 system）。
#[allow(clippy::too_many_arguments)]
pub async fn run_task(
    gateway: &Arc<Gateway>,
    channels: &LoopChannels,
    system_prompt: &str,
    messages: &[Value],
    cancel: &CancelFlag,
    cfg: &AgentConfig,
    sandbox_cfg: &SandboxConfig,
    // 沙箱整树 CPU 硬上限百分比（0=不限；settings.json 的 sandbox_cpu_percent）
    cpu_percent: u32,
    ctx_cfg: &ContextConfig,
    rec_cfg: &RecoveryConfig,
    mem: &crate::memory::Memory,
    prior_summary: Option<String>,
    // P9 B1：Plan 档（true=只读+先出计划；由请求级 plan_mode 带入，前端随会话持久化）
    plan_mode: bool,
    gate: &dyn ApprovalGate,
    // 审计 05：本任务固定使用的工作区（无头/定时任务按触发时钉住；前台每轮传当前全局）
    ws: std::path::PathBuf,
    on_event: impl Fn(AgentEvent) + Send + Sync,
) -> anyhow::Result<LoopOutcome> {
    let primary = &channels.primary;
    let max_output_tokens = primary.params.max_tokens;
    // P9 B1/B3：Plan 档注入行为约束与 ```plan 计划格式（增量不含身份/人设，坑 13）
    let system_prompt_owned;
    let system_prompt = if plan_mode {
        system_prompt_owned = format!("{system_prompt}\n\n{PLAN_PROMPT}");
        system_prompt_owned.as_str()
    } else {
        system_prompt
    };
    let mut msgs: Vec<Value> = Vec::with_capacity(messages.len() + 1);
    msgs.push(json!({ "role": "system", "content": system_prompt }));
    // P21 用量台账归属：本任务的会话 id（辅助调用与主流程同一归属）
    // P22-C：子代理会话填父任务 id——台账按父任务聚合（草案第 4 节，无数据迁移）
    let sid = gate.session_id().to_string();
    let mut usage_ctx = UsageCtx::main(&sid);
    if let Some(parent) = gate.usage_parent_session() {
        usage_ctx.parent_session = Some(parent.to_string());
    }
    // P21：任务超 token 预算后剩余步骤改走降级渠道（与 server 共享标志；未配渠道只通知不换道）
    let mut downgrade_noticed = false;
    // P10-A：任务起点边界事件（trace 时间线切分用；State kind 扩展属合法「只增」）
    observability::event(EventType::State, json!({ "kind": "task.start" }));
    if plan_mode {
        observability::event(
            EventType::State,
            json!({ "kind": "plan.mode", "mode": "plan", "step": 0 }),
        );
    }
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
        let sid = gate.session_id().to_string();
        tokio::task::spawn_blocking(move || -> Option<String> {
            let _ = recovery::checkpoint(&ws, "任务起点", &sid);
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
    // 会话标题素材：本任务第一次最终回答（引导续跑后不从「好的已结束」生成标题）
    let mut title_source: Option<String> = None;

    // A3 能力标记「工具调用」闭环（施工单 012）：capabilities.tools=false 时不携带 tools
    // 参数，模型无法发起新工具调用；历史消息里的 tool_calls 回放不受影响（replay 只动消息字段）。
    let tools_def = tools_def_for(&primary.params);
    let mut prev_tool_sigs: Vec<String> = Vec::new();
    // 上一轮工具调用是否**全部失败**（超时/后端重置/审批拒绝…）。连续两步相同调用
    // 通常是死循环，但「失败后重试」是正当行为——失败本身就是状态可能已变的信号。
    let mut prev_all_failed = false;

    for step in 0..cfg.max_steps {
        // 每轮模型决策前查取消标志（核心循环刚性检查点）
        if cancel.is_set() {
            observability::event(EventType::State, json!({ "kind": "cancel", "phase": "loop", "step": step }));
            on_event(AgentEvent::Snapshot { messages: snapshot_msgs(&msgs) });
            emit_checkpoint_bar(baseline, gate.session_id(), &on_event).await;
            return Ok(LoopOutcome::Cancelled);
        }
        // N1/025 运行中追加消息：下一步决策前并入（模型这一步就能看到），原轮不打断。
        // 落 Snapshot 让追加消息立刻进会话文件；前端按 Steer 事件补用户气泡。
        let steer = gate.take_steering();
        if !steer.is_empty() {
            let added: Vec<Value> =
                steer.iter().map(|t| json!({ "role": "user", "content": t })).collect();
            observability::event(
                EventType::State,
                json!({ "kind": "steer", "count": added.len(), "step": step }),
            );
            msgs.extend(added.iter().cloned());
            on_event(AgentEvent::Steer { messages: added });
            on_event(AgentEvent::Snapshot { messages: snapshot_msgs(&msgs) });
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
                    match summarize_folded(gateway, primary, channels.summary.as_ref(), &running_summary, &stats.folded, &sid).await {
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
                        emit_notice(
                            &mut msgs,
                            &on_event,
                            format!("上下文占用 {pct}%，已自动生成交接文档 {}", path.display()),
                        );
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
        observability::debug(&format!("[loop] step {step} 调用模型 {}", primary.model));

        // P8 降级链：主渠道当轮失败 → 按序换 fallback 重试同一步；取消置位立即停（不降级）
        // P21：任务已超 token 预算且有降级渠道时，本步起改走降级渠道（便宜模型跑剩余步骤）
        let mut chain: Vec<&Channel> = Vec::with_capacity(1 + channels.fallbacks.len());
        if let (true, Some(cheap)) = (
            channels.budget_hit.load(std::sync::atomic::Ordering::Relaxed),
            channels.downgrade.as_ref(),
        ) {
            if !downgrade_noticed {
                downgrade_noticed = true;
                observability::event(
                    EventType::State,
                    json!({ "kind": "cost.downgraded", "step": step, "from": primary.model, "to": cheap.model }),
                );
                emit_notice(
                    &mut msgs,
                    &on_event,
                    format!(
                        "本任务累计 token 已超预算（[hooks] budget_tokens），后续步骤改走降级渠道「{}」以节省花费",
                        cheap.model
                    ),
                );
            }
            chain.push(cheap);
        } else {
            chain.push(primary);
        }
        chain.extend(channels.fallbacks.iter());
        let (outcome, chain_notices) = run_chain(
            gateway,
            &chain,
            &msgs,
            tools_def.as_ref(),
            cancel,
            &usage_ctx,
            step,
            &on_event,
        )
        .await;
        if matches!(&outcome, Ok(ChatOutcome::Cancelled)) {
            return Ok(LoopOutcome::Cancelled);
        }
        // 换渠道的提示实时已推给前端，这里补进上下文随快照落盘（重启后气泡还在）
        for n in chain_notices {
            persist_notice(&mut msgs, &on_event, &n);
        }
        let outcome = outcome?;

        let turn = match outcome {
            ChatOutcome::Cancelled => return Ok(LoopOutcome::Cancelled),
            ChatOutcome::Done(t) => t,
        };

        // 渠道真实用量（缓存命中率展示；渠道不支持时为空）
        if let Some(p) = turn.prompt_tokens {
            on_event(AgentEvent::ApiUsage { prompt_tokens: p, cached_tokens: turn.cached_tokens });
        }

        // P7.6 输出截断检测：finish_reason=length → 提示调大最大输出/拆分写入。
        // P27.3 起思考预算已在网关侧扩容（effective_max_tokens：思考开启时发送
        // 配置值+思考余量），思考不再吃掉「最大输出长度」额度——此处截断即正文
        // 本身超限，提示口径回归真实。P27 的自动续写方案已撤（治标不治本且拖慢
        // 响应：续写要重传全量上下文，用户实锤「还会卡」）。
        if turn.finish_reason.as_deref() == Some("length") {
            observability::event(
                EventType::State,
                json!({ "kind": "model.truncated", "step": step, "max_output_tokens": max_output_tokens }),
            );
            emit_notice(
                &mut msgs,
                &on_event,
                "模型输出被截断（达到最大输出长度）。建议在设置里调大该模型的「最大输出长度」，或让它分多次写入。"
                    .to_string(),
            );
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
            // P9 B3：Plan 档最终回答解析 ```plan 结构化计划 → 前端计划卡
            if plan_mode {
                match parse_plan(&final_text) {
                    Some(plan) => {
                        observability::event(
                            EventType::State,
                            json!({
                                "kind": "plan.proposed", "step": step,
                                "steps": plan.steps.len(), "files": plan.files.len(),
                            }),
                        );
                        on_event(AgentEvent::PlanProposal {
                            goal: plan.goal,
                            steps: plan.steps,
                            files: plan.files,
                        });
                    }
                    None => emit_notice(
                        &mut msgs,
                        &on_event,
                        "Plan 档：模型未按 ```plan 围栏格式输出计划，可直接对话要求它补一份格式化计划。".to_string(),
                    ),
                }
            }
            // P6：最终回答写入会话并落盘（与前端看到的一致）；带 reasoning_content（思考内容
            // 随消息持久化，前端切回会话时恢复；回放侧由 replay.rs 按渠道契约决定去留）
            let mut final_msg = json!({ "role": "assistant", "content": final_text });
            if !turn.reasoning.is_empty() {
                final_msg["reasoning_content"] = json!(turn.reasoning);
            }
            msgs.push(final_msg);
            on_event(AgentEvent::Snapshot { messages: snapshot_msgs(&msgs) });
            // 标题素材取本任务**第一次**最终回答：引导续跑后的「好的，已结束」不该当会话标题
            if title_source.is_none() {
                title_source = Some(final_text.clone());
            }
            // 027 补口（2026-09-27 用户实锤）：决策点只在每步模型调用**之前**，「立即发送」
            // 若在最终回答流式输出期间到达，会错过全部决策点——按旧逻辑直接收尾，这条引导
            // 被服务层收尾 drain_all 捡去当 followup 另起一轮（用户看到自己没再说话却多出
            // 一轮「好的，已结束」）。最终回答落地后再取一次：有引导就并进本轮继续（不丢
            // 已流出的回答，模型下一步就看到），没有才是真的结束。
            let steer = gate.take_steering();
            if !steer.is_empty() {
                let added: Vec<Value> =
                    steer.iter().map(|t| json!({ "role": "user", "content": t })).collect();
                observability::event(
                    EventType::State,
                    json!({ "kind": "steer", "phase": "post_answer", "count": added.len(), "step": step }),
                );
                msgs.extend(added.iter().cloned());
                on_event(AgentEvent::Steer { messages: added });
                on_event(AgentEvent::Snapshot { messages: snapshot_msgs(&msgs) });
                continue;
            }
            emit_checkpoint_bar(baseline, gate.session_id(), &on_event).await;
            observability::event(
                EventType::State,
                json!({ "kind": "loop.finish", "step": step, "content_chars": turn.content.chars().count() }),
            );
            // P7 事件流水：任务完成
            mem.record("task.finish", json!({ "steps": step + 1, "chars": final_text.chars().count() }));
            // P8 TITLE 角色：任务结束用专用渠道生成会话标题（失败静默保留机械标题）
            if let Some(tch) = &channels.title
                && let Some(title) = generate_title(gateway, tch, title_source.as_deref().unwrap_or(&final_text), &sid).await {
                    observability::event(
                        EventType::State,
                        json!({ "kind": "session.titled", "title": title }),
                    );
                    on_event(AgentEvent::Title(title));
                }
            return Ok(LoopOutcome::Done);
        }

        // 护栏：模型连续两步请求完全相同的工具调用 → 死循环，立即终止。
        // 例外：上一轮相同调用**全部失败**时重试是正当行为（用户反馈：navigate 超时被
        // 当成死循环终止，用户看到的是天书错误横幅，而不是 AI 说「访问超时」）。
        // 真正要防的是对成功调用的重复请求；失败重试由步数上限兜底。
        let sigs: Vec<String> =
            turn.tool_calls.iter().map(|c| format!("{}|{}", c.name, c.arguments)).collect();
        if is_repeat_loop(&prev_tool_sigs, &sigs, prev_all_failed) {
            observability::event(
                EventType::Error,
                json!({ "message": "模型重复请求相同工具调用，已终止循环" }),
            );
            return Err(anyhow::anyhow!("模型重复请求相同工具调用，已终止"));
        }
        prev_tool_sigs = sigs;

        // 回填 assistant 消息（含 tool_calls）；思考内容随消息落盘（前端切回会话可恢复）
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
        let mut asst_msg = json!({
            "role": "assistant",
            "content": if turn.content.is_empty() { Value::Null } else { json!(turn.content) },
            "tool_calls": calls_json,
        });
        if !turn.reasoning.is_empty()
            && primary.params.thinking.as_deref() != Some("off")
        {
            // P26：思考=关时不落 reasoning_content——否则刷新/切回会话又从历史里显示出来
            asst_msg["reasoning_content"] = json!(turn.reasoning);
        }
        msgs.push(asst_msg);

        // —— P12 018：工具执行两阶段（判定串行 → 只读组并行 / 写类串行独占）——
        // fs_unconfined / unsandboxed 每步现读 gate：运行中切档，从下一步起按新档位执行
        let exec_cfg = ExecConfig {
            sandbox: sandbox_cfg.clone(),
            max_chars: cfg.tool_result_max_chars,
            unsandboxed: !gate.sandbox_enabled(),
            fs_unconfined: gate.file_tools_unconfined(),
            cpu_percent,
            workspace: ws.clone(),
        };
        use futures_util::StreamExt;

        // 阶段一「判定」（串行）：Plan 拦截 / 档位拦截 / 审批弹窗 / 取消——全部现有语义不变
        struct Decision {
            approved: bool,
            plan_reason: Option<String>,
            tier_reason: Option<String>,
            hook_reason: Option<String>,
            risk: u8,
            skip_conflict: bool,
        }
        let mut decisions: Vec<Decision> = Vec::with_capacity(turn.tool_calls.len());
        for call in &turn.tool_calls {
            if cancel.is_set() {
                observability::event(EventType::State, json!({ "kind": "cancel", "phase": "tool", "step": step }));
                on_event(AgentEvent::Snapshot { messages: snapshot_msgs(&msgs) });
                emit_checkpoint_bar(baseline, gate.session_id(), &on_event).await;
                return Ok(LoopOutcome::Cancelled);
            }
            observability::event(
                EventType::ToolCall,
                json!({ "step": step, "id": call.id, "name": call.name, "arguments": call.arguments }),
            );
            on_event(AgentEvent::ToolCall { name: call.name.clone(), args: call.arguments.clone() });
            observability::debug(&format!("[loop] 工具 {} 参数 {}", call.name, call.arguments));

            // P9 B2：Plan 档拦截优先于审批流；P9 细粒度审批单点不变
            let plan_reason = gate.plan_block_reason(&call.name, &call.arguments);
            // P19：sandbox 档硬拦截（如工作区文件写入）——不进审批流，直接驳回
            let tier_reason = if plan_reason.is_none() {
                gate.tier_block_reason(&call.name, &call.arguments)
            } else {
                None
            };
            // P18 钩子（027）：before_tool 只有否决权——deny 则不执行且不进审批流；
            // 钩子放行 ≠ 免审批，审批门照走（定案 7）。
            let hook_reason = if plan_reason.is_none() && tier_reason.is_none() {
                crate::hooks::before_tool(gate.session_id(), &call.name, &call.arguments)
                    .await
                    .err()
            } else {
                None
            };
            let approved = match (&plan_reason, &tier_reason, &hook_reason) {
                (Some(_), _, _) | (_, Some(_), _) | (_, _, Some(_)) => false,
                (None, None, None) if gate.needs_approval_for(&call.name, &call.arguments) => {
                    gate.ask(&call.name, &call.arguments).await
                }
                (None, None, None) => true,
            };
            let (risk, _) = crate::core::rules::evaluate_risk(
                &call.name,
                &serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({})),
                &tools::workspace_root(),
            );
            decisions.push(Decision {
                approved,
                plan_reason,
                tier_reason,
                hook_reason,
                risk: risk.as_u8(),
                skip_conflict: false,
            });
        }

        // 写冲突检测（P12 018）：同轮 write_file 同路径 → 仅首个执行，其余跳过回喂
        {
            let calls: Vec<(String, String)> = turn
                .tool_calls
                .iter()
                .map(|c| (c.name.clone(), c.arguments.clone()))
                .collect();
            for i in write_conflict_skips(&calls) {
                let d = &mut decisions[i];
                if d.approved && d.plan_reason.is_none() && !d.skip_conflict {
                    d.skip_conflict = true;
                    observability::event(
                        EventType::State,
                        json!({ "kind": "write.conflict", "step": step, "tool": turn.tool_calls[i].name, "arguments": turn.tool_calls[i].arguments }),
                    );
                }
            }
        }

        // 阶段二「执行」：Safe 只读组并行（无副作用无竞态），写/执行组串行独占。
        // P22-C：subagent_spawn 也进并行组——子代理各自独立会话，天然无共享竞态，
        // 一轮里发多个即真并行（受服务层并发度上限约束）。
        let mut results: Vec<(String, bool)> = vec![(String::new(), true); turn.tool_calls.len()];
        let parallel: Vec<usize> = (0..turn.tool_calls.len())
            .filter(|&i| {
                let d = &decisions[i];
                let is_subagent = turn.tool_calls[i].name == "subagent_spawn";
                d.approved && d.plan_reason.is_none() && !d.skip_conflict && (d.risk == 0 || is_subagent)
            })
            .collect();
        if parallel.len() > 1 {
            observability::event(
                EventType::State,
                json!({ "kind": "tools.parallel", "step": step, "count": parallel.len() }),
            );
        }
        let subagent_runner = channels.subagents.clone();
        let parent_sid = gate.session_id().to_string();
        let mut futs = futures_util::stream::FuturesUnordered::new();
        for &i in &parallel {
            let call = &turn.tool_calls[i];
            let cfg = exec_cfg.clone();
            let name = call.name.clone();
            let args = call.arguments.clone();
            let runner = subagent_runner.clone();
            let parent = parent_sid.clone();
            futs.push(Box::pin(async move {
                let exec = exec_tool_call(&cfg, &name, &args, runner.as_ref(), &parent);
                // P26：并行组同样可被取消即时中断（串行组同款，见下方 select! 注释）
                (
                    i,
                    tokio::select! {
                        r = exec => r,
                        () = cancel.cancelled() => Err("已取消：用户点了结束回答".to_string()),
                    },
                )
            }));
        }
        // 并行完成即发 result 事件（时间线可见并发；进度与完成顺序一致）
        while let Some((i, r)) = futs.next().await {
            let call = &turn.tool_calls[i];
            let (content, ok) = match r {
                Ok(s) => (s, true),
                Err(e) => (format!("工具执行失败: {e}"), false),
            };
            let preview: String = content.chars().take(300).collect();
            observability::event(
                EventType::ToolResult,
                json!({ "step": step, "id": call.id, "name": call.name, "ok": ok, "chars": content.chars().count(), "truncated": content.contains(context::OMITTED_MARK), "preview": preview }),
            );
            on_event(AgentEvent::ToolResult { name: call.name.clone(), ok, preview });
            crate::hooks::notify_after_tool(gate.session_id(), &call.name, ok);
            results[i] = (content, ok);
        }

        // 串行组（Plan 拦截 / 拒绝 / 写冲突 / 写类）：原逻辑逐个保持
        for i in 0..turn.tool_calls.len() {
            if parallel.contains(&i) {
                continue;
            }
            if cancel.is_set() {
                observability::event(EventType::State, json!({ "kind": "cancel", "phase": "tool", "step": step }));
                on_event(AgentEvent::Snapshot { messages: snapshot_msgs(&msgs) });
                emit_checkpoint_bar(baseline, gate.session_id(), &on_event).await;
                return Ok(LoopOutcome::Cancelled);
            }
            let call = &turn.tool_calls[i];
            let d = &decisions[i];
            let danger = tools::find(&call.name).map(|s| s.danger_level).unwrap_or(2);
            let (content, ok) = if let Some(reason) = &d.plan_reason {
                observability::event(
                    EventType::State,
                    json!({ "kind": "plan.blocked", "step": step, "tool": call.name, "reason": reason }),
                );
                emit_notice(&mut msgs, &on_event, format!("Plan 档已拦截 {}：{}", call.name, reason));
                (
                    format!(
                        "Plan 模式拦截（未执行）：{reason}。请勿执行写/改类操作——继续只读调研，并在最终回答中用 ```plan 围栏输出执行计划，等待用户批准。"
                    ),
                    true,
                )
            } else if let Some(reason) = &d.tier_reason {
                // P19 档位拦截（sandbox 档禁写等）：未执行，把理由回喂给模型调整做法。
                // 不发用户通知（用户拍板：模型拿到理由会自行解释/换路，[通知] 行是噪音）
                observability::event(
                    EventType::State,
                    json!({ "kind": "tier.blocked", "step": step, "tool": call.name, "reason": reason }),
                );
                (
                    format!("审批档位拦截（未执行）：{reason}。当前档位下这类操作不可用，请改用只读方式继续，或建议用户切换审批档位。"),
                    true,
                )
            } else if let Some(reason) = &d.hook_reason {
                // P18 钩子 deny（027）：未执行，把理由回喂给模型调整做法
                observability::event(
                    EventType::State,
                    json!({ "kind": "hook.blocked", "step": step, "tool": call.name, "reason": reason }),
                );
                emit_notice(&mut msgs, &on_event, format!("钩子已拦截 {}：{}", call.name, reason));
                (
                    format!("自定义钩子拦截（未执行）：{reason}。请换别的做法或直接回答用户。"),
                    true,
                )
            } else if d.skip_conflict {
                emit_notice(
                    &mut msgs,
                    &on_event,
                    format!("写冲突已跳过 {}：本轮已有对同一文件的写操作", call.name),
                );
                (
                    "写冲突：本轮已有对同一文件的写操作，本次已跳过。请把对同一文件的多次写入拆分到不同轮次。".to_string(),
                    true,
                )
            } else if !d.approved {
                ("用户拒绝了该操作，工具未执行。请换别的做法或直接回答用户。".to_string(), true)
            } else {
                // P6：写类/执行类工具动手前先拍影子仓库检查点；失败只记日志，绝不阻断。
                // P22-C：subagent_spawn 不拍——子代理会话对自己的改动拍自己的检查点
                // （父任务这边拍只会把子代理的改动混进父任务的基线口径）。
                if rec_cfg.checkpoint_enabled && danger >= 1 && call.name != "subagent_spawn" {
                    let reason = format!("{} {}", call.name, short_args(&call.arguments, 100));
                    let ws = tools::workspace_root().to_path_buf();
                    let sid = gate.session_id().to_string(); // 归属会话（改动条/撤销按会话对齐）
                    match tokio::task::spawn_blocking(move || recovery::checkpoint(&ws, &reason, &sid)).await {
                        Ok(Ok(Some(cp))) => {
                            observability::event(
                                EventType::State,
                                json!({ "kind": "checkpoint.created", "id": cp.id, "ts": cp.ts, "reason": cp.reason, "session": cp.session }),
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
                let exec = exec_tool_call(
                    &exec_cfg,
                    &call.name,
                    &call.arguments,
                    channels.subagents.as_ref(),
                    gate.session_id(),
                );
                // P26：取消即时中断——工具卡在浏览器 CDP 调用里时，select! 到取消当场丢 future
                // （以前只靠下个决策点轮询 is_set，用户点了「结束回答」还要干等工具超时，
                // 期间服务端仍显示在跑，新消息被排进队列——用户实测实锤）
                let exec = async {
                    tokio::select! {
                        r = exec => r,
                        () = cancel.cancelled() => Err("已取消：用户点了结束回答".to_string()),
                    }
                };
                match exec.await {
                    Ok(s) => (s, true),
                    Err(e) => (format!("工具执行失败: {e}"), false),
                }
            };
            // 串行组 result 事件即时发（与原行为一致）
            let preview: String = content.chars().take(300).collect();
            observability::event(
                EventType::ToolResult,
                json!({ "step": step, "id": call.id, "name": call.name, "ok": ok, "chars": content.chars().count(), "truncated": content.contains(context::OMITTED_MARK), "preview": preview }),
            );
            on_event(AgentEvent::ToolResult { name: call.name.clone(), ok, preview });
            crate::hooks::notify_after_tool(gate.session_id(), &call.name, ok);
            results[i] = (content, ok);
        }

        // —— 统一回填（按 tool_calls 原顺序，协议友好）——
        for (i, call) in turn.tool_calls.iter().enumerate() {
            let (content, ok) = &results[i];
            mem.record("tool.call", json!({ "name": call.name, "ok": ok }));
            msgs.push(json!({ "role": "tool", "tool_call_id": call.id, "content": content }));
        }
        // P6 会话落盘：本步工具回填完毕，状态常驻磁盘
        on_event(AgentEvent::Snapshot { messages: snapshot_msgs(&msgs) });
        // 记录本轮成败：下一步的「相同调用」护栏要靠它区分死循环与失败重试
        prev_all_failed = !results.is_empty() && results.iter().all(|(_, ok)| !ok);
    }

    observability::event(EventType::Error, json!({ "message": format!("已达步数上限 {}，强制终止", cfg.max_steps) }));
    on_event(AgentEvent::StepLimit(cfg.max_steps));
    Ok(LoopOutcome::Done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_def_follows_capability_mark() {
        // A3（施工单 012）：capabilities.tools=false → 请求不携带 tools 参数
        let mut p = ChatParams::default();
        assert!(tools_def_for(&p).is_some(), "默认（tools=true）携带 tools");
        p.tools_enabled = false;
        assert!(tools_def_for(&p).is_none(), "勾掉「工具调用」后不再携带 tools；历史 tool_calls 回放不受影响");
    }

    // —— P9 B3：plan 块解析 ——
    /// 重复调用护栏：成功后的重复 = 死循环；失败后的重试 = 放行（步数上限兜底）。
    #[test]
    fn repeat_loop_allows_retry_after_failure() {
        let a = vec!["browser|{\"method\":\"navigate\"}".to_string()];
        // 上一轮相同调用成功了 → 重复即死循环
        assert!(is_repeat_loop(&a, &a, false));
        // 上一轮相同调用失败了（超时/后端重置）→ 重试放行
        assert!(!is_repeat_loop(&a, &a, true));
        // 不同调用 / 首轮（prev 空）→ 都不是循环
        let b = vec!["browser|{\"method\":\"snapshot\"}".to_string()];
        assert!(!is_repeat_loop(&a, &b, false));
        assert!(!is_repeat_loop(&[], &a, false));
    }

    // —— P12 018：写冲突检测 ——
    #[test]
    fn write_conflict_detection() {        let mk = |n: &str, a: &str| (n.to_string(), a.to_string());
        let calls = vec![
            mk("write_file", r#"{"path": "src/a.rs"}"#),
            mk("read_file", r#"{"path": "src/a.rs"}"#),
            mk("write_file", r#"{"path": "SRC\\a.rs"}"#), // 大小写+反斜杠归一后同路径
            mk("write_file", r#"{"path": "src/b.rs"}"#),
            mk("shell", r#"{"command": "del x"}"#),
        ];
        let skip = write_conflict_skips(&calls);
        assert!(!skip.contains(&0), "首个写不跳过");
        assert!(!skip.contains(&1), "read_file 不参与检测");
        assert!(skip.contains(&2), "同路径（归一后）第二个写跳过");
        assert!(!skip.contains(&3), "不同路径不跳过");
        assert_eq!(skip, [2usize].into_iter().collect());
    }

    #[test]
    fn parse_plan_standard_block() {
        let text = "好的，我先调研一下。\n```plan\n目标: 重构审批模块\n步骤:\n1. 梳理 gate 调用点\n2. 抽出纯函数\n3. 补单测\n文件: src/core/agent.rs, src/server.rs\n```\n等待批准。";
        let p = parse_plan(text).expect("标准格式应解析成功");
        assert_eq!(p.goal, "重构审批模块");
        assert_eq!(p.steps, vec!["梳理 gate 调用点", "抽出纯函数", "补单测"]);
        assert_eq!(p.files, vec!["src/core/agent.rs", "src/server.rs"]);
    }

    #[test]
    fn parse_plan_tolerant_marks() {
        // 中文冒号 + markdown 加粗 + 「-」列表符 + 反引号包裹路径
        let text = "```plan\n**目标**：完成 P9\n步骤:\n- 第一步\n- 第二步\n**文件**：`a.rs`、`b.rs`\n```";
        let p = parse_plan(text).expect("容错格式应解析成功");
        assert_eq!(p.goal, "完成 P9");
        assert_eq!(p.steps, vec!["第一步", "第二步"]);
        assert_eq!(p.files, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn parse_plan_rejects_missing_or_empty() {
        assert!(parse_plan("没有围栏的普通回答").is_none(), "无 plan 围栏返回 None");
        assert!(parse_plan("```plan\n```").is_none(), "空围栏返回 None");
        assert!(
            parse_plan("```plan\n随便写两句不带头部标记的话\n```").is_none(),
            "围栏内无任何目标/步骤/文件标记返回 None"
        );
    }

    #[test]
    fn parse_plan_steps_only_is_ok() {
        // 只有步骤也算有效计划（弱模型可能漏掉目标/文件行）
        let text = "```plan\n步骤:\n1. 先做 A\n2. 再做 B\n```";
        let p = parse_plan(text).expect("仅步骤应解析成功");
        assert!(p.goal.is_empty());
        assert_eq!(p.steps, vec!["先做 A", "再做 B"]);
        assert!(p.files.is_empty());
    }

    // —— P27.3 思考预算扩容 + 截断提示（2026-09-29 用户实锤「思考内容算进输出长度
    // 把正文截断」；P27 的自动续写方案已撤——治标不治本且拖慢响应）——
    // mock SSE：一次请求即 finish_reason=length 半截正文。
    // 期望：① 请求体 max_tokens = 配置值+思考余量（4096 → 12288，思考不再吃正文额度）；
    //       ② 不自动续写（只有一次请求，正文就是半截）；③ 截断提示落盘成 display_only 气泡。
    #[tokio::test]
    async fn thinking_max_tokens_inflated_and_no_auto_continue() {
        // 日志落临时目录（与 gateway/memory 测试同款，不写仓库 logs/）
        fn init_logger() {
            static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            ONCE.get_or_init(|| {
                let dir = std::env::temp_dir().join("agent-test-logs");
                let _ = crate::observability::init(&dir, "agent.jsonl", format!("ag-{}", uuid::Uuid::new_v4()));
            });
        }
        init_logger();
        use crate::config::MemoryConfig;
        use crate::memory::Memory;
        use std::sync::atomic::AtomicUsize;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let hits_c = hits.clone();
        let bodies_c = bodies.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_c = stop.clone();
        let mock = std::thread::spawn(move || {
            use std::io::{Read, Write};
            while !stop_c.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        let _ = s.set_nonblocking(false);
                        let mut buf = [0u8; 65536];
                        let n = Read::read(&mut s, &mut buf).unwrap_or(0);
                        bodies_c.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).to_string());
                        hits_c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"半截正文\"}}]}\n\n\
                             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
                             data: [DONE]\n\n";
                        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{body}");
                        let _ = s.write_all(resp.as_bytes());
                        let _ = s.flush();
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(15)),
                }
            }
        });

        let gateway = Arc::new(Gateway::new());
        // 复刻用户实锤配置：step-5-preview + thinking=high + 最大输出长度 4096
        let params = ChatParams { max_tokens: Some(4096), thinking: Some("high".to_string()), ..Default::default() };
        let ch = Channel::new(&format!("http://127.0.0.1:{port}/v1"), "mock-model", "sk-dummy", params);
        let channels = LoopChannels {
            primary: ch,
            summary: None,
            title: None,
            fallbacks: Vec::new(),
            downgrade: None,
            budget_hit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            subagents: None,
        };
        let ws = std::env::temp_dir().join(format!("agent-trunc-{}-{}", std::process::id(), port));
        std::fs::create_dir_all(&ws).unwrap();
        let mem = Memory::open_at(
            &ws,
            &MemoryConfig { enabled: false, persist_sensitive: false },
            ws.join("global"),
        );
        let notices = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let snaps = Arc::new(std::sync::Mutex::new(Vec::<Vec<Value>>::new()));
        let notices_c = notices.clone();
        let snaps_c = snaps.clone();
        struct G;
        impl ApprovalGate for G {
            fn needs_approval_for(&self, _tool: &str, _args_json: &str) -> bool {
                false
            }
            fn sandbox_enabled(&self) -> bool {
                true
            }
            fn ask(&self, _tool: &str, _args: &str) -> std::pin::Pin<Box<dyn Future<Output = bool> + Send + '_>> {
                Box::pin(async { true })
            }
        }
        let cancel = CancelFlag::new();
        let outcome = run_task(
            &gateway,
            &channels,
            "sys prompt",
            &[json!({ "role": "user", "content": "写点东西" })],
            &cancel,
            &AgentConfig::default(),
            &SandboxConfig::default(),
            0,
            &ContextConfig::default(),
            &RecoveryConfig { checkpoint_enabled: false, ..Default::default() },
            &mem,
            None,
            false,
            &G,
            crate::tools::workspace_root(),
            move |ev: AgentEvent| match ev {
                AgentEvent::Notice(t) => notices_c.lock().unwrap().push(t),
                AgentEvent::Snapshot { messages } => snaps_c.lock().unwrap().push(messages),
                _ => {}
            },
        )
        .await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        mock.join().unwrap();
        let _ = std::fs::remove_dir_all(&ws);

        assert_eq!(outcome.unwrap(), LoopOutcome::Done);
        assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 1, "不再自动续写：只有一次请求");
        let reqs = bodies.lock().unwrap();
        let body_json = reqs[0]
            .split_once("\r\n\r\n")
            .map(|(_, b)| b)
            .unwrap_or_default();
        let sent: Value = serde_json::from_str(body_json).expect("请求体应为 JSON");
        assert_eq!(sent.get("max_tokens").and_then(Value::as_u64), Some(12288), "思考开启时 max_tokens 应扩容到 4096+8192");
        let notes = notices.lock().unwrap();
        assert!(
            notes.iter().any(|n| n.contains("模型输出被截断")),
            "应推送截断提示，实际 {:?}",
            *notes
        );
        let snaps = snaps.lock().unwrap();
        let last = snaps.last().expect("至少落一次快照");
        let final_msg = last
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
            .expect("快照应有最终回答");
        assert_eq!(final_msg.get("content").and_then(Value::as_str), Some("半截正文"), "不续写：正文就是半截");
        assert!(
            last.iter().any(|m| m.get("display_only").and_then(Value::as_bool) == Some(true)),
            "截断提示应以 display_only 消息落盘（重启后气泡还在）"
        );
    }
}
