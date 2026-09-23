//! 内置浏览器内核（P20 起，P21 对齐 ZCode 协议，P22 搬进桌面壳）—— 进程内直持 CDP
//! 会话的页面操作能力。
//!
//! P20 拉起本机 Edge/Chrome（headless），页面级 WebSocket 直连（cdp.rs）。P21 起：
//! - **统一命令面**（commands.rs，协议形状移植自 ZCode `browserCommandSchema`，Apache-2.0）：
//!   一个 `browser` 工具按 `method` 判别覆盖全部动作；结构化错误码不静默兜底。
//! - **快照双结构**：`elements`（可动作元素，带 ref/selector/rect/inViewport/parentRef/
//!   disabled/checked）与 `dom`（语义正文节点，不占 action ref）——正文节点不再消耗
//!   交互预算；ref 失效后可用 selector 兜底定位。
//! - **隔离世界**：快照/定位/求值在 `Page.createIsolatedWorld` 的隔离世界执行，
//!   页面覆写全局对象或 getter 也改不了结果。
//! - **screencast 帧流**：面板可见标签页跑 `Page.startScreencast`（jpeg），帧进内存
//!   缓冲，面板读帧零 CDP 往返（不再每次轮询全量 PNG 编码）。
//! - **标签页驻留**：后台（非面板可见、非 agent 页）超时空闲且活跃页超限时
//!   `Page.setWebLifecycleState(frozen)` 挂起，访问即恢复；agent 页永保护。
//! - **JS 弹窗**：`Page.javascriptDialogOpening` 捕获进 tab 状态，面板出「确定/取消」条，
//!   agent 侧 get_dialog / handle_dialog——弹窗不再把页面卡死。
//!
//! P22 起浏览器有两种宿主，**上层协议完全不变**：
//! - **shell 模式**（桌面壳在跑）：标签页是壳窗口里的 child webview（真页面、真交互、
//!   复用 App 自己的 WebView2 运行时，不再另起无头 Edge 进程）。壳的 WebView2 环境带
//!   `--remote-debugging-port=9223`（tauri.conf.json），本服务经页面级 CDP 驱动这些
//!   webview；标签页的开/关/挪/显隐由服务通过控制通道（shell.rs）指挥壳执行。
//! - **无头模式**（壳不在：start.bat 直跑服务、或用户在普通浏览器里开 5173）：
//!   沿用 P20 的无头 Edge/Chrome，行为与此前完全一致。
//!
//! 设计约定（窄而刚 v1，承 P20）：
//! - **单浏览器多标签页**：所有会话共用一个浏览器实例；面板与 agent 工具共享进程，
//!   各标签页独立 CDP 连接互不干扰；agent 工具缺省作用于 agent 专用标签页。
//! - **DOM 增量 diff**：快照时给元素钉 `data-agent-ref`（已存在的不重打——ref 稳定），
//!   服务端与上次快照按 ref 对比出「新增/移除/变化」。
//! - **语义定位为主，坐标兜底**：ref → selector → 文本/占位符 → 视口坐标。
//! - shell 模式下 screencast 只在「有非壳客户端在轮询帧」时开（壳里看的是真页面，
//!   不白编码）；壳外浏览器打开面板时自动恢复帧流，两条路径并存。

mod cdp;
mod commands;
mod jobguard;
pub mod shell;

pub use commands::{
    ALL_METHODS, BrowserCommand, BrowserError, ErrorCode, KeyModifier, MouseButton, PageState, Point,
    TabBrief, method_is_read_only,
};

use std::{
    collections::HashMap,
    os::windows::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::observability::{self, EventType};

/// 导航加载预算。
const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// 空转自动关闭阈值。
const IDLE_CLOSE: Duration = Duration::from_secs(600);
/// 隔离世界名（页面脚本无法触及，移植自 ZCode 的 zcode-playwright-helper 思路）。
const ISOLATED_WORLD: &str = "agent-browser-helper";
/// 活跃（未挂起）标签页上限：超过则挂起最久空闲的后台页。
const LIVE_TAB_LIMIT: usize = 4;
/// 后台页挂起前的空闲阈值。
const SUSPEND_IDLE: Duration = Duration::from_secs(60);
/// wait_for 默认/上限预算。
const WAIT_FOR_DEFAULT_MS: u64 = 5_000;
const WAIT_FOR_MAX_MS: u64 = 30_000;
/// 驻留/screencast 巡检周期。
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);

/// 单个 DOM 快照元素（服务端 diff、渲染与兜底定位用）。
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
    selector: String,
    in_viewport: bool,
    disabled: bool,
    checked: Option<bool>,
    parent_reference: Option<String>,
}

/// 语义正文节点（只负责"读懂页面"，不占 action ref）。
#[derive(Clone, PartialEq)]
struct DomNode {
    tag: String,
    depth: usize,
    in_viewport: bool,
    reference: Option<String>,
    role: Option<String>,
    name: String,
    text: String,
}

/// screencast 最新帧（jpeg 字节；Chromium 只在页面变化时产帧，有帧即为当前画面）。
#[derive(Clone)]
struct Frame {
    data: Vec<u8>,
}

/// 页面当前 JS 弹窗（alert/confirm/prompt/beforeunload）。
#[derive(Clone, Default)]
struct DialogInfo {
    kind: String,
    message: String,
    default_prompt: String,
}

/// 一个标签页 = 一条页面级 CDP 连接（P20.1 网页面板起多标签化；面板与 agent 工具
/// 共享同一个浏览器进程，各标签页连接互不干扰）。P21 增补驻留/帧/弹窗状态。
pub struct WebTab {
    /// CDP target id
    pub id: String,
    /// agent 工具（browser 统一工具）专用标签页
    pub agent: bool,
    pub cdp: cdp::Cdp,
    /// shell 模式下的 child webview label（关/挪/显隐都靠它；无头模式为 None）
    pub shell_label: Option<String>,
    /// agent 快照的增量 diff 基线（仅 agent 标签页使用）
    last_outline: Mutex<Vec<OutlineNode>>,
    /// ref → selector 映射（ref 失效后 selector 兜底定位）
    ref_selectors: Mutex<HashMap<String, String>>,
    /// 最近一次交互时间（驻留 victim 选择用）
    last_active_ms: AtomicU64,
    /// 是否被挂起（frozen）
    suspended: AtomicBool,
    /// 已淘汰（从会话移除）：事件任务见之即退
    retired: AtomicBool,
    /// screencast 是否在跑
    screencast_on: AtomicBool,
    /// 最新 screencast 帧
    frame: Mutex<Option<Frame>>,
    /// 当前 JS 弹窗
    dialog: Mutex<Option<DialogInfo>>,
    /// 视口覆盖（挂起恢复时重放；None = 未设置过）
    viewport: Mutex<Option<(i64, i64)>>,
}

impl WebTab {
    fn touch(&self) {
        self.last_active_ms.store(now_ms(), Ordering::Relaxed);
    }
    fn idle_secs(&self) -> u64 {
        now_ms().saturating_sub(self.last_active_ms.load(Ordering::Relaxed)) / 1000
    }
    /// ref 失效后按 selector 兜底定位（快照时存下的映射）。
    async fn selector_for(&self, reference: &str) -> Option<String> {
        self.ref_selectors.lock().await.get(reference).cloned()
    }
    async fn remember_selector(&self, reference: &str, selector: &str) {
        if !reference.is_empty() && !selector.is_empty() {
            self.ref_selectors.lock().await.insert(reference.to_string(), selector.to_string());
        }
    }
}

impl WebTab {
    fn new(id: String, agent: bool, cdp: cdp::Cdp, shell_label: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            id,
            agent,
            cdp,
            shell_label,
            last_outline: Mutex::new(Vec::new()),
            ref_selectors: Mutex::new(HashMap::new()),
            last_active_ms: AtomicU64::new(now_ms()),
            suspended: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            screencast_on: AtomicBool::new(false),
            frame: Mutex::new(None),
            dialog: Mutex::new(None),
            viewport: Mutex::new(None),
        })
    }
}

/// 浏览器宿主模式（P22）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum HostMode {
    /// 无头 Edge/Chrome（壳不在时的兜底）
    Headless,
    /// 桌面壳 child webview（服务经 CDP + 控制通道驱动）
    Shell,
}

struct BrowserSession {
    mode: HostMode,
    /// 无头模式的浏览器子进程（shell 模式为 None——没有自己的进程）
    child: Option<std::sync::Mutex<Child>>,
    /// 无头子进程的生命周期保险丝（kill-on-close；随会话 Drop 一起击杀整棵树）
    job: Option<jobguard::ChildJob>,
    /// CDP HTTP 端点端口（shell 模式 = 壳的 9223；无头 = 自拉实例的随机端口）
    port: u16,
    tabs: std::sync::Mutex<Vec<Arc<WebTab>>>,
    last_used_ms: AtomicU64,
    /// 面板当前可见的标签页（驱动 screencast 与驻留；None = 面板没在看）
    visible_tab: std::sync::Mutex<Option<String>>,
    /// **面板是否正在展示网页**（shell 模式壳里 webview 显隐的唯一依据）。
    /// 只有面板自己的动作（activate/new/viewport/frame/navigate）能置真；agent 工具的
    /// new_tab/activate_tab 不置——否则 agent 一开工就把 webview 弹到聊天区上空。
    panel_showing: std::sync::Mutex<bool>,
    /// shell 模式：当前在壳里显示中的 label（只发变化的显隐指令）
    shell_shown: std::sync::Mutex<Option<String>>,
    /// shell 模式：面板内容区最近一次矩形（新建 webview 时的初始位置）
    last_rect: std::sync::Mutex<Option<(i64, i64, i64, i64)>>,
    /// 最近一次帧轮询时间（screencast 门控：壳里看真页面，不白编码）
    last_frame_poll_ms: AtomicU64,
    /// agent 工具最近一次碰浏览器的时间戳——面板据此自动唤起（用户要求「agent 操作
    /// 内置浏览器时自动唤起右侧边栏」）。与面板自己的动作分开记：面板的显隐只认
    /// panel_showing，agent 的触碰只用来唤醒界面。
    agent_touched_ms: AtomicU64,
    /// agent 工具当前操作的页——面板据此跟随（用户预期「agent 干活时面板跟着走」）。
    /// 与 visible_tab（面板自己的视图）分开：agent 切页不再抢用户的视图，但界面要
    /// 能看见 agent 在哪儿干活。
    agent_tab_id: std::sync::Mutex<Option<String>>,
}

