//! MCP OAuth——远程 MCP 服务登录。架构照 ZCode `adapters/src/mcp/oauth-*` 移植：
//! discovery（protected-resource → authorization-server well-known）→ DCR 动态注册（无 client_id 时）
//! → PKCE(S256) + loopback 回调 → 授权码换 token → DPAPI 加密落盘 → Bearer 挂载 + 近过期刷新。
//!
//! 与 ZCode 的差异（本产品形态决定）：
//! - 授权事务在后台线程跑，HTTP 端点只发起 + 返回授权 URL，状态由 GET /api/mcp 轮询带回
//!   （ZCode 是 caller 等待 + 300s 事务寿命双轨）；
//! - token 存独立文件 `mcp_tokens.json`（DPAPI 逐条加密、原子写），不占 settings.json 写入路径；
//! - 环境变量 `PAWCH_AGENT_MCP_OAUTH_NO_BROWSER=1`（旧名 `AGENT_MCP_OAUTH_NO_BROWSER`
//!   兼容一个版本）时不调系统浏览器（无头/自动化验证用），
//!   授权 URL 由 tx_status 带回，调用方自行完成「浏览器那一步」。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::settings::{McpOAuthCfg, McpServerCfg, Settings};
use crate::observability;

/// 授权事务寿命（ZCode 同款 300s：真人点完同意平均 ~74s，留足余量）。
const TX_TTL: Duration = Duration::from_secs(300);
/// 距过期多久就刷新（ZCode 同款 60s 缓冲）。
const REFRESH_SKEW: i64 = 60;

// ———————————————— token 存取（DPAPI + 原子写） ————————————————

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub refresh_token: String,
    /// unix 秒；0=渠道未告知过期时间
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope: String,
}

impl McpTokens {
    /// 是否临近过期（该刷了）。
    pub fn near_expiry(&self) -> bool {
        self.expires_at > 0 && self.expires_at - now_unix() <= REFRESH_SKEW
    }
}

fn now_unix() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn tokens_path() -> PathBuf {
    crate::config::app_root().join("mcp_tokens.json")
}

fn token_cache() -> &'static Mutex<HashMap<String, McpTokens>> {
    static C: OnceLock<Mutex<HashMap<String, McpTokens>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 从文件加载全部 token（密文 DPAPI 解密；坏条目跳过，不阻塞）。
fn load_all() -> HashMap<String, McpTokens> {
    let path = tokens_path();
    let Ok(text) = std::fs::read_to_string(&path) else { return HashMap::new() };
    let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&text) else { return HashMap::new() };
    map.into_iter()
        .filter_map(|(k, cipher)| Settings::decrypt(&cipher).map(|json| (k, json)))
        .filter_map(|(k, json)| serde_json::from_str::<McpTokens>(&json).ok().map(|t| (k, t)))
        .collect()
}

/// 原子写回（逐条 DPAPI 加密；先临时文件再 rename）。
fn save_all(map: &HashMap<String, McpTokens>) -> Result<(), String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for (k, t) in map {
        let json = serde_json::to_string(t).map_err(|e| e.to_string())?;
        out.insert(k.clone(), Settings::protect(&json).map_err(|e| e.to_string())?);
    }
    let text = serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?;
    let path = tokens_path();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("写 token 文件失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("替换 token 文件失败: {e}"))?;
    Ok(())
}

/// 取某 server 的 token（内存缓存优先，未命中读盘）。
pub fn token_for(name: &str) -> Option<McpTokens> {
    if let Ok(c) = token_cache().lock()
        && let Some(t) = c.get(name)
    {
        return Some(t.clone());
    }
    let t = load_all().remove(name);
    if let Some(t) = &t
        && let Ok(mut c) = token_cache().lock()
    {
        c.insert(name.to_string(), t.clone());
    }
    t
}

