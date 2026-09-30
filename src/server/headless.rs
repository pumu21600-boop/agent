//! P22-A 无头会话（程序化唤醒）：不依赖 UI 的一问一答整轮
//!
//! 子代理结果合并、定时任务、CLI 一次性唤醒的共同底座（docs/subagent-draft.md 第 3 节：
//! 「无头是子代理、定时任务、Webhook、主动提醒的共同前提，顺序不能反」）。
//! 会话能被程序化唤醒、跑完整轮核心循环，最终回答与过程摘要结构化返回。
//!
//! 与 /api/chat 的关系：渠道组装（B2/B3）、档位归一、系统提示注入全部复用同一批函数
//! （assemble_turn_channels / normalize_mode / turn_system_prompt）——两路只有「事件去哪」不同：
//! chat 转 SSE 给前端，无头汇进收集器给调用方。审批判定同样复用 ChatGate（同一把尺子），
//! 只有 ask() 按无人值守策略自动决定（默认拒绝，显式 approve 才放行）。

use super::*;
use super::chat::{CancelRun, assemble_turn_channels, normalize_mode, turn_system_prompt};
use super::gate::ChatGate;
use super::subagent;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// 无头运行期间的工作区切换守卫：Drop 还原全局根。
/// 放在跑整轮的任务里（随任务生命周期）——取消/超时后任务在后台收尾期间
/// 工作区保持切换态，收尾完才还原，不会漏还原也不会提前还。
struct WorkspaceGuard {
    old: std::path::PathBuf,
}
impl Drop for WorkspaceGuard {
    fn drop(&mut self) {
        crate::tools::set_workspace_root(&self.old);
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct HeadlessRequest {
    /// 要唤醒的消息（必填）
    #[serde(default)]
    pub message: String,
    /// 唤醒哪条会话；缺省新建（返回的 session_id 即新会话）
    #[serde(default)]
    pub session_id: String,
    /// 请求级覆盖：非空优先于已保存设置（与 /api/chat 同义）
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    /// 审批档位（缺省 = config 默认档）；无人值守时命中审批的动作按 on_approval 处理
    #[serde(default)]
    pub approval_mode: String,
    /// Plan 档（只读 + 先出计划）
    #[serde(default)]
    pub plan_mode: bool,
    /// 需要审批的操作怎么办：deny（缺省，安全默认）= 一律拒绝并回喂模型；
    /// approve = 一律批准（调用方明确要对高危操作放行时才传）
    #[serde(default)]
    pub on_approval: String,
    /// 整轮超时（秒）；0 = 不限。超时即取消本轮，已产出的部分回答照常返回
    #[serde(default)]
    pub timeout_secs: u64,
    /// 唤醒来源（cli / schedule / subagent / webhook…），只进审计与台账口径
    #[serde(default)]
    pub purpose: String,
    /// P22-C：父任务会话 id（子代理派发时填）——台账按父任务聚合 + 取消级联
    #[serde(default)]
    pub parent_session: String,
    /// 任务执行所在工作区（目录路径；空 = 跟随当前全局工作区）。
    /// 非空时整轮切到该目录跑，跑完（含取消/超时后台收尾）还原全局根；
    /// 完成会话也记在这个项目下，侧栏按项目可见。
    #[serde(default)]
    pub workspace: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct HeadlessResult {
    pub session_id: String,
    /// done（正常跑完）/ cancelled（被停止）/ timeout（超时取消）/ error（出错）
    pub status: String,
    /// 本轮最终回答（模型正文全部拼接；超时/取消时为已产出的部分）
    pub answer: String,
    pub reasoning: String,
    /// 过程摘要：每次工具结果一条 {name, ok, preview}
    pub tool_calls: Vec<Value>,
    /// 模型请求次数（主动手数，不含摘要/标题等辅助调用）
    pub requests: u32,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    /// 角色解析失败、审批策略等提示（不静默）
    pub notices: Vec<String>,
    pub duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 无头一轮的过程收集器（没有 SSE，事件汇到这里）。
#[derive(Default)]
struct Collectors {
    answer: std::sync::Mutex<String>,
    reasoning: std::sync::Mutex<String>,
    tool_calls: std::sync::Mutex<Vec<Value>>,
    requests: AtomicU32,
    prompt_tokens: AtomicU64,
    cached_tokens: AtomicU64,
    last_snapshot: std::sync::Mutex<Vec<Value>>,
    summary: std::sync::Mutex<Option<String>>,
}

/// 无人值守审批门：档位语义全部复用 ChatGate（同一把尺子），只有 ask() 按策略自动决定。
struct HeadlessGate {
    inner: ChatGate,
    auto_approve: bool,
    session_id: String,
    /// P22-C：父任务会话 id（子代理派发；主任务为空）
    parent_session: String,
}

impl agent::ApprovalGate for HeadlessGate {
    fn needs_approval_for(&self, tool: &str, args_json: &str) -> bool {
        self.inner.needs_approval_for(tool, args_json)
    }
    fn plan_block_reason(&self, tool: &str, args_json: &str) -> Option<String> {
        self.inner.plan_block_reason(tool, args_json)
    }
    fn sandbox_enabled(&self) -> bool {
        self.inner.sandbox_enabled()
    }
    fn file_tools_unconfined(&self) -> bool {
        self.inner.file_tools_unconfined()
    }
    fn tier_block_reason(&self, tool: &str, args_json: &str) -> Option<String> {
        self.inner.tier_block_reason(tool, args_json)
    }
    fn session_id(&self) -> &str {
        &self.session_id
    }
    /// P22-C：子代理会话的模型调用记父任务 id（台账按父任务聚合）
    fn usage_parent_session(&self) -> Option<&str> {
        if self.parent_session.is_empty() {
            None
        } else {
            Some(&self.parent_session)
        }
    }
    /// 无头不吃「立即发送」引导消息：UI 给这条会话发的追加消息进排队队列，
    /// 由本轮收尾处的有界续跑处理（见 run_headless 尾部）。
    fn take_steering(&self) -> Vec<String> {
        Vec::new()
    }
    /// 无人值守：没有 UI 来回答审批。按 on_approval 策略决定并留审计。
    fn ask(&self, tool: &str, _args: &str) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        let approved = self.auto_approve;
        observability::event(
            EventType::State,
            json!({
                "kind": "headless.approval_auto",
                "session": self.session_id,
                "tool": tool,
                "approved": approved,
            }),
        );
        Box::pin(async move { approved })
    }
}

/// POST /api/headless/run —— 程序化唤醒一条会话，跑完整轮，返回结构化结果。
/// 会话已在跑（UI 聊着 / 另一个无头任务）→ 409，同一会话文件不并发写。
pub(super) async fn run_headless(
    State(state): State<AppState>,
    Json(req): Json<HeadlessRequest>,
) -> Response {
    match run_headless_inner(state, req).await {
        Ok(r) => Json(r).into_response(),
        Err((code, msg)) => (code, Json(json!({ "error": msg }))).into_response(),
    }
}

/// 无头会话主体（进程内入口）：HTTP handler 与调度器（P22-B 定时任务）共用。
/// Err = (HTTP 状态, 人话错误)。
pub(crate) async fn run_headless_inner(
    state: AppState,
    req: HeadlessRequest,
) -> Result<HeadlessResult, (StatusCode, String)> {
    let message = req.message.trim().to_string();
    if message.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "消息为空".to_string()));
    }
    // 会话 id：带合法 id 就复用（UI 里正聊着的会话也能被程序化唤醒），否则新建
    let session_id = {
        let sid = req.session_id.trim().to_string();
        let valid = !sid.is_empty()
            && sid.len() <= 64
            && sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if valid { sid } else { state.session.new_id() }
    };
    let Some(run_flag) = state.cancel.try_begin(&session_id) else {
        observability::event(
            EventType::State,
            json!({ "kind": "headless.rejected", "reason": "session_busy", "session": session_id }),
        );
        return Err((
            StatusCode::CONFLICT,
            format!("会话 {session_id} 正在运行中：等这一轮结束后再唤醒"),
        ));
    };

