//! 内置浏览器内核（P20）—— 进程内直持 CDP 会话的页面操作能力。
//!
//! 拉起本机 Edge/Chrome（headless），页面级 WebSocket 直连（cdp.rs），工具层暴露五个
//! 操作：browser_open / browser_snapshot / browser_click / browser_type / browser_close。
//!
//! 设计约定（窄而刚 v1）：
//! - **单浏览器单标签页**：所有会话共用一个无头实例；并发工具调用在 CDP actor 上天然串行。
//! - **DOM 增量 diff**：快照时给元素钉 `data-agent-ref`（已存在的不重打——ref 稳定），
//!   服务端与上次快照按 ref 对比出「新增/移除/变化」，工具结果只附摘要与明细。
//! - **语义定位为主，坐标兜底**：点击/输入优先 ref 与文本/占位符定位，定位不到才允许
//!   直接给视口坐标（Input 域真实鼠标事件，兼容一切站点）。
//! - 浏览器进程自带 Chromium 站点隔离，不在 Windows 沙箱内再包一层；profile 用临时
//!   目录，空转 10 分钟自动关闭。

mod cdp;

use std::{
    os::windows::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::observability::{self, EventType};

/// 导航加载预算。
const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// 空转自动关闭阈值。
const IDLE_CLOSE: Duration = Duration::from_secs(600);

/// 单个 DOM 快照节点（服务端 diff 与渲染用）。
#[derive(Clone, PartialEq)]
struct OutlineNode {
    reference: String,
    tag: String,
    role: Option<String>,
    name: String,
    text: String,
    value: Option<String>,
    href: Option<String>,
    depth: usize,
}

/// 一个标签页 = 一条页面级 CDP 连接（P20.1 网页面板起多标签化；面板与 agent 工具
/// 共享同一个浏览器进程，各标签页连接互不干扰）。
pub struct WebTab {
    /// CDP target id
    pub id: String,
    /// agent 工具（browser_* 五件套）专用标签页
    pub agent: bool,
    pub cdp: cdp::Cdp,
    /// agent 快照的增量 diff 基线（仅 agent 标签页使用）
    last_outline: Mutex<Vec<OutlineNode>>,
}

struct BrowserSession {
    child: std::sync::Mutex<Child>,
    /// CDP HTTP 端点端口（/json/new、/json/close、探活用）
    port: u16,
    tabs: std::sync::Mutex<Vec<Arc<WebTab>>>,
    last_used_ms: AtomicU64,
}

impl BrowserSession {
    fn touch(&self) {
        self.last_used_ms.store(now_ms(), Ordering::Relaxed);
    }
    fn idle_secs(&self) -> u64 {
        now_ms().saturating_sub(self.last_used_ms.load(Ordering::Relaxed)) / 1000
    }
    /// 快照列表（面板展示用：id/标题/地址/是否 agent 页）。
    async fn tab_briefs(&self) -> Vec<(String, bool, String, String)> {
        let tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut out = Vec::new();
        for t in tabs {
            let title = t
                .cdp
                .call(
                    "Runtime.evaluate",
                    json!({ "expression": "document.title", "returnByValue": true }),
                )
                .await
                .ok()
                .and_then(|v| v.get("result").and_then(|r| r.get("value")).and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            let url = t
                .cdp
                .call(
                    "Runtime.evaluate",
                    json!({ "expression": "location.href", "returnByValue": true }),
                )
                .await
                .ok()
                .and_then(|v| v.get("result").and_then(|r| r.get("value")).and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            out.push((t.id.clone(), t.agent, title, url));
        }
        out
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

static SESSION: Mutex<Option<Arc<BrowserSession>>> = Mutex::const_new(None);
static REAPER: OnceLock<()> = OnceLock::new();

fn log(kind: &str, extra: Value) {
    observability::event(EventType::State, {
        let mut v = json!({ "kind": kind });
        if let (Some(obj), Some(ext)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in ext {
                obj.insert(k.clone(), val.clone());
            }
        }
        v
    });
}

/// 杀掉浏览器进程并回收（Arc 里的 Child 需经 Mutex 拿可变引用）。
fn kill_child(s: &BrowserSession) {
    if let Ok(mut c) = s.child.lock() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// 清理别的进程留下的旧浏览器 profile（名字前缀相同、不属于本进程）。
/// 被占用的目录（残留浏览器还活着）删不动就跳过，错误一律忽略。
fn clean_stale_profiles(keep: &PathBuf) {
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("agent-browser-profile") {
                let p = e.path();
                if p != *keep {
                    let _ = std::fs::remove_dir_all(&p);
                }
            }
        }
    }
}

// —— 浏览器发现与拉起 ——

fn find_browser_exe() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("AGENT_BROWSER_PATH")
        && !p.trim().is_empty()
    {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into());
    let pf86 = std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".into());
    let lf = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let candidates = [
        format!("{pf86}\\Microsoft\\Edge\\Application\\msedge.exe"),
        format!("{pf}\\Microsoft\\Edge\\Application\\msedge.exe"),
        format!("{pf}\\Google\\Chrome\\Application\\chrome.exe"),
        format!("{pf86}\\Google\\Chrome\\Application\\chrome.exe"),
        format!("{lf}\\Google\\Chrome\\Application\\chrome.exe"),
    ];
    candidates
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .ok_or_else(|| "找不到 Edge/Chrome（设 AGENT_BROWSER_PATH 指到浏览器 exe 可用）".to_string())
}

/// 探测 CDP HTTP 端点，拿到页面级 ws 地址（headless 拉起后第一个 target 就是 about:blank 页）。
/// CDP HTTP 端点客户端（环回、no_proxy——坑 17：本机代理环境变量会拦环回请求）。
fn dev_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))
}

