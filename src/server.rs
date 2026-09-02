//! HTTP 层 —— 施工单 001 + 模型设置系统
//!
//! - 静态页托管（Svelte dist，未构建时内置页回退）
//! - /api/providers     供应商预设列表
//! - GET/PUT /api/settings  设置读写（key 用 DPAPI 加密持久化）
//! - /api/models        按当前 base_url+key 拉取渠道模型列表
//! - /api/chat          按已保存设置流式对话
//! - /cancel /api/cancel 取消标志

use std::{convert::Infallible, path::PathBuf, pin::Pin, sync::Arc};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{
        Html,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tower_http::services::{ServeDir, ServeFile};

use crate::{
    config::settings::Settings,
    core::cancel::CancelFlag,
    gateway::{self, Gateway},
    observability::{self, EventType},
};

use crate::config::AgentConfig;
use crate::core::agent::{self, AgentEvent, LoopOutcome};

#[derive(Clone)]
pub struct AppState {
    pub gateway: Arc<Gateway>,
    pub cancel: Arc<CancelFlag>,
    pub settings: Arc<std::sync::RwLock<Settings>>,
    pub agent_cfg: Arc<AgentConfig>,
    pub sandbox_cfg: Arc<crate::config::SandboxConfig>,
    pub context_cfg: Arc<crate::config::ContextConfig>,
    pub system_prompt: Arc<String>,
    /// 挂起的工具审批：approval id → 决定通道（施工单 005）
    pub approvals: Arc<std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
    /// 当前审批档位（侧栏切换即时生效，含进行中的请求）
    pub approval_mode: Arc<std::sync::RwLock<String>>,
}

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// 单轮消息（messages 为空时使用，兼容内置回退页）
    #[serde(default)]
    pub message: String,
    /// P2：完整消息数组（role: user/assistant/tool），非空则优先
    #[serde(default)]
    pub messages: Option<Vec<Value>>,
    /// 请求级覆盖：非空优先于已保存设置（页面当前选择即时生效）
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub base_url: String,
    /// 可选的请求级 key 覆盖（仅内存）；为空则用已保存设置
    #[serde(default)]
    pub api_key: String,
    /// 审批档位覆盖：suggest / auto-edit / full-auto（缺省用配置默认）
    #[serde(default)]
    pub approval_mode: String,
}

#[derive(Debug, Deserialize)]
pub struct ModelsRequest {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
}

#[derive(Debug, Deserialize)]
pub struct SettingsUpdate {
    /// 配置名称（改当前激活配置的名）
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// 弹窗里勾选保存的模型列表（多选）；Some 则整体替换
    #[serde(default)]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub clear_key: Option<bool>,
}

pub fn router(state: AppState, dist_dir: PathBuf) -> Router {
    let mut app = Router::new()
        .route("/api/providers", get(list_providers))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/models", post(list_models))
        .route("/api/configs", post(manage_configs))
        .route("/api/approvals", post(decide_approval))
        .route("/api/approval-mode", post(set_approval_mode))
        .route("/api/chat", post(chat))
        .route("/cancel", post(cancel))
        .route("/api/cancel", get(cancel_status))
        .route("/api/sandbox/selftest", get(sandbox_selftest))
        .route("/api/health", get(|| async { "ok" }))
        .with_state(state);

    let index = dist_dir.join("index.html");
    if index.exists() {
        app = app.fallback_service(ServeDir::new(dist_dir).fallback(ServeFile::new(index)));
    } else {
        observability::debug(
            "未找到 web/dist/index.html，使用内置冒烟页（cd web && npm run build 后切换为 Svelte 页）",
        );
        app = app.route("/", get(embedded_index));
    }
    app
}

fn json_event(payload: Value) -> Event {
    Event::default().json_data(payload).expect("SSE 事件序列化失败")
}

/// 供应商预设列表（数据驱动自 providers.json）。
async fn list_providers() -> Json<Value> {
    let list: Vec<Value> = gateway::providers::list()
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "name": p.name,
                "default_base_url": p.default_base_url,
                "default_model": p.default_model,
            })
        })
        .collect();
    Json(json!({ "providers": list }))
}

