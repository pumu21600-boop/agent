//! 审批门 ChatGate + 档位切换/审批决定端点（P19 自 server.rs 按域拆出）

use super::*;

/// 审批门（施工单 005 + 本机直跑）：SSE 发审批请求，oneshot 等用户决定；
/// 120 秒不决定视为拒绝；用户点「停止」（取消标志）立即按拒绝返回。
/// 档位读共享状态：侧栏切档即时生效（含进行中的请求）。
pub(super) struct ChatGate {
    /// 全局默认档（本会话没显式切过档时用它）
    pub(super) mode: Arc<std::sync::RwLock<String>>,
    /// M1：每会话档位覆盖（与 AppState 同一份）——并发多会话各读各的
    pub(super) session_modes: Arc<std::sync::RwLock<std::collections::HashMap<String, String>>>,
    pub(super) allow_unsandboxed: bool,
    pub(super) unsandboxed_approval: bool,
    pub(super) pending: Arc<std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
    /// P9 A3：本次请求所属会话（会话内同类允许的作用域）
    pub(super) session_id: String,
    /// P9 A3：挂起审批的元信息（id → scope/session），ask 时写入、decide 时取用
    pub(super) meta: Arc<std::sync::Mutex<std::collections::HashMap<String, (String, String)>>>,
    /// P9 A3：会话内已授权的审批范围（与 AppState.approved_scopes 同一份）
    pub(super) approved: Arc<std::sync::Mutex<std::collections::HashMap<String, std::collections::HashSet<String>>>>,
    /// P9 B1：本次请求的 Plan 档（请求级 plan_mode 带入）
    pub(super) plan_mode: bool,
    pub(super) tx: mpsc::UnboundedSender<Result<Event, Infallible>>,
    pub(super) cancel: Arc<CancelFlag>,
    /// 025：运行中追加消息队列（与 AppState 同一份）——每个决策点取一次并入本轮
    pub(super) steer: Arc<crate::core::steer::SteerQueue>,
}

impl ChatGate {
    /// 本会话生效的审批档（M1）：本会话显式切过档就用本会话的，否则全局默认。
    /// 每次判定都现读——侧栏切档对**本会话**进行中的请求即时生效（原语义保留）。
    fn effective_mode(&self) -> String {
        if let Ok(m) = self.session_modes.read()
            && let Some(v) = m.get(&self.session_id)
        {
            return v.clone();
        }
        self.mode.read().map(|m| m.clone()).unwrap_or_default()
    }
}

impl agent::ApprovalGate for ChatGate {
    /// 025：核心循环每个决策点取一次「立即发送（引导）」的消息并入本轮（原轮不打断）；
    /// 排队等下一轮的那些留在队列里，由服务层收尾取走。
    fn take_steering(&self) -> Vec<String> {
        self.steer.drain_immediate(&self.session_id)
    }

    /// P9 B2（Plan 档）：只读放行；Normal/Dangerous 直接驳回（不进审批弹窗）。
    /// 风险判定复用 rules::evaluate_risk 单点，与审批档位共用同一套语义。
    fn plan_block_reason(&self, tool: &str, args_json: &str) -> Option<String> {
        if !self.plan_mode {
            return None;
        }
        let ws = crate::tools::workspace_root();
        let val: Value = serde_json::from_str(args_json).unwrap_or_else(|_| json!({}));
        let (risk, reason) = crate::core::rules::evaluate_risk(tool, &val, &ws);
        if risk == crate::core::rules::RiskLevel::Safe {
            None
        } else {
            Some(format!("Plan 模式只放行只读操作（{}）：{}", risk.label(), reason))
        }
    }

    // 审批判定单点：只保留细粒度 needs_approval_for。
    // 旧版按工具静态 danger_level 的 needs_approval(mode, danger_level) 已随
    // 施工单 013 A2 删除——两套判定并存会造成语义分裂，禁止再加回来。
    fn needs_approval_for(&self, tool: &str, args_json: &str) -> bool {
        let mode = self.effective_mode();
        let ws = crate::tools::workspace_root();
        let val: Value = serde_json::from_str(args_json).unwrap_or_else(|_| json!({}));
        // 红线（2026-10-03 并三档轮改版）：只「撤销检查点」一项任何档位必问（回滚
        // 工作区文件不可逆）。其余破坏操作按档位走——档位即用户意愿（用户拍板）。
        if crate::core::rules::red_line_hold(tool, &val, &ws) {
            return true;
        }
        let (risk, _reason) = crate::core::rules::evaluate_risk(tool, &val, &ws);
        if (mode == "full-access" || mode == "local") && self.unsandboxed_approval {
            return risk >= crate::core::rules::RiskLevel::Dangerous;
        }
        if !crate::core::rules::should_ask_approval(&mode, risk) {
            return false;
        }
        // A3：本会话内「同类操作」已授权则免审。
        // 安全默认：Dangerous 档**不参与**会话内记忆——高危操作的设计语义就是
        // 「必须弹窗由用户确认」，逐次确认，不允许一次性放行一整类。
        if risk == crate::core::rules::RiskLevel::Dangerous {
            return true;
        }
        let scope = crate::core::rules::approval_scope(tool, &val, risk);
        let remembered = self
            .approved
            .lock()
            .ok()
            .and_then(|m| m.get(&self.session_id).map(|s| s.contains(&scope)))
            .unwrap_or(false);
        !remembered
    }