    let started = std::time::Instant::now();
    let s = state
        .settings
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let active_cfg = s.active_or_default().clone();
    let cfg = (*state.agent_cfg).clone();
    let sandbox_cfg = (*state.sandbox_cfg).clone();
    // 上下文预算：当前模型设了上下文长度就用它（覆盖全局 [context].budget_tokens）
    let ctx_cfg = {
        let mut c = (*state.context_cfg).clone();
        if let Some(cl) = active_cfg.context_length {
            c.budget_tokens = cl;
        }
        c
    };
    let rec_cfg = (*state.recovery_cfg).clone();
    let allow_unsandboxed = cfg.allow_unsandboxed;
    let (mode, mode_notice) = normalize_mode(&cfg, &req.approval_mode);
    // M1：档位写进本会话（与 /api/chat 同一张表，UI 切档对进行中的无头轮同样即时生效）
    if let Ok(mut m) = state.session_approval.write() {
        m.insert(session_id.clone(), mode.clone());
    }
    let auto_approve = req.on_approval.trim().eq_ignore_ascii_case("approve");
    let purpose = {
        let p = req.purpose.trim().to_string();
        if p.is_empty() { "headless".to_string() } else { p }
    };
    let timeout_secs = req.timeout_secs.min(86_400);
    observability::event(
        EventType::State,
        json!({
            "kind": "headless.run",
            "session": session_id,
            "purpose": purpose,
            "mode": mode,
            "auto_approve": auto_approve,
            "timeout_secs": timeout_secs,
        }),
    );