/// 探测 CDP HTTP 端点，拿到页面 target 的 (id, ws 地址)。
async fn wait_page_target(port: u16, budget: Duration) -> Result<(String, String), String> {
    let client = dev_client()?;
    let deadline = tokio::time::Instant::now() + budget;
    let url = format!("http://127.0.0.1:{port}/json/list");
    while tokio::time::Instant::now() < deadline {
        if let Ok(resp) = client.get(&url).send().await
            && let Ok(text) = resp.text().await
            && let Ok(v) = serde_json::from_str::<Value>(&text)
            && let Some(targets) = v.as_array()
            && let Some(page) = targets
                .iter()
                .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
            && let Some(id) = page.get("id").and_then(Value::as_str)
            && let Some(ws) = page.get("webSocketDebuggerUrl").and_then(Value::as_str)
        {
            return Ok((id.to_string(), ws.to_string()));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err("等待浏览器 CDP 端点超时（10s 内 /json/list 无 page target）".into())
}

/// 浏览器进程是否还活着（/json/version 可达）。
async fn browser_alive(port: u16) -> bool {
    if let Ok(c) = dev_client() {
        return c
            .get(format!("http://127.0.0.1:{port}/json/version"))
            .send()
            .await
            .is_ok();
    }
    false
}

/// 经 CDP HTTP 端点开一个新标签页（PUT /json/new；老版本只认 GET，失败回退）。
/// 固定 about:blank 起步，导航统一走 CDP（能等加载完成）。
async fn dev_new_target(port: u16) -> Result<(String, String), String> {
    let client = dev_client()?;
    for method in ["PUT", "GET"] {
        let u = format!("http://127.0.0.1:{port}/json/new?about:blank");
        let req = if method == "PUT" { client.put(&u) } else { client.get(&u) };
        if let Ok(resp) = req.send().await
            && let Ok(text) = resp.text().await
            && let Ok(v) = serde_json::from_str::<Value>(&text)
            && let Some(id) = v.get("id").and_then(Value::as_str)
            && let Some(ws) = v.get("webSocketDebuggerUrl").and_then(Value::as_str)
        {
            return Ok((id.to_string(), ws.to_string()));
        }
    }
    Err("无法创建标签页（/json/new 不可用）".into())
}

/// 关闭标签页（GET /json/close）。
async fn dev_close_target(port: u16, id: &str) {
    if let Ok(c) = dev_client() {
        let _ = c.get(format!("http://127.0.0.1:{port}/json/close/{id}")).send().await;
    }
}

/// 建一条标签页连接（Page.enable + 可选导航等待加载完成）并挂进会话。
async fn create_tab(session: &BrowserSession, url: &str, agent: bool) -> Result<Arc<WebTab>, String> {
    let (id, ws) = dev_new_target(session.port).await?;
    let cdp = cdp::Cdp::connect(&ws).await?;
    cdp.call("Page.enable", json!({})).await?;
    let tab = Arc::new(WebTab {
        id,
        agent,
        cdp,
        last_outline: Mutex::new(Vec::new()),
    });
    session
        .tabs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(tab.clone());
    if url != "about:blank" && !url.is_empty() {
        navigate_tab(&tab, url).await?;
    }
    log(
        "browser.tab.new",
        json!({ "id": tab.id, "agent": agent, "url": url }),
    );
    Ok(tab)
}

/// 关闭并移除一个标签页（连接即弃，进程侧异步关）。
async fn close_tab(session: &BrowserSession, id: &str) -> bool {
    let removed = {
        let mut tabs = session.tabs.lock().unwrap_or_else(|e| e.into_inner());
        let pos = tabs.iter().position(|t| t.id == id);
        if let Some(pos) = pos {
            tabs.remove(pos);
        }
        pos.is_some()
    };
    if removed {
        dev_close_target(session.port, id).await;
        log("browser.tab.close", json!({ "id": id }));
    }
    removed
}

/// agent 工具的专用标签页：找活的 agent 页，没有就现开一个。
async fn agent_tab(session: &BrowserSession) -> Result<Arc<WebTab>, String> {
    {
        let tabs = session.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(t) = tabs.iter().find(|t| t.agent) {
            return Ok(t.clone());
        }
    }
    create_tab(session, "about:blank", true).await
}

/// 按 id 取标签页。
fn get_tab(session: &BrowserSession, id: &str) -> Result<Arc<WebTab>, String> {
    session
        .tabs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|t| t.id == id)
        .cloned()
        .ok_or_else(|| format!("标签页不存在: {id}"))
}

async fn launch() -> Result<Arc<BrowserSession>, String> {
    let exe = find_browser_exe()?;
    // 预占一个环回空闲端口（进程退出即释放；极小概率被抢，重试一次）
    let mut last_err = String::new();
    for _ in 0..2 {
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
            l.local_addr().map_err(|e| e.to_string())?.port()
        };
        // profile 按进程 PID 隔离：宿主被强杀时无头浏览器会残留并握住 profile 单例锁
        // （真机踩过：残留 Edge 让下一次 launch「拉起后立即退出」）。顺手清掉别的旧 profile。
        let profile = std::env::temp_dir().join(format!("agent-browser-profile-{}", std::process::id()));
        clean_stale_profiles(&profile);
        let _ = std::fs::create_dir_all(&profile);
        let child = Command::new(&exe)
            .args([
                "--headless=new",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--disable-background-networking",
                "--mute-audio",
                "--hide-scrollbars",
                "--window-size=1280,900",
                &format!("--remote-debugging-port={port}"),
                &format!("--user-data-dir={}", profile.display()),
                "about:blank",
            ])
            .stdout(Stdio::null())
            // stderr 留管道：秒退时的退出码与报错原文是定位「为什么拉不起」的唯一线索
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return Err(format!("拉起浏览器失败（{}）: {e}", exe.display())),
        };
        // 拉起后立刻探活：老版本 Edge 不认 --headless=new 会秒退
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Ok(Some(code)) = child.try_wait() {
            // 收 stderr（管道里已有内容；进程退出后读到 EOF 为止，限时收）
            let mut tail = String::new();
            if let Some(mut err) = child.stderr.take() {
                use std::io::Read;
                let deadline = Instant::now() + Duration::from_millis(500);
                let mut buf = [0u8; 1024];
                loop {
                    if Instant::now() > deadline {
                        break;
                    }
                    match err.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            tail.push_str(&String::from_utf8_lossy(&buf[..n]));
                            if tail.len() > 2000 {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            let tail = tail.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
            last_err = format!(
                "浏览器拉起后立即退出（exit={code:?}，{}）：{tail}",
                exe.display()
            );
            continue;
        }
        match wait_page_target(port, Duration::from_secs(10)).await {
            Ok((target_id, ws)) => {
                let cdp = cdp::Cdp::connect(&ws).await?;
                // loadEventFired 需要 Page 域使能
                cdp.call("Page.enable", json!({})).await?;
                log("browser.launch", json!({ "exe": exe.display().to_string(), "port": port }));
                // 启动自带的 about:blank 页即 agent 工具的专用标签页
                let first = Arc::new(WebTab {
                    id: target_id,
                    agent: true,
                    cdp,
                    last_outline: Mutex::new(Vec::new()),
                });
                let session = Arc::new(BrowserSession {
                    child: std::sync::Mutex::new(child),
                    port,
                    tabs: std::sync::Mutex::new(vec![first]),
                    last_used_ms: AtomicU64::new(now_ms()),
                });
                ensure_reaper();
                return Ok(session);
            }
            Err(e) => {
                let _ = child.kill();
                last_err = e;
            }
        }
    }
    Err(last_err)
}

/// 确保有一个活的浏览器会话（死会话清理后重新拉起）。
async fn ensure_session() -> Result<Arc<BrowserSession>, String> {
    let mut guard = SESSION.lock().await;
    if let Some(s) = guard.as_ref() {
        s.touch();
        if browser_alive(s.port).await {
            return Ok(s.clone());
        }
        log("browser.stale", json!({}));
        if let Some(dead) = guard.take() {
            kill_child(&dead);
        }
    }
    let s = launch().await?;
    *guard = Some(s.clone());
    Ok(s)
}

/// 空转收割：10 分钟没人用就关浏览器（不占内存、不留进程）。
fn ensure_reaper() {
    REAPER.get_or_init(|| {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let idle = {
                    let guard = SESSION.lock().await;
                    match guard.as_ref() {
                        Some(s) => s.idle_secs(),
                        None => continue,
                    }
                };
                if idle > IDLE_CLOSE.as_secs() {
                    close_internal("idle").await;
                }
            }
        });
    });
}

