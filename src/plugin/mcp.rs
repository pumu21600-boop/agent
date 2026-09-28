//! MCP client（施工单 016/021，P11/P15）—— stdio 与 streamable HTTP 双 transport
//!
//! 总架构 §7：MCP 借生态省自研；进程级隔离天然防崩；tool poisoning 用
//! 「首装确认 + 同级审批」对冲（确认前一律 Dangerous）。
//!
//! 协议：JSON-RPC 2.0，只做 tools 子集：
//! initialize → notifications/initialized → tools/list → tools/call。
//!
//! - **stdio**（016）：换行分隔，子进程 stdin/stdout。
//! - **http**（021）：MCP Streamable HTTP —— POST JSON-RPC 到 url，响应可以是
//!   `application/json` 单包，也可以是 `text/event-stream`（读 `data:` 帧、按 id 取
//!   匹配帧）；`Mcp-Session-Id` 响应头拿到后在同 server 的后续请求里回带；
//!   通知（无 id）只校验 2xx/202 不看响应体。
//!
//! 线程模型：每个 server 串行 Mutex（工具调用本就串行）；两种 transport 都是阻塞
//! IO，整体调用包 tokio timeout（超时放弃等待，残留响应由「id 不匹配即跳过」自愈）。
//! 子进程 kill_on_drop，崩溃只标记离线，绝不拖垮主服务。

use std::{
    collections::{BTreeMap, HashMap},
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Mutex, OnceLock},
    time::Duration,
};

use serde_json::{json, Value};

use crate::config::settings::McpServerCfg;
use crate::plugin::ToolBrief;

/// 工具全名前缀（mcp__<server>__<tool>）。
pub const PREFIX: &str = "mcp__";
/// server 与 tool 的分隔（server 名禁止包含它，配置层校验）。
pub const SEP: &str = "__";

/// 该 server 是否走 HTTP transport。
fn is_http(cfg: &McpServerCfg) -> bool {
    cfg.transport.eq_ignore_ascii_case("http")
}

/// HTTP transport 运行态（stdio 不用）。
#[derive(Debug, Clone)]
struct HttpState {
    client: reqwest::blocking::Client,
    /// initialize 响应头 `Mcp-Session-Id`；有则后续请求回带
    session_id: Option<String>,
}

/// 单个 server 的运行态。
#[derive(Debug, Default)]
struct ServerState {
    cfg: Option<McpServerCfg>,
    /// tools/list 缓存：(工具名, 描述, inputSchema)
    tools: Vec<(String, String, Value)>,
    /// resources/list 缓存：(uri, name, mimeType)
    resources: Vec<(String, String, String)>,
    /// prompts/list 缓存：(name, description)
    prompts: Vec<(String, String)>,
    /// initialize 声明的能力：(resources, prompts)
    caps: (bool, bool),
    online: bool,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    reader: Option<BufReader<ChildStdout>>,
    /// HTTP transport 态（Some 即代表该 server 走 http）
    http: Option<HttpState>,
    next_id: u64,
}

impl ServerState {
    /// 断开连接（两种 transport 通用）：杀子进程 / 丢 HTTP 会话，标记离线。
    fn reset_proc(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
        }
        self.child = None;
        self.stdin = None;
        self.reader = None;
        self.http = None;
        self.online = false;
    }
}

static MANAGER: OnceLock<Mutex<HashMap<String, ServerState>>> = OnceLock::new();

fn mgr() -> &'static Mutex<HashMap<String, ServerState>> {
    MANAGER.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 从全名解析 (server, tool)；非 mcp__ 前缀返回 None。
pub fn split_full_name(full: &str) -> Option<(String, String)> {
    let rest = full.strip_prefix(PREFIX)?;
    let (srv, tool) = rest.split_once(SEP)?;
    if srv.is_empty() || tool.is_empty() {
        return None;
    }
    Some((srv.to_string(), tool.to_string()))
}

/// 强制某 server 下线（杀子进程/丢 HTTP 会话、清工具缓存）：撤销授权、配置大改后用。
/// 下次工具调用/refresh 时按需重连（无 token 的 OAuth 服务会拿到「需要授权」人话）。
pub fn force_offline(name: &str) {
    let mut m = mgr().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(st) = m.get_mut(name) {
        st.reset_proc();
        st.tools.clear();
    }
}

/// settings 加载/保存后同步配置：新增的占位、删除的杀进程下线。
/// 不在这里主动 spawn（首次工具调用/refresh 时按需启动）。
pub fn sync_configs(cfgs: &[McpServerCfg]) {
    let mut m = mgr().lock().unwrap_or_else(|e| e.into_inner());
    // 下线被删除/禁用的
    let names: Vec<String> = cfgs.iter().map(|c| c.name.clone()).collect();
    for (name, st) in m.iter_mut() {
        if !names.contains(name) {
            st.reset_proc();
            st.cfg = None;
            st.tools.clear();
        } else if let Some(c) = cfgs.iter().find(|c| &c.name == name) {
            let changed = st
                .cfg
                .as_ref()
                .map(|old| {
                    old.command != c.command
                        || old.args != c.args
                        || old.env != c.env
                        || old.transport != c.transport
                        || old.url != c.url
                        || old.headers != c.headers
                        || old.oauth != c.oauth
                })
                .unwrap_or(true);
            if changed {
                st.reset_proc(); // 命令/传输变了要重连
                st.tools.clear();
            }
            st.cfg = Some(c.clone());
        }
    }
    for c in cfgs {
        m.entry(c.name.clone()).or_default().cfg = Some(c.clone());
    }
}