    // 历史：被唤醒的已有会话接着自己的上下文跑；新会话就是这一条消息
    let mut history = state
        .session
        .get(&session_id)
        .map(|f| f.messages)
        .unwrap_or_default();
    history.push(json!({ "role": "user", "content": message }));
    let mut prior_summary = state.session.get(&session_id).and_then(|f| f.summary);
    let system_prompt = turn_system_prompt(&state, &session_id).await;
    let ch = assemble_turn_channels(&state, &s, &active_cfg, &req.model, &req.base_url, &req.api_key);

    let mut notices = ch.notices.clone();
    if let Some(n) = mode_notice {
        notices.push(n);
    }
    if !auto_approve {
        notices.push(
            "无人值守会话：遇到需要审批的操作按拒绝处理并回喂模型（要放行高危操作，调用时显式 on_approval=approve）"
                .to_string(),
        );
    }

    let col = Arc::new(Collectors::default());
    let notices_cell = Arc::new(std::sync::Mutex::new(notices));
    let (gtx, _grx) = mpsc::unbounded_channel();
    let gate = HeadlessGate {
        inner: ChatGate {
            mode: state.approval_mode.clone(),
            session_modes: state.session_approval.clone(),
            allow_unsandboxed,
            unsandboxed_approval: cfg.unsandboxed_approval,
            pending: state.approvals.clone(),
            session_id: session_id.clone(),
            meta: state.approval_meta.clone(),
            approved: state.approved_scopes.clone(),
            plan_mode: req.plan_mode,
            tx: gtx,
            cancel: run_flag.clone(),
            steer: state.steer.clone(),
        },
        auto_approve,
        session_id: session_id.clone(),
        parent_session: req.parent_session.clone(),
    };
    let mem = state
        .memory
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let gateway = state.gateway.clone();
    let cancel_registry = state.cancel.clone();
    let steer_queue = state.steer.clone();
    let session_mgr = state.session.clone();
    let plan_mode = req.plan_mode;
    let sid = session_id.clone();
    let col_task = col.clone();
    let notices_task = notices_cell.clone();
    let task_tokens = AtomicU64::new(0);
    // P22-C：子代理 token 并入本任务预算的水印（无头父任务同样防拆解绕过预算）
    let subagent_seen = AtomicU64::new(subagent::subagent_tokens(&session_id));
    let budget_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // P22-C 派发器：主任务（CLI/定时/脚本唤醒）才挂；**子代理自己的轮不挂**——
    // 靠 parent_session 识别（v1 禁嵌套，深度表是第二道闸）。
    let subagents: Option<Arc<dyn crate::core::subagent::SubagentRunner>> =
        if req.parent_session.trim().is_empty() {
            Some(Arc::new(subagent::SubagentDispatcher {
                state: state.clone(),
                limits: (*state.subagent_cfg).clone(),
            }))
        } else {
            None
        };

