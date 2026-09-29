//! HTTP 层（P19 巨石拆分：路由域子模块见同目录各文件）
//!
//! 本文件只留全局件：AppState / router / 来源校验与缓存中间件 / SSE 事件封装 / 内置冒烟页。
//! 路由域端点在 chat / gate / settings / workspace / sessions / memory / ext / git / system。

//! HTTP 层 —— 施工单 001 + 模型设置系统
//!
//! - 静态页托管（Svelte dist，未构建时内置页回退）
//! - /api/providers     供应商预设列表
//! - GET/PUT /api/settings  设置读写（key 用 DPAPI 加密持久化）
//! - /api/models        按当前 base_url+key 拉取渠道模型列表
//! - /api/chat          按已保存设置流式对话
//! - /cancel /api/cancel 取消标志（会话级：?session=<id> 只停该会话；不带 = 停全部）

pub(crate) use std::{convert::Infallible, path::PathBuf, pin::Pin, sync::Arc};

pub(crate) use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query, Request, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE, HOST, ORIGIN},
    },
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{delete, get, post},
};
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use serde_json::{Value, json};
pub(crate) use tokio::sync::mpsc;
pub(crate) use tokio_stream::wrappers::UnboundedReceiverStream;
pub(crate) use tower_http::services::{ServeDir, ServeFile};

pub(crate) use crate::{
    config::settings::Settings,
    core::cancel::{CancelFlag, CancelRegistry},
    gateway::{self, Gateway},
    observability::{self, EventType},
};

pub(crate) use crate::config::AgentConfig;
pub(crate) use crate::core::agent::{self, AgentEvent, LoopOutcome};

mod chat;
mod ext;
mod gate;
mod git;
mod headless;
pub(crate) mod memory;
mod schedule;
mod sessions;
mod settings;
mod subagent;
mod system;
mod usage;
mod webpanel;
mod workspace;

use chat::*;
use ext::*;
use gate::*;
use git::*;
use headless::*;
use memory::*;
use schedule::*;
use sessions::*;
use settings::*;
use system::*;
use usage::*;
use webpanel::*;
use workspace::*;

// P22-A 无头会话的进程内入口：定时任务（P22-B）不走 HTTP 直接唤醒
pub(crate) use headless::{HeadlessRequest, run_headless_inner};


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
    /// P22-C 子代理并行度（config.toml [subagent]：max_concurrent / per_parent）
    pub subagent_cfg: Arc<crate::config::SubagentConfig>,
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
        // P22-A 无头会话：程序化唤醒一条会话跑完整轮，结构化返回（子代理/定时/CLI 的共同底座）
        .route("/api/headless/run", post(run_headless))
        // P22-B 定时任务：清单（含下次运行/通知）/ 新建改 / 删 / 立即运行
        .route(
            "/api/schedules",
            get(list_schedules).post(save_schedule).delete(delete_schedule),
        )
        .route("/api/schedules/run", post(run_schedule_now))
        .route("/api/schedules/cron-text", get(cron_text))
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
        .route("/api/plugins", get(list_plugins))
        .route("/api/plugins/toggle", post(toggle_plugin))
        // P18 契约 v1（027）：模型自产插件 draft 隔离区 + 一键转正 + 丢弃
        .route("/api/plugins/draft", post(submit_draft).delete(discard_draft))
        .route("/api/plugins/promote", post(promote_draft))
        // P21 红线3：第三方插件验签（pin/unpin 内容指纹）
        .route("/api/plugins/trust", post(trust_plugin))
        // P21 成本台账：按供应商/模型的 token、次数、费用报表 + 价格表与重算
        .route("/api/usage", get(get_usage))
        .route("/api/usage/prices", get(get_usage_prices).put(put_usage_prices))
        .route("/api/usage/recompute", post(post_usage_recompute))
        // P18 存储一门（027）：插件命名空间的结构化读写（内核保留区不在任何插件写路径上）
        .route("/api/plugin/data", get(read_plugin_data).put(write_plugin_data).delete(delete_plugin_data))
        .route("/api/workspace/file", post(write_workspace_file))
        // 组件总线 v0：扩展静态资源（路径式路由，扩展内相对 URL 天然可用）
        // 尾斜杠那条约等于目录首页：不带它请求会掉进 SPA 兜底，把整个应用套进 iframe
        .route("/plugins/{name}", get(plugin_entry))
        .route("/plugins/{name}/", get(plugin_entry))
        .route("/plugins/{name}/{*path}", get(plugin_asset))
        .route("/cancel", post(cancel))
        .route("/api/cancel", get(cancel_status))
        .route("/api/sandbox/selftest", get(sandbox_selftest))
        .route("/api/sandbox/info", get(sandbox_info))
        // P20.1 网页面板：多标签管理 + 截图渲染 + 坐标回传（与 agent 浏览器工具共享实例）
        // P21：帧流（screencast）替代逐次截图轮询；activate 上报可见页驱动驻留
        .route("/api/browser/state", get(browser_state))
        .route("/api/browser/new", post(browser_new))
        .route("/api/browser/navigate", post(browser_navigate))
        .route("/api/browser/screenshot", get(browser_screenshot))
        .route("/api/browser/frame", get(browser_frame))
        .route("/api/browser/activate", post(browser_activate))
        .route("/api/browser/dialog", post(browser_dialog))
        .route("/api/browser/click", post(browser_click))
        .route("/api/browser/type", post(browser_type))
        .route("/api/browser/key", post(browser_key))
        .route("/api/browser/scroll", post(browser_scroll))
        .route("/api/browser/back", post(browser_back))
        .route("/api/browser/forward", post(browser_forward))
        .route("/api/browser/reload", post(browser_reload))
        .route("/api/browser/viewport", post(browser_viewport))
        .route("/api/browser/overlay", post(browser_overlay))
        .route("/api/browser/open-external", post(browser_open_external))
        .route("/api/browser/devtools", post(browser_devtools))
        .route("/api/browser/tab", delete(browser_close_tab))
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

pub(super) fn json_event(payload: Value) -> Event {
    Event::default().json_data(payload).expect("SSE 事件序列化失败")
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

}
