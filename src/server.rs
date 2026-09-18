//! HTTP 层 —— 施工单 001 + 模型设置系统
//!
//! - 静态页托管（Svelte dist，未构建时内置页回退）
//! - /api/providers     供应商预设列表
//! - GET/PUT /api/settings  设置读写（key 用 DPAPI 加密持久化）
//! - /api/models        按当前 base_url+key 拉取渠道模型列表
//! - /api/chat          按已保存设置流式对话
//! - /cancel /api/cancel 取消标志（会话级：?session=<id> 只停该会话；不带 = 停全部）

use std::{convert::Infallible, path::PathBuf, pin::Pin, sync::Arc};

use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, Request, State},
    http::{
        HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE, HOST, ORIGIN},
    },
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tower_http::services::{ServeDir, ServeFile};

use crate::{
    config::settings::Settings,
    core::cancel::{CancelFlag, CancelRegistry},
    gateway::{self, Gateway},
    observability::{self, EventType},
};

use crate::config::AgentConfig;
use crate::core::agent::{self, AgentEvent, LoopOutcome};

#[derive(Clone)]
pub struct AppState {
    pub gateway: Arc<Gateway>,
    /// 会话级取消注册表（D2）：每轮任务 begin 登记、结束注销，停止按钮只停目标会话
    pub cancel: Arc<CancelRegistry>,
    /// 运行中追加消息队列（025）：同会话已在跑时，新消息进这里并入当前一轮
    pub steer: Arc<crate::core::steer::SteerQueue>,
    pub settings: Arc<std::sync::RwLock<Settings>>,
    pub agent_cfg: Arc<AgentConfig>,
    pub sandbox_cfg: Arc<crate::config::SandboxConfig>,
    pub context_cfg: Arc<crate::config::ContextConfig>,
    pub recovery_cfg: Arc<crate::config::RecoveryConfig>,
    pub system_prompt: Arc<String>,
    /// 系统约定文件 prompts/system.md：记忆面板可编辑，每轮任务实时读盘（保存即生效）
    pub prompt_path: Arc<PathBuf>,
    /// 会话落盘（P6.1 多会话）：Snapshot 事件按 session_id 写入
    pub session: Arc<crate::session::SessionManager>,
    /// 日志目录（记忆库索引 <log_dir>/memory/library.json 的落点）
    pub log_dir: Arc<PathBuf>,
    /// P7 记忆层（events.log / AGENTS.md / index；开关在 [memory]）
    /// P8.2：工作区可切换 → 记忆层跟着重开，用 RwLock 换引用
    pub memory: Arc<std::sync::RwLock<Arc<crate::memory::Memory>>>,
    /// P8.2 记忆配置（切换工作区时重开 Memory 用）
    pub mem_cfg: Arc<crate::config::MemoryConfig>,
    /// 挂起的工具审批：approval id → 决定通道（施工单 005）
    pub approvals: Arc<std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
    /// P9 A3：挂起审批的附加信息 —— approval id → (scope, session_id)，
    /// 供「本会话内同类允许」在用户决定时回写记忆。
    pub approval_meta: Arc<std::sync::Mutex<std::collections::HashMap<String, (String, String)>>>,
    /// P9 A3：会话内已授权的审批范围 —— session_id → scope 集合（见 core::rules::approval_scope）。
    /// **仅内存，不落 settings.json**；清空会话时一并清除，服务重启即失效。
    pub approved_scopes: Arc<std::sync::Mutex<std::collections::HashMap<String, std::collections::HashSet<String>>>>,
    /// 当前审批档位（侧栏切换即时生效，含进行中的请求）
    pub approval_mode: Arc<std::sync::RwLock<String>>,
    /// 每会话审批档覆盖（M1）：并发多会话各跑各的档位，互不覆盖；
    /// 没有条目的会话用上面的全局默认档。仅内存，重启即失效（与档位本身语义一致）。
    pub session_approval: Arc<std::sync::RwLock<std::collections::HashMap<String, String>>>,
    /// P8 角色映射（config.toml [roles]：summary/title/embedding 的配置名）
    pub roles_cfg: Arc<crate::config::RolesConfig>,
    /// P8 网关级配置（config.toml [gateway]：max_rpm / fallback_configs）
    pub gateway_cfg: Arc<crate::config::GatewayConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaAttachment {
    pub name: String,
    pub mime: String,
    pub data: String,
}

impl MediaAttachment {
    pub fn is_audio(&self) -> bool {
        let n = self.name.to_lowercase();
        self.mime.starts_with("audio/")
            || n.ends_with(".mp3")
            || n.ends_with(".wav")
            || n.ends_with(".m4a")
            || n.ends_with(".ogg")
            || n.ends_with(".flac")
            || n.ends_with(".aac")
            || n.ends_with(".wma")
    }

    pub fn is_video(&self) -> bool {
        let n = self.name.to_lowercase();
        self.mime.starts_with("video/")
            || n.ends_with(".mp4")
            || n.ends_with(".webm")
            || n.ends_with(".mov")
            || n.ends_with(".avi")
            || n.ends_with(".mkv")
            || n.ends_with(".flv")
    }

    pub fn is_image(&self) -> bool {
        let n = self.name.to_lowercase();
        self.mime.starts_with("image/")
            || n.ends_with(".png")
            || n.ends_with(".jpg")
            || n.ends_with(".jpeg")
            || n.ends_with(".webp")
            || n.ends_with(".gif")
            || n.ends_with(".bmp")
            || n.ends_with(".svg")
            || (!self.is_audio() && !self.is_video())
    }

    pub fn audio_format(&self) -> String {
        let n = self.name.to_lowercase();
        if n.ends_with(".wav") || self.mime.contains("wav") {
            "wav".into()
        } else if n.ends_with(".ogg") || self.mime.contains("ogg") {
            "ogg".into()
        } else if n.ends_with(".m4a") || self.mime.contains("m4a") {
            "m4a".into()
        } else if n.ends_with(".flac") || self.mime.contains("flac") {
            "flac".into()
        } else if n.ends_with(".aac") || self.mime.contains("aac") {
            "aac".into()
        } else {
            "mp3".into()
        }
    }

    pub fn raw_base64(&self) -> String {
        if let Some(idx) = self.data.find(";base64,") {
            self.data[idx + 8..].to_string()
        } else if self.data.starts_with("data:") {
            if let Some(idx) = self.data.find(',') {
                self.data[idx + 1..].to_string()
            } else {
                self.data.clone()
            }
        } else {
            self.data.clone()
        }
    }

    pub fn data_url(&self) -> String {
        if self.data.starts_with("data:") {
            self.data.clone()
        } else {
            format!("data:{};base64,{}", self.effective_mime(), self.data)
        }
    }

    pub fn effective_mime(&self) -> String {
        if !self.mime.is_empty() && self.mime != "application/octet-stream" {
            return self.mime.clone();
        }
        let n = self.name.to_lowercase();
        if n.ends_with(".mp3") { "audio/mp3".into() }
        else if n.ends_with(".wav") { "audio/wav".into() }
        else if n.ends_with(".m4a") { "audio/m4a".into() }
        else if n.ends_with(".ogg") { "audio/ogg".into() }
        else if n.ends_with(".mp4") { "video/mp4".into() }
        else if n.ends_with(".webm") { "video/webm".into() }
        else if n.ends_with(".mov") { "video/quicktime".into() }
        else if n.ends_with(".png") { "image/png".into() }
        else if n.ends_with(".jpg") || n.ends_with(".jpeg") { "image/jpeg".into() }
        else if n.ends_with(".webp") { "image/webp".into() }
        else { "application/octet-stream".into() }
    }
}

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// 单轮消息（messages 为空时使用，兼容内置回退页）
    #[serde(default)]
    pub message: String,
    /// P2：完整消息数组（role: user/assistant/tool），非空则优先
    #[serde(default)]
    pub messages: Option<Vec<Value>>,
    /// P8.5 多模态媒体附件（图片、视频、音频）
    #[serde(default)]
    pub attachments: Option<Vec<MediaAttachment>>,
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
    /// P6.1 多会话：消息落进哪条会话；缺省自动新建（前端从 SSE `session` 事件拿回 id）
    #[serde(default)]
    pub session_id: String,
    /// 027：同会话正在跑时这条消息怎么处理——false（缺省）= 排队，等这一轮跑完自动发；
    /// true = 立即（引导）：并进正在跑的那一轮，模型下一个决策点就看到（不打断、不另起一轮）
    #[serde(default)]
    pub steer: bool,
    /// P9 B1：Plan 档（true=只读+先出计划；请求级字段，前端随会话持久化到 localStorage）
    #[serde(default)]
    pub plan_mode: bool,
}

#[derive(Debug, Deserialize)]
pub struct ModelsRequest {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
}
/// 设置更新请求。B1 清除语义（施工单 012 固化，前后端任何一方改动必须对照本条）：
///
/// - `0` = 清除长度类（context_length / max_output_tokens）
/// - 负数 = 清除采样类（temperature / top_p）
/// - 空串 / auto = 清除思考类（thinking / thinking_style）
/// - clear_key=true = 清 key
///
/// 其余字段：null / 缺省 = 不动。
#[derive(Debug, Deserialize, Default)]
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
    /// 当前模型上下文长度（tokens；0=清除恢复全局默认）
    #[serde(default)]
    pub context_length: Option<u64>,
    /// 当前模型最大输出长度（tokens；0=清除）
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// 采样参数（P8；null/缺省=不动，0 或 -1=清除走渠道默认）
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    /// 思考开关（P9）："on"/"off"/档位（low/medium/high/xhigh/max/minimal）/""（空=清除走渠道默认）
    #[serde(default)]
    pub thinking: Option<String>,
    /// 思考参数形态覆盖（P9，OpenClaw thinkingFormat 词表）："auto"/""=端点探测；
    /// none/openai/deepseek/zai/qwen/qwen-chat-template；P8.3 旧值自动映射
    #[serde(default)]
    pub thinking_style: Option<String>,
    /// 模型能力标记（工具调用/图片/视频/音频解析）。Some = 用户手动勾选并保存
    #[serde(default)]
    pub capabilities: Option<crate::config::settings::ModelCapabilities>,
    /// 角色路由映射（摘要/标题/多模态旁路/降级链）
    #[serde(default)]
    pub roles: Option<crate::config::settings::RolesSettings>,
}

pub fn router(state: AppState, dist_dir: PathBuf) -> Router {
    let mut app = Router::new()
        .route("/api/providers", get(list_providers))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/models", post(list_models))
        .route("/api/configs", post(manage_configs))
        .route("/api/workspace", get(get_workspace).post(set_workspace))
        .route("/api/approvals", post(decide_approval))
        .route("/api/approval-mode", post(set_approval_mode))
        .route("/api/chat", post(chat))
        .route("/api/session", get(get_session).delete(clear_session))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/new", post(new_session))
        .route("/api/sessions/open", post(open_session))
        .route("/api/sessions/meta", post(session_meta))
        .route("/api/sessions/fork", post(fork_session))
        .route("/api/sessions/batch", post(batch_sessions))
        .route("/api/workspace/picker", post(pick_workspace))
        .route("/api/workspace/remove", post(remove_workspace))
        .route("/api/workspace/alias", post(alias_workspace))
        .route("/api/handoff", post(make_handoff))
        .route("/api/checkpoints", get(list_checkpoints))
        .route("/api/git/status", get(git_status))
        .route("/api/git/info", get(git_runtime_info))
        .route("/api/git/commit", post(git_commit))
        .route("/api/checkpoints/restore", post(restore_checkpoint))
        .route("/api/trace", get(get_trace))
        .route("/api/memory", get(get_memory))
        .route("/api/memory/agents", post(save_memory_agents))
        .route("/api/memory/library", get(get_memory_library))
        .route("/api/memory/library/reindex", post(reindex_memory_library))
        .route("/api/memory/search", post(search_memory))
        .route("/api/mcp", get(get_mcp).post(post_mcp))
        .route("/api/version", get(get_version))
        .route("/api/ext", get(list_extensions))
        // 组件总线 v0：扩展静态资源（路径式路由，扩展内相对 URL 天然可用）
        // 尾斜杠那条约等于目录首页：不带它请求会掉进 SPA 兜底，把整个应用套进 iframe
        .route("/ui-ext/{name}", get(ui_ext_entry))
        .route("/ui-ext/{name}/", get(ui_ext_entry))
        .route("/ui-ext/{name}/{*path}", get(ui_ext_asset))
        .route("/cancel", post(cancel))
        .route("/api/cancel", get(cancel_status))
        .route("/api/sandbox/selftest", get(sandbox_selftest))
        .route("/api/workspace/files", get(list_workspace_files))
        .route("/api/workspace/file", get(read_workspace_file))
        .route("/api/changes", get(list_changes))
        .route("/api/shell", post(run_shell))
        .route("/api/skills", get(list_skills).post(post_skills))
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
    // index.html 不缓存：前端重建后普通刷新即可拿到新入口（带 hash 的 assets 不受影响）
    // 来源校验在最外层：非本机来源的请求在进任何 handler 之前就被挡掉
    app = app.layer(middleware::from_fn(no_cache_html));
    app = app.layer(middleware::from_fn(guard_local_only));
    app
}

/// 本机服务的来源校验（施工单 022）。
///
/// 绑定 `127.0.0.1` 只挡得住网络来的连接，挡不住 **DNS rebinding**：恶意页面把自己的域名
/// 解析到 127.0.0.1，浏览器就当成同源、不触发 CORS 预检，于是能直接读 `/api/settings`、
/// 或者 POST `/api/shell` 在本机执行命令。三道规则覆盖浏览器与命令行两类客户端：
///
/// 1. `Host` 必须是回环地址（`localhost` / `127.x.x.x` / `[::1]`）—— rebinding 时 Host 是
///    攻击者域名，这一条直接 403；
/// 2. 带 `Origin` 头（浏览器一定会带）时，Origin 必须是回环主机且**端口与 Host 一致**——
///    挡住「同一个本机、另一个端口上的恶意页面」；浏览器发来的 `Origin: null`（沙箱 iframe /
///    file://）一律拒；
/// 3. 不带 `Origin` 的请求放行 —— curl、eval 驱动器、服务自管理都不发这个头。
///
/// 开发热更（vite 5174）是代理转发：`web/vite.config.js` 里把 Origin 改写成主服务地址，
/// 规则 2 自然成立。
async fn guard_local_only(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let origin = req
        .headers()
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match check_origin(&host, origin.as_deref()) {
        Ok(()) => next.run(req).await,
        Err(msg) => (
            StatusCode::FORBIDDEN,
            format!("拒绝访问：{msg}（本服务只接受本机来源的请求）"),
        )
            .into_response(),
    }
}