/// 存/更新 token（内存 + 落盘）。
pub fn store_tokens(name: &str, tokens: McpTokens) {
    let snapshot = {
        let mut c = token_cache().lock().unwrap_or_else(|e| e.into_inner());
        c.insert(name.to_string(), tokens);
        c.clone()
    };
    // N4：落盘失败不许静默吞——token 只在内存里，重启后「已授权」凭空消失且无迹可查
    if let Err(e) = save_all(&snapshot) {
        observability::event(
            observability::EventType::Error,
            json!({ "kind": "mcp.token_persist_failed", "server": name, "error": format!("{e:#}") }),
        );
    }
}

/// 撤销授权（删 token；下次调用回到「未授权」人话错误）。
pub fn revoke(name: &str) {
    let snapshot = {
        let mut c = token_cache().lock().unwrap_or_else(|e| e.into_inner());
        c.remove(name);
        c.clone()
    };
    // N4：落盘失败不许静默吞——token 只在内存里，重启后「已授权」凭空消失且无迹可查
    if let Err(e) = save_all(&snapshot) {
        observability::event(
            observability::EventType::Error,
            json!({ "kind": "mcp.token_persist_failed", "server": name, "error": format!("{e:#}") }),
        );
    }
}

/// 授权状态（GET /api/mcp 展示 + 前端徽章）。
pub fn status_for(name: &str) -> Value {
    match token_for(name) {
        None => json!({ "state": "none" }),
        Some(t) => {
            let state = if t.near_expiry() { "expiring" } else { "authorized" };
            json!({ "state": state, "expires_at": t.expires_at })
        }
    }
}

// ———————————————— 授权事务（后台线程 + 状态快照） ————————————————

#[derive(Debug, Clone, Default)]
struct Tx {
    /// idle | pending | authorized | error
    status: String,
    message: String,
    authorization_url: String,
}

fn tx_map() -> &'static Mutex<HashMap<String, Tx>> {
    static T: OnceLock<Mutex<HashMap<String, Tx>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 授权事务状态快照（含授权 URL——无浏览器模式下由前端展示给用户点）。
pub fn tx_status(name: &str) -> Value {
    let tx = tx_map().lock().ok().and_then(|m| m.get(name).cloned()).unwrap_or_default();
    json!({
        "status": if tx.status.is_empty() { "idle".to_string() } else { tx.status.clone() },
        "message": tx.message,
        "authorization_url": tx.authorization_url,
        "oauth": status_for(name),
    })
}

fn tx_set(name: &str, status: &str, message: &str, url: &str) {
    if let Ok(mut m) = tx_map().lock() {
        let e = m.entry(name.to_string()).or_default();
        e.status = status.to_string();
        e.message = message.to_string();
        e.authorization_url = url.to_string();
    }
}

/// 发起授权（后台线程跑完整流程，立即返回）。授权 URL 会尝试调系统浏览器打开
/// （PAWCH_AGENT_MCP_OAUTH_NO_BROWSER=1 时跳过，改由 tx_status 带回给前端展示）。
pub fn start_authorize(name: &str, cfg: &McpServerCfg) -> Result<Value, String> {
    let Some(oauth) = cfg.oauth.clone() else {
        return Err("该 MCP 服务未配置 OAuth".into());
    };
    let base_url = cfg.url.trim().to_string();
    if base_url.is_empty() {
        return Err("http transport 需要先填服务 URL".into());
    }
    let name = name.to_string();
    tx_set(&name, "pending", "授权进行中：请在浏览器里完成登录", "");
    let name_thread = name.clone();
    std::thread::spawn(move || {
        let result = match oauth.r#type.as_str() {
            "client_credentials" => run_client_credentials(&base_url, &oauth),
            _ => run_authorization_code(&name_thread, &base_url, &oauth),
        };
        match result {
            Ok(tokens) => {
                store_tokens(&name_thread, tokens);
                tx_set(&name_thread, "authorized", "授权成功", "");
                observability::event(
                    observability::EventType::State,
                    json!({ "kind": "mcp.oauth.authorized", "server": name_thread }),
                );
            }
            Err(e) => {
                tx_set(&name_thread, "error", &e, "");
                observability::event(
                    observability::EventType::Error,
                    json!({ "kind": "mcp.oauth.failed", "server": name_thread, "error": e }),
                );
            }
        }
    });
    // 等一小会儿拿到授权 URL（授权码流程在开浏览器之前就能构造出来）
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(50));
        let tx = tx_map().lock().ok().and_then(|m| m.get(&name).cloned()).unwrap_or_default();
        if !tx.authorization_url.is_empty() || tx.status != "pending" {
            break;
        }
    }
    Ok(tx_status(&name))
}