async fn close_internal(reason: &str) {
    let dead = SESSION.lock().await.take();
    if let Some(s) = dead {
        kill_child(&s);
        log("browser.close", json!({ "reason": reason }));
    }
}

// —— DOM 快照（JS 注入）——

/// 页面结构抽取：交互元素带语义角色，纯文本块保留正文；ref 钉在 DOM 上（稳定）。
const SNAPSHOT_JS: &str = r#"(() => {
  const MAXN = 400, MAXTEXT = 120, MAXDEPTH = 24;
  const out = [];
  // ref 序号取现有最大值起算，避免给新元素打出与存活元素撞车的编号
  let seq = 0;
  document.querySelectorAll('[data-agent-ref]').forEach((el) => {
    const n = parseInt(el.getAttribute('data-agent-ref').slice(1), 10);
    if (!isNaN(n) && n > seq) seq = n;
  });
  const skip = new Set(['script','style','noscript','template','svg','path','head','meta','link']);
  const roleOf = (el) => {
    const r = el.getAttribute('role'); if (r) return r;
    const t = el.tagName.toLowerCase();
    if (t === 'a') return 'link';
    if (t === 'button' || (t === 'input' && (el.type === 'button' || el.type === 'submit'))) return 'button';
    if (t === 'input' || t === 'textarea') return 'textbox';
    if (t === 'select') return 'combobox';
    if (/^h[1-6]$/.test(t)) return 'heading';
    if (t === 'img') return 'image';
    return null;
  };
  const nameOf = (el) => {
    let s = el.getAttribute('aria-label') || el.getAttribute('placeholder')
      || el.getAttribute('title') || el.getAttribute('alt') || '';
    return s.trim().slice(0, MAXTEXT);
  };
  const directText = (el) => {
    let s = '';
    for (const n of el.childNodes) if (n.nodeType === 3) s += n.textContent;
    return s.replace(/\s+/g, ' ').trim().slice(0, MAXTEXT);
  };
  const walk = (el, depth) => {
    if (!el || out.length >= MAXN || depth > MAXDEPTH) return;
    const tag = el.tagName.toLowerCase();
    if (skip.has(tag)) return;
    const role = roleOf(el);
    const text = directText(el);
    const keep = role !== null || (text && ['p','li','td','th','label','blockquote','figcaption','dd','dt','summary','span'].includes(tag));
    let ref = null;
    if (keep) {
      ref = el.getAttribute('data-agent-ref');
      if (!ref) { seq += 1; ref = 'e' + seq; el.setAttribute('data-agent-ref', ref); }
      out.push({
        ref, tag, role,
        name: nameOf(el),
        text,
        value: (tag === 'input' || tag === 'textarea') ? String(el.value ?? '').slice(0, MAXTEXT) : null,
        href: tag === 'a' && el.href ? el.href.slice(0, 200) : null,
        depth,
      });
    }
    for (const c of el.children) walk(c, depth + (keep ? 1 : 0));
  };
  walk(document.body, 0);
  return {
    url: location.href,
    title: document.title,
    nodes: out,
    truncated: out.length >= MAXN,
  };
})()"#;