    fn sandbox_enabled(&self) -> bool {
        let mode = self.effective_mode();
        !((mode == "full-access" || mode == "local") && self.allow_unsandboxed)
    }

    /// P19：文件工具是否允许越出工作区——auto / full-access 档=true（auto 档越界写会按
    /// Dangerous 先弹审批；full-access 全免审），read-only 档锁死工作区。
    /// 每步现读：运行中切档，下一个决策步即按新档位执行。
    fn file_tools_unconfined(&self) -> bool {
        matches!(
            self.effective_mode().as_str(),
            "auto" | "full-auto" | "auto-edit" | "full-access" | "local"
        )
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn ask(&self, tool: &str, args: &str) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let (otx, orx) = tokio::sync::oneshot::channel::<bool>();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), otx);
        let ws = crate::tools::workspace_root();
        let val: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
        let (risk, reason) = crate::core::rules::evaluate_risk(tool, &val, &ws);
        // A3：记下本次审批的「同类范围」与所属会话，供 /api/approvals 回写会话内记忆
        let scope = crate::core::rules::approval_scope(tool, &val, risk);
        if let Ok(mut m) = self.meta.lock() {
            m.insert(id.clone(), (scope, self.session_id.clone()));
        }
        observability::event(
            EventType::State,
            json!({ "kind": "approval.requested", "id": id, "tool": tool, "risk": risk.as_u8(), "reason": reason }),
        );
        // 注意：SSE 可增字段（前端审批卡展示用），JSONL 事件不动——6 类 schema 已定死。
        let _ = self.tx.send(Ok(json_event(json!({
            "approval_request": {
                "id": id, "tool": tool, "args": args,
                "risk": risk.as_u8(), "risk_label": risk.label(), "reason": reason
            }
        }))));
        let pending = self.pending.clone();
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();
        let steer = self.steer.clone();
        let sid = self.session_id.clone();
        Box::pin(async move {
            // 引导让位（2026-09-27 用户实锤：审批挂 120s，期间「立即发送」毫无反应）：
            // 用户在审批等待期点了「立即发送」= 要改方向，挂着的操作按拒绝放行——
            // 引导消息本身留在队列里，下一个决策点被核心循环取走，模型拿着
            // 「操作已取消 + 新指令」马上重规划，引导秒级生效而不是等满超时。
            // （SteerQueue 没有异步通知臂，100ms 探针保留——坑 31 的既定取舍。）
            async fn wait_steer(q: Arc<crate::core::steer::SteerQueue>, sid: String) {
                loop {
                    if q.has_immediate(&sid) {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
            let decision = tokio::select! {
                r = orx => r.unwrap_or(false),
                _ = tokio::time::sleep(std::time::Duration::from_secs(120)) => false,
                // 取消走事件式等待（审计 10：原来是 100ms 手写轮询，CancelFlag 本有事件臂）
                _ = cancel.cancelled() => false,
                _ = wait_steer(steer, sid) => false,
                // 客户端断开（SSE 通道关闭）= 审批卡没人能点了（审计 10）：按拒绝放行
                // 立即返回，不再干等满 120s；服务端任务由既有取消机制兜底
                _ = tx.closed() => false,
            };
            pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            observability::event(
                EventType::State,
                json!({ "kind": "approval.decided", "id": id, "approved": decision }),
            );
            let _ = tx.send(Ok(json_event(json!({ "approval_decided": { "id": id, "approved": decision } }))));
            decision
        })
    }
}

/// POST /api/approval-mode —— 侧栏切档即时生效（含进行中的请求）。
pub(super) async fn set_approval_mode(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Some(mode) = req.get("mode").and_then(Value::as_str) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 mode" })));
    };
    if !matches!(
        mode,
        "read-only" | "sandbox" | "auto" | "full-access" | "suggest" | "auto-edit" | "full-auto" | "local"
    ) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "未知档位" })));
    }
    // 旧档名迁移（2026-10-03 并三档：sandbox 档取消——审批矩阵与 read-only 相同，
    // 旧值（localStorage/会话记忆里的）一律迁移 read-only，写文件从直接驳回变弹卡问）
    let mut mode = match mode {
        "suggest" | "sandbox" => "read-only",
        "auto-edit" | "full-auto" => "auto",
        "local" => "full-access",
        other => other,
    };
    // 完全访问需要 config 总开关支撑（脱沙箱直跑）；没开就真的降为 auto，
    // 否则前端显示「完全访问」而权限面还是沙箱档——档位与实际权限不一致。
    if mode == "full-access" && !state.agent_cfg.allow_unsandboxed {
        mode = "auto";
    }
    let mode = mode.to_string();
    // M1：带 session 就只改这条会话的档（并发多会话各切各的）；不带 session 改全局默认档
    let session = req.get("session").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if session.is_empty() {
        if let Ok(mut m) = state.approval_mode.write() {
            *m = mode.to_string();
        }
    } else if let Ok(mut m) = state.session_approval.write() {
        m.insert(session.clone(), mode.to_string());
    }
    observability::event(
        EventType::State,
        json!({ "kind": "approval.mode", "mode": mode, "session": session }),
    );
    (StatusCode::OK, Json(json!({ "ok": true, "mode": mode, "session": session })))
}