// ———————————————— 授权码流程 ————————————————

fn http_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("建 HTTP 客户端失败: {e}"))
}

/// protected-resource → authorization-server 两级 discovery（MCP 规范 + RFC 8414）。
fn discovery(base_url: &str) -> Result<Value, String> {
    let client = http_client()?;
    let origin = origin_of(base_url)?;
    // 1) MCP 规范的 resource metadata：授权服务器可能与资源不同源
    let mut auth_servers: Vec<String> = Vec::new();
    if let Ok(resp) = client
        .get(format!("{origin}/.well-known/oauth-protected-resource"))
        .header("Accept", "application/json")
        .send()
        && let Ok(doc) = resp.json::<Value>()
        && let Some(arr) = doc.get("authorization_servers").and_then(Value::as_array)
    {
        for s in arr.iter().filter_map(Value::as_str) {
            auth_servers.push(s.trim_end_matches('/').to_string());
        }
    }
    if auth_servers.is_empty() {
        auth_servers.push(origin.trim_end_matches('/').to_string());
    }
    // 2) 授权服务器元数据（RFC 8414 + OIDC 双路径）
    let auth_server = &auth_servers[0];
    for path in ["/.well-known/oauth-authorization-server", "/.well-known/openid-configuration"] {
        if let Ok(resp) = client
            .get(format!("{auth_server}{path}"))
            .header("Accept", "application/json")
            .send()
            && let Ok(doc) = resp.json::<Value>()
            && doc.get("authorization_endpoint").is_some()
        {
            return Ok(doc);
        }
    }
    Err(format!("授权服务器 {auth_server} 未提供 OAuth 元数据（discovery 失败）"))
}

fn origin_of(url: &str) -> Result<String, String> {
    let u = url.trim();
    for scheme in ["https://", "http://"] {
        if let Some(rest) = u.strip_prefix(scheme) {
            let host = rest.split('/').next().unwrap_or("");
            if host.is_empty() {
                return Err("url 缺少主机".into());
            }
            return Ok(format!("{scheme}{host}"));
        }
    }
    Err("url 需要 http(s):// 开头".into())
}