/// 来源判定（纯函数，可单测）。`origin = None` 表示请求没带 Origin 头。
fn check_origin(host_header: &str, origin: Option<&str>) -> Result<(), String> {
    let (host_name, host_port) = split_host(host_header);
    if host_name.is_empty() {
        return Err("缺少 Host 头".into());
    }
    if !is_loopback_host(host_name) {
        return Err(format!("Host 不是本机地址: {host_name}"));
    }
    let Some(origin) = origin else {
        return Ok(()); // 非浏览器客户端（curl / eval / 服务自管理）
    };
    let rest = origin.split_once("://").map(|(_, r)| r).ok_or_else(|| format!("Origin 不合法: {origin}"))?;
    let (o_host, o_port) = split_host(rest);
    if !is_loopback_host(o_host) {
        return Err(format!("Origin 不是本机地址: {origin}"));
    }
    let norm = |p: Option<u16>, scheme: &str| p.unwrap_or(if scheme == "https" { 443 } else { 80 });
    let scheme = origin.split_once("://").map(|(s, _)| s).unwrap_or("http");
    if norm(o_port, scheme) != norm(host_port, "http") {
        return Err(format!("Origin 与 Host 端口不一致: {origin} vs {host_header}"));
    }
    Ok(())
}

/// 拆 `主机名` + 可选端口：`127.0.0.1:5173` / `localhost` / `[::1]:5173` 都能认。
fn split_host(h: &str) -> (&str, Option<u16>) {
    let h = h.trim();
    if let Some(rest) = h.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once(']')
    {
        return (host, tail.strip_prefix(':').and_then(|p| p.parse().ok()));
    }
    match h.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (host, port.parse().ok())
        }
        _ => (h, None),
    }
}

/// 回环主机名（大小写不敏感）：`localhost` / `127.0.0.0/8` / `::1`。
fn is_loopback_host(h: &str) -> bool {
    let h = h.trim().trim_end_matches('.').to_ascii_lowercase();
    h == "localhost" || h == "::1" || h == "0:0:0:0:0:0:0:1" || h.starts_with("127.")
}

/// 给 text/html 响应加 Cache-Control: no-cache，防止 index.html 缓存旧入口引用旧资源
async fn no_cache_html(req: Request, next: Next) -> Response {
    let resp = next.run(req).await;
    let is_html = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"));
    if !is_html {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    parts
        .headers
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Response::from_parts(parts, body)
}

/// GET /api/workspace/files —— 工作区文件相对路径列表（「@ 添加上下文」选择器数据源；有界遍历）
async fn list_workspace_files() -> Json<Value> {
    const MAX_FILES: usize = 500;
    const SKIP_DIRS: [&str; 9] = [
        ".git",
        ".agent",
        ".attachments",
        ".workbuddy",
        "node_modules",
        "target",
        "dist",
        "logs",
        "__pycache__",
    ];
    let ws = crate::tools::workspace_root();
    let mut files: Vec<String> = Vec::new();
    let mut stack: Vec<(std::path::PathBuf, String)> = vec![(ws.clone(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        if files.len() >= MAX_FILES {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if files.len() >= MAX_FILES {
                break;
            }
            let name = e.file_name().to_string_lossy().to_string();
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let p = e.path();
            if p.is_dir() {
                // 深度上限：防目录联接（junction）成环导致死循环
                if SKIP_DIRS.contains(&name.as_str()) || rel.matches('/').count() >= 12 {
                    continue;
                }
                stack.push((p, child_rel));
            } else {
                files.push(child_rel);
            }
        }
    }
    files.sort();
    let truncated = files.len() >= MAX_FILES;
    Json(json!({ "files": files, "truncated": truncated }))
}

/// GET /api/skills —— 技能列表（项目级 + 全局，含全文供编辑器回显）
async fn list_skills() -> Json<Value> {
    Json(json!({ "skills": crate::skills::list() }))
}

#[derive(Deserialize)]
pub struct SkillReq {
    /// "save" | "delete" | "read"
    pub action: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub content: Option<String>,
}

/// POST /api/skills —— 技能保存 / 删除 / 读取
async fn post_skills(Json(req): Json<SkillReq>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let err = |e: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    match req.action.as_str() {
        "save" => {
            let name = req.name.clone().unwrap_or_default();
            match crate::skills::save(&name, req.description.as_deref().unwrap_or(""), req.content.as_deref().unwrap_or(""))
            {
                Ok(dir) => Ok(Json(json!({ "ok": true, "dir": dir }))),
                Err(e) => Err(err(e)),
            }
        }
        "delete" => {
            let name = req.name.clone().unwrap_or_default();
            match crate::skills::delete(&name) {
                Ok(()) => Ok(Json(json!({ "ok": true }))),
                Err(e) => Err(err(e)),
            }
        }
        "read" => {
            let name = req.name.clone().unwrap_or_default();
            match crate::skills::read(&name) {
                Ok(content) => Ok(Json(json!({ "ok": true, "content": content }))),
                Err(e) => Err(err(e)),
            }
        }
        other => Err(err(format!("未知 action: {other}"))),
    }
}

/// GET /api/workspace/file?path=xx —— 读工作区文本文件内容（「预览」标签数据源；沿用 read_file 的越界防护与截断）
async fn read_workspace_file(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let err = |e: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    let path = params.get("path").map(|s| s.trim()).unwrap_or("");
    if path.is_empty() {
        return Err(err("缺少 path".into()));
    }
    let sandbox_cfg = (*state.sandbox_cfg).clone();
    let exec = crate::tools::ExecConfig {
        max_chars: 200_000,
        sandbox: sandbox_cfg,
        unsandboxed: false,
    };
    match crate::tools::execute(
        &exec,
        "read_file",
        &json!({ "path": path }).to_string(),
    )
    .await
    {
        Ok(content) => Ok(Json(json!({ "ok": true, "content": content }))),
        Err(e) => Err(err(e)),
    }
}

/// GET /api/changes?baseline=xx —— 本任务相对基线的改动文件清单（「产物」标签数据源）
async fn list_changes(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let err = |e: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    let baseline = params.get("baseline").map(|s| s.trim()).unwrap_or("");
    if baseline.is_empty() {
        return Err(err("缺少 baseline".into()));
    }
    let ws = crate::tools::workspace_root().to_path_buf();
    let baseline = baseline.to_string();
    match tokio::task::spawn_blocking(move || crate::recovery::changed_files(&ws, &baseline)).await {
        Ok(Ok(files)) => Ok(Json(json!({ "ok": true, "files": files }))),
        Ok(Err(e)) => Err(err(e.to_string())),
        Err(e) => Err(err(e.to_string())),
    }
}

#[derive(Deserialize)]
pub struct ShellReq {
    pub command: String,
}

/// POST /api/shell —— 终端面板：在沙箱内执行一条用户输入的命令（复用 shell 工具的沙箱与超时）
async fn run_shell(
    State(state): State<AppState>,
    Json(req): Json<ShellReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let command = req.command.trim().to_string();
    if command.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "命令为空" })),
        ));
    }
    let sandbox_cfg = (*state.sandbox_cfg).clone();
    let exec = crate::tools::ExecConfig {
        max_chars: 100_000,
        sandbox: sandbox_cfg,
        unsandboxed: false,
    };
    match crate::tools::execute(&exec, "shell", &json!({ "command": command }).to_string()).await {
        Ok(output) => Ok(Json(json!({ "ok": true, "output": output }))),
        Err(e) => Ok(Json(json!({ "ok": false, "output": e }))),
    }
}

fn json_event(payload: Value) -> Event {
    Event::default().json_data(payload).expect("SSE 事件序列化失败")
}

/// P8/P9/P8.5：从配置项 + 供应商预设组装请求参数（采样/思考/端点探测线索/工具开关）。
fn build_chat_params(
    c: &crate::config::settings::Config,
    preset: Option<&gateway::providers::ProviderPreset>,
) -> gateway::ChatParams {
    let mut p = match preset {
        Some(p) => gateway::ChatParams::from_preset(p),
        None => gateway::ChatParams::default(),
    };
    p.max_tokens = c.max_output_tokens;
    p.temperature = c.temperature;
    p.top_p = c.top_p;
    p.thinking = c.thinking.clone();
    p.base_url = Some(c.effective_base_url());
    // A3：能力标记「工具调用」闭环（施工单 012）
    p.tools_enabled = c.capabilities.tools;
    apply_thinking_style(&mut p, c.thinking_style.as_deref());
    p
}

/// P9：思考参数形态覆盖——中转/自定义渠道端点探测认不出时显式指定（设置面板控件）。
/// B4 思考形态解析顺序（施工单 012 固化）：`Config.thinking_style` 显式覆盖 >
/// 端点探测（compat.rs：官方域名→native 供应商规范 / 本地→local / 其余→openai 兜底）>
/// providers.json 预设 `thinking_format` 兜底（预设值在 compat::resolve 内作为探测输入，
/// 当前仅豆包 zai/混元 qwen 带值且与探测结果一致）。
/// 词表（none/openai/deepseek/zai/qwen/qwen-chat-template）只增不改名；
/// 新形态入词表必须附真实渠道冒烟记录。none=不发任何思考参数。
fn apply_thinking_style(p: &mut gateway::ChatParams, style: Option<&str>) {
    use gateway::compat::ThinkingFormat as TF;
    match style {
        Some("none") => p.thinking_format = Some(TF::None),
        // 旧词表别名（P8.3 设置残留）：thinking_object=GLM 式对象 → zai；
        // reasoning_effort=档位 → openai；enable_thinking（_budget）=布尔 → qwen
        Some("thinking_object") => p.thinking_format = Some(TF::Zai),
        Some("reasoning_effort") => p.thinking_format = Some(TF::Openai),
        Some("enable_thinking" | "enable_thinking_budget") => p.thinking_format = Some(TF::Qwen),
        Some(v) => p.thinking_format = TF::parse(v),
        None => {} // auto/未知：跟端点探测
    }
}

/// P8/P8.5：把配置名（或配置 id）解析成一条可用渠道。解析失败（名字不存在/地址为空）返回 None。
/// B3 角色解析顺序（施工单 012 固化）：settings.roles.<role> > config.toml [roles] > 回落主渠道；
/// 角色值按「配置名或配置 id」解析（本函数），解析失败回落主渠道并 Notice。
fn find_channel(s: &Settings, name: &str) -> Option<gateway::Channel> {
    let c = s
        .configs
        .iter()
        .find(|c| c.name == name || c.id == name)?;
    let base_url = c.effective_base_url();
    if base_url.is_empty() {
        return None;
    }
    let model = c.effective_model();
    let preset = gateway::providers::find(&c.provider_id);
    let params = build_chat_params(c, preset);
    let key = s.config_key(c).unwrap_or_default();
    Some(gateway::Channel::new(&base_url, &model, &key, params))
}

/// 检查模型是否具备指定模态的输入能力（对齐 DSH acceptsMedia 门禁规范）
fn accepts_modality(model: &str, kind: &str) -> bool {
    let caps = crate::config::settings::ModelCapabilities::infer_from_model_id(model);
    match kind {
        "audio" => caps.audio,
        "video" => caps.video,
        "image" => caps.vision,
        _ => false,
    }
}

/// A5（施工单 012）：媒体附件落在 <workspace>/.attachments/，若工作区是 git 仓库且
/// .gitignore 未含该目录，自动追加一行 `.attachments/`（只追加，绝不重写用户文件），
/// 防止用户 `git add .` 把二进制附件提交进历史。
fn ensure_attachments_ignored(ws: &std::path::Path) {
    if !ws.join(".git").exists() {
        return;
    }
    let gi = ws.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".attachments/") {
        return;
    }
    let mut add = String::new();
    // 用户文件可能不带结尾换行：直接追加会粘成一行，先补一个换行
    if !existing.is_empty() && !existing.ends_with('\n') {
        add.push('\n');
    }
    add.push_str(".attachments/\n");
    let ok = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&gi)
        .and_then(|mut f| std::io::Write::write_all(&mut f, add.as_bytes()))
        .is_ok();
    if ok {
        // try_event：这条路径会在单测里被直接调用，logger 未初始化时不该 panic
        observability::try_event(
            EventType::State,
            json!({ "kind": "attachments.gitignored", "path": gi.display().to_string() }),
        );
    }
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
                // P9：预设的思考形态覆盖（多数官方域名可探测，仅豆包/混元带值）
                "thinking_format": p.thinking_format.map(|f| f.as_str()),
                "key_optional": p.key_optional,
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
    let s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    let mut v = config_state_json(&s);
    v["env_key"] = json!(std::env::var("DEEPSEEK_API_KEY").is_ok());
    Json(v)
}

/// 把 SettingsUpdate 应用到当前激活配置（put_settings 的主体逻辑，独立成纯函数便于单测）。
/// 返回 (url_changed, provider_id, effective_model)。
fn apply_settings_update(s: &mut Settings, req: &SettingsUpdate) -> (bool, String, String) {
    s.ensure_active_config();

    if let Some(v) = &req.name {
        s.set_active_name(v);
    }

    let mut result = (false, "deepseek".to_string(), "deepseek-chat".to_string());
    if let Some(c) = s.active_mut() {
        let url_before = crate::config::settings::normalize_base(&c.effective_base_url());
        if let Some(v) = &req.provider_id
            && !v.trim().is_empty() {
                c.provider_id = v.trim().to_string();
            }
        if let Some(v) = &req.base_url {
            c.base_url = v.trim().to_string();
        }
        if let Some(v) = &req.model {
            let v = v.trim().to_string();
            if c.model != v {
                // 能力标记归用户手动勾选，切换模型不改写（原「按模型名推导初值」已移除）
                c.model = v;
            }
        }
        if let Some(v) = &req.models {
            let mut list: Vec<String> =
                v.iter().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect();
            list.sort();
            list.dedup();
            c.models = list;
        }
        if let Some(v) = req.context_length {
            c.context_length = if v == 0 { None } else { Some(v) };
        }
        if let Some(v) = req.max_output_tokens {
            c.max_output_tokens = if v == 0 { None } else { Some(v) };
        }
        // P8 采样参数：temperature 0.0 是合法值（确定性输出），清除用负数
        if let Some(v) = req.temperature {
            c.temperature = if v < 0.0 { None } else { Some(v) };
        }
        if let Some(v) = req.top_p {
            c.top_p = if v < 0.0 { None } else { Some(v) };
        }
        if let Some(v) = req.thinking.as_deref() {
            let v = v.trim().to_lowercase();
            c.thinking = match v.as_str() {
                "on" | "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => {
                    Some(v.clone())
                }
                _ => None,
            };
        }
        if let Some(v) = req.thinking_style.as_deref() {
            let v = v.trim().to_lowercase();
            c.thinking_style = if v.is_empty() || v == "auto" { None } else { Some(v.to_string()) };
        }
        if let Some(caps) = &req.capabilities {
            c.capabilities = caps.clone();
            // 能力标记归用户所有：手动勾选即时保存
            c.capabilities_user_set = true;
        }

        let url_after = crate::config::settings::normalize_base(&c.effective_base_url());
        let changed = url_before != url_after;
        if changed {
            c.models.clear();
            c.model.clear();
        }
        result = (changed, c.provider_id.clone(), c.effective_model());
    }
    result
}