/// POST /api/approvals —— 用户对审批请求的决定。
pub(super) async fn decide_approval(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Some(id) = req.get("id").and_then(Value::as_str) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 id" })));
    };
    let approved = req.get("approved").and_then(Value::as_bool).unwrap_or(false);
    // P9 A3：remember=true 表示「本会话内同类操作不再询问」
    let remember = req.get("remember").and_then(Value::as_bool).unwrap_or(false);
    let meta = state.approval_meta.lock().ok().and_then(|mut m| m.remove(id));
    let sender = state.approvals.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    match sender {
        Some(tx) => {
            // 会话内同类允许：Dangerous 档不记忆（needs_approval_for 已拒记，此处二次兜底）
            if approved && remember
                && let Some((scope, session)) = meta {
                    let dangerous = scope.contains(":2:") || scope.ends_with(":2");
                    if !dangerous
                        && let Ok(mut m) = state.approved_scopes.lock() {
                            m.entry(session).or_default().insert(scope);
                        }
                }
            let _ = tx.send(approved);
            (StatusCode::OK, Json(json!({ "ok": true })))
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "审批不存在或已过期" })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent::ApprovalGate;
    /// M1：审批档按会话存——A 会话切「完全访问」不得影响 B 会话（并发多会话互不覆盖）。
    #[test]
    fn approval_mode_is_per_session() {
        let def = Arc::new(std::sync::RwLock::new("auto".to_string()));
        let modes = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let gate = |sid: &str| ChatGate {
            mode: def.clone(),
            session_modes: modes.clone(),
            allow_unsandboxed: false,
            unsandboxed_approval: false,
            pending: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            session_id: sid.to_string(),
            meta: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            approved: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            plan_mode: false,
            tx: tx.clone(),
            cancel: Arc::new(CancelFlag::new()),
            steer: Arc::new(crate::core::steer::SteerQueue::new()),
        };
        let a = gate("sess-a");
        let b = gate("sess-b");
        assert_eq!(a.effective_mode(), "auto");
        assert_eq!(b.effective_mode(), "auto");

        // 只给 A 会话切「完全访问」
        modes.write().unwrap_or_else(|e| e.into_inner()).insert("sess-a".into(), "full-access".into());
        assert_eq!(a.effective_mode(), "full-access");
        assert_eq!(b.effective_mode(), "auto", "别的会话不得被带着换档");

        // 全局默认变了：没切过档的会话跟着变，切过档的会话仍用自己的
        *def.write().unwrap_or_else(|e| e.into_inner()) = "read-only".into();
        assert_eq!(a.effective_mode(), "full-access");
        assert_eq!(b.effective_mode(), "read-only");
    }

    /// 2026-10-03 并三档：sandbox 档取消（旧值迁移 read-only），文件放行面按三档区分。
    #[test]
    fn tier_semantics_three_tiers_and_fs_scope() {
        let def = Arc::new(std::sync::RwLock::new("auto".to_string()));
        let modes = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let gate = |sid: &str, tier: &str| {
            modes
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(sid.to_string(), tier.to_string());
            ChatGate {
                mode: def.clone(),
                session_modes: modes.clone(),
                allow_unsandboxed: true,
                unsandboxed_approval: false,
                pending: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                session_id: sid.to_string(),
                meta: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                approved: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                plan_mode: false,
                tx: tx.clone(),
                cancel: Arc::new(CancelFlag::new()),
                steer: Arc::new(crate::core::steer::SteerQueue::new()),
            }
        };
        // 旧 sandbox 档位值仍在会话记忆里时：行为归 read-only（文件锁工作区、执行恒沙箱、
        // 写文件从「直接驳回」变「弹卡问」——并入 read-only 的询问语义）
        let s = gate("sess-s", "sandbox");
        assert!(!s.file_tools_unconfined(), "旧 sandbox 值文件工具锁死工作区");
        assert!(s.sandbox_enabled(), "旧 sandbox 值执行恒在沙箱内");
        assert!(
            s.needs_approval_for("write_file", r#"{"path":"a.txt","content":"x"}"#),
            "旧 sandbox 档位：写文件改走弹卡问（不再硬拦）"
        );
        // auto 档：文件工具放行出工作区，shell 仍沙箱
        let a = gate("sess-a", "auto");
        assert!(a.file_tools_unconfined(), "auto 档文件工具可越出工作区");
        assert!(a.sandbox_enabled(), "auto 档 shell 仍在沙箱内");
        // full-access 档：文件全放行 + 本机直跑
        let f = gate("sess-f", "full-access");
        assert!(f.file_tools_unconfined());
        assert!(!f.sandbox_enabled());
        // read-only 档：文件工具锁工作区，写/执行逐次问
        let r = gate("sess-r", "read-only");
        assert!(!r.file_tools_unconfined());
        assert!(r.needs_approval_for("shell", r#"{"command":"cargo build"}"#));
        assert!(!r.needs_approval_for("read_file", r#"{"path":"a.txt"}"#));
    }

    /// 2026-10-03 并三档轮拍板口径：只有「撤销检查点」任何档位必问；其余破坏操作按档位
    /// 走（full-access 放行、auto 问）；普通键与只读不受影响。
    #[test]
    fn red_line_hold_scoped_to_undo_checkpoint() {
        let def = Arc::new(std::sync::RwLock::new("auto".to_string()));
        let modes = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let gate = |sid: &str, tier: &str| {
            modes
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .insert(sid.to_string(), tier.to_string());
            ChatGate {
                mode: def.clone(),
                session_modes: modes.clone(),
                allow_unsandboxed: true,
                unsandboxed_approval: false,
                pending: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                session_id: sid.to_string(),
                meta: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                approved: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
                plan_mode: false,
                tx: tx.clone(),
                cancel: Arc::new(CancelFlag::new()),
                steer: Arc::new(crate::core::steer::SteerQueue::new()),
            }
        };
        // 完全访问档：撤销检查点仍必问（不可逆）；换渠道指向/直写自身配置改按档位放行
        let f = gate("sess-f", "full-access");
        assert!(
            f.needs_approval_for("ui", r#"{"method":"undo_checkpoint","id":"cp-1"}"#),
            "撤销检查点 = 回滚工作区文件不可逆，任何档位必问"
        );
        assert!(!f.needs_approval_for("settings", r#"{"action":"set","patch":{"base_url":"https://x/v1"}}"#));
        assert!(!f.needs_approval_for("write_file", r#"{"path":"config.toml","content":"x"}"#));
        assert!(!f.needs_approval_for("session", r#"{"method":"delete","id":"s1"}"#));
        assert!(!f.needs_approval_for("settings", r#"{"action":"set","patch":{"thinking":"high"}}"#));
        assert!(!f.needs_approval_for("settings", r#"{"action":"get"}"#));
        assert!(!f.needs_approval_for("write_file", r#"{"path":"src/main.rs","content":"x"}"#));
        // auto 档：破坏操作照旧问（Dangerous 档位映射），撤销检查点进红线分支也问
        let a = gate("sess-a", "auto");
        assert!(a.needs_approval_for("settings", r#"{"action":"set","patch":{"model":"glm-5.3-flash"}}"#));
        assert!(a.needs_approval_for("session", r#"{"method":"delete","id":"s1"}"#));
        assert!(a.needs_approval_for("ui", r#"{"method":"undo_checkpoint","id":"cp-1"}"#));
        assert!(!a.needs_approval_for("settings", r#"{"action":"set","patch":{"thinking":"high"}}"#));
        // read-only 档：只读静默，其余逐次问
        let r = gate("sess-r", "read-only");
        assert!(r.needs_approval_for("settings", r#"{"action":"set","patch":{"thinking":"high"}}"#));
        assert!(r.needs_approval_for("ui", r#"{"method":"undo_checkpoint","id":"cp-1"}"#));
    }

}