    // 任务指定工作区：跑前切过去（会话也记到该项目下），跑完 Drop 还原全局根
    let ws_override = req.workspace.trim().to_string();

    // 整轮（含排队追加消息的有界续跑）跑在独立任务里：取消守卫随任务生命周期，
    // 超时后任务仍在后台收尾也保持着「会话在跑」的登记——不会出现双写会话文件。
    let handle = tokio::spawn(async move {
        let cancel = run_flag.clone();
        let _run = CancelRun::from_flag(cancel_registry.clone(), &sid, run_flag);
        // 守卫随任务生命周期：取消/超时后后台收尾期间工作区保持切换态，收尾完才还原
        let _ws_guard = if ws_override.is_empty() {
            None
        } else {
            let p = std::path::PathBuf::from(&ws_override);
            if p.is_dir() {
                Some(WorkspaceGuard { old: crate::tools::set_workspace_root(&p) })
            } else {
                None
            }
        };
        let mut status = "done".to_string();
        let mut error: Option<String> = None;
        let mut followups = 0u32;
        loop {
            let outcome = agent::run_task(
                &gateway,
                &agent::LoopChannels {
                    primary: ch.primary.clone(),
                    summary: ch.summary.clone(),
                    title: ch.title.clone(),
                    fallbacks: ch.fallbacks.clone(),
                    downgrade: ch.downgrade.clone(),
                    budget_hit: budget_fired.clone(),
                    // P22-C：主任务挂派发器；子代理轮不挂（禁嵌套）
                    subagents: subagents.clone(),
                },
                &system_prompt,
                &history,
                &cancel,
                &cfg,
                &sandbox_cfg,
                s.sandbox_cpu_percent.unwrap_or(0),
                &ctx_cfg,
                &rec_cfg,
                &mem,
                prior_summary.clone(),
                plan_mode,
                &gate,
                |ev| match ev {
                    agent::AgentEvent::Content(c) => {
                        col_task.answer.lock().unwrap_or_else(|e| e.into_inner()).push_str(&c)
                    }
                    agent::AgentEvent::Reasoning(r) => {
                        // P26：思考=关时不收 reasoning（渠道可能仍自行思考，用户关了就不显示）
                        if ch.primary.params.thinking.as_deref() != Some("off") {
                            col_task.reasoning.lock().unwrap_or_else(|e| e.into_inner()).push_str(&r)
                        }
                    }
                    agent::AgentEvent::ToolResult { name, ok, preview } => {
                        col_task
                            .tool_calls
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(json!({ "name": name, "ok": ok, "preview": preview }));
                    }
                    agent::AgentEvent::ApiUsage { prompt_tokens, cached_tokens } => {
                        // P18 钩子 + P21 预算降级：与 /api/chat 同一套记账
                        // P22-C：子代理花销并进本任务预算（水印增量）
                        let sub_total = subagent::subagent_tokens(&sid);
                        let sub_fresh = sub_total.saturating_sub(subagent_seen.swap(sub_total, Ordering::Relaxed));
                        let total = task_tokens.fetch_add(prompt_tokens + sub_fresh, Ordering::Relaxed)
                            + prompt_tokens
                            + sub_fresh;
                        let budget = crate::hooks::budget_tokens();
                        if crate::hooks::budget_exceeded(total, budget)
                            && !budget_fired.swap(true, Ordering::Relaxed)
                        {
                            crate::hooks::notify_budget_exceeded(&sid, total, budget);
                        }
                        col_task.requests.fetch_add(1, Ordering::Relaxed);
                        col_task.prompt_tokens.fetch_add(prompt_tokens, Ordering::Relaxed);
                        col_task.cached_tokens.fetch_add(cached_tokens.unwrap_or(0), Ordering::Relaxed);
                    }
                    // P6 会话落盘：与 /api/chat 同一落点（无头轮也续命会话文件）
                    agent::AgentEvent::Snapshot { messages } => {
                        *col_task.last_snapshot.lock().unwrap_or_else(|e| e.into_inner()) = messages.clone();
                        let cur = col_task.summary.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        if let Err(e) = session_mgr.save(&sid, &messages, cur.as_deref()) {
                            observability::event(
                                EventType::Error,
                                json!({ "message": format!("会话落盘失败: {e}") }),
                            );
                        }
                    }
                    agent::AgentEvent::Summary(sm) => {
                        *col_task.summary.lock().unwrap_or_else(|e| e.into_inner()) = Some(sm);
                    }
                    agent::AgentEvent::Title(t) => {
                        let _ = session_mgr.rename(&sid, &t);
                    }
                    agent::AgentEvent::Notice(n) => {
                        notices_task.lock().unwrap_or_else(|e| e.into_inner()).push(n);
                    }
                    _ => {}
                },
            )
            .await;
            match outcome {
                Ok(agent::LoopOutcome::Done) => {}
                Ok(agent::LoopOutcome::Cancelled) => {
                    status = "cancelled".into();
                    break;
                }
                Err(e) => {
                    observability::event(
                        EventType::Error,
                        json!({ "message": format!("无头会话出错: {e}"), "session": sid }),
                    );
                    status = "error".into();
                    error = Some(e.to_string());
                    break;
                }
            }
            // 收尾：UI 给这条会话发的排队消息（会话在跑 → chat 走排队路径）→
            // 有界续跑（最多 3 轮），不丢消息也不无限循环
            let extra = steer_queue.drain_all(&sid);
            if extra.is_empty() {
                break;
            }
            followups += 1;
            if followups > 3 {
                observability::event(
                    EventType::State,
                    json!({ "kind": "headless.leftover", "session": sid, "count": extra.len() }),
                );
                break;
            }
            let mut next = col_task
                .last_snapshot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if next.is_empty() {
                next = history.clone();
            }
            let turn_msgs: Vec<Value> =
                extra.iter().map(|t| json!({ "role": "user", "content": t })).collect();
            next.extend(turn_msgs);
            history = next;
            prior_summary = col_task.summary.lock().unwrap_or_else(|e| e.into_inner()).clone();
        }
        // P18 钩子（027）：任务结束挂点（Done / Cancelled / 出错都算结束；含续跑轮）
        crate::hooks::session_end(&sid).await;
        (status, error)
    });