impl BrowserSession {
    fn touch(&self) {
        self.last_used_ms.store(now_ms(), Ordering::Relaxed);
    }
    fn idle_secs(&self) -> u64 {
        now_ms().saturating_sub(self.last_used_ms.load(Ordering::Relaxed)) / 1000
    }
    fn visible(&self) -> Option<String> {
        self.visible_tab.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn set_visible(&self, id: &str) {
        *self.visible_tab.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.to_string());
    }
    fn clear_visible(&self) {
        *self.visible_tab.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    /// 记 agent 当前操作的页（面板据此跟随；agent 的切页不抢面板自己的视图）。
    fn set_agent_tab(&self, id: &str) {
        *self.agent_tab_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.to_string());
    }
    fn agent_tab_now(&self) -> Option<String> {
        self.agent_tab_id.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// 面板是否正在展示网页（壳里 webview 显隐的唯一依据）。
    fn showing(&self) -> bool {
        *self.panel_showing.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn set_showing(&self, v: bool) {
        *self.panel_showing.lock().unwrap_or_else(|e| e.into_inner()) = v;
    }
    /// 快照列表（面板展示与 agent list 用）。
    async fn tab_briefs(&self) -> Vec<TabBrief> {
        let tabs = self.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let visible = self.visible();
        let mut out = Vec::new();
        for t in tabs {
            let (title, url) = tab_title_url(&t).await;
            out.push(TabBrief {
                id: t.id.clone(),
                agent: t.agent,
                title,
                url,
                visible: visible.as_deref() == Some(t.id.as_str()),
                loading: false,
                suspended: t.suspended.load(Ordering::Relaxed),
            });
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

/// 杀掉浏览器进程并回收（Arc 里的 Child 需经 Mutex 拿可变引用；shell 模式无进程）。
/// job 字段的存在意义就是把保险丝持有到会话结束（Drop = TerminateJobObject 团灭整树），
/// 这里读它一面上日志、一面让"字段没被读到"的告警闭嘴。
fn kill_child(s: &BrowserSession) {
    if let Some(c) = s.child.as_ref()
        && let Ok(mut c) = c.lock()
    {
        let _ = c.kill();
        let _ = c.wait();
    }
    if s.job.is_some() {
        log("browser.job.reap", json!({}));
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

/// 收集 WebView2 运行时目录下的 msedge.exe（真机踩过：用户机器**没装完整版
/// Edge/Chrome**，界面本身跑在 WebView2 里——运行时在
/// `Program Files (x86)\Microsoft\EdgeCore\<版本>\msedge.exe`，同样能
/// `--headless=new --remote-debugging-port` 起无头 CDP（实测 HeadlessChrome/153 OK）。
/// EdgeWebKit\Application\<版本> 是老布局，一并扫。只收数字开头的版本目录
/// （EdgeCore\Optimized 是占位副本，不是完整浏览器）。
fn webview2_runtime_exes(pf: &str, pf86: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for base in [pf86, pf] {
        for prefix in ["Microsoft\\EdgeCore", "Microsoft\\EdgeWebKit\\Application"] {
            let dir = PathBuf::from(base).join(prefix);
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            let mut found: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| {
                    e.path().is_dir()
                        && e.file_name().to_string_lossy().starts_with(|c: char| c.is_ascii_digit())
                })
                .map(|e| e.path().join("msedge.exe"))
                .filter(|p| p.is_file())
                .collect();
            found.sort();
            out.extend(found);
        }
    }
    out
}

fn find_browser_exe() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("AGENT_BROWSER_PATH")
        && !p.trim().is_empty()
    {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    // 环境变量可能整体缺失（如从某些最小环境里拉起服务）——回落硬编码常见路径
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into());
    let pf86 = std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".into());
    let lf = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let mut candidates: Vec<PathBuf> = vec![
        format!("{pf86}\\Microsoft\\Edge\\Application\\msedge.exe").into(),
        format!("{pf}\\Microsoft\\Edge\\Application\\msedge.exe").into(),
        format!("{pf}\\Google\\Chrome\\Application\\chrome.exe").into(),
        format!("{pf86}\\Google\\Chrome\\Application\\chrome.exe").into(),
        format!("{lf}\\Google\\Chrome\\Application\\chrome.exe").into(),
    ];
    // WebView2 运行时殿后（没装完整版浏览器时的唯一 Chromium 来源）
    candidates.extend(webview2_runtime_exes(&pf, &pf86));
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            "找不到 Edge/Chrome/WebView2 运行时（设 AGENT_BROWSER_PATH 指到浏览器 exe 可用）".to_string()
        })
}

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
    let (id, ws, shell_label) = match session.mode {
        // shell 模式一律 about:blank 起步：WebView2 的 child webview 带初始 URL 创建时，
        // 导航一提交 CDP target 就被换掉（实测 Edg/153：about:blank target 上线 180ms 后
        // 随导航消失），先建空白页再走 CDP Page.navigate 才能稳住 target
        HostMode::Shell => shell_create_target(session).await?,
        HostMode::Headless => {
            let (id, ws) = dev_new_target(session.port).await?;
            (id, ws, None)
        }
    };
    let cdp = cdp::Cdp::connect(&ws).await?;
    cdp.call("Page.enable", json!({})).await?;
    let tab = WebTab::new(id, agent, cdp, shell_label);
    spawn_tab_events(tab.clone());
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
        json!({ "id": tab.id, "agent": agent, "url": url, "shell": session.mode == HostMode::Shell }),
    );
    Ok(tab)
}

/// shell 模式建标签页：指挥壳建 about:blank child webview → 在壳的 CDP 端点上发现新
/// target → 连上它的页面级 WS。返回 (target id, ws url, shell label)。
async fn shell_create_target(session: &BrowserSession) -> Result<(String, String, Option<String>), String> {
    if !shell::available() {
        return Err("桌面壳控制通道未连接（壳没跑或已断开）".into());
    }
    let label = format!("wv-{}", session.tabs.lock().unwrap_or_else(|e| e.into_inner()).len() + 1);
    // 初始矩形：面板上次同步的矩形（没有就用个默认值；显示前 sweep 会补发正确矩形）
    let (x, y, w, h) = session
        .last_rect
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or((0, 0, 1024, 768));
    // 发现基线：**发指令前**快照全部 page target id——壳里上一个会话残留的 webview
    // （服务重启后壳不重启时就有）target 还在列表里，只排除自己的 tab 会认领到将死的
    // 旧 target，连上就断
    let known = all_page_target_ids(session.port).await;
    shell::create(&label, "about:blank", x as f64, y as f64, w as f64, h as f64, false);
    match wait_new_page_target(session.port, &known, Duration::from_secs(8)).await {
        Ok((id, ws)) => Ok((id, ws, Some(label))),
        Err(e) => {
            shell::close(&label); // 建出来没人认领的 webview 回收掉
            Err(format!(
                "等待桌面壳 webview 上线超时（CDP 端口 {port} 不可用？被别的程序占了？）: {e}",
                port = session.port
            ))
        }
    }
}

/// 拉一次壳 CDP 端点的 page target 列表（端点不可用/解析失败返回空）。
async fn fetch_page_targets(port: u16) -> Vec<Value> {
    let Ok(client) = dev_client() else { return Vec::new() };
    let Ok(resp) = client.get(format!("http://127.0.0.1:{port}/json/list")).send().await else {
        return Vec::new();
    };
    let Ok(text) = resp.text().await else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    v.as_array()
        .map(|a| {
            a.iter()
                .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// 轮询 page target 列表直到 `until` 满足或超时，返回最后一次看到的列表。
async fn poll_page_targets(port: u16, budget: Duration, until: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let list = fetch_page_targets(port).await;
        if until(&list) {
            return list;
        }
        if tokio::time::Instant::now() >= deadline {
            return list;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// 当前 CDP 端口上全部 page target id（发现基线用；端点不可用时返回空）。
async fn all_page_target_ids(port: u16) -> Vec<String> {
    fetch_page_targets(port)
        .await
        .iter()
        .filter_map(|t| t.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// 等「非壳内部」的 page target 全部消失（reset 后旧 webview 异步销毁，见 shell_launch）。
/// 只剩壳自己的 UI/DevTools 窗口即算稳了；超时也放行（不阻塞启动）。
async fn wait_targets_settled(port: u16, budget: Duration) {
    poll_page_targets(port, budget, |list| {
        list.iter().filter(|t| !is_shell_internal_target(t)).count() == 0
    })
    .await;
}

/// 轮询 /json/list 找新 page target（壳的 UI 自己的 webview 与 DevTools 窗口要排除）。
async fn wait_new_page_target(port: u16, known: &[String], budget: Duration) -> Result<(String, String), String> {
    let list = poll_page_targets(port, budget, |list| {
        list.iter().any(|t| {
            !known.iter().any(|k| Some(k.as_str()) == t.get("id").and_then(Value::as_str))
                && !is_shell_internal_target(t)
        })
    })
    .await;
    list.iter()
        .find(|t| {
            !known.iter().any(|k| Some(k.as_str()) == t.get("id").and_then(Value::as_str))
                && !is_shell_internal_target(t)
        })
        .and_then(|t| {
            let id = t.get("id").and_then(Value::as_str)?;
            let ws = t.get("webSocketDebuggerUrl").and_then(Value::as_str)?;
            Some((id.to_string(), ws.to_string()))
        })
        .ok_or_else(|| format!("{budget:?} 内没出现新的 page target"))
}

/// 壳自己的 UI webview（加载 5173 的那个）与 DevTools 窗口——都不是浏览器标签页。
fn is_shell_internal_target(t: &Value) -> bool {
    let url = t.get("url").and_then(Value::as_str).unwrap_or("");
    url.starts_with("devtools://") || url.starts_with("chrome-devtools://") || url.contains("127.0.0.1:5173")
}

/// 关闭并移除一个标签页（连接即弃，进程侧异步关）。
async fn close_tab(session: &BrowserSession, id: &str) -> bool {
    let removed = {
        let mut tabs = session.tabs.lock().unwrap_or_else(|e| e.into_inner());
        let pos = tabs.iter().position(|t| t.id == id);
        if let Some(pos) = pos {
            let tab = tabs.remove(pos);
            tab.retired.store(true, Ordering::Relaxed);
            // shell 模式：壳里的 child webview 也要关
            if let Some(label) = tab.shell_label.as_deref() {
                shell::close(label);
            }
        }
        pos.is_some()
    };
    if removed {
        if session.visible().as_deref() == Some(id) {
            session.clear_visible();
        }
        if session.mode == HostMode::Headless {
            dev_close_target(session.port, id).await;
        }
        log("browser.tab.close", json!({ "id": id }));
    }
    removed
}

/// agent 工具的专用标签页：找活的 agent 页，没有就现开一个。
async fn agent_tab(session: &BrowserSession) -> Result<Arc<WebTab>, String> {
    {
        let tabs = session.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(t) = tabs.iter().find(|t| t.agent) {
            session.set_agent_tab(&t.id);
            return Ok(t.clone());
        }
    }
    let tab = create_tab(session, "about:blank", true).await?;
    session.set_agent_tab(&tab.id);
    Ok(tab)
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

/// 桌面壳没跑时给人的话（不静默、不偷偷拉 Edge）。
const NO_SHELL_HINT: &str = "桌面壳没运行，内置浏览器不可用。P20.4 起网页在桌面壳的 child webview 里跑（不再另起 Edge 进程）：请启动 Agent 桌面壳（start.bat 现在会一起起壳），或在本面板「⋯ → 在默认浏览器中打开」看网页。确需无头模式（自动化/CI）设环境变量 AGENT_BROWSER_HEADLESS=1。";

/// 是否允许无头模式（默认关——没有壳就不起 Edge）。
fn headless_allowed() -> bool {
    std::env::var("AGENT_BROWSER_HEADLESS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

async fn launch() -> Result<Arc<BrowserSession>, String> {
    // shell 优先：桌面壳在跑就把浏览器搬进壳的 child webview（不再另起无头进程）
    if shell::available() {
        match shell_launch().await {
            Ok(s) => return Ok(s),
            Err(e) => {
                // 壳接着但 CDP 不可用（端口被抢/旧版壳）：禁用 shell 模式
                shell::disable();
                log("browser.shell.fallback", json!({ "err": e }));
            }
        }
    }
    // 没有壳：**默认不再自动拉无头 Edge**——用户多次明令不要起 Edge 进程，而 P20.4 起
    // 浏览器的家就是壳。确需无头（自动化 / CI / 特意不用壳）时设 AGENT_BROWSER_HEADLESS=1。
    if !headless_allowed() {
        return Err(NO_SHELL_HINT.to_string());
    }
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
                // iframe 内嵌 DevTools 时前端会带 Origin 头连 ws，Chromium 默认拒——
                // 本实例只听环回，放行任意 Origin
                "--remote-allow-origins=*",
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
        // 生命周期保险丝：服务被硬杀（stop.bat 走 taskkill /F）时无头浏览器跟着死，
        // 不留孤儿 Edge 占内存（真机踩过：用户机器上留了个杀不掉的提权孤儿）
        let job = jobguard::ChildJob::new();
        if let Some(j) = job.as_ref() {
            use std::os::windows::io::AsRawHandle;
            j.assign(child.as_raw_handle() as isize);
        }
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
            last_err = format!("浏览器拉起后立即退出（exit={code:?}，{}）：{tail}", exe.display());
            continue;
        }
        match wait_page_target(port, Duration::from_secs(10)).await {
            Ok((target_id, ws)) => {
                let cdp = cdp::Cdp::connect(&ws).await?;
                // loadEventFired 需要 Page 域使能
                cdp.call("Page.enable", json!({})).await?;
                log("browser.launch", json!({ "exe": exe.display().to_string(), "port": port }));
                // 启动自带的 about:blank 页即 agent 工具的专用标签页
                let first = WebTab::new(target_id, true, cdp, None);
                spawn_tab_events(first.clone());
                let session = Arc::new(BrowserSession {
                    mode: HostMode::Headless,
                    child: Some(std::sync::Mutex::new(child)),
                    job,
                    port,
                    tabs: std::sync::Mutex::new(vec![first]),
                    last_used_ms: AtomicU64::new(now_ms()),
                    visible_tab: std::sync::Mutex::new(None),
                    panel_showing: std::sync::Mutex::new(false),
                    shell_shown: std::sync::Mutex::new(None),
                    last_rect: std::sync::Mutex::new(None),
                    last_frame_poll_ms: AtomicU64::new(0),
                    agent_touched_ms: AtomicU64::new(0),
                    agent_tab_id: std::sync::Mutex::new(None),
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

/// 壳 CDP 端口轻量探活（GET /json/version）——shell_launch 用它验证壳的 CDP 真的
/// 可用（不可用就整体回退无头模式）。**不再靠预建一个空白 agent 页来验证**：那个页
/// 会以「空白页 agent」的身份排在用户标签前面，用户明确反馈多余（ agent 页按需创建）。
async fn probe_shell_cdp(port: u16) -> bool {
    let Ok(client) = dev_client() else { return false };
    matches!(
        client.get(format!("http://127.0.0.1:{port}/json/version")).send().await,
        Ok(r) if r.status().is_success()
    )
}

/// shell 模式起会话：不预建任何页（用户打开面板看到的就是空态；agent 页在模型第一次
/// 用浏览器工具时按需创建）。这一步同时验证壳的 CDP 端口真的可用——不可用就整体
/// 回退无头模式。
async fn shell_launch() -> Result<Arc<BrowserSession>, String> {
    let session = Arc::new(BrowserSession {
        mode: HostMode::Shell,
        child: None,
        job: None,
        port: shell::SHELL_CDP_PORT,
        tabs: std::sync::Mutex::new(Vec::new()),
        last_used_ms: AtomicU64::new(now_ms()),
        visible_tab: std::sync::Mutex::new(None),
        panel_showing: std::sync::Mutex::new(false),
        shell_shown: std::sync::Mutex::new(None),
        last_rect: std::sync::Mutex::new(None),
        last_frame_poll_ms: AtomicU64::new(0),
        agent_touched_ms: AtomicU64::new(0),
        agent_tab_id: std::sync::Mutex::new(None),
    });
    // 壳不重启而服务重启过时，上一个会话的 webview 还活着——先清干净再建，
    // 免得残留 target 干扰发现、白白常驻
    shell::reset();
    // 关掉旧 webview 后**等它们的 target 从 /json/list 消失**：实测旧 webview 的销毁
    // 是异步的（~180ms），这期间建出来的新 webview，其 target 会跟着旧的一起从列表
    // 里消失（CDP 连上就断）
    wait_targets_settled(session.port, Duration::from_secs(3)).await;
    if !probe_shell_cdp(session.port).await {
        return Err(format!("壳 CDP 端口 {} 探活失败", session.port));
    }
    ensure_reaper();
    log("browser.launch", json!({ "mode": "shell", "port": shell::SHELL_CDP_PORT }));
    Ok(session)
}

/// 确保有一个活的浏览器会话（死会话清理后重新拉起）。
async fn ensure_session() -> Result<Arc<BrowserSession>, String> {
    let mut guard = SESSION.lock().await;
    if let Some(s) = guard.as_ref() {
        s.touch();
        if browser_alive(s.port).await {
            // 端口活着不代表会话里的 tab 还活着：壳重启后 9223 复生，但旧连接的
            // actor 早退了（僵尸 tab，title/url 全空）。摘掉死 tab；全死就重建会话。
            if reap_dead_tabs(&mut guard).await? {
                return Ok(guard.as_ref().unwrap().clone());
            }
            return Ok(guard.as_ref().unwrap().clone());
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

/// 摘掉 CDP 已断的 tab（壳重启/浏览器进程死了就会这样），全死就连会话一起重建。
/// **读路径（panel_state / current_session）也必须调**：只挂在 ensure_session 上的话，
/// 面板一打开拿到的就是僵尸 tab 列表（title/url 全空），地址栏回车打到死连接上直接
/// 「CDP actor 已退出」——真机复现过。返回 true = 调用方手里的 guard 已 refreshes。
async fn reap_dead_tabs(guard: &mut Option<Arc<BrowserSession>>) -> Result<bool, String> {
    let Some(s) = guard.as_ref() else { return Ok(false) };
    let dead: Vec<String> = s
        .tabs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|t| !t.cdp.is_alive())
        .map(|t| t.id.clone())
        .collect();
    if dead.is_empty() {
        return Ok(false);
    }
    for id in &dead {
        close_tab(s, id).await;
    }
    let empty = s.tabs.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
    if empty {
        log("browser.stale", json!({ "reason": "tabs-dead" }));
        if let Some(dead_session) = guard.take() {
            kill_child(&dead_session);
        }
        let s = launch().await?;
        *guard = Some(s);
        return Ok(true);
    }
    Ok(false)
}

/// 空转收割 + 驻留巡检 + screencast 同步（5s 一拍）。
fn ensure_reaper() {
    REAPER.get_or_init(|| {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SWEEP_INTERVAL).await;
                let session = {
                    let guard = SESSION.lock().await;
                    match guard.as_ref() {
                        Some(s) => {
                            s.touch();
                            s.clone()
                        }
                        None => continue,
                    }
                };
                sweep_session(&session).await;
                if session.idle_secs() > IDLE_CLOSE.as_secs() {
                    close_internal("idle").await;
                }
            }
        });
    });
}

/// 驻留巡检：screencast 只服务「有客户端在轮询帧」的可见页；后台超时空闲且活跃页
/// 超限 → 挂起最久空闲者；shell 模式顺带同步壳里 webview 的显隐与矩形。
async fn sweep_session(session: &Arc<BrowserSession>) {
    let tabs = session.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let visible = session.visible();
    // screencast 同步：可见页 + 仍有客户端在轮询帧才开（壳里看的是真页面，不白编码；
    // 用户从普通浏览器开面板时轮询恢复，帧流自动跟上）
    let want_frames = now_ms().saturating_sub(session.last_frame_poll_ms.load(Ordering::Relaxed)) < 15_000;
    for t in &tabs {
        let on = visible.as_deref() == Some(t.id.as_str()) && !t.suspended.load(Ordering::Relaxed) && want_frames;
        ensure_screencast(t, on).await;
    }
    // shell 显隐同步：只看变化的（壳没连时指令静默丢弃）
    sync_shell_visibility(session, &tabs, &visible).await;
    // 驻留淘汰：非 agent、非可见、空闲超限、未挂起；活跃数超 LIVE_TAB_LIMIT 才动手
    let live: Vec<&Arc<WebTab>> = tabs.iter().filter(|t| !t.suspended.load(Ordering::Relaxed)).collect();
    if live.len() <= LIVE_TAB_LIMIT {
        return;
    }
    let mut victims: Vec<&Arc<WebTab>> = live
        .into_iter()
        .filter(|t| !t.agent && visible.as_deref() != Some(t.id.as_str()) && t.idle_secs() >= SUSPEND_IDLE.as_secs())
        .collect();
    // 最久空闲者先淘汰
    victims.sort_by_key(|t| t.last_active_ms.load(Ordering::Relaxed));
    for t in victims {
        let still_live = session
            .tabs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|x| !x.suspended.load(Ordering::Relaxed))
            .count();
        if still_live <= LIVE_TAB_LIMIT {
            break;
        }
        suspend_tab(t).await;
    }
}

/// shell 模式同步壳里 webview 的显隐与矩形。**两道门控缺一不可**：
/// ① `panel_showing`——只有面板自己在看时才显示。agent 工具的 new_tab/activate_tab
///    也会写 visible_tab（无头时代那是驱动 screencast 用的），但那不是"人在看"——
///    不门控的话 agent 一开工就把 webview 弹到聊天区上空（真机踩过）；
/// ② 矩形已知——面板还没同步过视口（首次打开/切页签的防抖窗口）时先藏着，
///    免得 webview 在错误位置闪一下。
/// 「⋯」菜单不走这里的显隐——它是壳里一个**透明置顶的独立 webview**（见 `shell::menu`），
/// 浮在下层内容 webview 之上，网页全程不动、不被遮也不被藏。
async fn sync_shell_visibility(session: &Arc<BrowserSession>, tabs: &[Arc<WebTab>], visible: &Option<String>) {
    if session.mode != HostMode::Shell {
        return;
    }
    // 矩形合理性门槛：面板内容区永远在页签栏+工具栏下方（实测 y≈125），y<40 只可能是
    // 量错了元素/量早了——这种矩形下显示 webview 会把它盖到面板 chrome 上（用户看到的
    // 就是"右侧边栏没了、页面从窗口顶开始"，且被盖住的 chrome 点不到）。宁可先藏着等
    // 下一次同步，也不能错位显示。
    let rect_sane = session
        .last_rect
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|(_, y, w, h)| y >= 40 && w >= 200 && h >= 200)
        .unwrap_or(false);
    let want_label: Option<String> = if session.showing() && rect_sane {
        match visible.as_deref() {
            Some(vid) => tabs.iter().find(|t| t.id == vid).and_then(|t| t.shell_label.clone()),
            None => None,
        }
    } else {
        None
    };
    let cur = session.shell_shown.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if cur == want_label {
        return;
    }
    if let Some(old) = cur.as_deref() {
        shell::visible(old, false);
    }
    if let (Some(label), Some(tab)) = (
        want_label.as_deref(),
        tabs.iter().find(|t| t.shell_label.as_deref() == want_label.as_deref()),
    ) {
        // 先摆正矩形再显示，避免在错误位置闪一下
        let (x, y, w, h) = shell_rect_for(session, tab).await;
        shell::rect(label, x, y, w, h);
        shell::visible(label, true);
    }
    *session.shell_shown.lock().unwrap_or_else(|e| e.into_inner()) = want_label;
}

/// shell 模式下一个标签页该占的矩形：视口覆盖值（宽高，来自面板内容区）拼上面板
/// 最近一次上报的左上角；都没有就用默认值。
async fn shell_rect_for(session: &Arc<BrowserSession>, tab: &WebTab) -> (f64, f64, f64, f64) {
    let last = *session.last_rect.lock().unwrap_or_else(|e| e.into_inner());
    let (x, y) = last.map(|(x, y, _, _)| (x as f64, y as f64)).unwrap_or((0.0, 0.0));
    let (w, h) = match *tab.viewport.lock().await {
        Some((w, h)) => (w as f64, h as f64),
        None => last
            .map(|(_, _, w, h)| (w as f64, h as f64))
            .unwrap_or((1024.0, 768.0)),
    };
    (x, y, w, h)
}

/// 挂起一个后台标签页：停 screencast → frozen（停 JS 定时器与渲染产出）。
async fn suspend_tab(tab: &Arc<WebTab>) {
    ensure_screencast(tab, false).await;
    if let Err(e) = tab.cdp.call("Page.setWebLifecycleState", json!({ "state": "frozen" })).await {
        log("browser.suspend.fail", json!({ "id": tab.id, "err": e }));
        return;
    }
    tab.suspended.store(true, Ordering::Relaxed);
    log("browser.tab.suspend", json!({ "id": tab.id }));
}

/// 恢复一个被挂起的标签页：解冻 + 重放视口覆盖 + 补一刀 Page.enable。
async fn ensure_live(tab: &Arc<WebTab>) -> Result<(), String> {
    if !tab.suspended.swap(false, Ordering::Relaxed) {
        return Ok(());
    }
    tab.cdp.call("Page.setWebLifecycleState", json!({ "state": "active" })).await?;
    let _ = tab.cdp.call("Page.enable", json!({})).await;
    if let Some((w, h)) = *tab.viewport.lock().await {
        let _ = apply_viewport_override(tab, w, h).await;
    }
    tab.touch();
    log("browser.tab.restore", json!({ "id": tab.id }));
    Ok(())
}

async fn apply_viewport_override(tab: &WebTab, w: i64, h: i64) -> Result<(), String> {
    tab.cdp
        .call(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": w, "height": h, "deviceScaleFactor": 1, "mobile": false }),
        )
        .await
        .map(|_| ())
}

async fn close_internal(reason: &str) {
    let dead = SESSION.lock().await.take();
    if let Some(s) = dead {
        // shell 模式：壳里的 child webview 逐个关掉（没有进程可杀）
        if s.mode == HostMode::Shell {
            let tabs = s.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
            for t in &tabs {
                if let Some(label) = t.shell_label.as_deref() {
                    shell::close(label);
                }
            }
            *s.shell_shown.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        kill_child(&s);
        log("browser.close", json!({ "reason": reason }));
    }
}

// —— 标签页事件任务（screencast 帧 / JS 弹窗；P21）——

/// 每标签页一个事件任务：消费 CDP 事件广播。retired 后退出；actor 退出（WS 断）即退。
fn spawn_tab_events(tab: Arc<WebTab>) {
    tokio::spawn(async move {
        let mut rx = tab.cdp.subscribe_events();
        loop {
            if tab.retired.load(Ordering::Relaxed) {
                break;
            }
            match rx.recv().await {
                Ok((method, params)) => match method.as_str() {
                    "Page.screencastFrame" => {
                        if tab.screencast_on.load(Ordering::Relaxed)
                            && let Some(data) = params.get("data").and_then(Value::as_str)
                            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data)
                        {
                            *tab.frame.lock().await = Some(Frame { data: bytes });
                        }
                        // ack 必须回：不回 Chromium 会停止产帧
                        let _ = tab
                            .cdp
                            .call(
                                "Page.screencastFrameAck",
                                json!({ "sessionId": params.get("sessionId").cloned().unwrap_or(Value::Null) }),
                            )
                            .await;
                    }
                    "Page.javascriptDialogOpening" => {
                        let info = DialogInfo {
                            kind: params.get("type").and_then(Value::as_str).unwrap_or("alert").to_string(),
                            message: params.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
                            default_prompt: params
                                .get("defaultPrompt")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        };
                        *tab.dialog.lock().await = Some(info);
                        log("browser.dialog", json!({ "id": tab.id, "type": params.get("type") }));
                    }
                    "Page.javascriptDialogClosed" => {
                        *tab.dialog.lock().await = None;
                    }
                    // 页面导航（用户点链接、JS 跳转等不走 navigate_tab 的路径）会销毁
                    // 隔离世界 context——这里主动清缓存，下次求值自动重建
                    "Page.loadEventFired" | "Page.frameNavigated" | "Page.navigatedWithinDocument" => {
                        tab.cdp.invalidate_worlds();
                    }
                    _ => {}
                },
                // 帧消费慢导致丢中间帧：最新帧胜出，可接受；dialog 低频不受影响
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
}

/// screencast 开关（幂等）：开=Page.startScreencast(jpeg q75)，关=Page.stopScreencast。
async fn ensure_screencast(tab: &Arc<WebTab>, on: bool) {
    if on {
        if tab.screencast_on.swap(true, Ordering::Relaxed) {
            return;
        }
        let r = tab
            .cdp
            .call("Page.startScreencast", json!({ "format": "jpeg", "quality": 75, "everyNthFrame": 1 }))
            .await;
        if r.is_err() {
            tab.screencast_on.store(false, Ordering::Relaxed);
        }
    } else if tab.screencast_on.swap(false, Ordering::Relaxed) {
        let _ = tab.cdp.call("Page.stopScreencast", json!({})).await;
    }
}

/// 页面标题与地址（两次轻量求值；隔离世界，页面篡改不了）。
async fn tab_title_url(tab: &WebTab) -> (String, String) {
    // 有 JS 弹窗时页面脚本全阻塞（隔离世界求值也跑不了）——别白等 10s 超时，
    // 标题/URL 留空，面板走弹窗条
    if tab.dialog.lock().await.is_some() {
        return (String::new(), String::new());
    }
    let title = tab
        .cdp
        .evaluate_in_isolated_world(ISOLATED_WORLD, "document.title")
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let url = tab
        .cdp
        .evaluate_in_isolated_world(ISOLATED_WORLD, "location.href")
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    (title, url)
}

// —— DOM 快照（JS 注入，隔离世界）——

/// 页面结构抽取：可交互元素（elements，带 ref/selector/rect/inViewport/parentRef/disabled/
/// checked）+ 语义正文节点（dom，不占 ref）。双结构移植自 ZCode browserSnapshotSchema——
/// 正文节点不消耗 action ref 预算；截断时先保证模型看到页面语义。
const SNAPSHOT_JS: &str = r#"((MAXN_IN, INCLUDE_HIDDEN) => {
  const MAXN = MAXN_IN || 400, MAXTEXT = 120, MAXDEPTH = 24;
  const out = [];
  let seq = 0;
  document.querySelectorAll('[data-agent-ref]').forEach((el) => {
    const n = parseInt(el.getAttribute('data-agent-ref').slice(1), 10);
    if (!isNaN(n) && n > seq) seq = n;
  });
  const INTERACTIVE = 'a[href],button,input,textarea,select,[role],[onclick],[tabindex],summary,details,label,[contenteditable]';
  const skip = new Set(['script','style','noscript','template','svg','path','head','meta','link','iframe','canvas']);
  const SEMANTIC = ['p','li','td','th','label','blockquote','figcaption','dd','dt','summary','span','h1','h2','h3','h4','h5','h6','pre','code','strong','em','b','i','a'];
  const inView = (el) => {
    const r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) return false;
    return r.bottom > 0 && r.right > 0 && r.top < innerHeight && r.left < innerWidth;
  };
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
    return s.replace(/\s+/g, ' ').trim();
  };
  const depthOf = (el) => { let d = 0, p = el.parentElement; while (p && d < MAXDEPTH) { d++; p = p.parentElement; } return d; };
  const parentRefOf = (el) => {
    let p = el.parentElement;
    while (p) { const r = p.getAttribute && p.getAttribute('data-agent-ref'); if (r) return r; p = p.parentElement; }
    return null;
  };
  // 有界稳定 selector：id / data-testid / aria-label / name / 标签+nth-of-type
  const selOf = (el) => {
    try {
      if (el.id) return '#' + CSS.escape(el.id);
      const t = el.getAttribute('data-testid'); if (t) return '[data-testid="' + t + '"]';
      const a = el.getAttribute('aria-label'); if (a) return '[aria-label="' + a + '"]';
      const n = el.getAttribute('name');
      if (n && ['INPUT','TEXTAREA','SELECT'].includes(el.tagName)) return el.tagName.toLowerCase() + '[name="' + n + '"]';
      let node = el, path = el.tagName.toLowerCase(), hop = 0;
      while (node.parentElement && hop < 4) {
        const p = node.parentElement;
        const sibs = Array.prototype.filter.call(p.children, (c) => c.tagName === node.tagName);
        if (sibs.length > 1) { path += ':nth-of-type(' + (sibs.indexOf(node) + 1) + ')'; break; }
        node = p; hop++;
      }
      return path;
    } catch (e) { return el.tagName.toLowerCase(); }
  };
  const pushEl = (el) => {
    if (out.length >= MAXN) return;
    const tag = el.tagName.toLowerCase();
    const rect = el.getBoundingClientRect();
    let ref = el.getAttribute('data-agent-ref');
    if (!ref) { seq += 1; ref = 'e' + seq; el.setAttribute('data-agent-ref', ref); }
    out.push({
      ref, tag, role: roleOf(el),
      name: nameOf(el),
      text: directText(el).slice(0, MAXTEXT),
      value: (tag === 'input' || tag === 'textarea' || tag === 'select') ? String(el.value ?? '').slice(0, MAXTEXT) : null,
      href: tag === 'a' && el.href ? el.href.slice(0, 200) : null,
      disabled: !!(el.disabled || el.getAttribute('aria-disabled') === 'true'),
      checked: (tag === 'input' && (el.type === 'checkbox' || el.type === 'radio')) ? !!el.checked : null,
      selector: selOf(el),
      rect: [Math.round(rect.x), Math.round(rect.y), Math.round(rect.width), Math.round(rect.height)],
      inViewport: inView(el),
      parentRef: parentRefOf(el),
      depth: depthOf(el),
    });
  };
  document.querySelectorAll(INTERACTIVE).forEach((el) => {
    const tag = el.tagName.toLowerCase();
    if (skip.has(tag)) return;
    if (!INCLUDE_HIDDEN && !el.getClientRects().length) return; // 不可见（display:none / 零尺寸）
    pushEl(el);
  });
  const dom = [];
  const domPush = (el, depth) => {
    if (!el || dom.length >= MAXN || depth > MAXDEPTH) return;
    const tag = el.tagName.toLowerCase();
    if (skip.has(tag)) return;
    const text = directText(el);
    if (SEMANTIC.includes(tag) && text) {
      dom.push({
        tag, depth, text: text.slice(0, MAXTEXT),
        ref: el.getAttribute('data-agent-ref') || null,
        role: el.getAttribute('role') || null,
        name: nameOf(el),
        inViewport: inView(el),
      });
    }
    for (const c of el.children) domPush(c, depth + 1);
  };
  domPush(document.body, 0);
  return {
    url: location.href, title: document.title,
    elements: out, dom,
    truncated: out.length >= MAXN, domTruncated: dom.length >= MAXN,
  };
})"#;

/// 解析快照 JS 返回值 → (elements, dom)。
fn parse_snapshot(value: &Value) -> Result<(Vec<OutlineNode>, Vec<DomNode>), String> {
    let mut elements = Vec::new();
    if let Some(arr) = value.get("elements").and_then(Value::as_array) {
        for n in arr {
            elements.push(OutlineNode {
                reference: n.get("ref").and_then(Value::as_str).unwrap_or_default().to_string(),
                tag: n.get("tag").and_then(Value::as_str).unwrap_or_default().to_string(),
                role: n.get("role").and_then(Value::as_str).map(str::to_string),
                name: n.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                text: n.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
                value: n.get("value").and_then(Value::as_str).map(str::to_string),
                href: n.get("href").and_then(Value::as_str).map(str::to_string),
                depth: n.get("depth").and_then(Value::as_u64).unwrap_or(0) as usize,
                selector: n.get("selector").and_then(Value::as_str).unwrap_or_default().to_string(),
                in_viewport: n.get("inViewport").and_then(Value::as_bool).unwrap_or(false),
                disabled: n.get("disabled").and_then(Value::as_bool).unwrap_or(false),
                checked: n.get("checked").and_then(Value::as_bool),
                parent_reference: n.get("parentRef").and_then(Value::as_str).map(str::to_string),
            });
        }
    }
    let mut dom = Vec::new();
    if let Some(arr) = value.get("dom").and_then(Value::as_array) {
        for n in arr {
            dom.push(DomNode {
                tag: n.get("tag").and_then(Value::as_str).unwrap_or_default().to_string(),
                depth: n.get("depth").and_then(Value::as_u64).unwrap_or(0) as usize,
                in_viewport: n.get("inViewport").and_then(Value::as_bool).unwrap_or(false),
                reference: n.get("ref").and_then(Value::as_str).map(str::to_string),
                role: n.get("role").and_then(Value::as_str).map(str::to_string),
                name: n.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                text: n.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
            });
        }
    }
    Ok((elements, dom))
}

/// 渲染一行可交互元素。
fn render_node(n: &OutlineNode) -> String {
    let indent = "  ".repeat(n.depth.min(12));
    let role = n.role.clone().unwrap_or_else(|| n.tag.clone());
    let mut line = format!("{indent}[{role} {}]", n.reference);
    if n.disabled {
        line.push_str(" 禁用");
    }
    if n.checked == Some(true) {
        line.push_str(" 已选");
    }
    if !n.in_viewport {
        line.push_str(" 屏外");
    }
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

/// 渲染一行语义正文节点。
fn render_dom_node(n: &DomNode) -> String {
    let indent = "  ".repeat(n.depth.min(12));
    let role = n.role.clone().unwrap_or_else(|| n.tag.clone());
    let mut line = format!("{indent}⟨{role}");
    if let Some(r) = &n.reference {
        line.push_str(&format!(" {r}"));
    }
    line.push('⟩');
    if !n.name.is_empty() {
        line.push_str(&format!(" \"{}\"", n.name));
    }
    if !n.text.is_empty() {
        line.push_str(&format!(" {}", n.text));
    }
    if !n.in_viewport {
        line.push_str("（屏外）");
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

/// 快照：隔离世界抽结构 → diff → 渲染（elements + dom 双结构）。
/// max_elements/include_hidden 来自 snapshot 命令（对齐 ZCode 的同名参数）。
async fn take_snapshot(
    tab: &WebTab,
    max_chars: usize,
    max_elements: Option<usize>,
    include_hidden: bool,
) -> Result<String, String> {
    let expr = format!(
        "{SNAPSHOT_JS}({}, {})",
        max_elements.unwrap_or(0),
        include_hidden
    );
    let value = tab
        .cdp
        .evaluate_in_isolated_world(ISOLATED_WORLD, &expr)
        .await
        .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e).to_string())?;
    let url = value.get("url").and_then(Value::as_str).unwrap_or("").to_string();
    let title = value.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    let truncated = value.get("truncated").and_then(Value::as_bool).unwrap_or(false);
    let dom_truncated = value.get("domTruncated").and_then(Value::as_bool).unwrap_or(false);
    let (nodes, dom) = parse_snapshot(&value)?;

    // ref → selector 映射落盘（ref 失效后兜底定位）
    for n in &nodes {
        tab.remember_selector(&n.reference, &n.selector).await;
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
    out.push_str("--- 可交互元素（ref 供 click/type/fill/hover/select/check 定位；「屏外」需先 scroll）---\n");
    out.push_str(&nodes.iter().map(render_node).collect::<Vec<_>>().join("\n"));
    if truncated {
        out.push_str("\n（元素过多，快照已截断——可用更精确的 ref 操作已知元素）");
    }
    if !dom.is_empty() {
        out.push_str("\n--- 页面正文 ---\n");
        out.push_str(&dom.iter().map(render_dom_node).collect::<Vec<_>>().join("\n"));
        if dom_truncated {
            out.push_str("\n（正文节点过多，已截断）");
        }
    }
    Ok(crate::context::compress(&out, max_chars))
}

// —— 元素定位（语义为主，selector 兜底） ——

/// 定位 JS 返回：{ok, x, y, tag, name, how} 或 {ok:false}。
/// 顺序：ref → selector → 文本/占位符语义匹配。
const LOCATE_JS: &str = r#"((ref, selector, text, label) => {
  let el = null, how = '';
  if (ref) { try { el = document.querySelector('[data-agent-ref="' + ref + '"]'); how = 'ref'; } catch (e) {} }
  if (!el && selector) { try { el = document.querySelector(selector); how = 'selector'; } catch (e) {} }
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
    if (el) how = text ? 'text' : 'label';
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
    how,
  };
})"#;

/// 修饰键 → CDP 位掩码（Alt=1, Ctrl=2, Meta=4, Shift=8）。
fn modifier_mask(modifiers: &Option<Vec<KeyModifier>>) -> i64 {
    let Some(list) = modifiers else { return 0 };
    let mut mask = 0;
    for m in list {
        mask |= match m {
            KeyModifier::Alt => 1,
            KeyModifier::Control => 2,
            KeyModifier::Meta => 4,
            KeyModifier::Shift => 8,
        };
    }
    mask
}

/// 解析定位参数 → 视口坐标与目标描述。ref → selector（兜底）→ 文本语义 → 坐标。
async fn locate(
    tab: &WebTab,
    reference: &str,
    text: &str,
    label: &str,
) -> Result<(i64, i64, String), BrowserError> {
    let reference = reference.trim();
    let text = text.trim();
    let label = label.trim();
    let mut selector = String::new();
    if !reference.is_empty() {
        // ref 在页面上已失效时，用快照时存下的 selector 再试一次
        selector = tab.selector_for(reference).await.unwrap_or_default();
    }
    if !reference.is_empty() || !selector.is_empty() || !text.is_empty() || !label.is_empty() {
        let expr = format!(
            "{LOCATE_JS}({}, {}, {}, {})",
            serde_json::to_string(reference).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(&selector).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(label).unwrap_or_else(|_| "\"\"".into())
        );
        let val = tab
            .cdp
            .evaluate_in_isolated_world(ISOLATED_WORLD, &expr)
            .await
            .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e))?;
        if val.get("ok").and_then(Value::as_bool) == Some(true) {
            let x = val.get("x").and_then(Value::as_i64).unwrap_or(0);
            let y = val.get("y").and_then(Value::as_i64).unwrap_or(0);
            let tag = val.get("tag").and_then(Value::as_str).unwrap_or("?").to_string();
            let name = val.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let how = val.get("how").and_then(Value::as_str).unwrap_or("?");
            return Ok((x, y, format!("{tag} \"{name}\"（{how} 定位）")));
        }
        if !reference.is_empty() {
            return Err(BrowserError::new(
                ErrorCode::RefNotFound,
                format!("ref {reference} 在页面上已不存在（DOM 变了）。先 browser snapshot 拿最新 ref；或给 text/label 语义定位、x/y 坐标兜底"),
            ));
        }
        return Err(BrowserError::new(
            ErrorCode::RefNotFound,
            "语义定位失败：文本/占位符没有可见匹配。可先 browser snapshot 拿最新 ref，或用坐标 x/y 兜底",
        ));
    }
    Err(BrowserError::new(
        ErrorCode::CapabilityUnsupported,
        "缺少定位参数：给 ref（快照编号）、text/label（语义匹配）或 x/y（视口坐标）之一",
    ))
}

async fn dispatch_click(
    tab: &WebTab,
    x: i64,
    y: i64,
    button: MouseButton,
    modifiers: i64,
) -> Result<(), String> {
    let (x, y) = (x as f64, y as f64);
    tab.cdp
        .call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": x, "y": y, "modifiers": modifiers }),
        )
        .await?;
    for t in ["mousePressed", "mouseReleased"] {
        tab.cdp
            .call(
                "Input.dispatchMouseEvent",
                json!({ "type": t, "x": x, "y": y, "button": button.as_str(), "clickCount": 1, "modifiers": modifiers }),
            )
            .await?;
    }
    Ok(())
}

/// 点击后的「沉降」：多数点击不触发导航，不能死等——轮询 readyState，
/// 没出现 loading（约 400ms 内）就返回；真导航了则等到加载完成（上限 6s）。
async fn click_and_settle(
    tab: &WebTab,
    x: i64,
    y: i64,
    button: MouseButton,
    modifiers: i64,
) -> Result<(), String> {
    dispatch_click(tab, x, y, button, modifiers).await?;
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
    tab.cdp.invalidate_worlds();
    Ok(())
}

// —— 对工具层暴露的操作（统一命令分发；P21）——

async fn current_session() -> Result<Arc<BrowserSession>, String> {
    let mut guard = SESSION.lock().await;
    // 先自愈：壳重启后旧会话里的 tab 可能全死了，不清就会把僵尸 tab 给出去
    if let Some(s) = guard.as_ref() {
        if browser_alive(s.port).await {
            let _ = reap_dead_tabs(&mut guard).await;
        } else {
            log("browser.stale", json!({}));
            if let Some(dead) = guard.take() {
                kill_child(&dead);
            }
        }
    }
    match guard.as_ref() {
        Some(s) => Ok(s.clone()),
        None => Err("浏览器未打开：先打开一个网页".into()),
    }
}

/// 页面状态（隔离世界求值 + 导航历史）。
async fn page_state(tab: &WebTab) -> Result<PageState, BrowserError> {
    let (title, url) = tab_title_url(tab).await;
    let mut st = PageState { url, title, ..Default::default() };
    if let Ok(hist) = tab.cdp.call("Page.getNavigationHistory", json!({})).await {
        let index = hist.get("currentIndex").and_then(Value::as_i64).unwrap_or(0);
        let count = hist.get("entries").and_then(Value::as_array).map(|a| a.len() as i64).unwrap_or(0);
        st.can_go_back = index > 0;
        st.can_go_forward = index + 1 < count;
    }
    let ready = tab
        .cdp
        .evaluate_in_isolated_world(ISOLATED_WORLD, "document.readyState")
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    st.loading = !ready.is_empty() && ready != "complete";
    if let Some((w, h)) = *tab.viewport.lock().await {
        st.viewport_w = w;
        st.viewport_h = h;
    }
    Ok(st)
}

fn check_url(url: &str) -> Result<(), BrowserError> {
    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("file://") || url == "about:blank" {
        Ok(())
    } else {
        Err(BrowserError::new(
            ErrorCode::NavigationBlocked,
            "地址必须以 http:// / https:// / file:// 开头（或 about:blank）",
        ))
    }
}

/// 在标签页里导航并等加载完成；返回超时提示（None=正常）。会话死了返回 Err。
async fn navigate_tab(tab: &WebTab, url: &str) -> Result<Option<&'static str>, String> {
    let nav = tab
        .cdp
        .call_and_wait_event("Page.navigate", json!({ "url": url }), "Page.loadEventFired", LOAD_TIMEOUT)
        .await;
    match nav {
        Ok(_) => {
            tab.cdp.invalidate_worlds(); // 导航销毁了旧的隔离世界 context
            Ok(None)
        }
        Err(e) if e.contains("等待事件超时") => Ok(Some("（页面加载未在预算内完成，以下为当前状态）\n")),
        Err(e) => Err(e),
    }
}

/// 历史导航（后退/前进）：Page.getNavigationHistory + navigateToHistoryEntry。
async fn history_go(tab: &WebTab, offset: i64) -> Result<(), BrowserError> {
    let hist = tab
        .cdp
        .call("Page.getNavigationHistory", json!({}))
        .await
        .map_err(exec_err)?;
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
            .await
            .map_err(exec_err)?;
        // 给导航一点沉降时间（不硬等 load：历史条目可能早已缓存）
        tokio::time::sleep(Duration::from_millis(400)).await;
        return Ok(());
    }
    Err(BrowserError::new(
        ErrorCode::CapabilityUnsupported,
        "没有更早/更晚的历史页",
    ))
}

/// wait_for 轮询 JS（隔离世界）：selector 出现/消失、文本出现/消失。
const WAIT_FOR_JS: &str = r#"((mode, needle) => {
  if (mode === 'selector') return !!document.querySelector(needle);
  if (mode === 'selectorGone') return !document.querySelector(needle);
  const body = document.body ? document.body.innerText : '';
  if (mode === 'text') return body.includes(needle);
  return !body.includes(needle);
})"#;

async fn wait_for(tab: &WebTab, cmd: &BrowserCommand) -> Result<String, BrowserError> {
    let (selector, text, text_gone, timeout_ms) = match cmd {
        BrowserCommand::WaitFor { selector, text, text_gone, timeout_ms, .. } => {
            (selector.clone(), text.clone(), text_gone.clone(), *timeout_ms)
        }
        _ => unreachable!(),
    };
    let (mode, needle) = if let Some(s) = selector {
        ("selector", s)
    } else if let Some(t) = text {
        ("text", t)
    } else if let Some(t) = text_gone {
        ("textGone", t)
    } else {
        return Err(BrowserError::new(
            ErrorCode::CapabilityUnsupported,
            "wait_for 需要 selector / text / text_gone 之一",
        ));
    };
    let budget = Duration::from_millis(timeout_ms.unwrap_or(WAIT_FOR_DEFAULT_MS).min(WAIT_FOR_MAX_MS));
    let expr = format!(
        "{WAIT_FOR_JS}({}, {})",
        serde_json::to_string(mode).unwrap(),
        serde_json::to_string(&needle).unwrap()
    );
    let deadline = Instant::now() + budget;
    loop {
        let hit = tab
            .cdp
            .evaluate_in_isolated_world(ISOLATED_WORLD, &expr)
            .await
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if hit {
            return Ok(match mode {
                "selector" => format!("已出现：selector「{needle}」"),
                "selectorGone" => format!("已消失：selector「{needle}」"),
                "text" => format!("已出现：文本「{needle}」"),
                _ => format!("已消失：文本「{needle}」"),
            });
        }
        if Instant::now() > deadline {
            return Err(BrowserError::new(
                ErrorCode::Timeout,
                format!("等待超时（{}ms）：{mode}「{needle}」未满足。可先 snapshot 看当前页面", budget.as_millis()),
            ));
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

/// element_info：给坐标反查命中元素（移植自 ZCode elementInfoRuntime）。
const ELEMENT_INFO_JS: &str = r#"((x, y) => {
  const roleOf = (el) => el.getAttribute('role')
    ?? (el.matches('button,input[type=button],input[type=submit]') ? 'button'
      : el.matches('a[href]') ? 'link'
      : el.matches('input:not([type]),input[type=text],textarea') ? 'textbox' : null);
  const interactable = (el) => Boolean(roleOf(el) || el.matches('input,select,textarea,[tabindex],[contenteditable]'));
  return document.elementsFromPoint(x, y).filter((el) => interactable(el)).slice(0, 5).map((el) => {
    const r = el.getBoundingClientRect();
    return {
      tag: el.tagName.toLowerCase(),
      role: roleOf(el),
      name: (el.getAttribute('aria-label') || el.getAttribute('placeholder') || (el.innerText || '').trim() || el.value || '').slice(0, 60),
      rect: [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)],
    };
  });
})"#;

/// select：对 <select> 按 value 或可见文本选择。
const SELECT_JS: &str = r#"((sel, values) => {
  const el = document.querySelector('[data-agent-ref="' + sel + '"]');
  if (!el || el.tagName !== 'SELECT') return { ok: false, why: 'ref 不是 select' };
  const want = new Set(values);
  let hit = 0;
  for (const o of el.options) {
    const match = want.has(o.value) || want.has((o.text || '').trim());
    if (el.multiple) { o.selected = match; if (match) hit++; }
    else if (match) { el.value = o.value; hit++; break; }
  }
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return { ok: hit > 0, value: el.value };
})"#;

/// check：设置 checkbox/radio 勾选态。
const CHECK_JS: &str = r#"((sel, checked) => {
  const el = document.querySelector('[data-agent-ref="' + sel + '"]');
  if (!el) return { ok: false, why: 'ref 不存在' };
  if (el.type !== 'checkbox' && el.type !== 'radio') return { ok: false, why: 'ref 不是 checkbox/radio' };
  el.checked = checked;
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return { ok: true, checked: el.checked };
})"#;