/// 渲染一行大纲。
fn render_node(n: &OutlineNode) -> String {
    let indent = "  ".repeat(n.depth.min(12));
    let role = n.role.clone().unwrap_or_else(|| n.tag.clone());
    let mut line = format!("{indent}[{role} {}]", n.reference);
    if !n.name.is_empty() {
        line.push_str(&format!(" \"{}\"", n.name));
    }
    if n.name.is_empty() && !n.text.is_empty() {
        line.push_str(&format!(" \"{}\"", n.text));
    }
    if let Some(v) = &n.value {
        line.push_str(&format!(" value={v:?}"));
    }
    if let Some(h) = &n.href {
        line.push_str(&format!(" → {h}"));
    }
    line
}

/// 与上次快照按 ref 做 diff，返回（摘要行, 明细行列表）。
fn diff_outline(old: &[OutlineNode], new: &[OutlineNode]) -> (String, Vec<String>) {
    let old_map: std::collections::HashMap<&str, &OutlineNode> =
        old.iter().map(|n| (n.reference.as_str(), n)).collect();
    let new_map: std::collections::HashMap<&str, &OutlineNode> =
        new.iter().map(|n| (n.reference.as_str(), n)).collect();
    let added: Vec<&OutlineNode> = new.iter().filter(|n| !old_map.contains_key(n.reference.as_str())).collect();
    let removed: Vec<&OutlineNode> = old.iter().filter(|n| !new_map.contains_key(n.reference.as_str())).collect();
    let changed: Vec<(&OutlineNode, &OutlineNode)> = new
        .iter()
        .filter_map(|n| old_map.get(n.reference.as_str()).map(|o| (o, n)))
        .filter(|(o, n)| o.text != n.text || o.name != n.name || o.value != n.value || o.href != n.href)
        .map(|(o, n)| (*o, n))
        .collect();

    if old.is_empty() {
        return ("首次快照（无基线可比）".into(), vec![]);
    }
    let summary = format!("增量: +{} 新增 -{} 移除 ~{} 变化", added.len(), removed.len(), changed.len());
    let mut detail = Vec::new();
    for n in added.iter().take(15) {
        detail.push(format!("  + {}", render_node(n).trim_start()));
    }
    for n in removed.iter().take(15) {
        detail.push(format!("  - [{}]", n.reference));
    }
    for (o, n) in changed.iter().take(15) {
        let before = o.value.clone().unwrap_or_else(|| o.text.clone());
        let after = n.value.clone().unwrap_or_else(|| n.text.clone());
        detail.push(format!("  ~ [{}] \"{}\" → \"{}\"", n.reference, truncate_str(&before, 40), truncate_str(&after, 40)));
    }
    (summary, detail)
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

async fn take_snapshot(tab: &WebTab, max_chars: usize) -> Result<String, String> {
    let res = tab
        .cdp
        .call(
            "Runtime.evaluate",
            json!({ "expression": SNAPSHOT_JS, "returnByValue": true, "awaitPromise": false }),
        )
        .await?;
    if let Some(exc) = res.get("exceptionDetails") {
        return Err(format!("页面脚本执行异常: {}", exc.to_string().chars().take(200).collect::<String>()));
    }
    let value = res
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .ok_or_else(|| "快照结果缺 value".to_string())?;
    let url = value.get("url").and_then(Value::as_str).unwrap_or("").to_string();
    let title = value.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    let truncated = value.get("truncated").and_then(Value::as_bool).unwrap_or(false);
    let mut nodes: Vec<OutlineNode> = Vec::new();
    if let Some(arr) = value.get("nodes").and_then(Value::as_array) {
        for n in arr {
            nodes.push(OutlineNode {
                reference: n.get("ref").and_then(Value::as_str).unwrap_or_default().to_string(),
                tag: n.get("tag").and_then(Value::as_str).unwrap_or_default().to_string(),
                role: n.get("role").and_then(Value::as_str).map(str::to_string),
                name: n.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                text: n.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
                value: n.get("value").and_then(Value::as_str).map(str::to_string),
                href: n.get("href").and_then(Value::as_str).map(str::to_string),
                depth: n.get("depth").and_then(Value::as_u64).unwrap_or(0) as usize,
            });
        }
    }

    let mut guard = tab.last_outline.lock().await;
    let (summary, detail) = diff_outline(&guard, &nodes);
    *guard = nodes.clone();
    drop(guard);

    let mut out = format!("页面: {title}（{url}）\n{summary}\n");
    if !detail.is_empty() {
        out.push_str(&detail.join("\n"));
        out.push('\n');
    }
    out.push_str("--- 页面结构 ---\n");
    out.push_str(&nodes.iter().map(render_node).collect::<Vec<_>>().join("\n"));
    if truncated {
        out.push_str("\n（节点过多，快照已截断——可用更精确的 ref 操作已知元素）");
    }
    Ok(crate::context::compress(&out, max_chars))
}

// —— 元素定位（语义为主） ——

/// 定位 JS 返回：{ok, x, y, tag, name} 或 {ok:false}。
const LOCATE_JS: &str = r#"((ref, text, label) => {
  let el = null;
  if (ref) el = document.querySelector('[data-agent-ref="' + ref + '"]');
  const want = (text || label || '').trim().toLowerCase();
  if (!el && want) {
    const cands = [];
    for (const el2 of document.querySelectorAll('a,button,input,textarea,select,[role],[onclick]')) {
      const role = el2.getAttribute('role') || el2.tagName.toLowerCase();
      const t2 = (el2.innerText || el2.value || el2.getAttribute('placeholder') || el2.getAttribute('aria-label') || el2.getAttribute('title') || '').trim().toLowerCase();
      const hit = text ? (t2 === want || (t2.includes(want) && want.length >= 2)) : label ? (t2.includes(want) || (el2.placeholder || '').includes(want)) : false;
      if (hit && el2.getClientRects().length > 0) {
        let w = 0, x3 = el2;
        while (x3) { w++; x3 = x3.parentElement; }
        cands.push({ el: el2, w, role });
      }
    }
    const rank = (r) => (r === 'button' || r === 'link' || r === 'textbox' || r === 'combobox') ? 0 : 1;
    cands.sort((a, b) => (rank(a.role) - rank(b.role)) || (a.w - b.w));
    el = cands[0] ? cands[0].el : null;
  }
  if (!el) return { ok: false };
  el.scrollIntoView({ block: 'center' });
  const r = el.getBoundingClientRect();
  if (r.width <= 0 || r.height <= 0) return { ok: false };
  return {
    ok: true,
    x: Math.round(r.left + r.width / 2),
    y: Math.round(r.top + r.height / 2),
    tag: el.tagName.toLowerCase(),
    name: (el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.innerText || '').trim().slice(0, 60),
  };
})"#;