/// DCR 动态注册（无预置 client_id 时；服务器不支持就明说，不瞎猜）。
fn dynamic_register(registration_endpoint: &str, redirect_uri: &str, scope: &str) -> Result<String, String> {
    let client = http_client()?;
    let mut body = json!({
        "client_name": "agent（本地编码助手）",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    if !scope.is_empty() {
        body["scope"] = json!(scope);
    }
    let resp = client
        .post(registration_endpoint)
        .json(&body)
        .send()
        .map_err(|e| format!("动态注册请求失败: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "授权服务器不支持动态注册（HTTP {}）：请在配置里预置 client_id",
            resp.status().as_u16()
        ));
    }
    resp.json::<Value>()
        .ok()
        .and_then(|d| d.get("client_id").and_then(Value::as_str).map(String::from))
        .ok_or_else(|| "动态注册响应里没有 client_id".into())
}

/// loopback 回调服务器：127.0.0.1:0 自选端口，等一个带 code/state 的 GET。
/// 返回 (port, receiver)；TTL 到点或 state 不符 → receiver 收到 Err。
fn spawn_callback_server(path: &str, state: &str) -> Result<(u16, Receiver<Result<String, String>>), String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("起回调监听失败: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let path = path.to_string();
    let state = state.to_string();
    let (tx, rx): (Sender<Result<String, String>>, _) = channel();
    std::thread::spawn(move || {
        let _ = listener.set_nonblocking(true);
        // TTL 轮询等连接：非阻塞 accept + 25ms 步进，到点收线程（不留孤儿监听）
        let deadline = std::time::Instant::now() + TX_TTL;
        let accepted = loop {
            match listener.accept() {
                Ok((s, _)) => break Some(s),
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => break None,
            }
        };
        let Some(mut stream) = accepted else {
            let _ = tx.send(Err("授权超时（300 秒内未完成）".into()));
            return;
        };
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]).to_string();
        let line = req.lines().next().unwrap_or("");
        let target = line.split_whitespace().nth(1).unwrap_or("");
        let (got_path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (target.to_string(), String::new()),
        };
        let params: HashMap<String, String> = query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if got_path != path {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            let _ = tx.send(Err("回调路径不匹配（授权服务器重定向到了别的路径）".into()));
            return;
        }
        let ok = params.get("state").map(String::as_str) == Some(state.as_str());
        let body = if ok {
            "<html><meta charset='utf-8'><body style='font-family:sans-serif;padding:40px'><h2>授权成功</h2><p>可以关闭这个页面，回到 agent。</p></body></html>"
        } else {
            "<html><meta charset='utf-8'><body style='font-family:sans-serif;padding:40px'><h2>授权失败</h2><p>state 不匹配，请重试。</p></body></html>"
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
        let outcome = if !ok {
            Err("state 不匹配（可能不是本次授权）".into())
        } else if let Some(err) = params.get("error") {
            Err(format!("授权服务器拒绝: {err}"))
        } else {
            let code = params.get("code").cloned().unwrap_or_default();
            if code.is_empty() {
                Err("回调里没有授权码".into())
            } else {
                Ok(code)
            }
        };
        let _ = tx.send(outcome);
    });
    Ok((port, rx))
}

/// 打开系统浏览器（默认浏览器；与「网页面板 → 在默认浏览器中打开」同款 explorer 转发）。
fn open_system_browser(url: &str) {
    if crate::config::env_pawch("MCP_OAUTH_NO_BROWSER").is_some() {
        return;
    }
    let _ = std::process::Command::new("explorer")
        .arg(url)
        .creation_flags(0x0800_0000)
        .spawn();
}

fn run_authorization_code(name: &str, base_url: &str, oauth: &McpOAuthCfg) -> Result<McpTokens, String> {
    let doc = discovery(base_url)?;
    let auth_ep = doc
        .get("authorization_endpoint")
        .and_then(Value::as_str)
        .ok_or("授权服务器元数据缺 authorization_endpoint")?
        .to_string();
    let token_ep = doc
        .get("token_endpoint")
        .and_then(Value::as_str)
        .ok_or("授权服务器元数据缺 token_endpoint")?
        .to_string();

    let redirect_path = if oauth.redirect_path.trim().is_empty() {
        "/callback".to_string()
    } else {
        oauth.redirect_path.trim().to_string()
    };
    let (verifier, challenge) = pkce_pair();
    let state = random_urlsafe(24);
    let (port, rx) = spawn_callback_server(&redirect_path, &state)?;
    let redirect_uri = format!("http://127.0.0.1:{port}{redirect_path}");

    let client_id = if oauth.client_id.trim().is_empty() {
        let reg_ep = doc
            .get("registration_endpoint")
            .and_then(Value::as_str)
            .ok_or("授权服务器不支持动态注册且未配置 client_id")?;
        dynamic_register(reg_ep, &redirect_uri, &oauth.scope)?
    } else {
        oauth.client_id.trim().to_string()
    };

    let mut url = format!(
        "{auth_ep}?response_type=code&client_id={client_id}&redirect_uri={}&state={state}&code_challenge={challenge}&code_challenge_method=S256",
        urlencoded(&redirect_uri),
    );
    if !oauth.scope.trim().is_empty() {
        url.push_str(&format!("&scope={}", urlencoded(oauth.scope.trim())));
    }
    url.push_str(&format!("&resource={}", urlencoded(base_url)));
    open_system_browser(&url);
    tx_set(name, "pending", "授权进行中：请在浏览器里完成登录", &url);

    let code = rx.recv().map_err(|e| format!("授权回调失败: {e}"))??;
    exchange_code(&token_ep, &client_id, oauth.client_secret.trim(), &code, &verifier, &redirect_uri, base_url)
}