/// 已保存 key 的尾 4 位提示（完整 key 不出内存、不落日志）。
fn key_hint_of(key: Option<String>) -> String {
    key.as_ref()
        .map(|k| {
            let n = k.chars().count();
            if n > 4 { k.chars().skip(n - 4).collect::<String>() } else { k.clone() }
        })
        .unwrap_or_default()
}

async fn get_settings(State(state): State<AppState>) -> Json<Value> {
    let s = state.settings.read().unwrap().clone();
    let mut v = config_state_json(&s);
    v["env_key"] = json!(std::env::var("DEEPSEEK_API_KEY").is_ok());
    Json(v)
}

/// 保存设置。api_key 非空则 DPAPI 加密持久化到当前激活配置；clear_key=true 则清除。
async fn put_settings(
    State(state): State<AppState>,
    Json(req): Json<SettingsUpdate>,
) -> (StatusCode, Json<Value>) {
    let mut s = state.settings.write().unwrap().clone();
    // 渠道地址变更检测：换渠道后旧渠道的模型清单/当前模型作废（重新获取重新勾选）
    let url_before = crate::config::settings::normalize_base(&s.effective_base_url());
    if let Some(v) = &req.name {
        s.set_active_name(v);
    }
    if let Some(v) = &req.provider_id {
        if !v.trim().is_empty() {
            s.provider_id = v.trim().to_string();
        }
    }
    if let Some(v) = &req.base_url {
        s.base_url = v.trim().to_string();
    }
    if let Some(v) = &req.model {
        s.model = v.trim().to_string();
    }
    if let Some(v) = &req.models {
        let mut list: Vec<String> =
            v.iter().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect();
        list.sort();
        list.dedup();
        s.models = list;
    }
    if url_before != crate::config::settings::normalize_base(&s.effective_base_url()) {
        s.models.clear();
        s.model.clear();
        observability::event(
            EventType::State,
            json!({ "kind": "settings.channel_changed", "base_url": s.effective_base_url() }),
        );
    }
    if req.clear_key == Some(true) {
        if let Err(e) = s.clear_key() {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
        }
    } else if let Some(k) = &req.api_key {
        if !k.trim().is_empty() {
            if let Err(e) = s.set_key(k.trim()) {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
            }
        }
    }
    // 改动即时回写激活配置（配置即生效，无"保存"概念）
    s.sync_config_from_mirror();
    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    *state.settings.write().unwrap() = s.clone();
    observability::event(
        EventType::State,
        json!({ "kind": "settings.updated", "provider_id": s.provider_id, "model": s.effective_model() }),
    );
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "has_key": s.get_key().is_some(), "key_hint": key_hint_of(s.get_key()) })),
    )
}