/// 保存设置。api_key 非空则 DPAPI 加密持久化到当前激活配置；clear_key=true 则清除。
async fn put_settings(
    State(state): State<AppState>,
    Json(req): Json<SettingsUpdate>,
) -> (StatusCode, Json<Value>) {
    let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner()).clone();
    let (url_changed, pid, eff_model) = apply_settings_update(&mut s, &req);

    if url_changed {
        observability::event(
            EventType::State,
            json!({ "kind": "settings.channel_changed", "base_url": s.effective_base_url() }),
        );
    }
    if req.clear_key == Some(true) {
        if let Err(e) = s.clear_key() {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
        }
    } else if let Some(k) = &req.api_key
        && !k.trim().is_empty()
            && let Err(e) = s.set_key(k.trim()) {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
            }

    if let Some(roles) = req.roles {
        s.roles = Some(roles);
    }

    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    crate::plugin::mcp::sync_configs(&s.mcp_servers); // P11：MCP 配置变更即同步管理器（幂等）
    observability::event(
        EventType::State,
        json!({ "kind": "settings.updated", "provider_id": pid, "model": eff_model }),
    );
    (StatusCode::OK, Json(config_state_json(&s)))
}

/// 拉取渠道模型列表。请求里没给 base_url/key 就用已保存设置。
async fn list_models(
    State(state): State<AppState>,
    Json(req): Json<ModelsRequest>,
) -> (StatusCode, Json<Value>) {
    let s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    let base_url = if req.base_url.trim().is_empty() { s.effective_base_url() } else { req.base_url.trim().to_string() };
    // B2 key 回落链（施工单 012 固化）：请求显式 key > 激活配置 DPAPI key > DEEPSEEK_API_KEY 环境变量。
    // key 永不出日志、永不出 JSONL。
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
    let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner()).clone();
    let err =
        |m: &str| (StatusCode::BAD_REQUEST, Json(json!({ "error": m })));
    match req.action.as_str() {
        "new" => {
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
    let pid = s.provider_id();
    let model = s.effective_model();
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    crate::plugin::mcp::sync_configs(&s.mcp_servers); // P11：MCP 配置变更即同步管理器（幂等）
    observability::event(
        EventType::State,
        json!({ "kind": "config.changed", "action": req.action, "provider_id": pid, "model": model }),
    );
    (StatusCode::OK, Json(config_state_json(&s)))
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceRequest {
    pub path: String,
}

/// GET /api/workspace —— 当前工作区 + 最近列表（P8.2）。
async fn get_workspace(State(state): State<AppState>) -> Json<Value> {
    let s = state.settings.read().unwrap_or_else(|e| e.into_inner());
    Json(json!({
        "current": crate::tools::workspace_root().display().to_string(),
        "recent": s.recent_workspaces,
    }))
}

/// 规范化工作区路径：Windows canonicalize 会加 `\\?\` 扩展前缀，显示与持久化都难看，剥掉。
fn canon_workspace(p: std::path::PathBuf) -> std::path::PathBuf {
    match p.canonicalize() {
        Ok(c) => {
            let s = c.display().to_string();
            match s.strip_prefix(r"\\?\") {
                Some(stripped) => std::path::PathBuf::from(stripped),
                None => c,
            }
        }
        Err(_) => p,
    }
}

/// 路径等价比较（Windows 大小写不敏感、斜杠方向与结尾分隔符归一）。
fn same_workspace(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// 切换工作区（/api/workspace 与「打开会话」共用）：改全局根 → 记忆层热重开
/// → settings 持久化 + 最近列表 → MCP 同步 → 事件。调用方负责校验目录存在。
fn apply_workspace(state: &AppState, canon: &std::path::Path) -> Result<(), String> {
    let old = crate::tools::set_workspace_root(canon);
    // 记忆层重开：新项目的双层存储/index 登记/AGENTS.md 模板
    let new_mem = Arc::new(crate::memory::Memory::open(canon, &state.mem_cfg));
    new_mem.ensure_agents_template(canon);
    *state.memory.write().unwrap_or_else(|e| e.into_inner()) = new_mem;
    // 持久化 + 最近列表（去重、新→旧、最多 8）
    let cur = canon.display().to_string();
    let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner()).clone();
    s.workspace = Some(cur.clone());
    s.recent_workspaces.retain(|w| *w != cur);
    s.recent_workspaces.insert(0, cur.clone());
    s.recent_workspaces.truncate(8);
    s.save().map_err(|e| e.to_string())?;
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    crate::plugin::mcp::sync_configs(&s.mcp_servers); // P11：MCP 配置变更即同步管理器（幂等）
    observability::event(
        EventType::State,
        json!({ "kind": "workspace.switched", "from": old.display().to_string(), "to": cur }),
    );
    Ok(())
}

/// POST /api/workspace —— 切换工作区：校验目录存在 → 改全局根 → 记忆层热重开
/// （新项目 .agent/index/AGENTS.md 模板）→ settings 持久化 + 最近列表。
/// 进行中的任务不拦（前端流式期间禁用按钮）；切换只影响之后的工具调用。
async fn set_workspace(
    State(state): State<AppState>,
    Json(req): Json<WorkspaceRequest>,
) -> (StatusCode, Json<Value>) {
    let path = req.path.trim();
    if path.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "路径为空" })));
    }
    let p = std::path::PathBuf::from(path);
    if !p.is_dir() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": format!("目录不存在: {path}") })));
    }
    let canon = canon_workspace(p);
    if let Err(e) = apply_workspace(&state, &canon) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e })));
    }
    let recent = state.settings.read().unwrap_or_else(|e| e.into_inner()).recent_workspaces.clone();
    (StatusCode::OK, Json(json!({ "ok": true, "current": canon.display().to_string(), "recent": recent })))
}

/// 配置快照（含各配置 key 状态与角色路由），供前端全量刷新。
fn config_state_json(s: &Settings) -> Value {
    let active = s.active_or_default();
    let preset = gateway::providers::find(&active.provider_id);
    let key = s.get_key();
    json!({
        "ok": true,
        "configs": s.configs.iter().map(|c| {
            let k = s.config_key(c);
            json!({
                "id": c.id,
                "name": c.name.clone(),
                "provider_id": c.provider_id,
                "base_url": c.base_url.clone(),
                "model": c.model.clone(),
                "models": c.models.clone(),
                "has_key": k.is_some(),
                "key_hint": key_hint_of(k),
                "context_length": c.context_length,
                "max_output_tokens": c.max_output_tokens,
                "temperature": c.temperature,
                "top_p": c.top_p,
                "thinking": c.thinking,
                "thinking_style": c.thinking_style,
                "capabilities": c.capabilities,
                "capabilities_user_set": c.capabilities_user_set,
            })
        }).collect::<Vec<Value>>(),
        "active_config_id": s.active_config_id,
        "name": active.name.clone(),
        "provider_id": active.provider_id,
        "base_url": active.base_url,
        "effective_base_url": active.effective_base_url(),
        "model": active.model,
        "models": active.models,
        "context_length": active.context_length,
        "max_output_tokens": active.max_output_tokens,
        "temperature": active.temperature,
        "top_p": active.top_p,
        "thinking": active.thinking,
        "capabilities": active.capabilities,
        "capabilities_user_set": active.capabilities_user_set,
        "has_key": key.is_some(),
        "key_hint": key_hint_of(key),
        // P8.2 工作区 + P9/P8.5 思考参数形态覆盖（前端按它渲染档位菜单与全协议规范）
        "workspace": crate::tools::workspace_root().display().to_string(),
        "recent_workspaces": s.recent_workspaces,
        "workspace_aliases": s.workspace_aliases,
        "thinking_style_override": active.thinking_style,
        "thinking_style": effective_thinking_style(active, preset),
        "roles": s.roles,
    })
}

/// 生效的思考参数形态（P9/P8.5，8大规范词表）：渠道覆盖 > 端点探测。
/// 值：none | openai | deepseek | zai | qwen | claude | moonshot | qwen-chat-template | auto
fn effective_thinking_style(
    c: &crate::config::settings::Config,
    preset: Option<&gateway::providers::ProviderPreset>,
) -> &'static str {
    use gateway::compat::ThinkingFormat as TF;
    match c.thinking_style.as_deref() {
        Some("none") => "none",
        // P8.3 旧词表别名 → 新词表
        Some("thinking_object") => TF::Zai.as_str(),
        Some("reasoning_effort") => TF::Openai.as_str(),
        Some("enable_thinking" | "enable_thinking_budget") => TF::Qwen.as_str(),
        Some(v) => TF::parse(v).map(|f| f.as_str()).unwrap_or("auto"),
        // auto：端点探测（预设 thinking_format 是探测认不出时的兜底）
        _ => gateway::compat::resolve(
            Some(c.provider_id.as_str()),
            Some(&c.effective_base_url()),
            &c.model,
            preset.and_then(|p| p.thinking_format),
        )
        .thinking_format
        .as_str(),
    }
}