/// 统一命令分发：agent 工具唯一入口。缺省 tab = agent 专用页。
pub async fn command(args: &str, max_chars: usize) -> Result<String, String> {
    let cmd = commands::parse(args).map_err(String::from)?;
    let out = dispatch(cmd, max_chars).await.map_err(String::from);
    // agent 碰过浏览器（不论成败）就记时间戳——面板据此自动唤起右侧栏（用户要求）。
    // 与面板显隐分开：agent 的触碰只唤醒界面，不直接把 webview 弹到聊天区上空。
    if let Ok(s) = current_session().await {
        s.agent_touched_ms.store(now_ms(), Ordering::Relaxed);
    }
    out
}

/// agent 工具的 tab 切换（new_tab / activate_tab）。**shell 模式下不写 visible_tab**：
/// visible_tab 是「面板正在看哪个页」，agent 一激活就把用户正在看的页换掉，紧接着那页
/// 还会因为「非可见 + 空闲」被驻留巡检挂起（frozen 的页面看着正常但 JS 停了，用户
/// 点击没反应——真机反馈"AI 点完之后我就点不了网站了"）。无头时代写它只为驱动
/// screencast，壳里这套语义已经不成立；agent 的页留在 tab 栏里，用户想看去点即可。
fn agent_activate(session: &Arc<BrowserSession>, id: &str) {
    session.set_agent_tab(id);
    if session.mode != HostMode::Shell {
        session.set_visible(id);
    }
}