/// 拉取渠道模型列表。请求里没给 base_url/key 就用已保存设置。
async fn list_models(
    State(state): State<AppState>,
    Json(req): Json<ModelsRequest>,
) -> (StatusCode, Json<Value>) {
    let s = state.settings.read().unwrap().clone();
    let base_url = if req.base_url.trim().is_empty() { s.effective_base_url() } else { req.base_url.trim().to_string() };
    let api_key = if !req.api_key.trim().is_empty() {
        req.api_key.trim().to_string()
    } else {
        s.get_key().or_else(|| std::env::var("DEEPSEEK_API_KEY").ok()).unwrap_or_default()
    };
    if base_url.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "base_url 为空" })));
    }
    if api_key.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "未设置 API key" })));
    }
    match state.gateway.list_models(&base_url, &api_key).await {
        Ok(models) => (StatusCode::OK, Json(json!({ "models": models }))),
        Err(e) => {
            observability::event(EventType::Error, json!({ "message": e.to_string() }));
            (StatusCode::BAD_GATEWAY, Json(json!({ "error": e.to_string() })))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ConfigRequest {
    /// new（新建并激活，起点复制当前配置）| delete | apply（切换激活）
    pub action: String,
    #[serde(default)]
    pub id: Option<String>,
}

/// 配置管理：new / delete / apply。改动即时生效（顶层字段即激活配置镜像）。
async fn manage_configs(
    State(state): State<AppState>,
    Json(req): Json<ConfigRequest>,
) -> (StatusCode, Json<Value>) {
    let mut s = state.settings.write().unwrap().clone();
    let err =
        |m: &str| (StatusCode::BAD_REQUEST, Json(json!({ "error": m })));
    match req.action.as_str() {
        "new" => {
            s.sync_config_from_mirror();
            s.new_config();
        }
        "delete" => {
            let Some(id) = req.id else { return err("缺少配置 id") };
            if !s.delete_config(&id) {
                return err("配置不存在");
            }
        }
        "apply" => {
            let Some(id) = req.id else { return err("缺少配置 id") };
            if !s.activate(&id) {
                return err("配置不存在");
            }
        }
        other => return err(&format!("未知操作: {other}")),
    }
    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    *state.settings.write().unwrap() = s.clone();
    observability::event(
        EventType::State,
        json!({ "kind": "config.changed", "action": req.action, "provider_id": s.provider_id, "model": s.model }),
    );
    (StatusCode::OK, Json(config_state_json(&s)))
}

/// 配置快照（含各配置 key 状态），供前端全量刷新。
fn config_state_json(s: &Settings) -> Value {
    json!({
        "ok": true,
        "configs": s.configs.iter().map(|c| {
            let key = s.config_key(c);
            json!({
                "id": c.id,
                "name": c.name.clone(),
                "provider_id": c.provider_id,
                "base_url": c.base_url.clone(),
                "model": c.model.clone(),
                "models": c.models.clone(),
                "has_key": key.is_some(),
                "key_hint": key_hint_of(key),
            })
        }).collect::<Vec<Value>>(),
        "active_config_id": s.active_config_id,
        "name": s.active().map(|c| c.name.clone()).unwrap_or_default(),
        "provider_id": s.provider_id,
        "base_url": s.base_url,
        "effective_base_url": s.effective_base_url(),
        "model": s.model,
        "models": s.models,
        "has_key": s.get_key().is_some(),
        "key_hint": key_hint_of(s.get_key()),
    })
}

/// 流式对话：base_url/model/key 全部来自已保存设置（请求级 api_key 可覆盖）。
async fn chat(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Sse<UnboundedReceiverStream<Result<Event, Infallible>>> {
    let message = req.message.trim().to_string();
    let req_model = req.model.trim().to_string();
    let req_base_url = req.base_url.trim().to_string();
    let req_api_key = req.api_key.trim().to_string();
    let history: Vec<Value> = match &req.messages {
        Some(m) if !m.is_empty() => m.clone(),
        _ => {
            if message.is_empty() {
                vec![]
            } else {
                vec![json!({ "role": "user", "content": message })]
            }
        }
    };
    let (tx, rx) = mpsc::unbounded_channel::<Result<Event, Infallible>>();
    let gateway = state.gateway.clone();
    let cancel = state.cancel.clone();
    let s = state.settings.read().unwrap().clone();
    let cfg = (*state.agent_cfg).clone();
    let sandbox_cfg = (*state.sandbox_cfg).clone();
    let ctx_cfg = (*state.context_cfg).clone();
    let prompt = state.system_prompt.clone();
    let approvals = state.approvals.clone();
    // 审批档位归一化（Codex 式三档）：旧档名映射保留兼容 → 写入共享档位供 gate 活读。
    // full-access 需 config 总开关，没开就回退 auto 并告知前端。
    let norm = |m: &str| -> String {
        match m.trim() {
            "suggest" => "read-only".into(),
            "auto-edit" | "full-auto" => "auto".into(),
            "local" | "full-access" => "full-access".into(),
            other @ ("read-only" | "auto") => other.to_string(),
            _ => "auto".into(),
        }
    };
    let allow_unsandboxed = cfg.allow_unsandboxed;
    let mode = if req.approval_mode.trim().is_empty() {
        norm(&cfg.default_approval)
    } else {
        norm(&req.approval_mode)
    };
    if mode == "full-access" && !allow_unsandboxed {
        let _ = tx.send(Ok(json_event(json!({
            "notice": "完全访问（脱沙箱）未开启：在 config.toml [agent] allow_unsandboxed = true 后重启服务生效，本次已回退为沙箱内自动"
        }))));
    }
    if let Ok(mut m) = state.approval_mode.write() {
        *m = mode;
    }

    // 新一轮开始：复位取消标志
    cancel.clear();

    tokio::spawn(async move {
        let send = |payload: Value| {
            let _ = tx.send(Ok(json_event(payload)));
        };
        if history.is_empty() {
            send(json!({ "error": "消息为空" }));
            return;
        }
        // 请求级覆盖优先：页面当前选择即时生效，不等保存
        let base_url = if !req_base_url.is_empty() { req_base_url } else { s.effective_base_url() };
        let model = if !req_model.is_empty() { req_model } else { s.effective_model() };
        let api_key = if !req_api_key.is_empty() {
            req_api_key
        } else {
            s.get_key().or_else(|| std::env::var("DEEPSEEK_API_KEY").ok()).unwrap_or_default()
        };
        let outcome = agent::run_task(
            &gateway,
            &base_url,
            &model,
            &api_key,
            // 把后端知道的模型名作为事实注入（模型自己不知道中转渠道里它叫什么）
            &format!(
                "{prompt}\n\n（系统注入的事实：你此刻运行的底层模型是「{model}」，经渠道 {base_url} 调用。被问是什么模型时，直接照实回答这个名字，不要说不知道。）"
            ),
            &history,
            &cancel,
            &cfg,
            &sandbox_cfg,
            &ctx_cfg,
            &ChatGate {
                mode: state.approval_mode.clone(),
                allow_unsandboxed,
                unsandboxed_approval: cfg.unsandboxed_approval,
                pending: approvals,
                tx: tx.clone(),
                cancel: cancel.clone(),
            },
            |ev| match ev {
                AgentEvent::Content(c) => send(json!({ "content": c })),
                AgentEvent::Reasoning(r) => send(json!({ "reasoning": r })),
                AgentEvent::ToolCall { name, args } => {
                    send(json!({ "tool_call": { "name": name, "args": args } }))
                }
                AgentEvent::ToolResult { name, ok, preview } => {
                    send(json!({ "tool_result": { "name": name, "ok": ok, "preview": preview } }))
                }
                AgentEvent::StepLimit(n) => send(json!({ "step_limit": n })),
                AgentEvent::ContextUsage { tokens, budget, breakdown } => send(json!({
                    "context_usage": {
                        "tokens": tokens,
                        "budget": budget,
                        "breakdown": {
                            "messages": breakdown.messages,
                            "tools": breakdown.tools,
                            "system": breakdown.system,
                            "other": breakdown.other,
                        },
                    }
                })),
            },
        )
        .await;
        match outcome {
            Ok(LoopOutcome::Done) => send(json!({ "done": true })),
            Ok(LoopOutcome::Cancelled) => send(json!({ "cancelled": true })),
            Err(e) => {
                observability::event(EventType::Error, json!({ "message": e.to_string() }));
                send(json!({ "error": e.to_string() }));
            }
        }
    });

    Sse::new(UnboundedReceiverStream::new(rx))
}

/// POST /cancel —— 置位全局取消标志。
async fn cancel(State(state): State<AppState>) -> Json<Value> {
    state.cancel.set();
    observability::event(EventType::State, json!({ "kind": "cancel", "flag": true }));
    Json(json!({ "cancelled": true }))
}

/// GET /api/cancel —— 查询取消标志当前状态。
async fn cancel_status(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "cancelled": state.cancel.is_set() }))
}