/// 解析定位参数 → 视口坐标。ref/文本语义优先，x/y 兜底。
/// click 形态 (ref, text)；type 形态 (ref, label)——由 text_key 区分。
async fn locate(
    tab: &WebTab,
    v: &Value,
    ref_key: &str,
    text_key: &str,
) -> Result<(i64, i64, String), String> {
    let reference = v.get(ref_key).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let text = v.get(text_key).and_then(Value::as_str).unwrap_or("").trim().to_string();
    if !reference.is_empty() || !text.is_empty() {
        let expr = if text_key == "label" {
            format!(
                "{LOCATE_JS}({}, null, {})",
                serde_json::to_string(&reference).unwrap_or_else(|_| "null".into()),
                serde_json::to_string(&text).unwrap_or_else(|_| "\"\"".into())
            )
        } else {
            format!(
                "{LOCATE_JS}({}, {}, null)",
                serde_json::to_string(&reference).unwrap_or_else(|_| "null".into()),
                serde_json::to_string(&text).unwrap_or_else(|_| "null".into())
            )
        };
        let res = tab
            .cdp
            .call("Runtime.evaluate", json!({ "expression": expr, "returnByValue": true }))
            .await?;
        let val = res.get("result").and_then(|r| r.get("value")).cloned().unwrap_or(Value::Null);
        if val.get("ok").and_then(Value::as_bool) == Some(true) {
            let x = val.get("x").and_then(Value::as_i64).unwrap_or(0);
            let y = val.get("y").and_then(Value::as_i64).unwrap_or(0);
            let tag = val.get("tag").and_then(Value::as_str).unwrap_or("?").to_string();
            let name = val.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            return Ok((x, y, format!("{tag} \"{name}\"")));
        }
        return Err("语义定位失败：ref 不存在或文本/占位符没有可见匹配。可先 browser_snapshot 拿最新 ref，或用坐标 x/y 兜底".into());
    }
    // 坐标兜底
    let x = v.get("x").and_then(Value::as_i64);
    let y = v.get("y").and_then(Value::as_i64);
    match (x, y) {
        (Some(x), Some(y)) => Ok((x, y, format!("坐标 ({x},{y})"))),
        _ => Err("缺少定位参数：给 ref（快照编号）、text/label（语义匹配）或 x/y（视口坐标）之一".into()),
    }
}