async fn dispatch(cmd: BrowserCommand, max_chars: usize) -> Result<String, BrowserError> {
    // 会话级命令（不作用单 tab）
    match &cmd {
        BrowserCommand::NewTab => {
            let session =
                ensure_session().await.map_err(|e| BrowserError::new(ErrorCode::BackendUnavailable, e))?;
            session.touch();
            let tab = create_tab(&session, "about:blank", false)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::BackendUnavailable, e))?;
            agent_activate(&session, &tab.id);
            sweep_session(&session).await;
            return Ok(format!("已新建标签页 {}（about:blank）。browser navigate 可加载地址。", tab.id));
        }
        BrowserCommand::List => {
            let session =
                current_session().await.map_err(|e| BrowserError::new(ErrorCode::BackendUnavailable, e))?;
            session.touch();
            let briefs = session.tab_briefs().await;
            if briefs.is_empty() {
                return Ok("当前没有标签页。".into());
            }
            let mut out = String::from("标签页：\n");
            for b in &briefs {
                out.push_str(&format!(
                    "  {} {}{}{} 「{}」 {}\n",
                    b.id,
                    if b.agent { "[agent] " } else { "" },
                    if b.visible { "[可见] " } else { "" },
                    if b.suspended { "[已挂起] " } else { "" },
                    if b.title.is_empty() { "（无标题）" } else { &b.title },
                    if b.url.is_empty() { "about:blank".to_string() } else { b.url.clone() },
                ));
            }
            return Ok(out);
        }
        BrowserCommand::ActivateTab { tab_id } => {
            let session =
                current_session().await.map_err(|e| BrowserError::new(ErrorCode::BackendUnavailable, e))?;
            session.touch();
            let tab = get_tab(&session, tab_id).map_err(|e| BrowserError::new(ErrorCode::CapabilityUnsupported, e))?;
            ensure_live(&tab)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::BackendUnavailable, e))?;
            agent_activate(&session, tab_id);
            sweep_session(&session).await;
            return Ok(format!("已切换到标签页 {tab_id}（面板可见）。"));
        }
        BrowserCommand::Capabilities => {
            let host = match current_session().await {
                Ok(s) if s.mode == HostMode::Shell => "桌面壳 child webview（真页面）+ 页面级 CDP".to_string(),
                _ => "无头 Edge/Chrome + 页面级 CDP".to_string(),
            };
            return Ok(format!(
                "内置浏览器（{host}）。方法: {}。\n定位优先级: ref → selector → text/label 语义 → x/y 坐标；快照/定位/求值均在隔离世界执行，页面脚本无法篡改。截图类方法刷新网页面板画面；agent 侧读取页面请用 snapshot。",
                ALL_METHODS.join(" / ")
            ));
        }
        BrowserCommand::CloseBrowser => {
            let mode = current_session().await.ok().map(|s| s.mode);
            let alive = mode.is_some();
            close_internal("tool").await;
            if alive {
                let how = if mode == Some(HostMode::Shell) { "壳内页面已全部关闭" } else { "进程已退出" };
                return Ok(format!("浏览器已关闭（{how}；下次命令会自动重新拉起）。"));
            }
            return Ok("浏览器本来就没在运行。".into());
        }
        _ => {}
    }

    // tab 级命令：解析目标页（缺省 = agent 专用页）
    let session = ensure_session().await.map_err(|e| BrowserError::new(ErrorCode::BackendUnavailable, e))?;
    session.touch();
    let tab = match cmd.tab_id() {
        Some(id) => get_tab(&session, id).map_err(|e| BrowserError::new(ErrorCode::CapabilityUnsupported, e))?,
        None => agent_tab(&session)
            .await
            .map_err(|e| BrowserError::new(ErrorCode::BackendUnavailable, e))?,
    };
    tab.touch();
    ensure_live(&tab)
        .await
        .map_err(|e| BrowserError::new(ErrorCode::BackendUnavailable, format!("恢复挂起标签页失败: {e}")))?;

    match cmd {
        BrowserCommand::Navigate { url, .. } => {
            check_url(&url)?;
            log("browser.navigate", json!({ "url": url, "tab": tab.id }));
            let note = match navigate_tab(&tab, &url).await {
                Ok(note) => note.unwrap_or(""),
                Err(e) => {
                    if e.contains("连接已断开") {
                        close_internal("dead").await;
                    }
                    return Err(BrowserError::new(
                        ErrorCode::BackendUnavailable,
                        format!("浏览器会话异常已重置（重试 navigate 即可）: {e}"),
                    ));
                }
            };
            let snap = take_snapshot(&tab, max_chars, None, false)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e))?;
            Ok(format!("{note}{snap}"))
        }
        BrowserCommand::Back { .. } | BrowserCommand::Forward { .. } => {
            let back = matches!(cmd, BrowserCommand::Back { .. });
            history_go(&tab, if back { -1 } else { 1 }).await?;
            tab.cdp.invalidate_worlds();
            let st = page_state(&tab).await?;
            Ok(format!("已{}。\n{}", if back { "后退" } else { "前进" }, st.render()))
        }
        BrowserCommand::Reload { .. } => {
            tab.cdp.call("Page.reload", json!({})).await.map_err(exec_err)?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            tab.cdp.invalidate_worlds();
            let st = page_state(&tab).await?;
            Ok(format!("已刷新。\n{}", st.render()))
        }
        BrowserCommand::Snapshot { max_elements, include_hidden, .. } => {
            take_snapshot(&tab, max_chars, max_elements, include_hidden == Some(true))
                .await
                .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e))
        }
        BrowserCommand::Click { reference, x, y, text, button, double_click, modifiers, .. } => {
            let (px, py, desc) = match (reference.as_deref(), text.as_deref(), x, y) {
                (None, None, Some(x), Some(y)) => (x as i64, y as i64, format!("坐标 ({x},{y})")),
                (r, t, _, _) => locate(&tab, r.unwrap_or(""), t.unwrap_or(""), "").await?,
            };
            let button = button.unwrap_or_default();
            let mask = modifier_mask(&modifiers);
            if double_click == Some(true) {
                dispatch_click(&tab, px, py, button, mask).await.map_err(exec_err)?;
                dispatch_click(&tab, px, py, button, mask).await.map_err(exec_err)?;
            } else {
                click_and_settle(&tab, px, py, button, mask).await.map_err(exec_err)?;
            }
            log("browser.click", json!({ "target": desc, "x": px, "y": py, "tab": tab.id }));
            let snap = take_snapshot(&tab, max_chars, None, false)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e))?;
            Ok(format!("已点击 {desc}（{px},{py}）\n{snap}"))
        }
        BrowserCommand::Fill { reference, value, .. } => {
            let (px, py, desc) = locate(&tab, &reference, "", "").await?;
            input_into(&tab, px, py, &value, false).await.map_err(exec_err)?;
            log("browser.fill", json!({ "target": desc, "chars": value.chars().count(), "tab": tab.id }));
            let snap = take_snapshot(&tab, max_chars, None, false)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e))?;
            Ok(format!("已填入 {desc}：{} 字符\n{snap}", value.chars().count()))
        }
        BrowserCommand::Type { reference, text, label, submit, .. } => {
            let (px, py, desc) = locate(&tab, reference.as_deref().unwrap_or(""), "", label.as_deref().unwrap_or("")).await?;
            input_into(&tab, px, py, &text, submit == Some(true)).await.map_err(exec_err)?;
            log("browser.type", json!({ "target": desc, "chars": text.chars().count(), "tab": tab.id }));
            let snap = take_snapshot(&tab, max_chars, None, false)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e))?;
            Ok(format!(
                "已向 {desc} 输入 {} 字符{}\n{snap}",
                text.chars().count(),
                if submit == Some(true) { " 并回车" } else { "" }
            ))
        }
        BrowserCommand::Press { key, reference, modifiers, .. } => {
            let mask = modifier_mask(&modifiers);
            if let Some(r) = reference.as_deref()
                && !r.is_empty()
            {
                // 先点聚焦（真实事件，触发站点自己的聚焦逻辑）
                if let Ok((px, py, _)) = locate(&tab, r, "", "").await {
                    let _ = dispatch_click(&tab, px, py, MouseButton::Left, 0).await;
                }
            }
            press_key(&tab, &key, mask).await.map_err(exec_err)?;
            tokio::time::sleep(Duration::from_millis(250)).await;
            tab.cdp.invalidate_worlds();
            let st = page_state(&tab).await?;
            Ok(format!("已按键 {key}。\n{}", st.render()))
        }
        BrowserCommand::Scroll { reference, x, y, delta_x, delta_y, .. } => {
            if let Some(r) = reference.as_deref()
                && !r.is_empty()
            {
                // ref 滚动：把元素滚到视口中央
                let expr = format!(
                    "(() => {{ const el = document.querySelector('[data-agent-ref=\"{}\"]'); if (!el) return false; el.scrollIntoView({{block:'center'}}); return true; }})()",
                    r.replace('\\', "\\\\").replace('"', "\\\"")
                );
                let ok = tab
                    .cdp
                    .evaluate_in_isolated_world(ISOLATED_WORLD, &expr)
                    .await
                    .ok()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if !ok {
                    return Err(BrowserError::new(
                        ErrorCode::RefNotFound,
                        format!("ref {r} 不存在，无法滚动到它"),
                    ));
                }
            } else {
                let dx = delta_x.unwrap_or(0.0);
                let dy = delta_y.unwrap_or(0.0);
                if dx == 0.0 && dy == 0.0 {
                    return Err(BrowserError::new(
                        ErrorCode::CapabilityUnsupported,
                        "scroll 需要 ref（滚到元素）或 delta_x/delta_y（滚轮增量）",
                    ));
                }
                let (ax, ay) = (x.unwrap_or(640.0), y.unwrap_or(450.0));
                tab.cdp
                    .call(
                        "Input.dispatchMouseEvent",
                        json!({ "type": "mouseWheel", "x": ax, "y": ay, "deltaX": dx, "deltaY": dy }),
                    )
                    .await
                    .map_err(exec_err)?;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
            let pos = tab
                .cdp
                .evaluate_in_isolated_world(ISOLATED_WORLD, "[Math.round(scrollX), Math.round(scrollY)]")
                .await
                .ok()
                .and_then(|v| v.as_array().cloned())
                .map(|a| {
                    format!(
                        "（滚动位置 {},{})",
                        a.first().and_then(Value::as_i64).unwrap_or(0),
                        a.get(1).and_then(Value::as_i64).unwrap_or(0)
                    )
                })
                .unwrap_or_default();
            Ok(format!("已滚动{pos}"))
        }
        BrowserCommand::Hover { reference, x, y, .. } => {
            let (px, py, desc) = match (reference.as_deref(), x, y) {
                (Some(r), _, _) if !r.is_empty() => locate(&tab, r, "", "").await?,
                (None, Some(x), Some(y)) => (x as i64, y as i64, format!("坐标 ({x},{y})")),
                _ => {
                    return Err(BrowserError::new(ErrorCode::CapabilityUnsupported, "hover 需要 ref 或 x/y"));
                }
            };
            tab.cdp
                .call("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": px as f64, "y": py as f64 }))
                .await
                .map_err(exec_err)?;
            Ok(format!("已悬停 {desc}"))
        }
        BrowserCommand::Select { reference, values, .. } => {
            let expr = format!(
                "{SELECT_JS}({}, {})",
                serde_json::to_string(&reference).unwrap(),
                serde_json::to_string(&values).unwrap()
            );
            let res = tab
                .cdp
                .evaluate_in_isolated_world(ISOLATED_WORLD, &expr)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e))?;
            if res.get("ok").and_then(Value::as_bool) != Some(true) {
                let why = res.get("why").and_then(Value::as_str).unwrap_or("没有匹配选项");
                return Err(BrowserError::new(ErrorCode::RefNotFound, format!("select 失败: {why}")));
            }
            let snap = take_snapshot(&tab, max_chars, None, false)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e))?;
            Ok(format!("已选择 {}。\n{snap}", values.join(" / ")))
        }
        BrowserCommand::Check { reference, checked, .. } => {
            let expr = format!(
                "{CHECK_JS}({}, {})",
                serde_json::to_string(&reference).unwrap(),
                serde_json::to_string(&checked.unwrap_or(true)).unwrap()
            );
            let res = tab
                .cdp
                .evaluate_in_isolated_world(ISOLATED_WORLD, &expr)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e))?;
            if res.get("ok").and_then(Value::as_bool) != Some(true) {
                let why = res.get("why").and_then(Value::as_str).unwrap_or("ref 不存在");
                return Err(BrowserError::new(ErrorCode::RefNotFound, format!("check 失败: {why}")));
            }
            let snap = take_snapshot(&tab, max_chars, None, false)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e))?;
            Ok(format!("已{}。\n{snap}", if checked.unwrap_or(true) { "勾选" } else { "取消勾选" }))
        }
        BrowserCommand::Drag { from_reference, to_reference, from, to, .. } => {
            let (fx, fy, _) = resolve_point(&tab, from_reference.as_deref(), from, "起点").await?;
            let (tx, ty, _) = resolve_point(&tab, to_reference.as_deref(), to, "终点").await?;
            tab.cdp
                .call(
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mousePressed", "x": fx as f64, "y": fy as f64, "button": "left", "clickCount": 1 }),
                )
                .await
                .map_err(exec_err)?;
            // 中间插几步 move：只发首尾的拖拽很多站点不认
            let (fx, fy, tx, ty) = (fx as f64, fy as f64, tx as f64, ty as f64);
            for i in 1..=4 {
                let mx = fx + (tx - fx) * i as f64 / 5.0;
                let my = fy + (ty - fy) * i as f64 / 5.0;
                tab.cdp
                    .call("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": mx, "y": my, "button": "left" }))
                    .await
                    .map_err(exec_err)?;
            }
            tab.cdp
                .call(
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseReleased", "x": tx, "y": ty, "button": "left", "clickCount": 1 }),
                )
                .await
                .map_err(exec_err)?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(format!("已从 ({fx},{fy}) 拖到 ({tx},{ty})"))
        }
        BrowserCommand::Screenshot { reference, full_page, clip, .. } => {
            // agent 侧拿不到图片内容（文本通道）：截图落进面板帧缓冲，用户可在面板直接看
            let mut params = json!({ "format": "jpeg", "quality": 80 });
            if full_page == Some(true) {
                params["captureBeyondViewport"] = json!(true);
            }
            // ref 元素截图：裁到元素rect（与 clip 互斥，clip 优先）
            let mut element_clip = None;
            if clip.is_none()
                && let Some(r) = reference.as_deref()
                && !r.is_empty()
            {
                let rect = tab
                    .cdp
                    .evaluate_in_isolated_world(
                        ISOLATED_WORLD,
                        &format!(
                            "(() => {{ const el = document.querySelector('[data-agent-ref=\"{}\"]'); if (!el) return null; const r = el.getBoundingClientRect(); return [Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)]; }})()",
                            r.replace('\\', "\\\\").replace('"', "\\\"")
                        ),
                    )
                    .await
                    .ok();
                match rect.as_ref().and_then(Value::as_array) {
                    Some(a) if a.len() == 4 => {
                        element_clip = Some((
                            a[0].as_f64().unwrap_or(0.0),
                            a[1].as_f64().unwrap_or(0.0),
                            a[2].as_f64().unwrap_or(0.0),
                            a[3].as_f64().unwrap_or(0.0),
                        ));
                    }
                    _ => {
                        return Err(BrowserError::new(
                            ErrorCode::RefNotFound,
                            format!("ref {r} 不存在，无法裁元素截图"),
                        ));
                    }
                }
            }
            if let Some(c) = clip {
                params["clip"] = json!({ "x": c.x, "y": c.y, "width": c.width, "height": c.height, "scale": 1 });
            } else if let Some((x, y, w, h)) = element_clip {
                params["clip"] = json!({ "x": x, "y": y, "width": w.max(1.0), "height": h.max(1.0), "scale": 1 });
            }
            let res = tab.cdp.call("Page.captureScreenshot", params).await.map_err(exec_err)?;
            let data = res
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| BrowserError::new(ErrorCode::ExecutionError, "截图结果缺 data"))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, format!("截图 base64 解码失败: {e}")))?;
            *tab.frame.lock().await = Some(Frame { data: bytes });
            let st = page_state(&tab).await?;
            Ok(format!(
                "截图已刷新到网页面板{}。\n{}",
                if full_page == Some(true) {
                    "（整页）"
                } else if element_clip.is_some() {
                    "（元素裁剪）"
                } else {
                    ""
                },
                st.render()
            ))
        }
        BrowserCommand::GetState { .. } => {
            let st = page_state(&tab).await?;
            let dialog = tab.dialog.lock().await.clone();
            let mut out = st.render();
            if let Some(d) = dialog {
                out.push_str(&format!("\n弹窗: [{}] {}", d.kind, d.message));
            }
            Ok(out)
        }
        BrowserCommand::ElementInfo { x, y, .. } => {
            let expr = format!("{ELEMENT_INFO_JS}({}, {})", x, y);
            let val = tab
                .cdp
                .evaluate_in_isolated_world(ISOLATED_WORLD, &expr)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e))?;
            let items = val.as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                return Ok(format!("({x},{y}) 处没有可交互元素"));
            }
            let mut out = format!("({x},{y}) 命中元素（从顶到底）：\n");
            for it in &items {
                let rect = it.get("rect").and_then(Value::as_array);
                let rect_s = match rect {
                    Some(r) if r.len() == 4 => format!(
                        "[{},{},{},{}]",
                        r[0].as_i64().unwrap_or(0),
                        r[1].as_i64().unwrap_or(0),
                        r[2].as_i64().unwrap_or(0),
                        r[3].as_i64().unwrap_or(0)
                    ),
                    _ => "?".into(),
                };
                out.push_str(&format!(
                    "  [{} {}] \"{}\" rect={}\n",
                    it.get("role").and_then(Value::as_str).unwrap_or("?"),
                    it.get("tag").and_then(Value::as_str).unwrap_or("?"),
                    it.get("name").and_then(Value::as_str).unwrap_or(""),
                    rect_s,
                ));
            }
            Ok(out)
        }
        BrowserCommand::Evaluate { expression, .. } => {
            let val = tab
                .cdp
                .evaluate_in_isolated_world(ISOLATED_WORLD, &expression)
                .await
                .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e))?;
            let s = serde_json::to_string(&val).unwrap_or_else(|_| val.to_string());
            Ok(format!("求值结果: {}", truncate_str(&s, 2000)))
        }
        BrowserCommand::WaitFor { .. } => wait_for(&tab, &cmd).await,
        BrowserCommand::GetDialog { .. } => {
            let dialog = tab.dialog.lock().await.clone();
            match dialog {
                Some(d) => Ok(format!(
                    "当前弹窗: [{}] {}{}",
                    d.kind,
                    d.message,
                    if d.default_prompt.is_empty() {
                        String::new()
                    } else {
                        format!("（默认值: {}）", d.default_prompt)
                    }
                )),
                None => Ok("当前没有弹窗".into()),
            }
        }
        BrowserCommand::HandleDialog { accept, prompt_text, .. } => {
            if tab.dialog.lock().await.is_none() {
                return Err(BrowserError::new(
                    ErrorCode::CapabilityUnsupported,
                    "当前没有弹窗可处理",
                ));
            }
            let mut params = json!({ "accept": accept });
            if let Some(t) = prompt_text {
                params["promptText"] = json!(t);
            }
            tab.cdp.call("Page.handleJavaScriptDialog", params).await.map_err(exec_err)?;
            *tab.dialog.lock().await = None;
            Ok(format!("已{}弹窗", if accept { "接受" } else { "取消" }))
        }
        BrowserCommand::Close { tab_id } => {
            let target = match tab_id.as_deref() {
                Some(id) => id.to_string(),
                None => tab.id.clone(),
            };
            if close_tab(&session, &target).await {
                Ok(format!("已关闭标签页 {target}"))
            } else {
                Err(BrowserError::new(
                    ErrorCode::CapabilityUnsupported,
                    format!("标签页不存在: {target}"),
                ))
            }
        }
        // 会话级命令已在函数开头分发，不会到这里
        BrowserCommand::NewTab
        | BrowserCommand::CloseBrowser
        | BrowserCommand::List
        | BrowserCommand::ActivateTab { .. }
        | BrowserCommand::Capabilities => Err(BrowserError::new(
            ErrorCode::CapabilityUnsupported,
            "该命令应在会话级处理",
        )),
    }
}