/// 审批门（施工单 005 + 本机直跑）：SSE 发审批请求，oneshot 等用户决定；
/// 120 秒不决定视为拒绝；用户点「停止」（取消标志）立即按拒绝返回。
/// 档位读共享状态：侧栏切档即时生效（含进行中的请求）。
struct ChatGate {
    mode: Arc<std::sync::RwLock<String>>,
    allow_unsandboxed: bool,
    unsandboxed_approval: bool,
    pending: Arc<std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
    tx: mpsc::UnboundedSender<Result<Event, Infallible>>,
    cancel: Arc<CancelFlag>,
}

impl agent::ApprovalGate for ChatGate {
    fn needs_approval(&self, danger_level: u8) -> bool {
        let mode = self.mode.read().map(|m| m.clone()).unwrap_or_default();
        // 完全访问档默认免审批；unsandboxed_approval=true 时执行类仍要批
        if (mode == "full-access" || mode == "local") && self.unsandboxed_approval {
            return danger_level >= 2;
        }
        agent::needs_approval(&mode, danger_level)
    }

    fn sandbox_enabled(&self) -> bool {
        let mode = self.mode.read().map(|m| m.clone()).unwrap_or_default();
        !((mode == "full-access" || mode == "local") && self.allow_unsandboxed)
    }