/// 流式对话：base_url/model/key 全部来自已保存设置（请求级 api_key 可覆盖）。
async fn chat(State(state): State<AppState>, Json(req): Json<ChatRequest>) -> Response {
    let message = req.message.trim().to_string();
    let req_model = req.model.trim().to_string();
    let req_base_url = req.base_url.trim().to_string();
    let req_api_key = req.api_key.trim().to_string();
    let attachments = req.attachments.unwrap_or_default();
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
    let cancel_registry = state.cancel.clone();
    let steer_queue = state.steer.clone();
    let s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
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
    // 系统约定每轮实时读盘（面板保存 prompts/system.md 后无需重启即生效）
    let prompt = read_system_prompt(&state);
    // P7 AGENTS.md 双层注入（全局在前、项目在后）；P8.2 记忆层随工作区热重开
    let agents_md = state.memory.read().unwrap_or_else(|e| e.into_inner()).agents_md();
    let approvals = state.approvals.clone();
    let session = state.session.clone();
    let mem = state.memory.read().unwrap_or_else(|e| e.into_inner()).clone();
    let rec_cfg = (*state.recovery_cfg).clone();
    // P6.1 多会话：请求带了合法 id 就用，没带就新建（前端从首个 SSE 事件拿 id）
    let session_id = {
        let sid = req.session_id.trim().to_string();
        let valid = !sid.is_empty()
            && sid.len() <= 64
            && sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if valid { sid } else { session.new_id() }
    };
    // 025：同一会话已在跑一轮 → **不拒收**：文本消息进追加队列，运行中的那一轮在下一个
    // 决策点就会看到（原轮不打断、不另起一轮，会话文件保持单写者）。判定与登记仍在同一把
    // 锁内（try_begin），并发请求不会双双通过检查；登记好的标志随任务移交 RAII 守卫
    // CancelRun，任务结束（含提前 return / panic）自动注销。
    // 例外：带附件的追加暂不支持（媒体流水线在轮首一次性构建），明确拒绝而不是悄悄丢附件。
    // M2：这里的 history 在「接管」分支里会被换成会话快照 + 队列消息，故为可变绑定。
    let mut history = history;
    let mut run_flag = state.cancel.try_begin(&session_id);
    if run_flag.is_none() {
        if !attachments.is_empty() {
            observability::event(
                EventType::State,
                json!({ "kind": "chat.rejected", "reason": "session_busy_attachments", "session": session_id }),
            );
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!("会话 {session_id} 正在运行中：追加消息暂不支持带附件，等这一轮跑完再发")
                })),
            )
                .into_response();
        }
        let text = if !message.is_empty() {
            message.clone()
        } else {
            history
                .iter()
                .rev()
                .find(|m| m["role"] == "user")
                .and_then(|m| m["content"].as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        if text.is_empty() {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": "消息为空" }))).into_response();
        }
        // 027：缺省 = 排队（这一轮跑完自动接着发）；steer=true = 立即（引导当前这一轮）
        let immediate = req.steer;
        let pending = state.steer.push(&session_id, text, immediate);
        observability::event(
            EventType::State,
            json!({
                "kind": "chat.steered",
                "session": session_id,
                "mode": if immediate { "immediate" } else { "queue" },
                "pending": pending,
            }),
        );
        let (stx, srx) = mpsc::unbounded_channel::<Result<Event, Infallible>>();
        let notice = if immediate {
            "已并入正在运行的一轮：模型下一个决策点就会看到（引导当前回答，不打断）".to_string()
        } else {
            "已排队：当前这一轮结束后自动发送（想马上引导当前回答，点这条消息上的「立即发送」）"
                .to_string()
        };
        let flag = if immediate { "steered" } else { "queued" };
        // M2 收口：入队的这一刻，上一轮可能刚好收尾（它的收尾 drain 已经跑过）——
        // 那样这条就没人取了。再抢一次占位：抢到 = 确实已结束 → 本次请求直接把队列
        // 里的消息跑掉（不让消息悬在队列里等下一次活动）；抢不到 = 真的还在跑，照常回执。
        if let Some(f) = state.cancel.try_begin(&session_id) {
            let extra = state.steer.drain_all(&session_id);
            let snap = session.get(&session_id).map(|s| s.messages).unwrap_or_default();
            let mut merged = if snap.is_empty() { history.clone() } else { snap };
            for t in &extra {
                merged.push(json!({ "role": "user", "content": t }));
            }
            observability::event(
                EventType::State,
                json!({ "kind": "chat.takeover", "session": session_id, "count": extra.len() }),
            );
            history = merged;
            run_flag = Some(f);
        } else {
            let _ = stx.send(Ok(json_event(json!({ "session": session_id.clone() }))));
            let _ = stx.send(Ok(json_event(json!({ flag: true, "notice": notice }))));
            let _ = stx.send(Ok(json_event(json!({ "done": true }))));
            return Sse::new(UnboundedReceiverStream::new(srx)).into_response();
        }
    }
    let run_flag = run_flag.expect("占位或接管必得其一");
    // P7 滚动摘要：从会话文件带出旧摘要继续压
    let prior_summary = session.get(&session_id).and_then(|f| f.summary);
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
    // M1：档位写进**本会话**（不再写全局默认，否则并发多会话会互相覆盖——
    // 后到者生效可能让另一个会话以比预期更宽松的档位执行）
    if let Ok(mut m) = state.session_approval.write() {
        m.insert(session_id.clone(), mode.clone());
    }

    tokio::spawn(async move {
        let send = |payload: Value| {
            let _ = tx.send(Ok(json_event(payload)));
        };
        // D2：本轮取消标志（会话级）——新会话不再复位别人的标志，停止只停本会话。
        // N1：标志已在进入 handler 时占位登记（同会话并发在门口就 409）。
        // 守卫在任务结束（含提前 return / panic）时注销登记，不留「永远在跑」的僵尸会话。
        // M2：run/cancel 可重绑——收尾时会先注销占位、再取队列续跑一轮
        let mut run = CancelRun::from_flag(cancel_registry.clone(), &session_id, run_flag);
        let mut cancel = run.flag.clone();
        // P7 摘要槽：Summary 事件写入，Snapshot 落盘时随会话持久化
        let summary_slot: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(prior_summary.clone()));
        // 025：本任务最新一次会话快照（消息全量，不含 system）——迟到的追加消息
        // 要接着再跑一轮时，用它当上下文，而不是重新问客户端要历史。
        let last_snapshot: Arc<std::sync::Mutex<Vec<Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        if history.is_empty() {
            send(json!({ "error": "消息为空" }));
            return;
        }
        // P6.1：先把会话 id 告诉前端（新建时它还不知道）
        send(json!({ "session": session_id }));

        // 新会话开场：注入「项目现状」摘要（上一条同项目会话 + 工作区未提交改动），
        // 让同一工作区里新开的对话直接接手项目，而不是从零问起。
        let project_brief = {
            let is_new = session.get(&session_id).map(|f| f.messages.is_empty()).unwrap_or(true);
            if is_new {
                let ws = crate::tools::workspace_root().to_path_buf();
                let stats = tokio::task::spawn_blocking(move || {
                    let head = crate::recovery::head_id(&ws).ok().flatten()?;
                    crate::recovery::diff_stat(&ws, &head).ok()
                })
                .await
                .ok()
                .flatten();
                session.project_brief(&session_id, stats).unwrap_or_default()
            } else {
                String::new()
            }
        };

        // P8.5 多模态媒体理解流水线：感知媒体附件与视觉/视频/音频解析能力
        let mut history = history;
        // 025：续跑一轮时摘要可能被上一轮更新（见收尾处的重新赋值）
        let mut prior_summary = prior_summary;
        if !attachments.is_empty() {
            let roles = s.roles.as_ref();
            let mut native_attachments: Vec<&MediaAttachment> = Vec::new();
            let mut forward_attachments: Vec<&MediaAttachment> = Vec::new();

            for att in &attachments {
                let is_vid = att.is_video();
                let is_aud = att.is_audio();
                let is_img = att.is_image();

                let supports_natively = (is_img && active_cfg.capabilities.vision)
                    || (is_vid && active_cfg.capabilities.video)
                    || (is_aud && active_cfg.capabilities.audio);

                if supports_natively {
                    native_attachments.push(att);
                } else {
                    forward_attachments.push(att);
                }
            }

            // 1. 对于主模型原生支持的媒体类型，挂载原生多模态结构
            if !native_attachments.is_empty()
                && let Some(last_msg) = history.iter_mut().rev().find(|m| m["role"] == "user") {
                    let mut parts = Vec::new();
                    if let Some(txt) = last_msg["content"].as_str() {
                        parts.push(json!({ "type": "text", "text": txt }));
                    }
                    for att in native_attachments {
                        if att.is_audio() {
                            parts.push(json!({
                                "type": "input_audio",
                                "input_audio": {
                                    "data": att.raw_base64(),
                                    "format": att.audio_format()
                                }
                            }));
                        } else {
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": { "url": att.data_url() }
                            }));
                        }
                    }
                    last_msg["content"] = json!(parts);
                }

            // 2. 对于主模型不支持的媒体类型，走专职转接与确定性文本投影（对齐 DSH media-understanding 规范）
            if !forward_attachments.is_empty() {
                let mut projections = Vec::new();

                for att in forward_attachments {
                    let is_vid = att.is_video();
                    let is_aud = att.is_audio();
                    let kind = if is_vid { "video" } else if is_aud { "audio" } else { "image" };
                    let kind_cn = if is_vid { "视频" } else if is_aud { "音频" } else { "图片" };
                    let success_label = if is_vid { "Video understanding" } else if is_aud { "Audio understanding" } else { "Image understanding" };

                    // 持久化附件到工作区 .attachments/ 便于主模型与工具查阅
                    let ws = crate::tools::workspace_root();
                    let att_dir = ws.join(".attachments");
                    let _ = std::fs::create_dir_all(&att_dir);
                    // A5：附件目录自动进工作区 git 排除（幂等，只追加）
                    ensure_attachments_ignored(&ws);
                    let safe_name = std::path::Path::new(&att.name)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("attachment");
                    let target_file = att_dir.join(safe_name);
                    use base64::Engine;
                    let clean = att.raw_base64().trim().replace(['\r', '\n', ' '], "");
                    let unpadded = clean.trim_end_matches('=');
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(&clean)
                        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(unpadded))
                        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(unpadded));
                    if let Ok(bytes) = decoded {
                        let _ = std::fs::write(&target_file, bytes);
                    }

                    // 1) 专属路由解析（对齐 DSH）：严格隔离，仅使用用户在 roles 中针对该模态显式配置的渠道，严禁自动跨渠道降级
                    let role_target = if is_vid {
                        roles.and_then(|r| r.video.as_deref())
                    } else if is_aud {
                        roles.and_then(|r| r.audio.as_deref())
                    } else {
                        roles.and_then(|r| r.vision.as_deref())
                    };

                    let mut candidates: Vec<gateway::Channel> = Vec::new();
                    if let Some(n) = role_target.filter(|n| !n.trim().is_empty())
                        && let Some(ch) = find_channel(&s, n) {
                            candidates.push(ch);
                        }

                    // 2) 若该模态未配置路由或无可用候选：对齐 DSH 替换为确定性占位文本（deterministic placeholder），绝不发起注定失败的请求
                    if candidates.is_empty() {
                        let placeholder = format!(
                            "[{kind} omitted because no {kind}-understanding model is configured; 附件「{}」已跳过并保存至工作区 .attachments/{}。当前未在“设置 → 角色分工”中配置{}理解专家 ({})，请在设置中指定具备{}能力的多模态模型（如 Gemini、Whisper、Qwen-Audio 等），或直接粘贴文字描述]",
                            att.name,
                            safe_name,
                            kind_cn,
                            if is_aud { "Audio" } else if is_vid { "Video" } else { "Vision" },
                            kind_cn
                        );
                        projections.push(placeholder);
                        continue;
                    }

                    // 3) 能力预检：以专家配置里用户勾选的能力标记为准（勾了「图片解析」就放行，
                    //    不再看模型名里有没有 vision 等关键字）；找不到对应配置时才退回按名推导
                    let first_ch = &candidates[0];
                    let first_provider = first_ch.params.provider_id.as_deref().unwrap_or("custom");
                    let first_model = &first_ch.model;

                    let gate_ok = match role_target
                        .and_then(|id| s.configs.iter().find(|c| c.id == id || c.name == id))
                    {
                        Some(expert) => match kind {
                            "audio" => expert.capabilities.audio,
                            "video" => expert.capabilities.video,
                            "image" => expert.capabilities.vision,
                            _ => false,
                        },
                        None => accepts_modality(first_model, kind),
                    };
                    if !gate_ok {
                        let text = format!(
                            "[{} not run: 专家配置「{}/{}」未勾选{}解析能力。该{}未展示给主模型。请在「设置 → 模型配置」选中该专家配置并勾选「{}解析」（或在「角色分工」把{}理解专家换成具备该能力的模型）]",
                            success_label, first_provider, first_model, kind_cn, kind_cn, kind_cn, kind_cn
                        );
                        projections.push(text);
                        continue;
                    }

                    let candidate_prompts: Vec<Value> = if is_aud {
                        vec![
                            // 格式 1（标准 OpenAI input_audio：媒体块在前，提示词在后）
                            json!([
                                {
                                    "role": "user",
                                    "content": [
                                        {
                                            "type": "input_audio",
                                            "input_audio": {
                                                "data": att.raw_base64(),
                                                "format": att.audio_format()
                                            }
                                        },
                                        {
                                            "type": "text",
                                            "text": "Transcribe and describe this audio in detail. Cover spoken content and notable non-speech sounds in order. 请仔细倾听并详细转录该音频内容：逐字转录全部语音对话内容（ASR），提取说话人语调与关键要点，并记录环境背景声音。"
                                        }
                                    ]
                                }
                            ]),
                            // 格式 2（备选 audio_url）
                            json!([
                                {
                                    "role": "user",
                                    "content": [
                                        {
                                            "type": "audio_url",
                                            "audio_url": { "url": att.data_url() }
                                        },
                                        {
                                            "type": "text",
                                            "text": "Transcribe and describe this audio in detail. Cover spoken content and notable non-speech sounds in order."
                                        }
                                    ]
                                }
                            ]),
                        ]
                    } else if is_vid {
                        vec![
                            json!([
                                {
                                    "role": "user",
                                    "content": [
                                        {
                                            "type": "video_url",
                                            "video_url": { "url": att.data_url() }
                                        },
                                        {
                                            "type": "text",
                                            "text": "Describe this video in detail: what happens across it, including visual content and any speech or captions, in chronological order. 请按时间顺序详细描述该视频：视频中发生了什么，包括画面视觉内容、任何语音旁白或字幕。"
                                        }
                                    ]
                                }
                            ]),
                            json!([
                                {
                                    "role": "user",
                                    "content": [
                                        {
                                            "type": "image_url",
                                            "image_url": { "url": att.data_url() }
                                        },
                                        {
                                            "type": "text",
                                            "text": "Describe this video in detail: what happens across it, including visual content and any speech or captions, in chronological order."
                                        }
                                    ]
                                }
                            ]),
                        ]
                    } else {
                        vec![
                            json!([
                                {
                                    "role": "user",
                                    "content": [
                                        {
                                            "type": "image_url",
                                            "image_url": { "url": att.data_url() }
                                        },
                                        {
                                            "type": "text",
                                            "text": "Describe this image in detail. Cover every element that would matter to someone who cannot see it, in the order a reader would need. 请详细观察并深度解析该图片内容：涵盖所有对看不见该图片的人至关重要的元素，逐字提取画面中的所有可见文本、代码片段、图表与数值数据。"
                                        }
                                    ]
                                }
                            ])
                        ]
                    };

                    let mut desc_result = None;
                    let mut last_route = (first_provider.to_string(), first_model.clone());
                    let mut last_error_msg = String::new();

                    for vch in &candidates {
                        let provider = vch.params.provider_id.as_deref().unwrap_or("custom");
                        last_route = (provider.to_string(), vch.model.clone());

                        let mut params = vch.params.clone();
                        // DSH 规范：上限 1500 tokens，清空思考参数
                        params.max_tokens = Some(params.max_tokens.unwrap_or(1500).min(1500));
                        params.thinking = None;

                        let mut call_err = None;
                        for prompt_variant in &candidate_prompts {
                            match gateway.complete(&vch.base_url, &vch.model, &vch.api_key, prompt_variant, &params).await {
                                Ok(text) if !text.trim().is_empty() => {
                                    let is_refusal = text.contains("没有能力听音频")
                                        || text.contains("无法听音频")
                                        || text.contains("无法识别音频")
                                        || text.contains("无法处理音频")
                                        || text.contains("无法解析音频")
                                        || text.contains("我是纯文本")
                                        || text.contains("不支持音频")
                                        || text.contains("未附带音频")
                                        || text.contains("没看到音频")
                                        || text.contains("don't have the ability to listen")
                                        || text.contains("cannot process audio")
                                        || text.contains("text-based AI assistant")
                                        || text.contains("no audio file was attached")
                                        || text.contains("don't see any audio file")
                                        || text.contains("looks like no audio");
                                    if is_refusal {
                                        call_err = Some(format!("上游网关或模型声明未收到/无法解析该音频: {}", text.trim()));
                                    } else {
                                        desc_result = Some(text.trim().to_string());
                                        break;
                                    }
                                }
                                Ok(_) => {
                                    call_err = Some("模型返回空内容".to_string());
                                }
                                Err(e) => {
                                    observability::event(
                                        EventType::Error,
                                        json!({ "kind": "media.error", "modality": kind, "model": vch.model, "url": vch.base_url, "error": format!("{e:#}") }),
                                    );
                                    call_err = Some(format!("{e}"));
                                }
                            }
                        }

                        if desc_result.is_some() {
                            break;
                        }
                        if let Some(err) = call_err {
                            last_error_msg = err;
                        }
                    }

                    // 4) 投影格式化（DSH 规范）
                    let substituted_text = if let Some(text) = desc_result {
                        format!("[{} ({}/{}):\n{}]", success_label, last_route.0, last_route.1, text)
                    } else {
                        format!("[{} failed ({}/{}): {}]", success_label, last_route.0, last_route.1, last_error_msg)
                    };
                    projections.push(substituted_text);
                }

                // 5) 历史投影（projectMessages）：直接替换用户消息内容，媒体字节不传给会话模型
                if !projections.is_empty()
                    && let Some(last_msg) = history.iter_mut().rev().find(|m| m["role"] == "user") {
                        let orig = match &last_msg["content"] {
                            Value::String(s) => s.clone(),
                            Value::Array(arr) => {
                                arr.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")
                            }
                            _ => "".to_string(),
                        };
                        let combined = if orig.trim().is_empty() {
                            projections.join("\n\n")
                        } else {
                            format!("{}\n\n{}", projections.join("\n\n"), orig.trim())
                        };
                        last_msg["content"] = json!(combined);
                    }
            }
        }
        // 请求级覆盖优先：页面当前选择即时生效，不等保存
        let base_url = if !req_base_url.is_empty() { req_base_url } else { active_cfg.effective_base_url() };
        let model = if !req_model.is_empty() { req_model } else { active_cfg.effective_model() };
        // B2 key 回落链（施工单 012 固化）：请求显式 key > 激活配置 DPAPI key > DEEPSEEK_API_KEY 环境变量。
        let api_key = if !req_api_key.is_empty() {
            req_api_key
        } else {
            s.config_key(&active_cfg).or_else(|| std::env::var("DEEPSEEK_API_KEY").ok()).unwrap_or_default()
        };
        // P8/P8.5：主渠道 = 激活配置 + 供应商预设 → ChatParams
        let mut params = build_chat_params(&active_cfg, gateway::providers::find(&active_cfg.provider_id));
        params.base_url = Some(base_url.clone());
        let primary = gateway::Channel::new(&base_url, &model, &api_key, params);
        // P8/P8.5 角色映射：优先使用 settings.roles，未设回落 config.toml [roles]
        // （B3 角色解析顺序，施工单 012 固化：settings.roles > config.toml [roles] > 回落主渠道；
        // 角色值按「配置名或配置 id」解析，解析失败回落主渠道并 Notice）
        let roles = s.roles.as_ref();
        let summary_name = roles.and_then(|r| r.summary.as_deref())
            .or_else(|| state.roles_cfg.summary.as_deref());
        let title_name = roles.and_then(|r| r.title.as_deref())
            .or_else(|| state.roles_cfg.title.as_deref());
        let summary = summary_name.and_then(|n| find_channel(&s, n));
        let title = title_name.and_then(|n| find_channel(&s, n));
        if let Some(n) = summary_name
            && summary.is_none() {
                send(json!({ "notice": format!("角色「会话摘要」配置的渠道「{n}」解析失败（配置名不存在或地址为空），已回落主渠道代跑") }));
            }
        if let Some(n) = title_name
            && title.is_none() {
                send(json!({ "notice": format!("角色「标题生成」配置的渠道「{n}」解析失败（配置名不存在或地址为空），已回落主渠道代跑") }));
            }
        // P8/P8.5 降级链：优先使用 settings.roles.fallbacks，未设回落 [gateway].fallback_configs
        let fallback_names: Vec<String> = roles
            .map(|r| r.fallbacks.clone())
            .filter(|f| !f.is_empty())
            .unwrap_or_else(|| state.gateway_cfg.fallback_configs.clone());
        let fallbacks: Vec<gateway::Channel> = fallback_names
            .iter()
            .filter_map(|n| find_channel(&s, n))
            .collect();
        loop {
            let outcome = agent::run_task(
                &gateway,
                // 续跑一轮时这些值复用（Channel 是克隆廉价的连接参数快照）
                &agent::LoopChannels {
                    primary: primary.clone(),
                    summary: summary.clone(),
                    title: title.clone(),
                    fallbacks: fallbacks.clone(),
                },
                // 提示词保持最小：prompts/system.md（"用中文简洁回答"）+ AGENTS.md 仓库事实 + 技能索引（无技能时不占上下文）。
                // 不注入模型名/身份/角色——用户明确要求，模型自造身份属可接受代价（坑 13）
                &format!("{prompt}{agents_md}{project_brief}{}", crate::skills::index_prompt()),
                &history,
                &cancel,
                &cfg,
                &sandbox_cfg,
                &ctx_cfg,
                &rec_cfg,
                &mem,
                prior_summary.clone(),
                req.plan_mode,
                &ChatGate {
                    mode: state.approval_mode.clone(),
                    session_modes: state.session_approval.clone(),
                    allow_unsandboxed,
                    unsandboxed_approval: cfg.unsandboxed_approval,
                    pending: approvals.clone(),
                    // P9 A3：会话内同类允许的作用域与记忆存储
                    session_id: session_id.clone(),
                    meta: state.approval_meta.clone(),
                    approved: state.approved_scopes.clone(),
                plan_mode: req.plan_mode,
                tx: tx.clone(),
                cancel: cancel.clone(),
                steer: steer_queue.clone(),
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
                    AgentEvent::ApiUsage { prompt_tokens, cached_tokens } => {
                        send(json!({ "api_usage": { "prompt_tokens": prompt_tokens, "cached_tokens": cached_tokens } }))
                    }
                    // P6 会话落盘：核心循环每关键点推全量消息（不含 system），写盘即续命
                    AgentEvent::Snapshot { messages } => {
                        *last_snapshot.lock().unwrap_or_else(|e| e.into_inner()) = messages.clone();
                        let cur_summary = summary_slot.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        match session.save(&session_id, &messages, cur_summary.as_deref()) {
                            Ok(()) => observability::event(
                                EventType::State,
                                json!({ "kind": "session.saved", "session": session_id, "messages": messages.len() }),
                            ),
                            Err(e) => observability::event(
                                EventType::Error,
                                json!({ "message": format!("会话落盘失败: {e}") }),
                            ),
                        }
                    }
                    // P7 滚动摘要：存槽里，下一次 Snapshot 落盘时随会话一起写 summary 字段
                    AgentEvent::Summary(s) => {
                        *summary_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
                        send(json!({ "summary": true }))
                    }
                    // P6.1 改动统计条：撤销按钮恢复到任务起点基线
                    AgentEvent::Checkpoint { id, files_changed, additions, deletions } => {
                        send(json!({ "checkpoint": { "id": id, "files": files_changed, "additions": additions, "deletions": deletions } }))
                    }
                    // P8 TITLE 角色：标题已生成 → 改会话文件并转前端
                    AgentEvent::Title(t) => {
                        match session.rename(&session_id, &t) {
                            Ok(()) => send(json!({ "title": t })),
                            Err(e) => observability::event(
                                EventType::Error,
                                json!({ "message": format!("会话改名失败: {e}") }),
                            ),
                        }
                    }
                    // P9 B3：Plan 档结构化计划 → 前端计划卡（SSE 增字段；JSONL 由 agent 层记 plan.proposed）
                    AgentEvent::PlanProposal { goal, steps, files } => {
                        send(json!({ "plan_proposal": { "goal": goal, "steps": steps, "files": files } }))
                    }
                    AgentEvent::StepReset { content, reasoning } => {
                        send(json!({ "reset": { "content": content, "reasoning": reasoning } }))
                    }
                    // 025：运行中追加的用户消息已并入本轮 → 前端补用户气泡
                    AgentEvent::Steer { messages } => {
                        send(json!({ "steer": { "messages": messages } }))
                    }
                    AgentEvent::Notice(n) => send(json!({ "notice": n }))
                },
            )
            .await;
        match outcome {
            Ok(LoopOutcome::Done) => {}
            Ok(LoopOutcome::Cancelled) => {
                send(json!({ "cancelled": true }));
                // 用户点了停止：排队的消息一并作废（「停」= 这一轮和还没轮上的话都别跑了）。
                // 先注销占位再清队列（M2 同一顺序）：此后到达的消息会走正常路径自己跑，不会悬空。
                drop(run);
                steer_queue.drain_all(&session_id);
                break;
            }
            Err(e) => {
                // 出错也继续走下面的收尾：已排队的追加消息是用户发出去的话，不能因上一轮
                // 失败被悄悄吞掉（若下一轮同样失败，会再报一次错，语义仍然清楚）
                observability::event(EventType::Error, json!({ "message": e.to_string() }));
                send(json!({ "error": e.to_string() }));
            }
        }
        // M2 收尾顺序：**先注销占位、再取队列**。顺序反过来的话，「入队后没人取」的窗口
        // 就会出现（消息在收尾 drain 之后到达 → 队列里没人消费）。先注销后，这一瞬之后
        // 到达的消息会走正常路径自己起一轮（而不是排队等人取）。
        drop(run);
        // 025/027：一轮跑完后还剩的排队消息（缺省模式，或最后一个决策点之后才到的）→
        // 用最后一轮快照当上下文，紧接着再跑一轮（同一个 SSE 流）：「进了队列的消息一定被处理」。
        let extra = steer_queue.drain_all(&session_id);
        if extra.is_empty() {
            send(json!({ "done": true }));
            break;
        }
        // 还有排队的消息 → 抢回占位自己续跑；抢不到说明已有别的请求起来了，
        // 把它们放回队列（那一轮的收尾会取走——入队与收尾的顺序保证不会悬空）。
        let Some(f) = cancel_registry.try_begin(&session_id) else {
            for t in extra {
                steer_queue.push(&session_id, t, false);
            }
            send(json!({ "done": true }));
            break;
        };
        run = CancelRun::from_flag(cancel_registry.clone(), &session_id, f);
        cancel = run.flag.clone();
        let mut next: Vec<Value> = last_snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if next.is_empty() {
            // 理论上到不了（每轮至少落一次快照）；兜底退回本轮历史，别把整轮丢掉
            next = history.clone();
        }
        observability::event(
            EventType::State,
            json!({ "kind": "steer", "phase": "followup", "session": session_id, "count": extra.len() }),
        );
        let turn_msgs: Vec<Value> =
            extra.iter().map(|t| json!({ "role": "user", "content": t })).collect();
        // 通知前端：这些排队消息现在开始跑了（气泡从「排队中」转成正式消息、开新回答气泡）
        send(json!({ "turn": { "messages": turn_msgs } }));
        next.extend(turn_msgs);
        history = next;
        // 滚动摘要跟着上一轮走（跑的过程中 Summary 事件更新过槽）
        prior_summary = summary_slot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        }
    });

    Sse::new(UnboundedReceiverStream::new(rx)).into_response()
}