/// 拉起（或重连）并握手（initialize → initialized → tools/list）。
/// 已在线则直接返回；失败断开并返回 Err（离线）。两种 transport 共用同一握手序列。
fn ensure_online(st: &mut ServerState) -> Result<(), String> {
    // 先收割已退出的子进程（崩溃自愈：标记离线，走下方重连路径）
    if let Some(child) = st.child.as_mut()
        && matches!(child.try_wait(), Ok(Some(_))) {
            st.reset_proc();
        }
    if st.online && (st.http.is_some() || (st.stdin.is_some() && st.reader.is_some())) {
        return Ok(());
    }
    st.reset_proc();
    let cfg = st.cfg.clone().ok_or("无配置")?;
    if is_http(&cfg) {
        let url = cfg.url.trim().to_string();
        if url.is_empty() {
            return Err("transport=http 但 url 为空".into());
        }
        let mut builder = reqwest::blocking::Client::builder()
            // 单请求超时由 http_round 逐次设置；这里给的是整体上限兜底
            .timeout(Duration::from_secs(300));
        // 环回地址的 MCP server 不经代理：环境里配了 http_proxy 时（挂着代理/VPN 很常见），
        // 代理多半连不到 127.0.0.1，本机 server 会莫名其妙连不上。
        if is_loopback_url(&url) {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|e| format!("HTTP 客户端创建失败: {e}"))?;
        st.http = Some(HttpState { client, session_id: None });
    } else {
        spawn_stdio(st, &cfg)?;
    }

    // —— 握手：initialize ——
    let init = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "agent-local", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    let init_resp = rpc_round(st, &init, Some(0), 15_000).map_err(|e| {
        st.reset_proc();
        format!("initialize 失败: {e}")
    })?;
    // 能力声明：决定要不要暴露 resources / prompts 相关工具（拿不到就按不支持处理）
    let caps = init_resp.get("capabilities").cloned().unwrap_or_else(|| json!({}));
    st.caps = (caps.get("resources").is_some(), caps.get("prompts").is_some());
    // initialized 通知（无 id，无响应体）
    let _ = rpc_round(
        st,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        None,
        15_000,
    );
    // —— tools/list ——
    let list_req = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} });
    let resp = rpc_round(st, &list_req, Some(1), 15_000).map_err(|e| {
        st.reset_proc();
        format!("tools/list 失败: {e}")
    })?;
    let tools = resp
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    st.tools = tools
        .iter()
        .filter_map(|t| {
            let name = t.get("name").and_then(Value::as_str)?.to_string();
            let desc = t.get("description").and_then(Value::as_str).unwrap_or("").to_string();
            let schema = t.get("inputSchema").cloned().unwrap_or_else(|| json!({"type":"object"}));
            Some((name, desc, schema))
        })
        .collect();
    // —— resources/list、prompts/list（声明了能力才拉；失败不影响上线，按空清单处理）——
    if st.caps.0 {
        let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "resources/list", "params": {} });
        if let Ok(resp) = rpc_round(st, &req, Some(2), 10_000) {
            st.resources = parse_resource_list(&resp);
        }
    }
    if st.caps.1 {
        let req = json!({ "jsonrpc": "2.0", "id": 3, "method": "prompts/list", "params": {} });
        if let Ok(resp) = rpc_round(st, &req, Some(3), 10_000) {
            st.prompts = parse_prompt_list(&resp);
        }
    }
    st.online = true;
    Ok(())
}

/// resources/list 响应 → (uri, name, mimeType)。
fn parse_resource_list(resp: &Value) -> Vec<(String, String, String)> {
    resp.get("resources")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let uri = r.get("uri").and_then(Value::as_str)?.to_string();
                    let name = r.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let mime = r.get("mimeType").and_then(Value::as_str).unwrap_or("").to_string();
                    Some((uri, name, mime))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// prompts/list 响应 → (name, description)。
fn parse_prompt_list(resp: &Value) -> Vec<(String, String)> {
    resp.get("prompts")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    let name = p.get("name").and_then(Value::as_str)?.to_string();
                    let desc = p.get("description").and_then(Value::as_str).unwrap_or("").to_string();
                    Some((name, desc))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// URL 主机是不是环回（127.x / localhost / [::1]）——决定要不要绕过环境代理。
fn is_loopback_url(url: &str) -> bool {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host_port = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(h) = host_port.strip_prefix('[') {
        h.split(']').next().unwrap_or("") // IPv6 字面量
    } else {
        host_port.rsplit_once(':').map(|(h, _)| h).unwrap_or(host_port)
    };
    let h = host.trim().to_ascii_lowercase();
    h == "localhost" || h == "::1" || h.starts_with("127.")
}

/// stdio transport：起子进程并把 stdin/stdout 挂到运行态。
fn spawn_stdio(st: &mut ServerState, cfg: &McpServerCfg) -> Result<(), String> {
    if cfg.command.trim().is_empty() {
        return Err("command 为空".into());
    }
    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args)
        // 密钥不进子进程（与 P3 沙箱同一红线）：MCP server 不继承主服务环境，
        // 白名单重建 + 用户在 server 配置里显式填的 env 才注入。
        .env_clear()
        .envs(sanitize_env(&cfg.env))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null()); // server 的日志走 stderr，丢弃避免干扰（Windows 句柄由 Child drop 关闭）
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：后台服务不起黑窗
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动 {} 失败: {e}", cfg.command))?;
    let stdin = child.stdin.take().ok_or("无 stdin")?;
    let stdout = child.stdout.take().ok_or("无 stdout")?;
    st.stdin = Some(stdin);
    st.reader = Some(BufReader::new(stdout));
    st.child = Some(child);
    Ok(())
}