/// 拖拽点解析：ref → 坐标。
async fn resolve_point(
    tab: &WebTab,
    reference: Option<&str>,
    point: Option<Point>,
    what: &str,
) -> Result<(i64, i64, String), BrowserError> {
    if let Some(p) = point {
        return Ok((p.x as i64, p.y as i64, format!("{what} 坐标")));
    }
    if let Some(r) = reference {
        return locate(tab, r, "", "").await.map(|(x, y, d)| (x, y, format!("{what} {d}")));
    }
    Err(BrowserError::new(
        ErrorCode::CapabilityUnsupported,
        format!("drag 的{what}需要 ref 或 x/y"),
    ))
}

fn exec_err(e: String) -> BrowserError {
    if e.contains("连接已断开") {
        BrowserError::new(
            ErrorCode::BackendUnavailable,
            format!("浏览器连接已断开（标签页崩溃或浏览器退出）: {e}"),
        )
    } else {
        BrowserError::new(ErrorCode::ExecutionError, e)
    }
}

/// 输入：聚焦（真实点击）→ 全选清空 → 插文本 → 可选回车。
async fn input_into(tab: &WebTab, x: i64, y: i64, text: &str, submit: bool) -> Result<(), String> {
    dispatch_click(tab, x, y, MouseButton::Left, 0).await?;
    // Ctrl+A 全选后输入即替换（真实用户习惯，比直接置 value 更贴站点行为）
    for t in ["rawKeyDown", "keyUp"] {
        let _ = tab
            .cdp
            .call(
                "Input.dispatchKeyEvent",
                json!({ "type": t, "key": "a", "code": "KeyA", "windowsVirtualKeyCode": 65, "modifiers": 2 }),
            )
            .await;
    }
    if !text.is_empty() {
        tab.cdp.call("Input.insertText", json!({ "text": text })).await?;
    }
    if submit {
        press_key(tab, "Enter", 0).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        tab.cdp.invalidate_worlds();
    }
    Ok(())
}