    fn ask(&self, tool: &str, args: &str) -> Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let (otx, orx) = tokio::sync::oneshot::channel::<bool>();
        self.pending.lock().unwrap().insert(id.clone(), otx);
        observability::event(
            EventType::State,
            json!({ "kind": "approval.requested", "id": id, "tool": tool }),
        );
        let _ = self.tx.send(Ok(json_event(json!({
            "approval_request": { "id": id, "tool": tool, "args": args }
        }))));
        let pending = self.pending.clone();
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();
        Box::pin(async move {
            async fn wait_cancelled(flag: Arc<CancelFlag>) {
                loop {
                    if flag.is_set() {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
            let decision = tokio::select! {
                r = orx => r.unwrap_or(false),
                _ = tokio::time::sleep(std::time::Duration::from_secs(120)) => false,
                _ = wait_cancelled(cancel) => false,
            };
            pending.lock().unwrap().remove(&id);
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
async fn set_approval_mode(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Some(mode) = req.get("mode").and_then(Value::as_str) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 mode" })));
    };
    if !matches!(mode, "read-only" | "auto" | "full-access" | "suggest" | "auto-edit" | "full-auto" | "local") {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "未知档位" })));
    }
    let mode = match mode {
        "suggest" => "read-only",
        "auto-edit" | "full-auto" => "auto",
        "local" => "full-access",
        other => other,
    };
    if let Ok(mut m) = state.approval_mode.write() {
        *m = mode.to_string();
    }
    observability::event(EventType::State, json!({ "kind": "approval.mode", "mode": mode }));
    (StatusCode::OK, Json(json!({ "ok": true, "mode": mode })))
}

/// POST /api/approvals —— 用户对审批请求的决定。
async fn decide_approval(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Some(id) = req.get("id").and_then(Value::as_str) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 id" })));
    };
    let approved = req.get("approved").and_then(Value::as_bool).unwrap_or(false);
    let sender = state.approvals.lock().unwrap().remove(id);
    match sender {
        Some(tx) => {
            let _ = tx.send(approved);
            (StatusCode::OK, Json(json!({ "ok": true })))
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "审批不存在或已过期" })),
        ),
    }
}

/// GET /api/sandbox/selftest —— 跑快速探针（真实拉起受限进程），逐项返回结果。
/// 阻塞在 spawn_blocking 里做；kill-tree / 内存上限探针较慢，只在 cargo test 跑。
async fn sandbox_selftest(State(state): State<AppState>) -> Json<Value> {
    let cfg = (*state.sandbox_cfg).clone();
    let results = tokio::task::spawn_blocking(move || crate::sandbox::probes::run_fast_suite(&cfg))
        .await
        .unwrap_or_default();
    observability::event(
        EventType::State,
        json!({ "kind": "sandbox.selftest", "ok": results.iter().all(|r| r.ok) }),
    );
    Json(json!({
        "ok": results.iter().all(|r| r.ok),
        "results": results
            .iter()
            .map(|r| json!({ "name": r.name, "ok": r.ok, "detail": r.detail }))
            .collect::<Vec<Value>>(),
    }))
}

/// 内置冒烟页（frontend/dist 未构建时的回退）：设置面板 + 输入框 + 发送 + 流式显示 + 停止。
async fn embedded_index() -> Html<&'static str> {
    Html(EMBEDDED_INDEX)
}