/// 一轮 JSON-RPC：按 transport 分发。
/// `expect_id = None` 表示通知（无响应体，只确认送达）。
fn rpc_round(
    st: &mut ServerState,
    req: &Value,
    expect_id: Option<u64>,
    timeout_ms: u64,
) -> Result<Value, String> {
    if st.http.is_some() {
        return http_round(st, req, expect_id, timeout_ms);
    }
    let line = format!("{}\n", serde_json::to_string(req).map_err(|e| e.to_string())?);
    // 写与读分两段拿走借用，避免同时持有 stdin/reader 的可变借用
    let stdin = st.stdin.as_mut().ok_or("进程未启动")?;
    stdin.write_all(line.as_bytes()).map_err(|e| format!("写 stdin: {e}"))?;
    stdin.flush().map_err(|e| format!("flush stdin: {e}"))?;
    let Some(expect_id) = expect_id else {
        return Ok(Value::Null); // 通知：stdio 侧无需等待
    };

    let reader = st.reader.as_mut().ok_or("进程未启动")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if std::time::Instant::now() > deadline {
            return Err(format!("超时（{timeout_ms}ms）"));
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return Err("server 已退出（EOF）".into()),
            Ok(_) => {}
            Err(e) => return Err(format!("读 stdout: {e}")),
        }
        let v: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue, // 非 JSON 行跳过
        };
        let id = v.get("id").and_then(Value::as_u64);
        if id != Some(expect_id) {
            continue; // notification / 日志 / 串扰行跳过（自愈）
        }
        if let Some(err) = v.get("error") {
            return Err(format!("server 错误: {err}"));
        }
        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
    }
}

/// HTTP（MCP Streamable HTTP）：POST JSON-RPC。响应可以是 application/json 单包，
/// 也可以是 text/event-stream（读 `data:` 帧、取 id 匹配的那一帧）。
/// 会话头 `Mcp-Session-Id` 一旦从响应里拿到，同 server 后续请求回带。
/// 传输层失败（连不上/非 2xx/解析不出）标记离线 → 下次调用重连重握手；协议层错误
/// （JSON-RPC error）不标记离线，调用方能看见 server 的真实报错。
fn http_round(
    st: &mut ServerState,
    req: &Value,
    expect_id: Option<u64>,
    timeout_ms: u64,
) -> Result<Value, String> {
    let cfg = st.cfg.clone().ok_or("无配置")?;
    let url = cfg.url.trim().to_string();
    let (client, session_id) = {
        let h = st.http.as_ref().ok_or("HTTP 会话未建立")?;
        (h.client.clone(), h.session_id.clone())
    };
    let mut rb = client
        .post(&url)
        .timeout(Duration::from_millis(timeout_ms))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(req);
    if let Some(sid) = &session_id {
        rb = rb.header("Mcp-Session-Id", sid.as_str());
    }
    for (k, v) in &cfg.headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    // P24 OAuth：配置了 oauth 的远端服务，Bearer 从 token 存取取（近过期自动刷新）
    if let Some(oauth) = &cfg.oauth {
        match crate::plugin::mcp_oauth::token_for(&cfg.name) {
            Some(t) => {
                let t = crate::plugin::mcp_oauth::refresh_if_needed(&cfg.name, &url, oauth, &t)
                    .map_err(|e| format!("MCP 授权已过期且刷新失败：{e}"))?;
                rb = rb.header("Authorization", format!("Bearer {}", t.access_token));
            }
            None => {
                return Err(format!(
                    "MCP 服务「{}」需要授权：设置 → 插件 → MCP → 点「授权」完成登录",
                    cfg.name
                ));
            }
        }
    }
    let resp = rb
        .send()
        .map_err(|e| offline(st, format!("HTTP 请求失败（{url}）: {e}")))?;
    // 会话头：initialize 响应里带，之后每次都要回带（规范 2025-03-26）
    if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok())
        && let Some(h) = st.http.as_mut()
    {
        h.session_id = Some(sid.to_string());
    }
    let status = resp.status();
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !status.is_success() {
        let body = resp.text().unwrap_or_default();
        let snip: String = body.chars().take(300).collect();
        // P24：OAuth 服务吃 401/403——token 过期/被撤销，人话指路重新授权
        if matches!(status.as_u16(), 401 | 403) && cfg.oauth.is_some() {
            return Err(offline(
                st,
                format!(
                    "MCP 服务「{}」授权被拒绝（HTTP {}）：token 可能已过期或被撤销，请到设置 → 插件 → MCP 点「授权」重新登录",
                    cfg.name,
                    status.as_u16()
                ),
            ));
        }
        return Err(offline(st, format!("HTTP {}: {snip}", status.as_u16())));
    }
    let Some(expect_id) = expect_id else {
        return Ok(Value::Null); // 通知：2xx/202 即视为已送达
    };
    let body = resp.text().map_err(|e| offline(st, format!("读响应体失败: {e}")))?;
    let frames: Vec<Value> = if ctype.contains("text/event-stream") {
        parse_sse_frames(&body)
    } else if body.trim().is_empty() {
        Vec::new()
    } else {
        vec![serde_json::from_str(body.trim())
            .map_err(|e| offline(st, format!("响应不是合法 JSON: {e}")))?]
    };
    for v in frames {
        if v.get("id").and_then(Value::as_u64) != Some(expect_id) {
            continue; // 心跳/进度通知/别的响应帧
        }
        if let Some(err) = v.get("error") {
            return Err(format!("server 错误: {err}"));
        }
        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
    }
    Err(offline(st, format!("响应里没有 id={expect_id} 的结果帧")))
}

/// 传输层失败统一出口：标记离线（下次 ensure_online 重连重握手）并回原错误串。
fn offline(st: &mut ServerState, msg: String) -> String {
    st.online = false;
    msg
}