/// 本轮任务的取消标志守卫（D2）：登记即持标志，Drop 即注销。
/// 任务正常结束、提前 return 或 panic 都会走到 Drop，注册表不留僵尸登记。
struct CancelRun {
    registry: Arc<CancelRegistry>,
    session: String,
    flag: Arc<CancelFlag>,
}

impl CancelRun {
    /// 接管一个**已登记**的标志（登记在 handler 入口由 `try_begin` 完成，
    /// 这样「拒并发」与「占位」是同一个原子动作）。
    fn from_flag(registry: Arc<CancelRegistry>, session_id: &str, flag: Arc<CancelFlag>) -> Self {
        Self { registry, session: session_id.to_string(), flag }
    }
}

impl Drop for CancelRun {
    fn drop(&mut self) {
        self.registry.end(&self.session, &self.flag);
    }
}

#[derive(Debug, Deserialize)]
pub struct CancelQuery {
    /// 目标会话（可空：不带 = 停全部在跑任务，命令行客户端与内置页的兼容语义）
    #[serde(default)]
    pub session: String,
}

/// POST /cancel[?session=<id>] —— 会话级停止：只停指定会话当前这一轮；
/// 不带 session 时停全部在跑任务。
async fn cancel(State(state): State<AppState>, Query(q): Query<CancelQuery>) -> Json<Value> {
    let session = q.session.trim();
    if session.is_empty() {
        let count = state.cancel.cancel_all();
        observability::event(
            EventType::State,
            json!({ "kind": "cancel", "scope": "all", "count": count }),
        );
        Json(json!({ "cancelled": true, "scope": "all", "count": count }))
    } else {
        let hit = state.cancel.cancel(session);
        observability::event(
            EventType::State,
            json!({ "kind": "cancel", "scope": "session", "session": session, "active": hit }),
        );
        Json(json!({ "cancelled": true, "scope": "session", "session": session, "active": hit }))
    }
}

/// GET /api/cancel[?session=<id>] —— 带 session 查该会话；不带查「是否有在跑会话被取消」。
async fn cancel_status(State(state): State<AppState>, Query(q): Query<CancelQuery>) -> Json<Value> {
    let session = q.session.trim();
    if session.is_empty() {
        Json(json!({
            "cancelled": state.cancel.any_cancelled(),
            "active": state.cancel.active_count(),
        }))
    } else {
        Json(json!({
            "cancelled": state.cancel.is_set(session),
            "running": state.cancel.is_active(session),
        }))
    }
}

// —— P10-A（施工单 014）：trace 时间线 ——

#[derive(Debug, Deserialize)]
pub struct TraceQuery {
    /// 最近任务数（1–50，默认 5）
    #[serde(default = "default_trace_limit")]
    pub limit: usize,
}
fn default_trace_limit() -> usize {
    5
}

// —— P10.5 设置面板「记忆」分区 ——

/// 系统约定（prompts/system.md）当前内容：每轮任务实时读盘，保存即生效；
/// 文件缺失/读不到时回落到启动时载入的提示词。
fn read_system_prompt(state: &AppState) -> String {
    std::fs::read_to_string(&*state.prompt_path).unwrap_or_else(|_| (*state.system_prompt).clone())
}

/// 保存系统约定：写 prompts/system.md（目录不存在则建）。
fn save_system_prompt(state: &AppState, content: &str) -> anyhow::Result<PathBuf> {
    let path = (*state.prompt_path).clone();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::fsx::write_atomic_str(&path, content)?;
    Ok(path)
}

/// GET /api/memory —— 记忆层状态 + 三类约定文件原文（编辑器数据源，每次调用读盘 = 显示文件实时内容）。
async fn get_memory(State(state): State<AppState>) -> Json<Value> {
    let mem = state.memory.read().unwrap_or_else(|e| e.into_inner()).clone();
    let projects_registered = mem.load_index().projects.len();
    Json(json!({
        "enabled": mem.enabled,
        "global_dir": mem.global_dir().display().to_string(),
        "project_dir": mem.project_dir().display().to_string(),
        "events_lines": mem.events_lines(),
        "projects_registered": projects_registered,
        "system_path": state.prompt_path.display().to_string(),
        "global_agents_path": mem.global_agents_path().display().to_string(),
        "project_agents_path": mem.project_agents_path().display().to_string(),
        "system_prompt": read_system_prompt(&state),
        "agents_global": mem.agents_raw("global"),
        "agents_project": mem.agents_raw("project"),
    }))
}

#[derive(Debug, Deserialize)]
pub struct SaveAgentsRequest {
    /// "system"（prompts/system.md）| "global"（~/.agent/AGENTS.md）| "project"（工作区根 AGENTS.md）
    pub scope: String,
    pub content: String,
}