const EMBEDDED_INDEX: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Agent</title>
</head>
<body>
<h3>模型设置</h3>
<div id="settings">
  供应商: <select id="provider"></select><br>
  Base URL: <input id="baseUrl" style="width:60%"><br>
  API Key: <input id="key" type="password" style="width:60%"><br>
  模型: <select id="model"></select>
  <button id="fetchModels">获取模型</button>
  <button id="save">保存</button>
  <span id="settingsMsg"></span>
</div>
<div id="modal" style="display:none;position:fixed;inset:0;background:rgba(0,0,0,.4);z-index:10">
  <div style="background:#fff;border-radius:6px;width:560px;max-height:80vh;margin:8vh auto;padding:12px;display:flex;flex-direction:column">
    <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:8px">
      <b id="modalTitle">获取到的模型</b>
      <span>
        <button id="selAll">全选</button>
        <button id="selNone">全不选</button>
        <button id="modalSave">保存</button>
        <button id="modalClose">关闭</button>
      </span>
    </div>
    <div id="modalList" style="overflow-y:auto;border:1px solid #eee;padding:6px"></div>
  </div>
</div>
<hr>
<input id="msg" placeholder="输入消息，Enter 发送" style="width:60%">
<button id="send">发送</button>
<button id="stop" disabled>停止</button>
<pre id="out" style="white-space:pre-wrap;border:1px solid #ccc;min-height:200px;padding:8px"></pre>
<script>
let controller = null;
const out = document.getElementById('out');
const $ = (id) => document.getElementById(id);
function msg(t) { $('settingsMsg').textContent = t; }

async function loadProviders() {
  const r = await fetch('/api/providers').then(r => r.json());
  const sel = $('provider');
  sel.innerHTML = '';
  for (const p of r.providers) {
    const o = document.createElement('option');
    o.value = p.id; o.textContent = p.name;
    o.dataset.base = p.default_base_url || '';
    o.dataset.model = p.default_model || '';
    sel.appendChild(o);
  }
  sel.onchange = () => {
    const o = sel.selectedOptions[0];
    if (o.dataset.base) $('baseUrl').value = o.dataset.base;
    if (o.dataset.model) setModelSelect([], o.dataset.model);
  };
}

let savedModels = [];

function setModelSelect(list, current) {
  const sel = $('model');
  const cur = current !== undefined ? current : sel.value;
  sel.innerHTML = '';
  if (cur && !list.includes(cur)) {
    const o = document.createElement('option');
    o.value = cur; o.textContent = cur;
    sel.appendChild(o);
  }
  for (const m of list) {
    const o = document.createElement('option');
    o.value = m; o.textContent = m;
    sel.appendChild(o);
  }
  if (!list.length && !cur) {
    const o = document.createElement('option');
    o.value = ''; o.textContent = '（先获取模型）';
    sel.appendChild(o);
  }
  sel.value = cur || '';
}

async function loadSettings() {
  const s = await fetch('/api/settings').then(r => r.json());
  $('provider').value = s.provider_id || 'custom';
  $('baseUrl').value = s.base_url || $('provider').selectedOptions[0].dataset.base || '';
  savedModels = s.models || [];
  setModelSelect(savedModels, s.model || '');
  $('key').placeholder = s.has_key
    ? '已保存(…' + (s.key_hint || '') + ')，留空不修改'
    : 'sk-...';
}

let fetchedModels = [];
const picked = new Set();

function openModal(list) {
  fetchedModels = list;
  picked.clear();
  for (const m of list) if (savedModels.includes(m)) picked.add(m);
  $('modalTitle').textContent = '获取到的模型（' + list.length + ' 个）';
  const box = $('modalList');
  box.innerHTML = '';
  for (const m of list) {
    const l = document.createElement('label');
    l.style.cssText = 'display:flex;gap:6px;align-items:center;padding:3px 0;font-family:monospace;font-size:13px';
    const c = document.createElement('input');
    c.type = 'checkbox'; c.checked = picked.has(m);
    c.onchange = () => { c.checked ? picked.add(m) : picked.delete(m); };
    l.appendChild(c);
    l.appendChild(document.createTextNode(m));
    box.appendChild(l);
  }
  $('modal').style.display = 'block';
}