/// 解析 SSE 响应体：把 `data:` 帧拼回 JSON 值（多行 data 以换行连接）。
/// 注释行（`:` 开头）、心跳空帧、非 JSON 帧一律跳过——HTTP MCP server 普遍夹这些。
/// 纯函数（可单测）：不碰网络。
fn parse_sse_frames(body: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut Vec<Value>| {
        let t = buf.trim();
        if !t.is_empty()
            && let Ok(v) = serde_json::from_str::<Value>(t)
        {
            out.push(v);
        }
        buf.clear();
    };
    for raw in body.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            flush(&mut buf, &mut out); // 空行 = 帧结束
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(rest.trim_start());
        }
        // 其余字段（event:/id:/retry:/注释）v0 不需要
    }
    flush(&mut buf, &mut out); // 末尾无空行也收一帧
    out
}

/// 公开：拉起（或重连）一个 server 并刷新工具清单。
pub fn ensure_online_by_name(name: &str) -> Result<usize, String> {
    let mut m = mgr().lock().unwrap_or_else(|e| e.into_inner());
    let st = m.get_mut(name).ok_or("未知 server")?;
    ensure_online(st)?;
    Ok(st.tools.len())
}

/// 公开：server 状态快照（UI 用）。
pub fn status() -> Vec<(String, bool, Vec<ToolBrief>)> {
    let mut m = mgr().lock().unwrap_or_else(|e| e.into_inner());
    // 先回收已退出的进程：否则 server 崩了没人调用的话，界面会长期显示「在线」假状态
    reap_dead_locked(&mut m);
    m.iter()
        .map(|(name, st)| {
            (
                name.clone(),
                st.online,
                st.tools.iter().map(|(n, d, _)| (n.clone(), d.clone())).collect(),
            )
        })
        .collect()
}

/// 审批档判定（rules.rs mcp__ 分支调用）。
/// 未确认 → Dangerous；禁用 → Dangerous；确认后按 server 配置档。
pub fn risk_for(full_name: &str) -> Option<u8> {
    let (srv, _tool) = split_full_name(full_name)?;
    let m = mgr().lock().unwrap_or_else(|e| e.into_inner());
    let st = m.get(&srv)?;
    let cfg = st.cfg.as_ref()?;
    if !cfg.enabled || !cfg.confirmed {
        return Some(2); // Dangerous：未确认/禁用的 MCP 工具不可静默执行
    }
    Some(match cfg.risk.as_str() {
        "dangerous" => 2,
        _ => 1, // normal
    })
}

/// 资源/提示词相关的合成工具名（server 自己若已有同名工具，以它为准，不覆盖）。
const SYNTH_TOOLS: [&str; 4] = ["list_resources", "read_resource", "list_prompts", "get_prompt"];

/// 合成工具的 function 定义（reads resources / prompts）。
fn synth_def(srv: &str, name: &str) -> Value {
    let (desc, params) = match name {
        "list_resources" => (
            "列出这个 MCP 服务暴露的资源（URI 清单）。要读正文用 read_resource。",
            json!({"type":"object","properties":{}}),
        ),
        "read_resource" => (
            "读取一个 MCP 资源的正文（uri 来自 list_resources）。",
            json!({"type":"object","properties":{"uri":{"type":"string","description":"资源 URI"}},"required":["uri"]}),
        ),
        "list_prompts" => (
            "列出这个 MCP 服务提供的提示词模板（名称 + 说明）。要取模板用 get_prompt。",
            json!({"type":"object","properties":{}}),
        ),
        "get_prompt" => (
            "取一个 MCP 提示词模板（可带参数），返回渲染后的提示词正文。",
            json!({"type":"object","properties":{
                "name":{"type":"string","description":"模板名（来自 list_prompts）"},
                "arguments":{"type":"object","description":"模板参数（可选，键值均为字符串）"}
            },"required":["name"]}),
        ),
        _ => unreachable!("未知合成工具"),
    };
    json!({
        "type": "function",
        "function": { "name": format!("{PREFIX}{srv}{SEP}{name}"), "description": desc, "parameters": params }
    })
}

/// 已启用 + 已确认 + 在线工具的 OpenAI function 定义（含声明了 resources/prompts 时的合成工具）。
pub fn tool_definitions() -> Vec<Value> {
    let m = mgr().lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    for (srv, st) in m.iter() {
        if !st.online {
            continue;
        }
        let Some(cfg) = st.cfg.as_ref() else { continue };
        if !cfg.enabled || !cfg.confirmed {
            continue;
        }
        for (name, desc, schema) in &st.tools {
            out.push(json!({
                "type": "function",
                "function": {
                    "name": format!("{PREFIX}{srv}{SEP}{name}"),
                    "description": if desc.is_empty() { format!("MCP 工具（{srv}）") } else { desc.clone() },
                    "parameters": schema,
                }
            }));
        }
        let has_own = |n: &str| st.tools.iter().any(|(t, _, _)| t == n);
        let (want_res, want_prompts) = (st.caps.0, st.caps.1);
        for name in SYNTH_TOOLS {
            let wanted = match name {
                "list_resources" | "read_resource" => want_res,
                "list_prompts" | "get_prompt" => want_prompts,
                _ => false,
            };
            if wanted && !has_own(name) {
                out.push(synth_def(srv, name));
            }
        }
    }
    out
}

/// 公开：各 server 的资源/提示词清单（设置面板展示用）。
pub fn catalogs() -> std::collections::HashMap<String, (Vec<String>, Vec<String>)> {
    let m = mgr().lock().unwrap_or_else(|e| e.into_inner());
    m.iter()
        .map(|(name, st)| {
            let res: Vec<String> = st.resources.iter().map(|(uri, n, _)| if n.is_empty() { uri.clone() } else { format!("{n} ({uri})") }).collect();
            let prompts: Vec<String> = st.prompts.iter().map(|(n, _)| n.clone()).collect();
            (name.clone(), (res, prompts))
        })
        .collect()
}