fn exchange_code(
    token_ep: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    base_url: &str,
) -> Result<McpTokens, String> {
    let client = http_client()?;
    let mut form: Vec<(String, String)> = vec![
        ("grant_type".into(), "authorization_code".into()),
        ("code".into(), code.to_string()),
        ("code_verifier".into(), verifier.to_string()),
        ("redirect_uri".into(), redirect_uri.to_string()),
        ("client_id".into(), client_id.to_string()),
    ];
    if !client_secret.is_empty() {
        form.push(("client_secret".into(), client_secret.to_string()));
    }
    form.push(("resource".into(), base_url.to_string()));
    token_request(&client, token_ep, form)
}

fn run_client_credentials(base_url: &str, oauth: &McpOAuthCfg) -> Result<McpTokens, String> {
    let doc = discovery(base_url)?;
    let token_ep = doc
        .get("token_endpoint")
        .and_then(Value::as_str)
        .ok_or("授权服务器元数据缺 token_endpoint")?
        .to_string();
    if oauth.client_id.trim().is_empty() || oauth.client_secret.trim().is_empty() {
        return Err("client_credentials 需要 client_id 与 client_secret".into());
    }
    let client = http_client()?;
    let mut form: Vec<(String, String)> = vec![
        ("grant_type".into(), "client_credentials".into()),
        ("client_id".into(), oauth.client_id.trim().to_string()),
        ("client_secret".into(), oauth.client_secret.trim().to_string()),
    ];
    if !oauth.scope.trim().is_empty() {
        form.push(("scope".into(), oauth.scope.trim().to_string()));
    }
    form.push(("resource".into(), base_url.to_string()));
    token_request(&client, &token_ep, form)
}