    // 等整轮跑完（超时 = 取消本轮 + 返回已产出的部分；任务自己在后台收尾）
    let (mut status, mut error) = if timeout_secs == 0 {
        match handle.await {
            Ok(r) => r,
            Err(e) => ("error".to_string(), Some(format!("无头任务异常退出: {e}"))),
        }
    } else {
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), handle).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => ("error".to_string(), Some(format!("无头任务异常退出: {e}"))),
            Err(_) => {
                state.cancel.cancel(&session_id);
                // P22-C：父任务超时取消 → 级联取消它的活跃子代理
                subagent::cancel_children(&session_id);
                observability::event(
                    EventType::State,
                    json!({ "kind": "headless.timeout", "session": session_id, "secs": timeout_secs }),
                );
                ("timeout".to_string(), None)
            }
        }
    };
    if status == "timeout" {
        error = Some(format!("整轮超过 {timeout_secs} 秒已取消；任务在后台收尾，回答为已产出的部分"));
    }

    let answer = std::mem::take(&mut *col.answer.lock().unwrap_or_else(|e| e.into_inner()));
    let reasoning = std::mem::take(&mut *col.reasoning.lock().unwrap_or_else(|e| e.into_inner()));
    let tool_calls = std::mem::take(&mut *col.tool_calls.lock().unwrap_or_else(|e| e.into_inner()));
    let requests = col.requests.load(Ordering::Relaxed);
    let prompt_tokens = col.prompt_tokens.load(Ordering::Relaxed);
    let cached_tokens = col.cached_tokens.load(Ordering::Relaxed);
    let notices = std::mem::take(&mut *notices_cell.lock().unwrap_or_else(|e| e.into_inner()));
    let duration_ms = started.elapsed().as_millis();
    // 兜底：带了 error 就必须是 error 态（调用方按 status 判退出码，不能「done + error」）
    if error.is_some() && status == "done" {
        status = "error".into();
    }
    observability::event(
        EventType::State,
        json!({
            "kind": "headless.done",
            "session": session_id,
            "status": status,
            "purpose": purpose,
            "duration_ms": duration_ms,
            "requests": requests,
            "prompt_tokens": prompt_tokens,
        }),
    );
    Ok(HeadlessResult {
        session_id,
        status,
        answer,
        reasoning,
        tool_calls,
        requests,
        prompt_tokens,
        cached_tokens,
        notices,
        duration_ms,
        error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent::ApprovalGate;

    /// 无人值守的审批策略：默认拒绝；只有显式 approve 才放行（安全默认不可反）。
    #[test]
    fn headless_approval_policy_defaults_to_deny() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let gate = |approve: bool| HeadlessGate {
            inner: ChatGate {
                mode: Arc::new(std::sync::RwLock::new("auto".to_string())),
                session_modes: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                allow_unsandboxed: false,
                unsandboxed_approval: false,
                pending: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                session_id: "s1".into(),
                meta: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                approved: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                plan_mode: false,
                tx: tx.clone(),
                cancel: Arc::new(CancelFlag::new()),
                steer: Arc::new(crate::core::steer::SteerQueue::new()),
            },
            auto_approve: approve,
            session_id: "s1".into(),
            parent_session: String::new(),
        };
        let deny = gate(false);
        // auto 档下高危操作（删文件类）才需要审批——判定尺子与 /api/chat 同一把
        assert!(deny.needs_approval_for("shell", r#"{"command":"del /f /q important.txt"}"#));
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert!(!rt.block_on(deny.ask("shell", r#"{"command":"del /f /q important.txt"}"#)));
        let approve = gate(true);
        assert!(rt.block_on(approve.ask("shell", r#"{"command":"del /f /q important.txt"}"#)));
        // 无头不吃引导消息
        assert!(deny.take_steering().is_empty());
        // 档位语义仍走 ChatGate 同一把尺子（auto 档 shell 在沙箱内、文件可越界）
        assert!(deny.sandbox_enabled());
        assert!(deny.file_tools_unconfined());
        // P22-C：父任务归属（子代理轮填、主任务空）
        assert_eq!(deny.usage_parent_session(), None);
        let with_parent = gate(false);
        assert_eq!(with_parent.parent_session, "");
    }

    /// 请求反序列化：缺省值 = 安全侧（deny / 600s / purpose=headless 由 handler 补）。
    #[test]
    fn headless_request_defaults() {
        let req: HeadlessRequest = serde_json::from_str(r#"{"message":" 干活 "}"#).unwrap();
        assert_eq!(req.message, " 干活 ");
        assert!(req.session_id.is_empty());
        assert!(req.on_approval.is_empty(), "缺省不是 approve");
        assert_eq!(req.timeout_secs, 0, "缺省 0 = 不限时由 handler 解释");
        let req: HeadlessRequest =
            serde_json::from_str(r#"{"message":"m","on_approval":"approve","timeout_secs":30}"#).unwrap();
        assert!(req.on_approval.eq_ignore_ascii_case("approve"));
        assert_eq!(req.timeout_secs, 30);
    }
}