/// 按键（rawKeyDown + keyUp，带修饰键位掩码）。
async fn press_key(tab: &WebTab, key: &str, modifiers: i64) -> Result<(), String> {
    let vk = virtual_key_code(key);
    for t in ["rawKeyDown", "keyUp"] {
        tab.cdp
            .call(
                "Input.dispatchKeyEvent",
                json!({ "type": t, "key": key, "windowsVirtualKeyCode": vk, "modifiers": modifiers }),
            )
            .await?;
    }
    Ok(())
}

/// 常用键的 Windows 虚拟键码（缺省 0 让 Chromium 按 key 名自己推）。
fn virtual_key_code(key: &str) -> i64 {
    match key {
        "Enter" => 13,
        "Tab" => 9,
        "Escape" => 27,
        "Backspace" => 8,
        "Delete" => 46,
        "ArrowUp" => 38,
        "ArrowDown" => 40,
        "ArrowLeft" => 37,
        "ArrowRight" => 39,
        "Home" => 36,
        "End" => 35,
        "PageUp" => 33,
        "PageDown" => 34,
        " " => 32,
        _ => 0,
    }
}

// —— 网页面板（P20.1 起，P21 帧流/驻留/dialog）——

/// 面板默认视口尺寸（launch 的 --window-size 一致；前端按比例换算点击坐标）。
pub const VIEWPORT_W: i64 = 1280;
pub const VIEWPORT_H: i64 = 900;