$('fetchModels').onclick = async () => {
  msg('获取模型中…');
  try {
    const r = await fetch('/api/models', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ base_url: $('baseUrl').value, api_key: $('key').value }),
    });
    const d = await r.json();
    if (!r.ok) throw new Error(d.error || ('HTTP ' + r.status));
    openModal(d.models || []);
    msg('');
  } catch (e) { msg('失败: ' + e.message); }
};

$('selAll').onclick = () => {
  for (const c of $('modalList').querySelectorAll('input')) c.checked = true;
  picked.clear(); fetchedModels.forEach(m => picked.add(m));
};
$('selNone').onclick = () => {
  for (const c of $('modalList').querySelectorAll('input')) c.checked = false;
  picked.clear();
};
$('modalClose').onclick = () => { $('modal').style.display = 'none'; };
$('modalSave').onclick = async () => {
  const list = [...picked].sort();
  let cur = $('model').value;
  if (!list.includes(cur) && list.length) cur = list[0];
  const body = {
    provider_id: $('provider').value,
    base_url: $('baseUrl').value,
    model: cur,
    models: list,
    api_key: $('key').value.trim(),
  };
  const r = await fetch('/api/settings', {
    method: 'PUT', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  const d = await r.json();
  if (d.error) { msg('保存失败: ' + d.error); return; }
  savedModels = list;
  $('key').value = '';
  $('key').placeholder = d.has_key ? '已保存（留空不修改）' : 'sk-...';
  setModelSelect(list, cur);
  $('modal').style.display = 'none';
  msg('已保存 ' + list.length + ' 个模型');
};

$('save').onclick = async () => {
  const body = {
    provider_id: $('provider').value,
    base_url: $('baseUrl').value,
    model: $('model').value,
    models: savedModels,
    api_key: $('key').value.trim(),
  };
  const r = await fetch('/api/settings', {
    method: 'PUT', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  });
  const d = await r.json();
  if (d.error) { msg('保存失败: ' + d.error); return; }
  $('key').value = '';
  $('key').placeholder = d.has_key ? '已保存（留空不修改）' : 'sk-...';
  msg('已保存');
};

function finish() { controller = null; $('stop').disabled = true; }
function send() {
  const m = $('msg').value.trim();
  if (!m || controller) return;
  controller = new AbortController();
  $('stop').disabled = false;
  out.textContent += '\n[你] ' + m + '\n[模型] ';
  fetch('/api/chat', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ message: m, model: $('model').value, base_url: $('baseUrl').value }),
    signal: controller.signal,
  })
    .then(res => {
      if (!res.ok) throw new Error('HTTP ' + res.status);
      const reader = res.body.getReader();
      const dec = new TextDecoder();
      let buf = '';
      function pump() {
        return reader.read().then(({ done, value }) => {
          if (done) { finish(); return; }
          buf += dec.decode(value, { stream: true });
          let idx;
          while ((idx = buf.indexOf('\n\n')) !== -1) {
            const raw = buf.slice(0, idx);
            buf = buf.slice(idx + 2);
            for (const line of raw.split('\n')) {
              if (!line.startsWith('data: ')) continue;
              try {
                const d = JSON.parse(line.slice(6));
                if (d.content) out.textContent += d.content;
                else if (d.reasoning) out.textContent += d.reasoning;
                else if (d.error) out.textContent += '\n[错误] ' + d.error;
                else if (d.cancelled) out.textContent += '\n[已停止]';
              } catch (e) { /* 忽略坏行 */ }
            }
          }
          return pump();
        });
      }
      return pump();
    })
    .catch(e => {
      if (e.name !== 'AbortError') out.textContent += '\n[连接错误] ' + e;
      finish();
    });
}
function stop() {
  fetch('/cancel', { method: 'POST' });
  if (controller) controller.abort();
}
$('send').onclick = send;
$('stop').onclick = stop;
$('msg').addEventListener('keydown', e => { if (e.key === 'Enter') send(); });
loadProviders().then(loadSettings);
</script>
</body>
</html>
"#;