/// 执行 MCP 工具调用（tools::execute 对 mcp__ 前缀分发到这里）。
pub async fn call_tool(full_name: &str, args_json: &str) -> Result<String, String> {
    let (srv, tool) = split_full_name(full_name).ok_or("非法 MCP 工具名")?;
    let args: Value = if args_json.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(args_json).map_err(|e| format!("参数不是合法 JSON: {e}"))?
    };
    // 资源/提示词是客户端侧合成的工具，直接走本地实现（不进 tools/call）
    if SYNTH_TOOLS.contains(&tool.as_str()) {
        return call_resource_or_prompt(&srv, &tool, args).await;
    }

    // 阻塞 IO 丢进 blocking 线程池；外层 timeout 到点放弃（残留行由 id 过滤自愈）
    let full = full_name.to_string();
    let fut = tokio::task::spawn_blocking(move || {
        let mut m = mgr().lock().unwrap_or_else(|e| e.into_inner());
        let st = m.get_mut(&srv).ok_or_else(|| format!("未知 server: {srv}"))?;
        ensure_online(st)?;
        let req = json!({
            "jsonrpc": "2.0", "id": st.next_id, "method": "tools/call",
            "params": { "name": tool, "arguments": args }
        });
        let id = st.next_id;
        st.next_id += 1;
        let resp = rpc_round(st, &req, Some(id), 120_000)?;
        // 拼接 content 里全部 text 段
        let mut texts: Vec<String> = Vec::new();
        let mut is_err = false;
        if let Some(arr) = resp.get("content").and_then(Value::as_array) {
            for c in arr {
                if c.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = c.get("text").and_then(Value::as_str) {
                        texts.push(t.to_string());
                    }
            }
        }
        if resp.get("isError").and_then(Value::as_bool) == Some(true) {
            is_err = true;
        }
        if texts.is_empty() {
            texts.push(String::new());
        }
        let joined = texts.join("\n");
        if is_err {
            Err(format!("MCP 工具报告失败: {joined}"))
        } else {
            Ok(joined)
        }
    });
    match tokio::time::timeout(std::time::Duration::from_secs(125), fut).await {
        Ok(r) => r.map_err(|e| format!("MCP 调用任务失败: {e}"))?,
        Err(_) => Err("MCP 调用超时（120s）".into()),
    }
    .inspect(|_r| {
        let _ = &full;
    })
}

/// 进程存活自检：把已退出的 server 标记离线。调用方须已持有管理器锁（status() 内用）。
/// resources / prompts 合成工具的本地实现（阻塞 IO 交给 blocking 线程池，超时 60s）。
async fn call_resource_or_prompt(srv: &str, tool: &str, args: Value) -> Result<String, String> {
    let srv = srv.to_string();
    let tool = tool.to_string();
    let fut = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let mut m = mgr().lock().unwrap_or_else(|e| e.into_inner());
        let st = m.get_mut(&srv).ok_or_else(|| format!("未知 server: {srv}"))?;
        ensure_online(st)?;
        let rpc = |st: &mut ServerState, method: &str, params: Value| -> Result<Value, String> {
            let id = st.next_id;
            st.next_id += 1;
            let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
            rpc_round(st, &req, Some(id), 60_000)
        };
        match tool.as_str() {
            "list_resources" => {
                if st.resources.is_empty() {
                    let resp = rpc(st, "resources/list", json!({}))?;
                    st.resources = parse_resource_list(&resp);
                }
                if st.resources.is_empty() {
                    return Ok("（这个 MCP 服务没有暴露资源）".into());
                }
                let list: Vec<String> = st
                    .resources
                    .iter()
                    .map(|(uri, name, mime)| {
                        if mime.is_empty() { format!("{uri}\t{name}") } else { format!("{uri}\t{name}\t{mime}") }
                    })
                    .collect();
                Ok(format!("共 {} 个资源（uri\\t名称\\t类型）：\n{}", list.len(), list.join("\n")))
            }
            "read_resource" => {
                let uri = args.get("uri").and_then(Value::as_str).ok_or("缺少 uri")?;
                let resp = rpc(st, "resources/read", json!({ "uri": uri }))?;
                let mut out = String::new();
                for c in resp.get("contents").and_then(Value::as_array).cloned().unwrap_or_default() {
                    if let Some(t) = c.get("text").and_then(Value::as_str) {
                        out.push_str(t);
                        out.push('\n');
                    } else if let Some(b) = c.get("blob").and_then(Value::as_str) {
                        out.push_str(&format!(
                            "（二进制资源 {}：{} 字节 base64，未解码展示）\n",
                            c.get("uri").and_then(Value::as_str).unwrap_or(uri),
                            b.len()
                        ));
                    }
                }
                if out.trim().is_empty() {
                    return Err(format!("资源 {uri} 内容为空"));
                }
                Ok(out)
            }
            "list_prompts" => {
                if st.prompts.is_empty() {
                    let resp = rpc(st, "prompts/list", json!({}))?;
                    st.prompts = parse_prompt_list(&resp);
                }
                if st.prompts.is_empty() {
                    return Ok("（这个 MCP 服务没有提供提示词模板）".into());
                }
                let list: Vec<String> = st
                    .prompts
                    .iter()
                    .map(|(name, desc)| if desc.is_empty() { name.clone() } else { format!("{name}\t{desc}") })
                    .collect();
                Ok(format!("共 {} 个提示词模板：\n{}", list.len(), list.join("\n")))
            }
            "get_prompt" => {
                let name = args.get("name").and_then(Value::as_str).ok_or("缺少 name")?;
                let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let resp = rpc(st, "prompts/get", json!({ "name": name, "arguments": arguments }))?;
                let mut out = String::new();
                for msg in resp.get("messages").and_then(Value::as_array).cloned().unwrap_or_default() {
                    let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
                    let text = msg
                        .get("content")
                        .and_then(|c| c.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    out.push_str(&format!("[{role}] {text}\n"));
                }
                if out.trim().is_empty() {
                    return Err(format!("提示词 {name} 渲染结果为空"));
                }
                Ok(out)
            }
            other => Err(format!("未知合成工具: {other}")),
        }
    });
    match tokio::time::timeout(std::time::Duration::from_secs(65), fut).await {
        Ok(r) => r.map_err(|e| format!("MCP 调用任务失败: {e}"))?,
        Err(_) => Err("MCP 资源/提示词调用超时（60s）".into()),
    }
}