fn check_panel_url(url: &str) -> Result<(), BrowserError> {
    check_url(url)
}

/// 面板状态：浏览器是否在跑 + 全部标签页（含可见/加载/挂起）+ 可见页弹窗。
pub async fn panel_state() -> Value {
    let mut guard = SESSION.lock().await;
    let Some(s) = guard.as_ref() else {
        return json!({ "running": false, "tabs": [], "dialog": null, "mode": "headless" });
    };
    let mode = mode_name(s.mode);
    if !browser_alive(s.port).await {
        // 端口没了（壳关了/浏览器进程死了）：会话整体作废——下次动作会自动重建
        log("browser.stale", json!({}));
        if let Some(dead) = guard.take() {
            kill_child(&dead);
        }
        return json!({ "running": false, "tabs": [], "dialog": null, "mode": mode });
    }
    // 端口活着也要摘僵尸 tab：壳重启后 9223 复生，但旧连接的 actor 早退了
    let _ = reap_dead_tabs(&mut guard).await;
    let Some(s) = guard.as_ref() else {
        return json!({ "running": false, "tabs": [], "dialog": null, "mode": "headless" });
    };
    let mut briefs = s.tab_briefs().await;
    let visible = s.visible();
    // loading / dialog 只对可见页细算（其余页不值得花 CDP 往返）
    let mut dialog = Value::Null;
    if let Some(vid) = &visible {
        let tab = s
            .tabs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|t| &t.id == vid)
            .cloned();
        if let Some(tab) = tab {
            // 弹窗开着时先报弹窗（页面脚本阻塞，readyState 求值会白等超时）
            if let Some(d) = tab.dialog.lock().await.clone() {
                dialog = json!({ "kind": d.kind, "message": d.message, "defaultPrompt": d.default_prompt });
            } else if let Ok(ready) = tab.cdp.evaluate_in_isolated_world(ISOLATED_WORLD, "document.readyState").await
                && let Some(st) = ready.as_str()
            {
                for b in briefs.iter_mut() {
                    if b.id == tab.id {
                        b.loading = st != "complete";
                    }
                }
            }
        }
    }
    json!({
        "running": true,
        "mode": mode_name(s.mode),
        "port": s.port,
        "tabs": briefs.iter().map(TabBrief::to_json).collect::<Vec<_>>(),
        "dialog": dialog,
        // agent 工具最近一次碰浏览器的时间戳（0 = 没有过）——前端据此自动唤起面板
        "agentTouched": s.agent_touched_ms.load(Ordering::Relaxed),
        "agentTab": s.agent_tab_now(),
    })
}

/// 宿主模式名（透给前端：shell = 壳里托管真 webview，headless = 截图帧流）。
fn mode_name(mode: HostMode) -> &'static str {
    match mode {
        HostMode::Shell => "shell",
        HostMode::Headless => "headless",
    }
}

/// 面板开新标签页（可选直接导航）。
pub async fn panel_new(url: Option<String>) -> Result<Value, String> {
    let url = url.unwrap_or_else(|| "about:blank".into());
    if url != "about:blank" {
        check_panel_url(&url).map_err(|e| e.to_string())?;
    }
    let session = ensure_session().await?;
    session.touch();
    let tab = create_tab(&session, &url, false).await?;
    session.set_visible(&tab.id);
    session.set_showing(true); // 面板在建页 = 人在看
    sweep_session(&session).await;
    Ok(json!({ "id": tab.id }))
}

/// 面板导航（地址栏回车）。
pub async fn panel_navigate(id: &str, url: &str) -> Result<Value, String> {
    check_panel_url(url).map_err(|e| e.to_string())?;
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    session.set_showing(true);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    match navigate_tab(&tab, url).await {
        Ok(_) => {
            sweep_session(&session).await;
            Ok(json!({ "ok": true }))
        }
        Err(e) => {
            if e.contains("连接已断开") {
                close_internal("dead").await;
            }
            Err(format!("导航失败: {e}"))
        }
    }
}

/// 面板截图：PNG 字节（旧路径保留；帧流不可用时回落）。
pub async fn panel_screenshot(id: &str) -> Result<Vec<u8>, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    // 弹窗开着时页面脚本阻塞，readyState 求值会白等超时——直接跳过等待
    let dialog_open = tab.dialog.lock().await.is_some();
    for _ in 0..4 {
        if dialog_open {
            break;
        }
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
    let res = tab.cdp.call("Page.captureScreenshot", json!({ "format": "png" })).await?;
    let data = res
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| "截图结果缺 data".to_string())?;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("截图 base64 解码失败: {e}"))
}

/// 面板帧（P21）：screencast 最新 JPEG（内存直出，零 CDP 往返）；
/// 没有帧（screencast 刚起步/失败）时回落 captureScreenshot（PNG）。
/// P22：轮询即「有客户端要帧」的信号，驱动 screencast 门控（壳里看真页面不编码）。
pub async fn panel_frame(id: &str) -> Result<(Vec<u8>, &'static str), String> {
    let session = current_session().await?;
    session.touch();
    session.last_frame_poll_ms.store(now_ms(), Ordering::Relaxed);
    session.set_visible(id);
    session.set_showing(true); // 轮询帧 = 有客户端在面板里看
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    sweep_session(&session).await; // 可见页 screencast 拉起；首帧到达前走下面的回落
    if let Some(f) = tab.frame.lock().await.clone() {
        return Ok((f.data, "image/jpeg"));
    }
    Ok((panel_screenshot(id).await?, "image/png"))
}

/// 面板设置标签页视口（截图尺寸跟随面板大小——固定 1280×900 会在窄面板里
/// 显示得又小又留大黑边，ZCode 的面板浏览器就是视口=面板尺寸）。P22：x/y 是面板
/// 内容区在壳窗口里的左上角（shell 模式据此把 child webview 摆到面板位置上）。
pub async fn panel_viewport(id: &str, x: i64, y: i64, w: i64, h: i64) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    session.set_showing(true);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    let w = w.clamp(320, 1600);
    let h = h.clamp(400, 2400);
    // shell 模式跳过 Emulation 覆盖：壳里 webview 的 HWND 尺寸就是视口，页面按新尺寸
    // 自己重排，CDP 覆盖既多余又每次多一个往返（拖面板宽度时 100ms 一同步，能省则省）
    if session.mode != HostMode::Shell {
        apply_viewport_override(&tab, w, h).await?;
    }
    *tab.viewport.lock().await = Some((w, h));
    // shell 模式：把壳里的 webview 挪到面板内容区（用户看到的就是 CDP 驱动的那个页面）。
    // 矩形一到就立即同步显隐——不然要等最多 5s 的巡检，面板刚打开那会儿会白一片
    if session.mode == HostMode::Shell
        && let Some(label) = tab.shell_label.as_deref()
    {
        *session.last_rect.lock().unwrap_or_else(|e| e.into_inner()) = Some((x, y, w, h));
        shell::rect(label, x as f64, y as f64, w as f64, h as f64);
        let tabs = session.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let visible = session.visible();
        sync_shell_visibility(&session, &tabs, &visible).await;
    }
    Ok(json!({ "ok": true, "w": w, "h": h }))
}

/// 用系统默认浏览器打开一个地址（explorer.exe 转发，URL 里的 & 不会被 cmd 吃掉）。
fn open_url_in_system(url: &str) -> Result<Value, String> {
    Command::new("explorer")
        .arg(url)
        .creation_flags(0x0800_0000)
        .spawn()
        .map_err(|e| format!("打开系统浏览器失败: {e}"))?;
    Ok(json!({ "ok": true }))
}

/// 「在默认浏览器中打开」当前标签页。
pub async fn panel_open_external(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    let url = tab
        .cdp
        .evaluate_in_isolated_world(ISOLATED_WORLD, "location.href")
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    if url.is_empty() || url == "about:blank" {
        return Err("当前是空白页，没有可打开的地址".into());
    }
    open_url_in_system(&url)
}