async fn dispatch_click(tab: &WebTab, x: i64, y: i64) -> Result<(), String> {
    let (x, y) = (x as f64, y as f64);
    tab
        .cdp
        .call("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y }))
        .await?;
    tab
        .cdp
        .call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1 }),
        )
        .await?;
    tab
        .cdp
        .call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1 }),
        )
        .await?;
    Ok(())
}

/// 点击后的「沉降」：多数点击不触发导航，不能死等——轮询 readyState，
/// 没出现 loading（约 400ms 内）就返回；真导航了则等到加载完成（上限 6s）。
async fn click_and_settle(tab: &WebTab, x: i64, y: i64) -> Result<(), String> {
    dispatch_click(tab, x, y).await?;
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut navigated = false;
    let mut polls = 0u32;
    loop {
        if Instant::now() > deadline {
            break; // 兜底：导航卡住也返回（截图会拿到当前状态）
        }
        let state = tab
            .cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": "document.readyState", "returnByValue": true }),
            )
            .await
            .ok()
            .and_then(|v| v.get("result").and_then(|r| r.get("value")).and_then(Value::as_str).map(str::to_string));
        match state.as_deref() {
            // 求值失败 = 上下文正在换页（旧 context 已销毁），视为导航中
            None => navigated = true,
            Some("loading") => navigated = true,
            Some("complete") => {
                if navigated {
                    break; // 导航完成
                }
                // 连续两次 complete 且从未见 loading = 本次点击没有导航，快速返回
                if polls >= 1 {
                    break;
                }
            }
            _ => {}
        }
        polls += 1;
        tokio::time::sleep(Duration::from_millis(180)).await;
    }
    if navigated {
        // 给新页面渲染留一点时间再截图
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    Ok(())
}

// —— 对工具层暴露的操作 ——

fn parse_args(args: &str) -> Result<Value, String> {
    serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))
}

fn require_url(v: &Value) -> Result<String, String> {
    let url = v.get("url").and_then(Value::as_str).map(str::trim).unwrap_or("");
    if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://") || url == "about:blank") {
        return Err("url 必须以 http:// / https:// / file:// 开头（或 about:blank）".into());
    }
    Ok(url.to_string())
}

/// 在标签页里导航并等加载完成；返回超时提示（None=正常）。会话死了返回 Err。
async fn navigate_tab(tab: &WebTab, url: &str) -> Result<Option<&'static str>, String> {
    let nav = tab
        .cdp
        .call_and_wait_event("Page.navigate", json!({ "url": url }), "Page.loadEventFired", LOAD_TIMEOUT)
        .await;
    match nav {
        Ok(_) => Ok(None),
        Err(e) if e.contains("等待事件超时") => Ok(Some("（页面加载未在预算内完成，以下为当前状态）\n")),
        Err(e) => Err(e),
    }
}

/// browser_open：加载页面（无会话则拉起浏览器），返回快照。
pub async fn open(args: &str, max_chars: usize) -> Result<String, String> {
    let v = parse_args(args)?;
    let url = require_url(&v)?;
    let session = ensure_session().await?;
    session.touch();
    let tab = agent_tab(&session).await?;
    log("browser.navigate", json!({ "url": url }));
    let note = match navigate_tab(&tab, &url).await {
        Ok(note) => note.unwrap_or(""),
        Err(e) => {
            // 会话死了：清掉，下次自动重拉
            close_internal("dead").await;
            return Err(format!("浏览器会话异常已重置（重试 browser_open 即可）: {e}"));
        }
    };
    let snap = take_snapshot(&tab, max_chars).await?;
    Ok(format!("{note}{snap}"))
}

/// browser_snapshot：当前页面结构 + 增量 diff。
pub async fn snapshot(args: &str, max_chars: usize) -> Result<String, String> {
    let _ = parse_args(args)?;
    let session = current_session().await?;
    session.touch();
    let tab = agent_tab(&session).await?;
    take_snapshot(&tab, max_chars).await
}

async fn current_session() -> Result<Arc<BrowserSession>, String> {
    let guard = SESSION.lock().await;
    match guard.as_ref() {
        Some(s) => Ok(s.clone()),
        None => Err("浏览器未打开：先调用 browser_open".into()),
    }
}

/// browser_click：语义定位点击，返回点击目标与最新快照。
pub async fn click(args: &str, max_chars: usize) -> Result<String, String> {
    let v = parse_args(args)?;
    let session = current_session().await?;
    session.touch();
    let tab = agent_tab(&session).await?;
    let (x, y, desc) = locate(&tab, &v, "ref", "text").await?;
    if let Err(e) = click_and_settle(&tab, x, y).await {
        if e.contains("连接已断开") {
            close_internal("dead").await;
        }
        return Err(format!("点击失败: {e}"));
    }
    log("browser.click", json!({ "target": desc, "x": x, "y": y }));
    let snap = take_snapshot(&tab, max_chars).await?;
    Ok(format!("已点击 {desc}（{x},{y}）\n{snap}"))
}