/// POST /api/memory/agents —— 保存用户编辑的约定文件（system 走实时读盘，保存即生效；
/// 两层 AGENTS.md 下次任务注入自动生效）。
async fn save_memory_agents(
    State(state): State<AppState>,
    Json(req): Json<SaveAgentsRequest>,
) -> (StatusCode, Json<Value>) {
    if !matches!(req.scope.as_str(), "system" | "global" | "project") {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "scope 须为 system / global / project" })));
    }
    let mem = state.memory.read().unwrap_or_else(|e| e.into_inner()).clone();
    let saved = if req.scope == "system" {
        save_system_prompt(&state, &req.content)
    } else {
        mem.save_agents(&req.scope, &req.content)
    };
    match saved {
        Ok(path) => {
            observability::event(
                EventType::State,
                json!({ "kind": "memory.agents_saved", "scope": req.scope, "path": path.display().to_string() }),
            );
            (StatusCode::OK, Json(json!({ "ok": true, "path": path.display().to_string() })))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

// —— 记忆库（星云视图）：会话级记忆索引 + 向量检索 ——

/// 解析 EMBEDDING 角色渠道（B3 顺序：settings.roles.embedding > config.toml [roles].embedding）。
/// 嵌入**不走主渠道兜底**：主模型多半没有 /embeddings 端点，未配置时用本地哈希向量更可靠。
fn resolve_embedder<'a>(state: &'a AppState, s: &Settings) -> Option<crate::memory::library::Embedder<'a>> {
    let name = s
        .roles
        .as_ref()
        .and_then(|r| r.embedding.as_deref())
        .or_else(|| state.roles_cfg.embedding.as_deref())?;
    let ch = find_channel(s, name)?;
    if ch.api_key.trim().is_empty() {
        return None;
    }
    Some(crate::memory::library::Embedder {
        gw: &state.gateway,
        base_url: ch.base_url,
        model: ch.model,
        api_key: ch.api_key,
    })
}

/// 增量同步索引 → 记忆库 JSON（光点列表 + 项目聚合 + 索引状态）。
async fn build_library(state: &AppState, force: bool) -> Value {
    use crate::memory::library as lb;
    let settings = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    let embedder = resolve_embedder(state, &settings);
    let report = lb::sync(&state.session, &state.log_dir, embedder, force).await;
    let lib = lb::load(&state.log_dir);
    let items: Vec<Value> = lib
        .entries
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "title": e.title,
                "project": e.project,
                "workspace": e.workspace,
                "created_at": e.created_at,
                "updated_at": e.updated_at,
                "count": e.count,
                "chars": e.chars,
                "pinned": e.pinned,
                "archived": e.archived,
                "snippet": e.snippet,
                "indexed": !e.vec.is_empty(),
            })
        })
        .collect();
    let mut proj: std::collections::BTreeMap<String, (String, usize)> = std::collections::BTreeMap::new();
    for e in &lib.entries {
        let slot = proj.entry(e.project.clone()).or_insert_with(|| (e.workspace.clone(), 0));
        slot.1 += 1;
    }
    let projects: Vec<Value> = proj
        .iter()
        .map(|(name, (ws, n))| json!({ "name": name, "workspace": ws, "count": n }))
        .collect();
    let indexed = lib.entries.iter().filter(|e| !e.vec.is_empty()).count();
    let mem = state.memory.read().unwrap_or_else(|e| e.into_inner()).clone();
    json!({
        "items": items,
        "projects": projects,
        "stats": {
            "sessions": lib.entries.len(),
            "projects": projects.len(),
            "indexed": indexed,
            "pending": lib.entries.len() - indexed,
            "model": lib.model,
            "mode": report.mode,
        },
        "sync": report,
        "events_lines": mem.events_lines(),
        "notice": report.error,
    })
}

/// GET /api/memory/library —— 记忆库光点数据（每次调用做一次有界增量同步）。
async fn get_memory_library(State(state): State<AppState>) -> Json<Value> {
    Json(build_library(&state, false).await)
}

/// POST /api/memory/library/reindex —— 强制重建向量（忽略指纹），返回同款光点数据。
async fn reindex_memory_library(State(state): State<AppState>) -> Json<Value> {
    observability::event(EventType::State, json!({ "kind": "memory.reindex" }));
    Json(build_library(&state, true).await)
}

#[derive(Debug, Deserialize)]
pub struct MemorySearchRequest {
    #[serde(default)]
    pub query: String,
    /// 「相似会话」：以该会话为源找相近的其它会话（用索引里已存的向量，不调外部渠道）
    #[serde(default)]
    pub like: Option<String>,
    #[serde(default)]
    pub top_k: Option<usize>,
    /// 匹配下限（0..1，越小越宽）；搜索缺省 0.18、相似缺省 0.3
    #[serde(default)]
    pub min_score: Option<f32>,
}

/// POST /api/memory/search —— 向量检索（混合排序），返回命中的会话 id 供光点过滤。
async fn search_memory(State(state): State<AppState>, Json(req): Json<MemorySearchRequest>) -> Json<Value> {
    use crate::memory::library as lb;
    let lib = lb::load(&state.log_dir);
    let top_k = req.top_k.unwrap_or(40).clamp(1, 200);
    if let Some(id) = req.like.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let results = lb::similar(&lib, id, top_k, req.min_score.unwrap_or(0.3).clamp(0.0, 1.0));
        observability::event(EventType::State, json!({ "kind": "memory.similar", "id": id, "hits": results.len() }));
        return Json(json!({
            "results": results,
            "total": lib.entries.len(),
            "mode": "similar",
            "model": lib.model,
            "notice": Value::Null,
        }));
    }
    let query = req.query.trim().to_string();
    if query.is_empty() {
        return Json(json!({ "results": [], "total": 0, "mode": "none", "notice": "查询为空" }));
    }
    let min_score = req.min_score.unwrap_or(0.18).clamp(0.0, 1.0);
    // 查询向量必须与索引同口径：本地索引直接本地算；外部索引调嵌入席位，
    // 失败/未配置则退化为纯词法（qvec 为空，search 内部自动降级）。
    let mut notice: Option<String> = None;
    let qvec = if lib.model.is_empty() || lib.model == lb::LOCAL_MODEL {
        lb::local_vec(&query)
    } else {
        let settings = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
        match resolve_embedder(&state, &settings) {
            Some(emb) => {
                match emb.gw.embed(&emb.base_url, &emb.model, &emb.api_key, std::slice::from_ref(&query)).await {
                    Ok(vs) if !vs.is_empty() => lb::norm_vec(&vs[0]),
                    Ok(_) => {
                        notice = Some("嵌入渠道未返回向量，本次只用关键词匹配".into());
                        Vec::new()
                    }
                    Err(e) => {
                        notice = Some(format!("嵌入渠道不可用（{e}），本次只用关键词匹配"));
                        Vec::new()
                    }
                }
            }
            None => {
                notice = Some("嵌入席位未配置或 key 缺失，本次只用关键词匹配".into());
                Vec::new()
            }
        }
    };
    let results = lb::search(&lib, &qvec, &query, top_k, min_score);
    let mode = if qvec.is_empty() {
        "lexical"
    } else if lib.model == lb::LOCAL_MODEL {
        "local"
    } else {
        "external"
    };
    observability::event(
        EventType::State,
        json!({ "kind": "memory.search", "query": query, "hits": results.len(), "mode": mode }),
    );
    Json(json!({
        "results": results,
        "total": lib.entries.len(),
        "mode": mode,
        "model": lib.model,
        "notice": notice,
    }))
}

// —— P11（施工单 016）：MCP 插件 ——

/// GET /api/mcp —— server 列表 + WASM 插件（配置/在线状态/工具清单）。
async fn get_mcp(State(state): State<AppState>) -> Json<Value> {
    get_mcp_inner(&state)
}

#[derive(Debug, Deserialize)]
pub struct McpActionRequest {
    /// save（新增/更新）| remove | confirm（首装确认）| refresh（重连发现）| toggle（启停）
    pub action: String,
    #[serde(default)]
    pub server: Option<Value>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub risk: Option<String>,
}

/// POST /api/mcp —— 配置管理与生命周期动作（全部落 settings.json 并同步管理器）。
async fn post_mcp(
    State(state): State<AppState>,
    Json(req): Json<McpActionRequest>,
) -> (StatusCode, Json<Value>) {
    let mut s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    let err = |m: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": m })));
    match req.action.as_str() {
        "save" => {
            let Some(srv) = req.server.clone() else {
                return err("缺少 server 配置".into());
            };
            let cfg: crate::config::settings::McpServerCfg = match serde_json::from_value(srv) {
                Ok(c) => c,
                Err(e) => return err(format!("server 配置不合法: {e}")),
            };
            if !crate::plugin::mcp::valid_name(&cfg.name) {
                return err("名称非法（非空、仅字母数字-_、禁止包含 __）".into());
            }
            // transport 合法性 + 该 transport 的必需字段（早失败好过调用时才报错）
            match cfg.transport.as_str() {
                "http" | "HTTP" | "Http" => {
                    if !cfg.url.trim().starts_with("http://") && !cfg.url.trim().starts_with("https://") {
                        return err("http transport 需要 http(s):// 开头的 url".into());
                    }
                }
                "stdio" | "STDIO" | "Stdio" => {
                    if cfg.command.trim().is_empty() {
                        return err("stdio transport 需要 command".into());
                    }
                }
                other => return err(format!("未知 transport: {other}（stdio | http）")),
            }
            s.mcp_servers.retain(|c| c.name != cfg.name);
            s.mcp_servers.push(cfg);
        }
        "remove" => {
            let Some(name) = &req.name else { return err("缺少 name".into()) };
            s.mcp_servers.retain(|c| &c.name != name);
        }
        "confirm" => {
            let Some(name) = &req.name else { return err("缺少 name".into()) };
            let Some(c) = s.mcp_servers.iter_mut().find(|c| &c.name == name) else {
                return err(format!("未知 server: {name}"));
            };
            c.confirmed = true;
            if let Some(r) = &req.risk
                && (r == "dangerous" || r == "normal") {
                    c.risk = r.clone();
                }
        }
        "toggle" => {
            let Some(name) = &req.name else { return err("缺少 name".into()) };
            let Some(c) = s.mcp_servers.iter_mut().find(|c| &c.name == name) else {
                return err(format!("未知 server: {name}"));
            };
            c.enabled = !c.enabled;
        }
        "refresh" => {
            let Some(name) = &req.name else { return err("缺少 name".into()) };
            if !s.mcp_servers.iter().any(|c| &c.name == name) {
                return err(format!("未知 server: {name}"));
            }
            // 只重连+重新发现；配置不动（ensure_online 用管理器内已同步的 cfg）
        }
        other => return err(format!("未知操作: {other}")),
    }

    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    crate::plugin::mcp::sync_configs(&s.mcp_servers);

    // refresh：同步后拉起并重新发现工具（阻塞 IO 丢 blocking 池）
    crate::plugin::wasm::rescan(); // 017：顺带重扫 WASM 插件
    if req.action == "refresh" {
        let name = req.name.clone().unwrap_or_default();
        let r = tokio::task::spawn_blocking(move || crate::plugin::mcp::ensure_online_by_name(&name)).await;
        match r {
            Ok(Ok(n)) => {
                return (StatusCode::OK, Json(json!({ "ok": true, "tools_found": n })));
            }
            Ok(Err(e)) => {
                return (StatusCode::OK, Json(json!({ "ok": false, "error": e })));
            }
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
            }
        }
    }

    (StatusCode::OK, get_mcp_inner(&state))
}