/// 「打开调试工具」：打开 CDP 自带的调试端点页（`http://127.0.0.1:端口/json`）——
/// 页面里每个标签的 inspect 链接就是真正的 DevTools（Chromium 官方远程调试流程）。
/// 不直接用 /json/list 返回的 devtoolsFrontendUrl：Edge 新版把它指向 aka.ms 短链，
/// 该短链已失效（302 跳必应，真机踩过）；devtools:// scheme 不是系统注册 scheme，
/// 外链起不来。
pub async fn panel_devtools_url(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    // shell 模式：child webview 支持真 DevTools——直接指挥壳开（无头时代做不到的事）
    if session.mode == HostMode::Shell {
        let Some(label) = tab.shell_label.as_deref() else {
            return Err("该标签页不在桌面壳里（无头模式）".into());
        };
        shell::devtools(label, true);
        return Ok(json!({ "ok": true, "opened": true }));
    }
    Ok(json!({
        "ok": true,
        "opened": false,
        "hint": "无头模式的页面没有真 DevTools；点「在默认浏览器中打开」后按 F12"
    }))
}

/// 面板点击（视口坐标）→ 等可能的导航沉降。
pub async fn panel_click(id: &str, x: i64, y: i64) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    click_and_settle(&tab, x, y, MouseButton::Left, 0)
        .await
        .map_err(|e| format!("点击失败: {e}"))?;
    Ok(json!({ "ok": true }))
}

/// 面板输入：向当前聚焦元素插文本（点过输入框后用）。
pub async fn panel_type(id: &str, text: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    if !text.is_empty() {
        tab.cdp.call("Input.insertText", json!({ "text": text })).await?;
    }
    Ok(json!({ "ok": true }))
}

/// 面板按键（如 Enter 提交搜索）。
pub async fn panel_key(id: &str, key: &str, code: i64) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    for t in ["rawKeyDown", "keyUp"] {
        tab.cdp
            .call(
                "Input.dispatchKeyEvent",
                json!({ "type": t, "key": key, "windowsVirtualKeyCode": code }),
            )
            .await?;
    }
    Ok(json!({ "ok": true }))
}

/// 面板滚轮。
pub async fn panel_scroll(id: &str, delta_y: i64) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    tab.cdp
        .call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseWheel", "x": VIEWPORT_W / 2, "y": VIEWPORT_H / 2, "deltaX": 0, "deltaY": delta_y }),
        )
        .await?;
    Ok(json!({ "ok": true }))
}

pub async fn panel_back(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    history_go(&tab, -1).await.map_err(|e| e.to_string())?;
    tab.cdp.invalidate_worlds();
    Ok(json!({ "ok": true }))
}

pub async fn panel_forward(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    history_go(&tab, 1).await.map_err(|e| e.to_string())?;
    tab.cdp.invalidate_worlds();
    Ok(json!({ "ok": true }))
}

pub async fn panel_reload(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    tab.cdp.call("Page.reload", json!({})).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    tab.cdp.invalidate_worlds();
    Ok(json!({ "ok": true }))
}

/// 面板关标签页；全部关完浏览器进程自然退出。
pub async fn panel_close_tab(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    close_tab(&session, id).await;
    Ok(json!({ "ok": true }))
}

/// 「⋯」菜单开关的**服务端唯一真相源**。壳模式菜单是壳里的浮层 webview，它的关闭
/// （点菜单项/backdrop/Esc）由浮层页面自己发请求，前端主界面未必知道——所以状态记在
/// 服务端：前端的「⋯」点击一律发 toggle，由这里翻转，两边不会失同步。
static MENU_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 「⋯」菜单开合（P20.4 补丁四）。shell 模式下开菜单 = 让壳建一个**透明置顶的菜单
/// webview** 铺在面板矩形上（`shell::menu`）：它压在下层内容 webview 之上，菜单因此浮在
/// 网页上面——**网页纹丝不动、不被遮也不被藏**（对齐 ZCode；主界面的 HTML 菜单做不到，
/// 因为 child webview 是 HWND，永远盖在 Svelte 之上）。菜单页由服务同源提供
/// （/menu.html，锚点走 URL query），点击经同源 fetch 调 open-external/devtools 端点。
/// 无头模式没有壳：菜单是前端 HTML 下拉（盖在截图上，z-index 正常），这里只记账。
pub async fn panel_overlay(
    open: bool,
    toggle: bool,
    panel: Option<(f64, f64, f64, f64)>,
    anchor: Option<(f64, f64, f64, f64)>,
) -> Result<Value, String> {
    let open = if toggle {
        !MENU_OPEN.load(std::sync::atomic::Ordering::Relaxed)
    } else {
        open
    };
    MENU_OPEN.store(open, std::sync::atomic::Ordering::Relaxed);
    if !open {
        shell::menu_close();
        return Ok(json!({ "ok": true, "open": false }));
    }
    let (Some(panel), Some(anchor)) = (panel, anchor) else {
        MENU_OPEN.store(false, std::sync::atomic::Ordering::Relaxed);
        return Err("开菜单需要面板矩形与按钮锚点".into());
    };
    let session = current_session().await?;
    session.touch();
    if session.mode != HostMode::Shell {
        return Ok(json!({ "ok": true, "open": true, "shell": false }));
    }
    // 菜单项要操作的那一页：当前可见标签（没有就空——页面会禁用菜单项）
    let tab = session.visible().unwrap_or_default();
    shell::menu(panel.0, panel.1, panel.2, panel.3, anchor, &tab);
    Ok(json!({ "ok": true, "open": true, "shell": true, "tab": tab }))
}

/// 面板上报可见标签页（驱动 screencast 与驻留；对齐 ZCode activateTab）。
/// 空 id = 「面板不看了」——shell 模式下这是壳里 webview 隐藏的唯一信号。
pub async fn panel_activate(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    // 菜单开着时激活/切页签 = 浮层锚定的那一页失效了，顺手关掉壳里浮层（幂等）
    if MENU_OPEN.swap(false, std::sync::atomic::Ordering::Relaxed) {
        shell::menu_close();
    }
    if id.is_empty() {
        session.clear_visible();
        session.set_showing(false);
        sweep_session(&session).await;
        return Ok(json!({ "ok": true }));
    }
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    session.set_visible(id);
    session.set_showing(true);
    sweep_session(&session).await;
    Ok(json!({ "ok": true }))
}

/// 面板处理 JS 弹窗（页面被 alert/confirm 卡住时，用户直接在这里确定/取消）。
pub async fn panel_dialog(id: &str, accept: bool, prompt_text: Option<String>) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    if tab.dialog.lock().await.is_none() {
        return Err("当前没有弹窗".into());
    }
    let mut params = json!({ "accept": accept });
    if let Some(t) = prompt_text {
        params["promptText"] = json!(t);
    }
    tab.cdp.call("Page.handleJavaScriptDialog", params).await?;
    *tab.dialog.lock().await = None;
    Ok(json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机冒烟（ignored，手工跑：cargo test browser_smoke -- --ignored --nocapture）
    /// 本机需装 Edge/Chrome。覆盖：拉起 → file:// 快照 → 语义点击 → 增量 diff →
    /// fill → scroll → wait_for → tabs list → 关闭。
    #[tokio::test]
    #[ignore]
    async fn browser_smoke_local() {
        let _ = crate::observability::init(
            &std::env::temp_dir().join("agent-browser-test-logs"),
            "browser-test.jsonl",
            "browser-test".into(),
        );
        // fixture：一个计数按钮 + 一个搜索输入框 + 一个下拉
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
<select id="s"><option value="a">甲</option><option value="b">乙</option></select>
<p id="echo"></p>
</body></html>"#,
        )
        .unwrap();
        let url = format!("file://{}", page.display().to_string().replace('\\', "/"));

        // 1) navigate + 快照
        let out = command(&format!(r#"{{"method":"navigate","url":"{url}"}}"#), 8000)
            .await
            .expect("navigate 失败");
        println!("== navigate ==\n{out}");
        assert!(out.contains("冒烟测试页"), "快照应含标题");
        assert!(out.contains("button"), "快照应含按钮角色");

        // 2) 语义点击（文本定位）
        let out = command(r#"{"method":"click","text":"点我加一"}"#, 8000)
            .await
            .expect("click 失败");
        println!("== click ==\n{out}");
        assert!(out.contains("已点击"), "应报告点击目标");
        assert!(out.contains("计数：1"), "点击后计数应变 1");

        // 3) fill（ref 定位输入框）
        let out = command(r#"{"method":"fill","ref":"e2","value":"你好浏览器"}"#, 8000)
            .await
            .expect("fill 失败");
        println!("== fill ==\n{out}");
        assert!(out.contains("你好浏览器"), "快照应含输入值");

        // 4) select（下拉选乙）
        let out = command(r#"{"method":"select","ref":"e3","values":["乙"]}"#, 8000)
            .await
            .expect("select 失败");
        println!("== select ==\n{out}");
        assert!(out.contains("已选择"), "应报告选择结果");

        // 5) wait_for：等输入回显文本
        let out = command(r#"{"method":"wait_for","text":"输入：你好浏览器","timeout_ms":3000}"#, 8000)
            .await
            .expect("wait_for 失败");
        assert!(out.contains("已出现"), "wait_for 应命中: {out}");

        // 6) scroll + get_state + list
        let out = command(r#"{"method":"scroll","delta_y":200}"#, 8000).await.expect("scroll 失败");
        assert!(out.contains("已滚动"), "scroll 应报告: {out}");
        let out = command(r#"{"method":"get_state"}"#, 8000).await.expect("get_state 失败");
        assert!(out.contains("地址:"), "get_state 应含地址: {out}");
        let out = command(r#"{"method":"list"}"#, 8000).await.expect("list 失败");
        assert!(out.contains("agent"), "list 应含 agent 页: {out}");

        // 7) evaluate（隔离世界）
        let out = command(r#"{"method":"evaluate","expression":"document.querySelector('h1').textContent"}"#, 8000)
            .await
            .expect("evaluate 失败");
        assert!(out.contains("冒烟测试页"), "evaluate 应拿到 h1 文本: {out}");
    }

    #[test]
    fn truncate_str_caps_length() {
        assert_eq!(truncate_str("abcdef", 3), "abc…");
        assert_eq!(truncate_str("abc", 5), "abc");
    }

    #[test]
    fn diff_outline_first_snapshot_has_no_baseline() {
        let (summary, detail) = diff_outline(&[], &[]);
        assert!(summary.contains("首次快照"));
        assert!(detail.is_empty());
    }

    #[test]
    fn diff_outline_reports_added_removed_changed() {
        let old = vec![OutlineNode {
            reference: "e1".into(),
            tag: "button".into(),
            role: Some("button".into()),
            name: "旧".into(),
            text: "旧".into(),
            value: None,
            href: None,
            depth: 0,
            selector: "#old".into(),
            in_viewport: true,
            disabled: false,
            checked: None,
            parent_reference: None,
        }];
        let new = vec![
            OutlineNode {
                reference: "e1".into(),
                tag: "button".into(),
                role: Some("button".into()),
                name: "新".into(),
                text: "新".into(),
                value: None,
                href: None,
                depth: 0,
                selector: "#old".into(),
                in_viewport: true,
                disabled: false,
                checked: None,
                parent_reference: None,
            },
            OutlineNode {
                reference: "e2".into(),
                tag: "a".into(),
                role: Some("link".into()),
                name: "链接".into(),
                text: "链接".into(),
                value: None,
                href: Some("https://example.com".into()),
                depth: 0,
                selector: "a".into(),
                in_viewport: false, // 屏外
                disabled: false,
                checked: None,
                parent_reference: None,
            },
        ];
        let (summary, detail) = diff_outline(&old, &new);
        assert!(summary.contains("+1 新增"), "应报 1 个新增: {summary}");
        assert!(summary.contains("~1 变化"), "应报 1 个变化: {summary}");
        assert!(detail.iter().any(|l| l.contains("e2")), "明细应含新元素");
        // 渲染：屏外链接要带标记
        let line = render_node(&new[1]);
        assert!(line.contains("屏外"), "屏外元素应有标记: {line}");
        assert!(line.contains("→ https://example.com"), "链接应带 href: {line}");
    }

    #[test]
    fn parse_snapshot_splits_elements_and_dom() {
        let v = json!({
            "url": "https://x.test",
            "title": "T",
            "elements": [{ "ref": "e1", "tag": "button", "role": "button", "name": "OK",
                "text": "", "value": null, "href": null, "disabled": false, "checked": null,
                "selector": "#ok", "rect": [1,2,3,4], "inViewport": true, "parentRef": null, "depth": 1 }],
            "dom": [{ "tag": "p", "depth": 2, "text": "正文", "ref": null, "role": null, "name": "", "inViewport": true }],
            "truncated": false, "domTruncated": false,
        });
        let (elements, dom) = parse_snapshot(&v).unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].selector, "#ok");
        assert!(elements[0].in_viewport);
        assert_eq!(dom.len(), 1);
        assert_eq!(dom[0].text, "正文");
        // dom 行不带 ref 尖括号内容
        let line = render_dom_node(&dom[0]);
        assert!(line.contains("正文"), "dom 行应含正文: {line}");
    }

    #[test]
    fn virtual_key_codes_for_common_keys() {
        assert_eq!(virtual_key_code("Enter"), 13);
        assert_eq!(virtual_key_code("Tab"), 9);
        assert_eq!(virtual_key_code("Escape"), 27);
        assert_eq!(virtual_key_code("x"), 0);
    }

    #[test]
    fn modifier_mask_matches_cdp_bits() {
        let none: Option<Vec<KeyModifier>> = None;
        assert_eq!(modifier_mask(&none), 0);
        let ctrl = Some(vec![KeyModifier::Control]);
        assert_eq!(modifier_mask(&ctrl), 2);
        let ctrl_shift = Some(vec![KeyModifier::Control, KeyModifier::Shift]);
        assert_eq!(modifier_mask(&ctrl_shift), 10);
    }
}