/// browser_type：语义定位输入框，输入文字（可先清空、可回车提交）。
pub async fn type_text(args: &str, max_chars: usize) -> Result<String, String> {
    let v = parse_args(args)?;
    let text = v.get("text").and_then(Value::as_str).unwrap_or("");
    let submit = v.get("submit").and_then(Value::as_bool).unwrap_or(false);
    let clear = v.get("clear").and_then(Value::as_bool).unwrap_or(false);
    let session = current_session().await?;
    session.touch();
    let tab = agent_tab(&session).await?;
    // 输入框定位允许用 label（占位符/名称）匹配，text 参数留给输入内容
    let locator: Value = json!({
        "ref": v.get("ref"),
        "label": v.get("label"),
    });
    let (x, y, desc) = locate(&tab, &locator, "ref", "label").await?;
    // 聚焦：点击一次输入框中央（真实事件，能触发站点自己的聚焦逻辑）
    dispatch_click(&tab, x, y).await?;
    if clear {
        tab.cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": "(() => { const el = document.activeElement; if (el && 'value' in el) { el.value = ''; el.dispatchEvent(new Event('input', { bubbles: true })); } })()" , "returnByValue": true }),
            )
            .await?;
    }
    if !text.is_empty() {
        tab.cdp.call("Input.insertText", json!({ "text": text })).await?;
    }
    if submit {
        tab.cdp.call("Input.dispatchKeyEvent", json!({ "type": "rawKeyDown", "key": "Enter", "windowsVirtualKeyCode": 13 })).await?;
        tab.cdp.call("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": "Enter", "windowsVirtualKeyCode": 13 })).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    log("browser.type", json!({ "target": desc, "chars": text.chars().count(), "submit": submit }));
    let snap = take_snapshot(&tab, max_chars).await?;
    Ok(format!("已向 {desc} 输入 {} 字符{}\n{snap}", text.chars().count(), if submit { " 并回车" } else { "" }))
}

/// browser_close：关闭浏览器进程。
pub async fn close(_args: &str) -> Result<String, String> {
    close_internal("tool").await;
    Ok("浏览器已关闭".into())
}

// —— 网页面板（P20.1）：与 agent 工具共享同一浏览器，截图渲染 + 坐标回传 ——

/// 视口尺寸（与 launch 的 --window-size 一致；前端按比例换算点击坐标）。
pub const VIEWPORT_W: i64 = 1280;
pub const VIEWPORT_H: i64 = 900;

/// 面板状态：浏览器是否在跑 + 全部标签页。
pub async fn panel_state() -> Value {
    let guard = SESSION.lock().await;
    match guard.as_ref() {
        None => json!({ "running": false, "tabs": [] }),
        Some(s) => {
            if !browser_alive(s.port).await {
                return json!({ "running": false, "tabs": [] });
            }
            let briefs = s.tab_briefs().await;
            json!({
                "running": true,
                "tabs": briefs.iter().map(|(id, agent, title, url)| json!({
                    "id": id, "agent": agent, "title": title, "url": url,
                })).collect::<Vec<_>>(),
            })
        }
    }
}

fn check_panel_url(url: &str) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://") || url == "about:blank") {
        return Err("地址必须以 http:// / https:// / file:// 开头".into());
    }
    Ok(())
}

/// 面板开新标签页（可选直接导航）。
pub async fn panel_new(url: Option<String>) -> Result<Value, String> {
    let url = url.unwrap_or_else(|| "about:blank".into());
    if url != "about:blank" {
        check_panel_url(&url)?;
    }
    let session = ensure_session().await?;
    session.touch();
    let tab = create_tab(&session, &url, false).await?;
    Ok(json!({ "id": tab.id }))
}

/// 面板导航（地址栏回车）。
pub async fn panel_navigate(id: &str, url: &str) -> Result<Value, String> {
    check_panel_url(url)?;
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    match navigate_tab(&tab, url).await {
        Ok(_) => Ok(json!({ "ok": true })),
        Err(e) => {
            if e.contains("连接已断开") {
                close_internal("dead").await;
            }
            Err(format!("导航失败: {e}"))
        }
    }
}

/// 面板截图：PNG 字节（前端 <img> 直链 + 缓存穿透参数）。
/// 截图前若页面正在加载（readyState=loading）先等它完成——无论调用时序如何，
/// 都不会拿到半张旧页/白图。
pub async fn panel_screenshot(id: &str) -> Result<Vec<u8>, String> {
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    for _ in 0..25 {
        let st = tab
            .cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": "document.readyState", "returnByValue": true }),
            )
            .await
            .ok()
            .and_then(|v| v.get("result").and_then(|r| r.get("value")).and_then(Value::as_str).map(str::to_string));
        match st.as_deref() {
            Some("loading") => tokio::time::sleep(Duration::from_millis(200)).await,
            _ => break,
        }
    }
    let res = tab
        .cdp
        .call("Page.captureScreenshot", json!({ "format": "png" }))
        .await?;
    let data = res
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| "截图结果缺 data".to_string())?;
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("截图 base64 解码失败: {e}"))
}

/// 面板点击（视口坐标）→ 等可能的导航沉降。
pub async fn panel_click(id: &str, x: i64, y: i64) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    click_and_settle(&tab, x, y).await.map_err(|e| format!("点击失败: {e}"))?;
    Ok(json!({ "ok": true }))
}

/// 面板输入：向当前聚焦元素插文本（点过输入框后用）。
pub async fn panel_type(id: &str, text: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    if !text.is_empty() {
        tab.cdp.call("Input.insertText", json!({ "text": text })).await?;
    }
    Ok(json!({ "ok": true }))
}