fn get_mcp_inner(state: &AppState) -> Json<Value> {
    let wasm_list: Vec<Value> = crate::plugin::wasm::status()
        .into_iter()
        .map(|(stem, ok, tools, err)| {
            json!({
                "stem": stem, "ok": ok, "load_error": err,
                "tools": tools.iter().map(|(n, d)| json!({"name": n, "desc": d})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    let status: std::collections::HashMap<String, (bool, Vec<(String, String)>)> =
        crate::plugin::mcp::status()
            .into_iter()
            .map(|(n, o, t)| (n, (o, t)))
            .collect();
    // P17：resources / prompts 清单（声明了能力的 server 才有内容）
    let catalogs = crate::plugin::mcp::catalogs();
    let servers: Vec<Value> = s
        .mcp_servers
        .iter()
        .map(|c| {
            let (online, tools) = status
                .get(&c.name)
                .cloned()
                .unwrap_or((false, Vec::new()));
            let (res, prompts) = catalogs.get(&c.name).cloned().unwrap_or_default();
            json!({
                "name": c.name, "transport": c.transport, "command": c.command, "args": c.args,
                "env": c.env, "url": c.url, "headers": c.headers,
                "enabled": c.enabled, "confirmed": c.confirmed, "risk": c.risk,
                "online": online,
                "tools": tools.iter().map(|(n, d)| json!({"name": n, "desc": d})).collect::<Vec<_>>(),
                "resources": res,
                "prompts": prompts,
            })
        })
        .collect();
    Json(json!({ "servers": servers, "wasm": wasm_list }))
}

/// GET /api/version —— 版本与运行形态（界面「关于」卡片用）。
/// root 是**运行时认定的应用根**（打包形态=exe 同级，开发形态=源码目录），
/// 是排查「配置/扩展/前端产物读到哪一份」的第一手线索。
async fn get_version() -> Json<Value> {
    Json(json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "root": crate::config::app_root().display().to_string(),
        "exe": std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
    }))
}

/// GET /api/ext —— 组件总线 v0：UI 扩展清单（应用根内置 + 当前工作区自带）。
async fn list_extensions() -> Json<Value> {
    Json(json!({
        "extensions": crate::plugin::ui::scan(),
        "dir": crate::plugin::ui::DIR,
        "app_dir": crate::config::app_root().join(crate::plugin::ui::DIR).display().to_string(),
        "workspace_dir": crate::tools::workspace_root().join(crate::plugin::ui::DIR).display().to_string(),
    }))
}

/// GET /ui-ext/{name} —— 扩展入口页（entry 由 manifest 指定）。
async fn ui_ext_entry(AxumPath(name): AxumPath<String>) -> Response {
    serve_ext(&name, "")
}

/// GET /ui-ext/{name}/{*path} —— 扩展目录内的静态资源。
async fn ui_ext_asset(AxumPath((name, path)): AxumPath<(String, String)>) -> Response {
    serve_ext(&name, &path)
}

/// 分发扩展资产：越界/缺失一律 404（不区分，避免探测目录结构）。
fn serve_ext(name: &str, rel: &str) -> Response {
    let rel = if rel.trim().is_empty() {
        crate::plugin::ui::scan()
            .into_iter()
            .find(|e| e.name == name)
            .map(|e| e.entry)
            .unwrap_or_else(|| "index.html".to_string())
    } else {
        rel.to_string()
    };
    match crate::plugin::ui::resolve_asset(name, &rel) {
        Ok(p) => match std::fs::read(&p) {
            Ok(bytes) => ([(CONTENT_TYPE, crate::plugin::ui::content_type(&p))], bytes).into_response(),
            Err(e) => (StatusCode::NOT_FOUND, format!("读文件失败: {e}")).into_response(),
        },
        Err(e) => (StatusCode::NOT_FOUND, e).into_response(),
    }
}

/// GET /api/trace?limit=N —— 最近 N 个任务的事件流（时间线面板数据源）。
/// **只读解析** JSONL（payload 原样透传，6 类 schema 不动）；连续 model_chunk 聚合；
/// 坏行跳过；文件不存在/为空返回空 tasks 而非报错。
async fn get_trace(Query(q): Query<TraceQuery>) -> (StatusCode, Json<Value>) {
    let Some(path) = observability::log_path() else {
        return (StatusCode::OK, Json(json!({ "tasks": [], "note": "logger 未初始化" })));
    };
    let limit = q.limit.clamp(1, 50);
    let path = path.to_path_buf();
    let res = tokio::task::spawn_blocking(move || observability::read_tasks(&path, limit)).await;
    match res {
        Ok(Ok(tasks)) => {
            let list: Vec<Value> = tasks
                .iter()
                .map(|t| {
                    json!({
                        "start_ts": t.start_ts,
                        "end_ts": t.end_ts,
                        "duration_ms": t.duration_ms(),
                        "events": observability::trace::render_events(t),
                    })
                })
                .collect();
            (StatusCode::OK, Json(json!({ "tasks": list })))
        }
        Ok(Err(e)) => (StatusCode::OK, Json(json!({ "tasks": [], "note": format!("读取失败: {e}") }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// 审批门（施工单 005 + 本机直跑）：SSE 发审批请求，oneshot 等用户决定；
/// 120 秒不决定视为拒绝；用户点「停止」（取消标志）立即按拒绝返回。
/// 档位读共享状态：侧栏切档即时生效（含进行中的请求）。
struct ChatGate {
    /// 全局默认档（本会话没显式切过档时用它）
    mode: Arc<std::sync::RwLock<String>>,
    /// M1：每会话档位覆盖（与 AppState 同一份）——并发多会话各读各的
    session_modes: Arc<std::sync::RwLock<std::collections::HashMap<String, String>>>,
    allow_unsandboxed: bool,
    unsandboxed_approval: bool,
    pending: Arc<std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<bool>>>>,
    /// P9 A3：本次请求所属会话（会话内同类允许的作用域）
    session_id: String,
    /// P9 A3：挂起审批的元信息（id → scope/session），ask 时写入、decide 时取用
    meta: Arc<std::sync::Mutex<std::collections::HashMap<String, (String, String)>>>,
    /// P9 A3：会话内已授权的审批范围（与 AppState.approved_scopes 同一份）
    approved: Arc<std::sync::Mutex<std::collections::HashMap<String, std::collections::HashSet<String>>>>,
    /// P9 B1：本次请求的 Plan 档（请求级 plan_mode 带入）
    plan_mode: bool,
    tx: mpsc::UnboundedSender<Result<Event, Infallible>>,
    cancel: Arc<CancelFlag>,
    /// 025：运行中追加消息队列（与 AppState 同一份）——每个决策点取一次并入本轮
    steer: Arc<crate::core::steer::SteerQueue>,
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
async fn decide_approval(
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

/// GET /api/sessions —— 会话列表（新→旧）+ 项目名（P6.1 多会话）。
async fn list_sessions(State(state): State<AppState>) -> Json<Value> {
    let project = crate::tools::workspace_root()
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Json(json!({ "project": project, "sessions": state.session.list() }))
}

#[derive(Debug, Deserialize)]
pub struct SessionMetaUpdate {
    pub id: String,
    /// Some=重命名会话标题
    #[serde(default)]
    pub title: Option<String>,
    /// Some=置顶/取消置顶
    #[serde(default)]
    pub pinned: Option<bool>,
    /// Some=归档/取消归档
    #[serde(default)]
    pub archived: Option<bool>,
}

/// POST /api/sessions/meta —— DSH 式侧边栏：置顶/取消置顶、归档/取消归档、重命名。
async fn session_meta(
    State(state): State<AppState>,
    Json(req): Json<SessionMetaUpdate>,
) -> (StatusCode, Json<Value>) {
    let sess = state.session.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        if let Some(t) = &req.title {
            sess.rename(&req.id, t.trim())?;
        }
        if req.pinned.is_some() || req.archived.is_some() {
            sess.set_flags(&req.id, req.pinned, req.archived)?;
        }
        Ok(())
    })
    .await;
    match res {
        Ok(Ok(())) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

#[derive(Debug, Deserialize)]
pub struct ForkRequest {
    pub id: String,
}

/// POST /api/sessions/fork —— DSH 式「分叉会话」：复制消息/摘要/工作区为新会话。
async fn fork_session(
    State(state): State<AppState>,
    Json(req): Json<ForkRequest>,
) -> (StatusCode, Json<Value>) {
    let sess = state.session.clone();
    let id = req.id.clone();
    match tokio::task::spawn_blocking(move || sess.fork(&id)).await {
        Ok(Ok(new_id)) => (StatusCode::OK, Json(json!({ "ok": true, "id": new_id }))),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

#[derive(Debug, Deserialize)]
pub struct SessionBatchRequest {
    pub ids: Vec<String>,
    /// Some=批量置顶/取消置顶
    #[serde(default)]
    pub pinned: Option<bool>,
    /// Some=批量归档/取消归档（管理模式主用）
    #[serde(default)]
    pub archived: Option<bool>,
}

/// POST /api/sessions/batch —— 管理模式批量操作（归档/取消归档/置顶）。
async fn batch_sessions(
    State(state): State<AppState>,
    Json(req): Json<SessionBatchRequest>,
) -> (StatusCode, Json<Value>) {
    let sess = state.session.clone();
    let ids = req.ids.clone();
    let (pinned, archived) = (req.pinned, req.archived);
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
        let mut n = 0;
        for id in &ids {
            if sess.set_flags(id, pinned, archived).is_ok() {
                n += 1;
            }
        }
        Ok(n)
    })
    .await;
    match res {
        Ok(Ok(n)) => (StatusCode::OK, Json(json!({ "ok": true, "updated": n }))),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// POST /api/workspace/picker —— 弹 Win 原生选文件夹窗口（rfd/IFileDialog），返回所选路径。
/// 用户取消时 canceled=true。对话框由本地 server 进程弹出，即系统原生窗口。
async fn pick_workspace() -> (StatusCode, Json<Value>) {
    let res = tokio::task::spawn_blocking(|| rfd::FileDialog::new().pick_folder()).await;
    match res {
        Ok(Some(path)) => {
            // Windows canonicalize 加 \\?\ 前缀难看；rfd 返回的是用户选择的原始路径，直接清洗
            let s = path.display().to_string();
            let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
            (StatusCode::OK, Json(json!({ "ok": true, "path": s })))
        }
        Ok(None) => (StatusCode::OK, Json(json!({ "ok": true, "canceled": true }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// POST /api/workspace/remove —— 从侧边栏项目列表（recent_workspaces）移除（不删目录）。
async fn remove_workspace(
    State(state): State<AppState>,
    Json(req): Json<WorkspaceRequest>,
) -> (StatusCode, Json<Value>) {
    let path = req.path.trim();
    if path.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "路径为空" })));
    }
    let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner()).clone();
    s.recent_workspaces.retain(|w| w != path);
    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    crate::plugin::mcp::sync_configs(&s.mcp_servers); // P11：MCP 配置变更即同步管理器（幂等）
    observability::event(EventType::State, json!({ "kind": "workspace.removed", "path": path }));
    (StatusCode::OK, Json(json!({ "ok": true, "recent": s.recent_workspaces })))
}

/// POST /api/workspace/alias —— 侧边栏「重命名项目」：工作区显示别名；空名=清除别名。
async fn alias_workspace(
    State(state): State<AppState>,
    Json(req): Json<WorkspaceAliasRequest>,
) -> (StatusCode, Json<Value>) {
    let path = req.path.trim();
    if path.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "路径为空" })));
    }
    let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner()).clone();
    let name = req.name.trim().to_string();
    if name.is_empty() {
        s.workspace_aliases.remove(path);
    } else {
        s.workspace_aliases.insert(path.to_string(), name.clone());
    }
    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    crate::plugin::mcp::sync_configs(&s.mcp_servers); // P11：MCP 配置变更即同步管理器（幂等）
    observability::event(
        EventType::State,
        json!({ "kind": "workspace.aliased", "path": path, "name": name }),
    );
    (StatusCode::OK, Json(json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub struct WorkspaceAliasRequest {
    pub path: String,
    pub name: String,
}

/// GET /api/session?id=xxx —— 恢复指定会话；缺省返回最近更新的一条。
async fn get_session(State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>) -> (StatusCode, Json<Value>) {
    let f = match q.get("id") {
        Some(id) => state.session.get(id),
        None => state.session.latest(),
    };
    match f {
        Some(f) => (StatusCode::OK, Json(json!({ "id": f.id, "title": f.title, "updated_at": f.updated_at, "summary": f.summary, "workspace": f.workspace, "messages": f.messages }))),
        None => (StatusCode::OK, Json(json!({ "messages": [] }))),
    }
}

#[derive(Debug, Deserialize)]
pub struct SessionOpenRequest {
    pub id: String,
}

/// POST /api/sessions/new —— 新建一条空会话并立即落盘（当前项目下），
/// 侧栏马上能见到该会话；首条消息随后写入同一条。同工作区已有空会话则复用。
async fn new_session(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let ws = crate::tools::workspace_root().display().to_string();
    match state.session.create_empty(&ws, "新会话") {
        Ok(f) => {
            observability::event(
                EventType::State,
                json!({ "kind": "session.created", "session": f.id, "workspace": ws }),
            );
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "id": f.id, "title": f.title, "workspace": ws, "created_at": f.created_at })),
            )
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// POST /api/sessions/open —— 打开会话：其所属工作区与当前不同时先切过去，再返回消息。
/// 「切会话 = 回到那个项目」的闭环入口；目录已不存在时不硬失败，降级只读打开并回 warning。
async fn open_session(
    State(state): State<AppState>,
    Json(req): Json<SessionOpenRequest>,
) -> (StatusCode, Json<Value>) {
    let Some(f) = state.session.get(&req.id) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "会话不存在" })));
    };
    let current = crate::tools::workspace_root().display().to_string();
    let mut switched = false;
    let mut warning: Option<String> = None;
    let target = f.workspace.trim();
    if !target.is_empty() && !same_workspace(&current, target) {
        let p = std::path::PathBuf::from(target);
        if p.is_dir() {
            let canon = canon_workspace(p);
            match apply_workspace(&state, &canon) {
                Ok(()) => switched = true,
                Err(e) => warning = Some(format!("切换项目失败：{e}")),
            }
        } else {
            warning = Some(format!("原项目目录已不存在，仍在当前项目里打开该会话：{target}"));
        }
    }
    (
        StatusCode::OK,
        Json(json!({
            "id": f.id,
            "title": f.title,
            "updated_at": f.updated_at,
            "summary": f.summary,
            "messages": f.messages,
            "workspace": f.workspace,
            "current_workspace": crate::tools::workspace_root().display().to_string(),
            "switched": switched,
            "warning": warning,
        })),
    )
}

/// DELETE /api/session?id=xxx —— 删除一条会话（其余不受影响）。
async fn clear_session(State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>) -> (StatusCode, Json<Value>) {
    let Some(id) = q.get("id") else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 id" })));
    };
    match state.session.delete(id) {
        Ok(()) => {
            // P9 A3：删会话即撤销其「会话内同类允许」记忆（仅内存，从不落盘）
            if let Ok(mut m) = state.approved_scopes.lock() {
                m.remove(id);
            }
            observability::event(EventType::State, json!({ "kind": "session.cleared", "session": id }));
            (StatusCode::OK, Json(json!({ "ok": true })))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// POST /api/handoff —— 手动生成交接文档（消息取自会话落盘）。
async fn make_handoff(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let ws = crate::tools::workspace_root().to_path_buf();
    let session_id = observability::get().session_id().to_string();
    let messages = state.session.latest().map(|f| f.messages);
    let input = crate::recovery::HandoffInput {
        trigger: "手动（用户请求）".to_string(),
        session_id,
        messages,
    };
    let res = tokio::task::spawn_blocking(move || crate::recovery::write_handoff(&ws, &input)).await;
    match res {
        Ok(Ok(path)) => {
            observability::event(
                EventType::State,
                json!({ "kind": "handoff.written", "path": path.display().to_string(), "trigger": "manual" }),
            );
            (StatusCode::OK, Json(json!({ "ok": true, "path": path.display().to_string() })))
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// GET /api/git/status —— 工作区 git 概览（分支/HEAD/脏文件数/上游领先落后/未提交明细）。
/// 非 git 仓库返回 is_repo=false，前端隐藏该信息。
async fn git_status(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let _ = &state;
    let ws = crate::tools::workspace_root().to_path_buf();
    match tokio::task::spawn_blocking(move || crate::recovery::status(&ws)).await {
        Ok(s) => (StatusCode::OK, Json(serde_json::to_value(s).unwrap_or_else(|_| json!({ "is_repo": false })))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// GET /api/git/info —— git 运行时状态（设置面板「插件」分区：可用性/版本/来源/内置包）。
async fn git_runtime_info() -> Json<Value> {
    Json(crate::tools::git_info())
}

#[derive(Debug, Deserialize)]
pub struct GitCommitRequest {
    pub message: String,
    /// 要提交的路径；缺省/空数组 = 全部改动（add -A）
    #[serde(default)]
    pub files: Vec<String>,
}

/// POST /api/git/commit —— 用户手动提交：按选中路径 stage 后 commit，返回短哈希。
async fn git_commit(
    State(state): State<AppState>,
    Json(req): Json<GitCommitRequest>,
) -> (StatusCode, Json<Value>) {
    let _ = &state;
    let ws = crate::tools::workspace_root().to_path_buf();
    let msg = req.message.clone();
    let files = req.files.clone();
    let res = tokio::task::spawn_blocking(move || crate::recovery::commit(&ws, &msg, &files)).await;
    match res {
        Ok(Ok(id)) => {
            observability::event(
                EventType::State,
                json!({ "kind": "git.committed", "id": id, "files": req.files.len(), "manual": true }),
            );
            (StatusCode::OK, Json(json!({ "ok": true, "id": id })))
        }
        // git 自己的报错（如「没有可提交的改动」）直接给用户看，比翻译成中文更有用
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// GET /api/checkpoints[?session=<id>] —— 影子仓库检查点列表（新→旧，最多 20 个）。
/// 带 session 时额外返回 `mine`：该会话自己的检查点（改动条基线优先取它）。
async fn list_checkpoints(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let _ = &state;
    let want = q.get("session").map(|s| s.trim().to_string()).unwrap_or_default();
    let ws = crate::tools::workspace_root().to_path_buf();
    match tokio::task::spawn_blocking(move || crate::recovery::list(&ws, 20)).await {
        Ok(Ok(list)) => {
            let mine: Vec<_> = list.iter().filter(|c| !want.is_empty() && c.session == want).cloned().collect();
            (StatusCode::OK, Json(json!({ "checkpoints": list, "mine": mine })))
        }
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// POST /api/checkpoints/restore —— 工作树回滚到指定检查点（shell 副作用不可回滚）。
async fn restore_checkpoint(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let _ = &state;
    let Some(id) = req.get("id").and_then(Value::as_str) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 id" })));
    };
    let ws = crate::tools::workspace_root().to_path_buf();
    let id = id.to_string();
    match tokio::task::spawn_blocking(move || crate::recovery::restore(&ws, &id)).await {
        Ok(Ok(())) => {
            observability::event(
                EventType::State,
                json!({ "kind": "checkpoint.restored", "id": req.get("id").and_then(Value::as_str).unwrap_or_default() }),
            );
            (StatusCode::OK, Json(json!({ "ok": true })))
        }
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用配置（custom 中转端点：探测不出规范 → openai 兜底，适合测显式覆盖）。
    fn test_config() -> crate::config::settings::Config {
        crate::config::settings::Config {
            id: "cfg-t".into(),
            name: "test".into(),
            provider_id: "custom".into(),
            base_url: "https://relay.example.com/v1".into(),
            model: "m".into(),
            models: vec![],
            key_cipher: None,
            context_length: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            thinking: None,
            thinking_style: None,
            capabilities: Default::default(),
            capabilities_user_set: false,
        }
    }

    fn test_settings() -> Settings {
        let mut s = Settings::default();
        s.configs.push(test_config());
        s.active_config_id = Some("cfg-t".into());
        s
    }

    // —— 来源校验（施工单 022）：堵 DNS rebinding 的那三条规则 ——

    #[test]
    fn host_parsing_covers_ipv4_ipv6_and_bare_names() {
        assert_eq!(split_host("127.0.0.1:5173"), ("127.0.0.1", Some(5173)));
        assert_eq!(split_host("localhost"), ("localhost", None));
        assert_eq!(split_host("[::1]:5173"), ("::1", Some(5173)));
        assert_eq!(split_host("[::1]"), ("::1", None));
        assert_eq!(split_host("example.com:80"), ("example.com", Some(80)));
        // 非数字「端口」不当端口解析（否则 example.com:abc 会被当成主机 example.com）
        assert_eq!(split_host("example.com:abc"), ("example.com:abc", None));
    }

    #[test]
    fn loopback_detection() {
        for ok in ["localhost", "LOCALHOST", "127.0.0.1", "127.1.2.3", "::1", "localhost."] {
            assert!(is_loopback_host(ok), "{ok} 应判为回环");
        }
        for bad in ["example.com", "evil.test", "0.0.0.0", "10.0.0.5", "192.168.1.9", ""] {
            assert!(!is_loopback_host(bad), "{bad} 不应判为回环");
        }
    }

    /// 核心场景：rebinding 时 Host 是攻击者域名 → 拒；正常本机访问 → 放行。
    #[test]
    fn rebinding_host_is_rejected() {
        let e = check_origin("evil.example.com:5173", None).expect_err("非本机 Host 必须拒");
        assert!(e.contains("不是本机地址"), "{e}");
        assert!(check_origin("127.0.0.1:5173", None).is_ok());
        assert!(check_origin("localhost:5173", Some("http://localhost:5173")).is_ok());
        // 不带 Origin 的命令行客户端（curl / eval）放行
        assert!(check_origin("127.0.0.1:5173", None).is_ok());
        // 缺 Host 头：拒（HTTP/1.0 老客户端）
        assert!(check_origin("", None).is_err());
    }

    #[test]
    fn cross_origin_and_opaque_origins_are_rejected() {
        // 外站页面的 AJAX
        assert!(check_origin("127.0.0.1:5173", Some("http://evil.example.com")).is_err());
        // 本机另一个端口上的页面（也是跨源）
        assert!(check_origin("127.0.0.1:5173", Some("http://127.0.0.1:9999")).is_err());
        // 沙箱 iframe / file:// 的不透明源
        assert!(check_origin("127.0.0.1:5173", Some("null")).is_err());
        // 端口缺省的显式写法：http 默认 80 与 Host 无端口同源
        assert!(check_origin("localhost", Some("http://localhost")).is_ok());
        assert!(check_origin("localhost:80", Some("http://localhost:80")).is_ok());
    }

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

    #[test]
    fn thinking_style_override() {
        let _ = gateway::providers::init(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("providers.json"),
        );
        // 自定义中转端点探测认不出 → 覆盖为 qwen 布尔形态
        let preset = gateway::providers::find("custom");
        let mut c = crate::config::settings::Config {
            id: "cfg-t".into(),
            name: "test".into(),
            provider_id: "custom".into(),
            base_url: "".into(),
            model: "m".into(),
            models: vec![],
            key_cipher: None,
            context_length: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            thinking: Some("off".into()),
            thinking_style: Some("qwen".into()),
            capabilities: Default::default(),
            capabilities_user_set: false,
        };
        let p = build_chat_params(&c, preset);
        assert_eq!(p.thinking_format, Some(gateway::compat::ThinkingFormat::Qwen));
        // 覆盖后 off 真的会发 enable_thinking:false
        let body = gateway::build_body("m", &json!([]), true, &p, None);
        assert_eq!(body["enable_thinking"], json!(false));
        // P8.3 旧设置值自动映射：thinking_object → zai
        c.thinking_style = Some("thinking_object".into());
        let p = build_chat_params(&c, preset);
        assert_eq!(p.thinking_format, Some(gateway::compat::ThinkingFormat::Zai));
        // none：任何思考参数都不发（none 用 openai 形态但不发档位=零思考字段）
        c.thinking_style = Some("none".into());
        let p = build_chat_params(&c, gateway::providers::find("zhipu"));
        let body = gateway::build_body("glm-4.6", &json!([]), true, &p, None);
        assert!(body.get("thinking").is_none() && body.get("enable_thinking").is_none());
        // auto：端点探测（zhipu 官方域名 → zai 形态）
        c.thinking_style = None;
        c.thinking = Some("on".into());
        let p = build_chat_params(&c, gateway::providers::find("zhipu"));
        let body = gateway::build_body("glm-4.6", &json!([]), true, &p, None);
        assert_eq!(p.thinking_format, None, "auto 不写覆盖，探测说了算");
        assert_eq!(body["thinking"], json!({"type":"enabled"}), "探测出 zai 形态后 on 生效");
    }

    #[test]
    fn effective_thinking_style_resolution() {
        let _ = gateway::providers::init(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("providers.json"),
        );
        let mut c = crate::config::settings::Config {
            id: "cfg-t".into(),
            name: "test".into(),
            provider_id: "custom".into(),
            base_url: "".into(),
            model: "m".into(),
            models: vec![],
            key_cipher: None,
            context_length: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            thinking: None,
            thinking_style: None,
            capabilities: Default::default(),
            capabilities_user_set: false,
        };
        // 自定义中转（空地址探测不出）→ openai 形态兜底
        c.provider_id = "custom".into();
        assert_eq!(effective_thinking_style(&c, gateway::providers::find("custom")), "openai");
        // 智谱官方域名探测 → zai
        c.provider_id = "zhipu".into();
        c.base_url = "https://open.bigmodel.cn/api/paas/v4".into();
        assert_eq!(effective_thinking_style(&c, gateway::providers::find("zhipu")), "zai");
        // 覆盖优先，旧词表也映射
        c.thinking_style = Some("enable_thinking_budget".into());
        assert_eq!(effective_thinking_style(&c, gateway::providers::find("zhipu")), "qwen");
        c.thinking_style = Some("none".into());
        assert_eq!(effective_thinking_style(&c, gateway::providers::find("zhipu")), "none");
    }

    #[test]
    fn thinking_accepts_effort_levels() {
        let _ = gateway::providers::init(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("providers.json"),
        );
        let mut c = crate::config::settings::Config {
            id: "cfg-t".into(),
            name: "test".into(),
            provider_id: "openai".into(),
            base_url: "".into(),
            model: "m".into(),
            models: vec![],
            key_cipher: None,
            context_length: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            thinking: None,
            thinking_style: None,
            capabilities: Default::default(),
            capabilities_user_set: false,
        };
        // 输入栏 🧠 菜单发的档位要能持久化（P8.3 期间被 put_settings 吞掉的老问题）
        for lvl in ["low", "medium", "high", "on", "off"] {
            c.thinking = Some(lvl.into());
            let p = build_chat_params(&c, gateway::providers::find("openai"));
            assert_eq!(p.thinking.as_deref(), Some(lvl));
        }
    }

    #[test]
    fn attachment_save_and_decode() {
        let att = MediaAttachment {
            name: "test.mp3".into(),
            mime: "audio/mp3".into(),
            data: "data:audio/mp3;base64,//uQxAAAAAAAAAAAAAAAAAAAAAA==".into(),
        };
        assert_eq!(att.raw_base64(), "//uQxAAAAAAAAAAAAAAAAAAAAAA==");
        use base64::Engine;
        let clean = att.raw_base64().trim().replace(['\r', '\n', ' '], "");
        let unpadded = clean.trim_end_matches('=');
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&clean)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(unpadded))
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(unpadded))
            .unwrap();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn thinking_style_manual_override_end_to_end() {
        // A1（施工单 012）验收路径：custom 中转渠道手动指定 zai 形态 → 商汤式网关 on/off 生效
        let _ = gateway::providers::init(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("providers.json"),
        );
        let mut s = test_settings();
        let req = SettingsUpdate {
            thinking_style: Some("zai".into()),
            ..Default::default()
        };
        apply_settings_update(&mut s, &req);
        assert_eq!(s.active().unwrap().thinking_style.as_deref(), Some("zai"));
        let mut p = build_chat_params(s.active().unwrap(), gateway::providers::find("custom"));
        // 端点探测对中转域名给 openai 兜底；显式覆盖必须赢
        p.thinking = Some("on".into());
        let body = gateway::build_body(
            "deepseek-v4-flash",
            &json!([{ "role": "user", "content": "hi" }]),
            true,
            &p,
            None,
        );
        assert_eq!(body["thinking"], json!({"type":"enabled"}), "覆盖 zai 后 on 发 thinking 对象");
        p.thinking = Some("off".into());
        let body = gateway::build_body(
            "deepseek-v4-flash",
            &json!([{ "role": "user", "content": "hi" }]),
            true,
            &p,
            None,
        );
        assert_eq!(body["thinking"], json!({"type":"disabled"}), "覆盖 zai 后 off 生效");
        // auto/空串 = 清除显式覆盖，回到端点探测
        let req = SettingsUpdate {
            thinking_style: Some("auto".into()),
            ..Default::default()
        };
        apply_settings_update(&mut s, &req);
        assert!(s.active().unwrap().thinking_style.is_none(), "auto=清除覆盖");
    }

    #[test]
    fn capabilities_user_mark_survives_model_switch() {
        // 能力标记归用户所有：手动勾选后，任何模型切换都不得改写（推导机制已移除）
        let mut s = test_settings();
        // 模拟用户勾选「图片解析」→ capabilities_user_set 置位
        let req = SettingsUpdate {
            capabilities: Some(crate::config::settings::ModelCapabilities {
                tools: true,
                vision: true,
                video: false,
                audio: false,
            }),
            ..Default::default()
        };
        apply_settings_update(&mut s, &req);
        assert!(s.active().unwrap().capabilities_user_set);
        assert!(s.active().unwrap().capabilities.vision);
        // 切到别的模型：手动标记原样保留
        let req = SettingsUpdate {
            model: Some("qwen3.8-flash".into()),
            ..Default::default()
        };
        apply_settings_update(&mut s, &req);
        assert!(
            s.active().unwrap().capabilities.vision,
            "用户手动标记的 vision 不得被模型切换冲掉"
        );
        assert!(s.active().unwrap().capabilities_user_set);
        // 从未被标记过的配置切模型：能力标记同样保持不变（不再按模型名推导）
        let mut s2 = test_settings();
        let req = SettingsUpdate {
            model: Some("qwen-vl-max".into()),
            ..Default::default()
        };
        apply_settings_update(&mut s2, &req);
        let c = s2.active().unwrap();
        assert!(
            !c.capabilities.vision,
            "无用户标记时切模型也不再推导，保持默认（仅工具调用）"
        );
        assert!(c.capabilities.tools, "默认工具调用能力不受影响");
    }

    #[test]
    fn capabilities_tools_gate_wires_to_params() {
        // A3（施工单 012）：勾掉「工具调用」→ ChatParams.tools_enabled=false → 请求体无 tools
        let mut s = test_settings();
        let req = SettingsUpdate {
            capabilities: Some(crate::config::settings::ModelCapabilities {
                tools: false,
                vision: false,
                video: false,
                audio: false,
            }),
            ..Default::default()
        };
        apply_settings_update(&mut s, &req);
        let p = build_chat_params(s.active().unwrap(), gateway::providers::find("custom"));
        assert!(!p.tools_enabled);
        let body = gateway::build_body(
            "m",
            &json!([{ "role": "user", "content": "hi" }]),
            true,
            &p,
            None,
        );
        assert!(body.get("tools").is_none(), "tools=false 时请求体不得携带 tools");
        // 对照：默认（不勾掉）→ tools_enabled=true，agent 循环携带 tools
        let p = build_chat_params(&test_config(), gateway::providers::find("custom"));
        assert!(p.tools_enabled);
    }

    #[test]
    fn attachments_dir_appended_to_gitignore() {
        // A5（施工单 012）：附件目录自动进工作区 git 排除（只追加，不重写用户文件）
        let dir = std::env::temp_dir().join(format!("agent-a5-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let gi = dir.join(".gitignore");
        std::fs::write(&gi, "target/\nlogs/").unwrap(); // 故意不带结尾换行
        ensure_attachments_ignored(&dir);
        assert_eq!(
            std::fs::read_to_string(&gi).unwrap(),
            "target/\nlogs/\n.attachments/\n",
            "只追加一行并补齐换行"
        );
        // 幂等：再跑一次不重复追加
        ensure_attachments_ignored(&dir);
        let text = std::fs::read_to_string(&gi).unwrap();
        assert_eq!(text.matches(".attachments/").count(), 1, "不得重复追加");
        // 非 git 仓库不动
        let dir2 = std::env::temp_dir().join(format!("agent-a5-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir2).unwrap();
        ensure_attachments_ignored(&dir2);
        assert!(!dir2.join(".gitignore").exists(), "非 git 仓库不落 .gitignore");
    }
}