fn reap_dead_locked(m: &mut HashMap<String, ServerState>) {
    for st in m.values_mut() {
        if let Some(child) = st.child.as_mut()
            && let Ok(Some(_)) = child.try_wait() {
                st.reset_proc();
            }
    }
}

/// 校验 server 名（禁止包含分隔符）。
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(SEP) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Windows 子进程跑起来必需的最小环境集（PATH 让 npx/node/python 这类命令能找到自己）。
const ENV_WHITELIST: [&str; 13] = [
    "SystemRoot", "WINDIR", "COMSPEC", "PATH", "PATHEXT", "TEMP", "TMP", "USERPROFILE",
    "HOME", "APPDATA", "LOCALAPPDATA", "COMPUTERNAME", "USERNAME",
];

/// 白名单式子进程环境：只从主服务环境里挑上面那几项，再叠上用户在 server 配置里显式填的 env
/// （用户项优先，可覆盖 PATH）。主服务环境里的密钥（DEEPSEEK_API_KEY 等）绝不进 MCP 子进程。
pub fn sanitize_env(user: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    build_server_env(|k| std::env::var(k).ok(), user)
}

/// 纯逻辑（可单测）：parent 取值函数 + 用户显式 env → 子进程环境。
fn build_server_env(
    parent: impl Fn(&str) -> Option<String>,
    user: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for k in ENV_WHITELIST {
        if let Some(v) = parent(k) {
            out.insert(k.to_string(), v);
        }
    }
    for (k, v) in user {
        out.insert(k.clone(), v.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// MCP 管理器是进程级单例、`sync_configs` 整体替换配置——碰管理器的测试必须串行，
    /// 否则并行跑会互相把对方刚同步的配置清掉（表现为偶发「无配置」）。
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
        L.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn server_env_drops_secrets_keeps_whitelist() {
        // 父环境模拟：既有必需的 PATH，也有密钥——密钥绝不能进子进程
        let parent = map(&[("PATH", "C:\\x"), ("DEEPSEEK_API_KEY", "sk-secret"), ("FOO", "bar")]);
        let out = build_server_env(|k| parent.get(k).cloned(), &BTreeMap::new());
        assert_eq!(out.get("PATH").map(String::as_str), Some("C:\\x"));
        assert!(!out.contains_key("DEEPSEEK_API_KEY"), "密钥泄漏进 MCP 子进程: {out:?}");
        assert!(!out.contains_key("FOO"));
    }

    #[test]
    fn server_env_user_entries_pass_through_and_override() {
        let parent = map(&[("PATH", "parent-path")]);
        let user = map(&[("PATH", "user-path"), ("API_TOKEN", "t-1")]);
        let out = build_server_env(|k| parent.get(k).cloned(), &user);
        // 用户显式配置优先
        assert_eq!(out.get("PATH").map(String::as_str), Some("user-path"));
        assert_eq!(out.get("API_TOKEN").map(String::as_str), Some("t-1"));
    }

    // ——— HTTP transport（021） ———

    #[test]
    fn sse_frames_parsed_and_junk_skipped() {
        // 真实 server 会在帧里夹注释、心跳空帧、event 名，且末尾可能没有空行
        let body = ": ping\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\ndata: not-json\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\n\
data: \"result\":{\"n\":7}}";
        let frames = parse_sse_frames(body);
        assert_eq!(frames.len(), 2, " комментарий/非 JSON 帧应被跳过: {frames:?}");
        assert_eq!(frames[0]["id"], 1);
        assert_eq!(frames[0]["result"]["ok"], true);
        // 多行 data 按换行拼接后仍是合法 JSON
        assert_eq!(frames[1]["id"], 2);
        assert_eq!(frames[1]["result"]["n"], 7);
        assert!(parse_sse_frames("").is_empty());
        assert!(parse_sse_frames("event: message\n").is_empty());
    }

    #[test]
    fn name_and_full_name_rules() {
        assert!(valid_name("fs-tools"));
        assert!(!valid_name("fs__tools"), "禁止包含分隔符");
        assert!(!valid_name(""));
        assert_eq!(split_full_name("mcp__srv__tool"), Some(("srv".into(), "tool".into())));
        assert_eq!(split_full_name("mcp__srv"), None);
        assert_eq!(split_full_name("wasm__x__y"), None);
    }

    /// 极简 HTTP/1.1 mock（只服务测试）：读请求头 + Content-Length 体 → 交给闭包出响应。
    /// 闭包拿到 (path, headers, body)；返回 base url。线程随测试进程结束，不做优雅退出。
    fn mock_http(f: impl Fn(&str, &[(String, String)], &Value) -> MockResp + Send + 'static) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定随机端口");
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let req_body;
                let headers: Vec<(String, String)>;
                let path;
                {
                    use std::io::{BufRead, BufReader, Read};
                    let Ok(clone) = s.try_clone() else { continue };
                    let mut r = BufReader::new(clone);
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        continue;
                    }
                    path = line.split(' ').nth(1).unwrap_or("/").to_string();
                    let mut len = 0usize;
                    let mut hs: Vec<(String, String)> = Vec::new();
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).unwrap_or(0) == 0 {
                            break;
                        }
                        let t = h.trim_end().to_string();
                        if t.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = t.split_once(':') {
                            let k = k.trim().to_ascii_lowercase();
                            let v = v.trim().to_string();
                            if k == "content-length" {
                                len = v.parse().unwrap_or(0);
                            }
                            hs.push((k, v));
                        }
                    }
                    headers = hs;
                    let mut body = vec![0u8; len];
                    if len > 0 && r.read_exact(&mut body).is_err() {
                        continue;
                    }
                    req_body = serde_json::from_slice(&body).unwrap_or(Value::Null);
                }
                let r = f(&path, &headers, &req_body);
                use std::io::Write;
                let reason = if r.status == 200 { "OK" } else { "Accepted" };
                let mut head = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    r.status,
                    reason,
                    r.ctype,
                    r.body.len()
                );
                for (k, v) in &r.extra {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(r.body.as_bytes());
                let _ = s.flush();
            }
        });
        format!("http://{addr}/mcp")
    }

    struct MockResp {
        status: u16,
        ctype: &'static str,
        body: String,
        extra: Vec<(&'static str, String)>,
    }

    impl MockResp {
        /// SSE 帧响应（每帧 `event: message` + `data:`，另夹一个心跳注释帧）
        fn sse(frames: &[Value]) -> Self {
            let mut body = String::from(": keep-alive\n\n");
            for f in frames {
                body.push_str(&format!("event: message\ndata: {f}\n\n"));
            }
            Self { status: 200, ctype: "text/event-stream", body, extra: Vec::new() }
        }
        fn accepted() -> Self {
            Self { status: 202, ctype: "application/json", body: String::new(), extra: Vec::new() }
        }
    }

    /// HTTP transport 端到端：initialize（JSON + 会话头）→ initialized（202 通知）
    /// → tools/list（SSE 帧）→ tools/call（SSE 帧，夹心跳与串扰帧）。同时断言会话头被回带。
    #[test]
    fn http_transport_handshake_and_call() {
        let _guard = test_lock();
        // (method, 该请求带的 Mcp-Session-Id)
        let seen: std::sync::Arc<Mutex<Vec<(String, String)>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let rec = seen.clone();
        let url = mock_http(move |_path, headers, req| {
            let method = req.get("method").and_then(Value::as_str).unwrap_or("").to_string();
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            let sid = headers
                .iter()
                .find(|(k, _)| k == "mcp-session-id")
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| "-".into());
            rec.lock().unwrap_or_else(|e| e.into_inner()).push((method.clone(), sid));
            match method.as_str() {
                "initialize" => MockResp {
                    status: 200,
                    ctype: "application/json",
                    body: json!({ "jsonrpc": "2.0", "id": id, "result": { "protocolVersion": "2024-11-05" } }).to_string(),
                    extra: vec![("Mcp-Session-Id", "sess-1".to_string())],
                },
                "notifications/initialized" => MockResp::accepted(),
                "tools/list" => MockResp::sse(&[json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "tools": [
                        { "name": "echo", "description": "echo it", "inputSchema": { "type": "object" } },
                        { "name": "ping", "description": "ping", "inputSchema": { "type": "object" } }
                    ] }
                })]),
                "tools/call" => MockResp::sse(&[
                    // 串扰帧：id 对不上，必须被跳过
                    json!({ "jsonrpc": "2.0", "id": 999, "result": { "content": [{ "type": "text", "text": "wrong" }] } }),
                    json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": { "content": [{ "type": "text", "text": "echo:7" }] }
                    }),
                ]),
                _ => MockResp { status: 400, ctype: "text/plain", body: "unknown".into(), extra: Vec::new() },
            }
        });

        let cfg = McpServerCfg {
            name: "mockhttp".into(),
            transport: "http".into(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: url.clone(),
            headers: BTreeMap::from([("X-Test".to_string(), "1".to_string())]),
            oauth: None,
            enabled: true,
            confirmed: true,
            risk: "normal".into(),
        };
        sync_configs(std::slice::from_ref(&cfg));

        let n = ensure_online_by_name("mockhttp").expect("HTTP 握手应成功");
        assert_eq!(n, 2, "tools/list 应发现 2 个工具");
        assert_eq!(risk_for("mcp__mockhttp__echo"), Some(1), "已确认 server 默认 Normal 档");

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let out = rt
            .block_on(call_tool("mcp__mockhttp__echo", r#"{"v":7}"#))
            .expect("HTTP tools/call 应成功");
        assert_eq!(out, "echo:7", "应取 id 匹配的那一帧，忽略串扰帧");

        // 会话头回带：initialize 之后的每个请求都要带 Mcp-Session-Id: sess-1
        let seen = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let after_init: Vec<&(String, String)> = seen.iter().filter(|(m, _)| m != "initialize").collect();
        assert!(after_init.len() >= 3, "应有 initialized/tools/list/tools/call 记录: {seen:?}");
        assert!(
            after_init.iter().all(|(_, sid)| sid == "sess-1"),
            "initialize 之后的请求应全部回带会话头: {seen:?}"
        );
        let init_sid = seen.iter().find(|(m, _)| m == "initialize").map(|(_, s)| s.clone());
        assert_eq!(init_sid.as_deref(), Some("-"), "首个请求不该自带会话头");
    }

    /// resources / prompts（P17）：握手发现能力 → 合成工具可调用 → 清单进 catalogs（UI 展示）。
    #[test]
    fn http_resources_and_prompts_end_to_end() {
        let _guard = test_lock();
        let url = mock_http(|_path, _headers, req| {
            let method = req.get("method").and_then(Value::as_str).unwrap_or("");
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            match method {
                "initialize" => MockResp {
                    status: 200,
                    ctype: "application/json",
                    body: json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {
                            "protocolVersion": "2024-11-05",
                            "capabilities": { "tools": {}, "resources": { "subscribe": false }, "prompts": {} }
                        }
                    })
                    .to_string(),
                    extra: Vec::new(),
                },
                "notifications/initialized" => MockResp::accepted(),
                "tools/list" => MockResp::sse(&[json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "tools": [{ "name": "echo", "description": "echo", "inputSchema": { "type": "object" } }] }
                })]),
                "resources/list" => MockResp::sse(&[json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "resources": [
                        { "uri": "file:///notes/a.md", "name": "A 笔记", "mimeType": "text/markdown" },
                        { "uri": "file:///notes/b.md", "name": "B 笔记" }
                    ] }
                })]),
                "prompts/list" => MockResp::sse(&[json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "prompts": [{ "name": "review", "description": "代码评审" }] }
                })]),
                "resources/read" => MockResp::sse(&[json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "contents": [{ "uri": "file:///notes/a.md", "text": "笔记正文：状态 OK" }] }
                })]),
                "prompts/get" => MockResp::sse(&[json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "messages": [
                        { "role": "user", "content": { "type": "text", "text": "请评审：{{path}}" } }
                    ] }
                })]),
                other => MockResp { status: 400, ctype: "text/plain", body: format!("unknown {other}"), extra: Vec::new() },
            }
        });

        let cfg = McpServerCfg {
            name: "mockres".into(),
            transport: "http".into(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url,
            headers: BTreeMap::new(),
            oauth: None,
            enabled: true,
            confirmed: true,
            risk: "normal".into(),
        };
        sync_configs(std::slice::from_ref(&cfg));
        ensure_online_by_name("mockres").expect("握手应成功");

        // 1) 声明了能力 → 暴露 4 个合成工具；2) 清单进 catalogs（UI 用）
        let defs = tool_definitions();
        let names: Vec<String> = defs
            .iter()
            .filter_map(|d| d["function"]["name"].as_str().map(str::to_string))
            .collect();
        for want in [
            "mcp__mockres__list_resources",
            "mcp__mockres__read_resource",
            "mcp__mockres__list_prompts",
            "mcp__mockres__get_prompt",
        ] {
            assert!(names.contains(&want.to_string()), "缺少合成工具 {want}: {names:?}");
        }
        let cat = catalogs();
        let (res, prompts) = cat.get("mockres").cloned().expect("应有清单");
        assert_eq!(res.len(), 2, "{res:?}");
        assert!(res[0].contains("file:///notes/a.md"), "{res:?}");
        assert_eq!(prompts, vec!["review".to_string()]);

        // 3) 合成工具真能取到内容（走 resources/read 与 prompts/get）
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let text = rt
            .block_on(call_tool("mcp__mockres__read_resource", r#"{"uri":"file:///notes/a.md"}"#))
            .expect("read_resource 应成功");
        assert_eq!(text.trim(), "笔记正文：状态 OK");
        let listed = rt
            .block_on(call_tool("mcp__mockres__list_resources", "{}"))
            .expect("list_resources 应成功");
        assert!(listed.contains("file:///notes/b.md") && listed.contains("共 2 个资源"), "{listed}");
        let prompt = rt
            .block_on(call_tool("mcp__mockres__get_prompt", r#"{"name":"review"}"#))
            .expect("get_prompt 应成功");
        assert!(prompt.contains("[user] 请评审：{{path}}"), "{prompt}");
    }

    /// 没声明 resources/prompts 能力的 server 不该出现合成工具（避免给模型空能力）。
    #[test]
    fn synth_tools_hidden_without_capabilities() {
        let _guard = test_lock();
        let url = mock_http(|_path, _headers, req| {
            let method = req.get("method").and_then(Value::as_str).unwrap_or("");
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            match method {
                "initialize" => MockResp {
                    status: 200,
                    ctype: "application/json",
                    body: json!({ "jsonrpc": "2.0", "id": id, "result": { "capabilities": { "tools": {} } } }).to_string(),
                    extra: Vec::new(),
                },
                "notifications/initialized" => MockResp::accepted(),
                "tools/list" => MockResp::sse(&[json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }] }
                })]),
                other => MockResp { status: 400, ctype: "text/plain", body: format!("unknown {other}"), extra: Vec::new() },
            }
        });
        let cfg = McpServerCfg {
            name: "mockbare".into(),
            transport: "http".into(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url,
            headers: BTreeMap::new(),
            oauth: None,
            enabled: true,
            confirmed: true,
            risk: "normal".into(),
        };
        sync_configs(std::slice::from_ref(&cfg));
        ensure_online_by_name("mockbare").expect("握手应成功");
        let names: Vec<String> = tool_definitions()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str().map(str::to_string))
            .collect();
        assert!(names.contains(&"mcp__mockbare__echo".to_string()));
        assert!(
            !names.iter().any(|n| n.contains("list_resources") || n.contains("get_prompt")),
            "没声明能力不该有合成工具: {names:?}"
        );
    }

    /// 出错路径：URL 打不通 → 报错且标记离线（下次调用重连，而不是假装在线）。
    #[test]
    fn http_transport_failure_marks_offline() {
        let _guard = test_lock();
        let cfg = McpServerCfg {
            name: "deadhttp".into(),
            transport: "http".into(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: "http://127.0.0.1:1/mcp".into(),
            headers: BTreeMap::new(),
            oauth: None,
            enabled: true,
            confirmed: true,
            risk: "normal".into(),
        };
        sync_configs(std::slice::from_ref(&cfg));
        let e = ensure_online_by_name("deadhttp").expect_err("连不上必须报错");
        assert!(e.starts_with("initialize 失败"), "错误串应指明握手失败: {e}");
        let m = mgr().lock().unwrap_or_else(|e| e.into_inner());
        assert!(!m.get("deadhttp").unwrap().online, "失败后不得留在「在线」态");
    }
}