fn token_request(client: &reqwest::blocking::Client, token_ep: &str, form: Vec<(String, String)>) -> Result<McpTokens, String> {
    let resp = client
        .post(token_ep)
        .form(&form)
        .send()
        .map_err(|e| format!("token 请求失败: {e}"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        let snip: String = body.chars().take(300).collect();
        return Err(format!("token 端点返回 HTTP {}: {snip}", status.as_u16()));
    }
    let doc: Value = serde_json::from_str(&body).map_err(|e| format!("token 响应不是 JSON: {e}"))?;
    let access_token = doc
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("token 响应缺 access_token")?
        .to_string();
    let expires_at = doc
        .get("expires_in")
        .and_then(Value::as_i64)
        .map(|s| now_unix() + s)
        .unwrap_or(0);
    Ok(McpTokens {
        access_token,
        refresh_token: doc.get("refresh_token").and_then(Value::as_str).unwrap_or("").to_string(),
        expires_at,
        scope: doc.get("scope").and_then(Value::as_str).unwrap_or("").to_string(),
    })
}

/// 近过期则用 refresh_token 续期（ZCode 同款：过期前 60s 主动刷）。
pub fn refresh_if_needed(name: &str, base_url: &str, oauth: &McpOAuthCfg, tokens: &McpTokens) -> Result<McpTokens, String> {
    if !tokens.near_expiry() || tokens.refresh_token.is_empty() {
        return Ok(tokens.clone());
    }
    let doc = discovery(base_url)?;
    let token_ep = doc
        .get("token_endpoint")
        .and_then(Value::as_str)
        .ok_or("授权服务器元数据缺 token_endpoint")?
        .to_string();
    let client = http_client()?;
    let form: Vec<(String, String)> = vec![
        ("grant_type".into(), "refresh_token".into()),
        ("refresh_token".into(), tokens.refresh_token.clone()),
        ("client_id".into(), oauth.client_id.trim().to_string()),
        ("resource".into(), base_url.to_string()),
    ];
    match token_request(&client, &token_ep, form) {
        Ok(mut fresh) => {
            // 部分授权服务器刷新不回 refresh_token：保留旧的
            if fresh.refresh_token.is_empty() {
                fresh.refresh_token = tokens.refresh_token.clone();
            }
            store_tokens(name, fresh.clone());
            Ok(fresh)
        }
        Err(e) => {
            // 刷新失败不静默：撤销旧 token，让下次调用回「未授权」人话错误
            revoke(name);
            Err(format!("token 刷新失败（{e}），请重新授权"))
        }
    }
}

// ———————————————— 工具函数 ————————————————

/// PKCE 对：verifier（64 位 url-safe 随机）+ challenge（S256(verifier) 的 base64url）。
pub fn pkce_pair() -> (String, String) {
    let verifier = random_urlsafe(64);
    let digest = sha256(verifier.as_bytes());
    (verifier, base64_url_nopad(&digest))
}

fn random_urlsafe(n: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64_url_nopad(&bytes).chars().take(n).collect()
}

fn base64_url_nopad(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

fn urlencoded(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_pair_is_s256() {
        let (verifier, challenge) = pkce_pair();
        assert_eq!(verifier.len(), 64);
        // challenge = base64url(sha256(verifier))
        let digest = sha256(verifier.as_bytes());
        assert_eq!(challenge, base64_url_nopad(&digest));
        assert!(!challenge.contains('='), "base64url 无填充");
        // 随机性：两次不同
        assert_ne!(pkce_pair().0, verifier);
    }

    #[test]
    fn origin_of_strips_path() {
        assert_eq!(origin_of("https://host.example.com/mcp").unwrap(), "https://host.example.com");
        assert_eq!(origin_of("http://127.0.0.1:9399/mcp").unwrap(), "http://127.0.0.1:9399");
        assert!(origin_of("host/mcp").is_err());
    }

    #[test]
    fn tokens_near_expiry_window() {
        let now = now_unix();
        let t = McpTokens { expires_at: now + 30, ..Default::default() };
        assert!(t.near_expiry(), "30s 内过期该刷了");
        let t = McpTokens { expires_at: now + 600, ..Default::default() };
        assert!(!t.near_expiry());
        let t = McpTokens { expires_at: 0, ..Default::default() };
        assert!(!t.near_expiry(), "渠道没说过期时间就不刷");
    }

    #[tokio::test]
    async fn callback_server_receives_code() {
        let state = "st_abc123";
        let (port, rx) = spawn_callback_server("/callback", state).unwrap();
        // 模拟浏览器回调（等价授权服务器 302 到 redirect_uri）。
        // 必须 no_proxy：gateway 测试会进程级改 HTTPS_PROXY 指向死代理，默认客户端
        // 会把环回请求也灌进去，并发一撞车就 ConnectionRefused（审计 03）。
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://127.0.0.1:{port}/callback?code=the_code&state={state}");
        let body = client.get(&url).send().await.unwrap().text().await.unwrap();
        assert!(body.contains("授权成功"), "回调页应提示成功");
        let code = rx.recv().unwrap().unwrap();
        assert_eq!(code, "the_code");
    }

    #[tokio::test]
    async fn callback_server_rejects_bad_state() {
        let (port, rx) = spawn_callback_server("/callback", "st_right").unwrap();
        // 同上：no_proxy 直连环回，不吃进程级代理环境变量
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://127.0.0.1:{port}/callback?code=x&state=st_wrong");
        let _ = client.get(&url).send().await.unwrap().text().await.unwrap();
        assert!(rx.recv().unwrap().is_err(), "state 不匹配必须拒绝");
    }
}
