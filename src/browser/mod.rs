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
    time::Duration,
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

struct BrowserSession {
    child: std::sync::Mutex<Child>,
    cdp: cdp::Cdp,
    /// 上次快照（增量 diff 基线）。
    last_outline: Mutex<Vec<OutlineNode>>,
    last_used_ms: AtomicU64,
}

impl BrowserSession {
    fn touch(&self) {
        self.last_used_ms.store(now_ms(), Ordering::Relaxed);
    }
    fn idle_secs(&self) -> u64 {
        now_ms().saturating_sub(self.last_used_ms.load(Ordering::Relaxed)) / 1000
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
async fn wait_page_target(port: u16, budget: Duration) -> Result<String, String> {
    // 坑 17 同源问题：本机代理环境变量会把 127.0.0.1 的请求拦进代理——CDP 端点必须直连
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))?;
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
            && let Some(ws) = page.get("webSocketDebuggerUrl").and_then(Value::as_str)
        {
            return Ok(ws.to_string());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err("等待浏览器 CDP 端点超时（10s 内 /json/list 无 page target）".into())
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
        let profile = std::env::temp_dir().join("agent-browser-profile");
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
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return Err(format!("拉起浏览器失败（{}）: {e}", exe.display())),
        };
        // 拉起后立刻探活：老版本 Edge 不认 --headless=new 会秒退
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Ok(Some(_)) = child.try_wait() {
            last_err = format!("浏览器拉起后立即退出（{} 可能不支持 --headless=new）", exe.display());
            continue;
        }
        match wait_page_target(port, Duration::from_secs(10)).await {
            Ok(ws) => {
                let cdp = cdp::Cdp::connect(&ws).await?;
                // loadEventFired 需要 Page 域使能
                cdp.call("Page.enable", json!({})).await?;
                log("browser.launch", json!({ "exe": exe.display().to_string(), "port": port }));
                let session = Arc::new(BrowserSession {
                    child: std::sync::Mutex::new(child),
                    cdp,
                    last_outline: Mutex::new(Vec::new()),
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
        if s.cdp.call("Runtime.evaluate", json!({ "expression": "1+1", "returnByValue": true })).await.is_ok() {
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

async fn take_snapshot(session: &BrowserSession, max_chars: usize) -> Result<String, String> {
    let res = session
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

    let mut guard = session.last_outline.lock().await;
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
    session: &BrowserSession,
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
        let res = session
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

async fn dispatch_click(session: &BrowserSession, x: i64, y: i64) -> Result<(), String> {
    let (x, y) = (x as f64, y as f64);
    session
        .cdp
        .call("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y }))
        .await?;
    session
        .cdp
        .call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1 }),
        )
        .await?;
    session
        .cdp
        .call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1 }),
        )
        .await?;
    Ok(())
}

/// 点击可能触发导航：先注册 load 事件等待器（同步入队），再点击；事件到了就多给
/// 渲染一点时间，没到（无导航的普通点击）照常返回。
async fn click_and_settle(session: &BrowserSession, x: i64, y: i64) -> Result<(), String> {
    let rx = session.cdp.register_wait("Page.loadEventFired")?;
    dispatch_click(session, x, y).await?;
    if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_secs(6), rx).await {
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

/// browser_open：加载页面（无会话则拉起浏览器），返回快照。
pub async fn open(args: &str, max_chars: usize) -> Result<String, String> {
    let v = parse_args(args)?;
    let url = require_url(&v)?;
    let session = ensure_session().await?;
    session.touch();
    log("browser.navigate", json!({ "url": url }));
    let nav = session
        .cdp
        .call_and_wait_event("Page.navigate", json!({ "url": url }), "Page.loadEventFired", LOAD_TIMEOUT)
        .await;
    let note = match nav {
        Ok(_) => String::new(),
        Err(e) if e.contains("等待事件超时") => "（页面加载未在预算内完成，以下为当前快照）\n".to_string(),
        Err(e) => {
            // 会话死了：清掉，下次自动重拉
            close_internal("dead").await;
            return Err(format!("浏览器会话异常已重置（重试 browser_open 即可）: {e}"));
        }
    };
    let snap = take_snapshot(&session, max_chars).await?;
    Ok(format!("{note}{snap}"))
}

/// browser_snapshot：当前页面结构 + 增量 diff。
pub async fn snapshot(args: &str, max_chars: usize) -> Result<String, String> {
    let _ = parse_args(args)?;
    let session = current_session().await?;
    session.touch();
    take_snapshot(&session, max_chars).await
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
    let (x, y, desc) = locate(&session, &v, "ref", "text").await?;
    if let Err(e) = click_and_settle(&session, x, y).await {
        if e.contains("连接已断开") {
            close_internal("dead").await;
        }
        return Err(format!("点击失败: {e}"));
    }
    log("browser.click", json!({ "target": desc, "x": x, "y": y }));
    let snap = take_snapshot(&session, max_chars).await?;
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
    // 输入框定位允许用 label（占位符/名称）匹配，text 参数留给输入内容
    let locator: Value = json!({
        "ref": v.get("ref"),
        "label": v.get("label"),
    });
    let (x, y, desc) = locate(&session, &locator, "ref", "label").await?;
    // 聚焦：点击一次输入框中央（真实事件，能触发站点自己的聚焦逻辑）
    dispatch_click(&session, x, y).await?;
    if clear {
        session
            .cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": "(() => { const el = document.activeElement; if (el && 'value' in el) { el.value = ''; el.dispatchEvent(new Event('input', { bubbles: true })); } })()" , "returnByValue": true }),
            )
            .await?;
    }
    if !text.is_empty() {
        session.cdp.call("Input.insertText", json!({ "text": text })).await?;
    }
    if submit {
        session.cdp.call("Input.dispatchKeyEvent", json!({ "type": "rawKeyDown", "key": "Enter", "windowsVirtualKeyCode": 13 })).await?;
        session.cdp.call("Input.dispatchKeyEvent", json!({ "type": "keyUp", "key": "Enter", "windowsVirtualKeyCode": 13 })).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    log("browser.type", json!({ "target": desc, "chars": text.chars().count(), "submit": submit }));
    let snap = take_snapshot(&session, max_chars).await?;
    Ok(format!("已向 {desc} 输入 {} 字符{}\n{snap}", text.chars().count(), if submit { " 并回车" } else { "" }))
}

/// browser_close：关闭浏览器进程。
pub async fn close(_args: &str) -> Result<String, String> {
    close_internal("tool").await;
    Ok("浏览器已关闭".into())
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