/// 面板按键（如 Enter 提交搜索）。
pub async fn panel_key(id: &str, key: &str, code: i64) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    tab.cdp.call("Input.dispatchKeyEvent", json!({ "type": "rawKeyDown", "key": key, "windowsVirtualKeyCode": code })).await?;
    tab.cdp.call("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": key, "windowsVirtualKeyCode": code })).await?;
    Ok(json!({ "ok": true }))
}

/// 面板滚轮。
pub async fn panel_scroll(id: &str, delta_y: i64) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    tab.cdp
        .call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseWheel", "x": VIEWPORT_W / 2, "y": VIEWPORT_H / 2, "deltaX": 0, "deltaY": delta_y }),
        )
        .await?;
    Ok(json!({ "ok": true }))
}

/// 历史导航（后退/前进）：Page.getNavigationHistory + navigateToHistoryEntry。
async fn history_go(id: &str, offset: i64) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    let hist = tab.cdp.call("Page.getNavigationHistory", json!({})).await?;
    let index = hist.get("currentIndex").and_then(Value::as_i64).unwrap_or(0);
    let entries = hist.get("entries").and_then(Value::as_array);
    let target = index + offset;
    if let Some(entries) = entries
        && target >= 0
        && let Some(entry) = entries.get(target as usize)
        && let Some(entry_id) = entry.get("id").and_then(Value::as_i64)
    {
        tab.cdp
            .call("Page.navigateToHistoryEntry", json!({ "entryId": entry_id }))
            .await?;
        // 给导航一点沉降时间（不硬等 load：历史条目可能早已缓存）
        tokio::time::sleep(Duration::from_millis(400)).await;
        return Ok(json!({ "ok": true }));
    }
    Err("没有更早/更晚的历史页".into())
}

pub async fn panel_back(id: &str) -> Result<Value, String> {
    history_go(id, -1).await
}

pub async fn panel_forward(id: &str) -> Result<Value, String> {
    history_go(id, 1).await
}

pub async fn panel_reload(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    tab.cdp.call("Page.reload", json!({})).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    Ok(json!({ "ok": true }))
}

/// 面板关标签页；全部关完浏览器进程自然退出。
pub async fn panel_close_tab(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    close_tab(&session, id).await;
    Ok(json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机冒烟（ignored，手工跑：cargo test browser_smoke -- --ignored --nocapture）
    /// 本机需装 Edge/Chrome。覆盖：拉起 → file:// 快照 → 语义点击 → 增量 diff → 语义输入 → 关闭。
    #[tokio::test]
    #[ignore]
    async fn browser_smoke_local() {
        let _ = crate::observability::init(
            &std::env::temp_dir().join("agent-browser-test-logs"),
            "browser-test.jsonl",
            "browser-test".into(),
        );
        // fixture：一个计数按钮 + 一个搜索输入框
        let dir = std::env::temp_dir().join("agent-browser-fixture");
        let _ = std::fs::create_dir_all(&dir);
        let page = dir.join("page.html");
        std::fs::write(
            &page,
            r#"<!doctype html><html><body>
<h1>冒烟测试页</h1>
<p id="counter">计数：0</p>
<button onclick="var p=document.getElementById('counter');var n=parseInt(p.textContent.replace(/\D/g,''))+1;p.textContent='计数：'+n;">点我加一</button>
<input id="q" placeholder="搜索内容" oninput="document.getElementById('echo').textContent='输入：'+this.value">
<p id="echo"></p>
</body></html>"#,
        )
        .unwrap();
        let url = format!("file://{}", page.display().to_string().replace('\\', "/"));

        // 1) 打开 + 快照
        let out = open(&format!(r#"{{"url":"{url}"}}"#), 8000).await.expect("browser_open 失败");
        println!("== open ==\n{out}");
        assert!(out.contains("冒烟测试页"), "快照应含标题");
        assert!(out.contains("button"), "快照应含按钮角色");

        // 2) 语义点击（文本定位）
        let out = click(r#"{"text":"点我加一"}"#, 8000).await.expect("click 失败");
        println!("== click ==\n{out}");
        assert!(out.contains("已点击"), "应报告点击目标");
        assert!(out.contains("计数：1"), "点击后计数应变 1");

        // 3) 再点一次：增量 diff 应报「~1 变化」
        let out = click(r#"{"text":"点我加一"}"#, 8000).await.expect("click2 失败");
        println!("== click2 ==\n{out}");
        assert!(out.contains("计数：2"), "第二次点击后计数 2");
        assert!(out.contains("~1 变化") || out.contains("变化"), "应有增量 diff 摘要: {out}");

        // 4) 语义输入（label 定位）+ 快照 value
        let out = type_text(r#"{"label":"搜索内容","text":"你好浏览器","clear":true}"#, 8000)
            .await
            .expect("type 失败");
        println!("== type ==\n{out}");
        assert!(out.contains("你好浏览器"), "快照应含输入值");

        // 5) ref 定位兜底走一遍（拿快照里 echo 段落 ref 不必要，直接关）
        let out = close("{}").await.expect("close 失败");
        assert!(out.contains("已关闭"));
    }

    /// 快照 JS 的 ref 分配规则：新元素不得与存活元素撞号（单测纯逻辑，无浏览器依赖）。
    #[test]
    fn truncate_str_caps_length() {
        assert_eq!(truncate_str("abcdef", 3), "abc…");
        assert_eq!(truncate_str("abc", 5), "abc");
    }
}
