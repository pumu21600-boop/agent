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
mod command;
mod panel;
mod session;
mod snapshot;

pub mod shell;

pub use commands::{
    ALL_METHODS, BrowserCommand, BrowserError, ErrorCode, KeyModifier, MouseButton, PageState, Point,
    TabBrief, method_is_read_only,
};
pub(crate) use command::*;
pub(crate) use panel::*;
pub(crate) use session::*;
pub(crate) use snapshot::*;

/// 内置浏览器能力的载体插件名（manifest `source:"built_in"`，只带 skills 不带界面）。
pub(crate) const BROWSER_PLUGIN: &str = "browser";

/// 能力闸关掉时给的人话。agent 工具与面板接口共用——用户在两处看到同一句，
/// 才知道去哪儿把它打开。
pub(crate) const BROWSER_OFF_MSG: &str =
    "内置浏览器已停用：去 设置→插件 打开「浏览器」后才能用（停用时 agent 与面板都调不动）";

/// 浏览器能力是否在线 = 「浏览器」插件是否被停用。
///
/// 能力总闸（用户 2026-10-06 拍板）：插件开关 = 这个能力在不在。停用名单由
/// `plugin::ui::set_disabled` 在启动与启停端点两处灌入，与技能停用同一份数据源。
pub(crate) fn capability_on() -> bool {
    !crate::plugin::ui::disabled().iter().any(|d| d == BROWSER_PLUGIN)
}

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
pub(crate) struct OutlineNode {
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
pub(crate) struct DomNode {
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
    /// agent 工具（browser 统一工具）当前的工作页（跨站点导航会迁到新页，旧页摘牌）。
    /// AtomicBool：agent 换页时要能摘掉旧页的标记（见 `agent_tab`）。
    pub agent: AtomicBool,
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
    /// 导航后重置快照状态（2026-10-08 修）：diff 基线与 ref→selector 映射都是
    /// **按页面内容**编号的——整文档替换后 JS 的 seq 从 0 重编号，新页第一个交互
    /// 元素又拿到 e1。不清的话：① 旧 ref 在新页命中另一个元素（click/fill 打错
    /// 目标）；② diff 把旧基线元素误报「~变化」而非「+新增」；③ 失效 ref 的
    /// selector 兜底会在新页 querySelector 命中错误元素。
    pub(crate) async fn reset_snapshot_state(&self) {
        self.last_outline.lock().await.clear();
        self.ref_selectors.lock().await.clear();
    }
}

impl WebTab {
    fn new(id: String, agent: bool, cdp: cdp::Cdp, shell_label: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            id,
            agent: AtomicBool::new(agent),
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
pub(crate) enum HostMode {
    /// 无头 Edge/Chrome（壳不在时的兜底）
    Headless,
    /// 桌面壳 child webview（服务经 CDP + 控制通道驱动）
    Shell,
}

pub(crate) struct BrowserSession {
    mode: HostMode,
    /// 无头模式的浏览器子进程（shell 模式为 None——没有自己的进程）
    child: Option<std::sync::Mutex<Child>>,
    /// 无头子进程的生命周期保险丝（kill-on-close；随会话 Drop 一起击杀整棵树）
    job: Option<crate::jobguard::ChildJob>,
    /// CDP HTTP 端点端口（shell 模式 = 壳的 9223；无头 = 自拉实例的随机端口）
    port: u16,
    tabs: std::sync::Mutex<Vec<Arc<WebTab>>>,
    last_used_ms: AtomicU64,
    /// 面板当前可见的标签页（驱动 screencast 与驻留；None = 面板没在看）
    visible_tab: std::sync::Mutex<Option<String>>,
    /// shell 模式：壳里该显示哪个标签页的 webview。**只有壳里那个客户端能写**（面板
    /// 接口的 `native` 参数）。与 visible_tab 分开：visible_tab 是 screencast 目标，
    /// 任何客户端（含浏览器直开 5173 的第二个）都能设——它的面板切页签只该换自己的
    /// 截图；shell_tab 才是"壳里那块 HWND 显示谁"，被第二个客户端写的话，壳里的网页
    /// 会跟着那个浏览器窗口的视图换页/消失（用户实锤「界面卡位」的另一副面孔）。
    shell_tab: std::sync::Mutex<Option<String>>,
    /// **面板是否正在展示网页**（shell 模式壳里 webview 显隐的唯一依据）。
    /// 只有面板自己的动作（activate/new/viewport/frame/navigate）能置真；agent 工具的
    /// new_tab/activate_tab 不置——否则 agent 一开工就把 webview 弹到聊天区上空。
    /// **且只有壳里那个客户端能置**（面板接口的 `native` 参数）：同一个服务上可能挂着
    /// 第二个客户端（用户在普通浏览器里开 127.0.0.1:5173），它的「在看/不看」和它量的
    /// x/y 都是那个浏览器窗口的坐标——拿来置壳的显隐/矩形，壳里网页就会浮到错误位置
    /// （用户实锤「界面卡位」）。见各 panel_* 函数的 native 参数。
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
    /// shell 模式 webview label 的单调计数器（2026-10-08 修）：原先是
    /// `wv-{tabs.len()+1}`——关掉一个再建就复用同名 label，而壳侧关闭是异步的
    /// （无 ack），短暂窗口里两个 webview 同名，close/visible/rect 作用对象歧义。
    shell_label_seq: AtomicU64,
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
    /// 壳里该显示的标签页（只有壳客户端写；见 `shell_tab` 字段注释）。
    fn shell_tab(&self) -> Option<String> {
        self.shell_tab.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn set_shell_tab(&self, id: &str) {
        *self.shell_tab.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.to_string());
    }
    fn clear_shell_tab(&self) {
        *self.shell_tab.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
        // P26：面板收起时作废上次矩形——否则重新打开时壳拿旧矩形立刻显示 webview
        // （侧边栏展开动画还没完，页面先闪在旧位置再被新矩形套进去，用户反馈很突兀）
        if !v {
            *self.last_rect.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
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
                agent: t.agent.load(Ordering::Relaxed),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 后端类错误人话化：超时要让模型能自然地说「访问超时」，技术细节留尾部诊断。
    #[test]
    fn friendly_backend_error_leads_with_human_words() {
        let t = friendly_backend_error("CDP 调用超时: Page.navigate");
        assert!(t.starts_with("访问超时："), "{t}");
        assert!(t.contains("重试"), "要给出路：{t}");
        assert!(t.contains("CDP 调用超时"), "原始细节保留供诊断：{t}");
        let d = friendly_backend_error("连接已断开: pipe closed");
        assert!(d.starts_with("浏览器连接已断开："), "{d}");
        let o = friendly_backend_error("拉起失败: 没找到浏览器");
        assert!(o.starts_with("浏览器暂时不可用："), "{o}");
        assert!(o.contains("没找到浏览器"), "{o}");
        // chrome_error_note 生成的已是人话：原样透传不再二次包装
        let pass = "访问失败：页面停在浏览器错误页——不要再重试导航。";
        assert_eq!(friendly_backend_error(pass), pass);
    }

    /// 确诊「代理客户端活着、系统代理开关关闭」后文案必须终局：不许再邀请重试
    /// （2026-09-27 用户实锤：模型照旧文案连试 close_browser + 导航，多烧一分多钟）。
    #[test]
    fn diagnosed_proxy_misconfig_forbids_retry() {
        let diag = ProxyDiagnosis::ClientAliveSwitchOff("127.0.0.1:7897".into());
        let t = timeout_error_text("CDP 调用超时: Page.navigate", &diag);
        assert!(t.starts_with("访问超时："), "{t}");
        assert!(t.contains("不要再重试"), "终局文案要点名不许重试：{t}");
        assert!(t.contains("close_browser 重开都救不了"), "点名重开也没用：{t}");
        assert!(!t.contains("可以重试一次"), "不得残留重试邀请：{t}");
        assert!(t.contains("结束本轮"), "让模型直接结束并向用户解释：{t}");
        // 未确诊（探测不到可信结论）时保留原来的「给出路」文案
        let t2 = timeout_error_text("CDP 调用超时: Page.navigate", &ProxyDiagnosis::Silent);
        assert!(t2.contains("可以重试一次"), "{t2}");
        // chrome-error 落点同理：确诊 = 重试重开都点名救不了
        let c = chrome_error_text("chrome-error://chromewebdata/", &diag);
        assert!(c.contains("不要再重试导航"), "{c}");
        assert!(c.contains("重开都救不了"), "{c}");
        let c2 = chrome_error_text("chrome-error://chromewebdata/", &ProxyDiagnosis::Silent);
        assert!(c2.contains("close_browser 重开浏览器实例再试一次"), "{c2}");
    }

    /// 进程内 CDP mock：按方法回固定响应（evaluate_ok=false 时 Runtime.evaluate 回
    /// CDP 错误，触发客户端清缓存重试 + 回退导航历史——覆盖两条取源路径）。
    async fn spawn_mock_cdp(evaluate_ok: bool) -> String {
        use futures_util::StreamExt;
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = l.accept().await else { break };
                let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await else { break };
                loop {
                    let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(txt))) = ws.next().await
                    else { break };
                    let Ok(v) = serde_json::from_str::<Value>(&txt) else { continue };
                    let Some(id) = v.get("id").and_then(Value::as_u64) else { continue };
                    let method = v.get("method").and_then(Value::as_str).unwrap_or("");
                    // (是否成功, 载荷)：失败按真实 CDP 语法走顶层 error（客户端的清缓存
                    // 重试路径依赖真实错误形态，mock 不忠实就测不出回退路径）
                    let (ok, payload): (bool, Value) = match method {
                        "Page.getFrameTree" => (true, json!({ "frameTree": { "frame": { "id": "F1" } } })),
                        "Page.createIsolatedWorld" => (true, json!({ "executionContextId": 7 })),
                        "Runtime.evaluate" if evaluate_ok => {
                            (true, json!({ "result": { "value": "chrome-error://chromewebdata/" } }))
                        }
                        "Page.getNavigationHistory" => (true, json!({
                            "currentIndex": 0,
                            "entries": [ { "url": "https://www.google.com/" } ]
                        })),
                        // Runtime.evaluate 失败形态（求值不可用场景）
                        _ => (false, json!({ "message": "Execution context was destroyed", "data": "" })),
                    };
                    let body = if ok {
                        json!({ "id": id, "result": payload })
                    } else {
                        json!({ "id": id, "error": payload })
                    };
                    let _ = futures_util::SinkExt::send(
                        &mut ws,
                        tokio_tungstenite::tungstenite::Message::Text(body.to_string()),
                    )
                    .await;
                }
            }
        });
        format!("ws://127.0.0.1:{port}/devtools/page/MOCK")
    }

    /// 错误页 URL 取源（2026-09-27 实锤漏检）：chrome 错误页的导航历史仍记**原目标 URL**，
    /// 只有页面自己报的 location.href 才是 chrome-error://——旧实现只看导航历史，
    /// `chrome_error_note` 永远探不到错误页，navigate 把错误页快照当成功（ok:true）返回。
    #[tokio::test]
    async fn current_page_url_prefers_location_href() {
        // 场景一：求值可用 → location.href 胜出（导航历史给的是 google.com）
        let ws = spawn_mock_cdp(true).await;
        let cdp = cdp::Cdp::connect(&ws).await.unwrap();
        let tab = WebTab::new("T1".into(), true, cdp, None);
        let url = current_page_url(&tab).await.unwrap();
        assert_eq!(url, "chrome-error://chromewebdata/", "必须取页面自报的 href");
        let note = chrome_error_note(&tab).await;
        assert!(note.is_some_and(|n| n.contains("不要再重试导航")), "错误页必须被识别成落点失败");
        // 场景二：求值不可用（弹窗/页面楔死）→ 回退导航历史，与旧版行为一致
        let ws2 = spawn_mock_cdp(false).await;
        let cdp2 = cdp::Cdp::connect(&ws2).await.unwrap();
        let tab2 = WebTab::new("T2".into(), true, cdp2, None);
        let url2 = current_page_url(&tab2).await.unwrap();
        assert_eq!(url2, "https://www.google.com/");
    }

    /// 系统代理解析：reg query 输出 → (开关, 地址)；分号形态的 ProxyServer 也能取。
    #[test]
    fn parse_proxy_state_reads_registry_shapes() {
        // 开着
        let (on, srv) = parse_proxy_state(
            "\r\n    ProxyEnable    REG_DWORD    0x1\r\n    ProxyServer    REG_SZ    127.0.0.1:7897\r\n",
        );
        assert!(on);
        assert_eq!(srv.as_deref(), Some("127.0.0.1:7897"));
        // 关着（2026-09-27 现场就是它）
        let (on, srv) = parse_proxy_state(
            "\r\n    ProxyEnable    REG_DWORD    0x0\r\n    ProxyServer    REG_SZ    127.0.0.1:7897\r\n",
        );
        assert!(!on);
        assert_eq!(srv.as_deref(), Some("127.0.0.1:7897"));
        // 分号形态（http/https 分别配置）
        let (on, srv) = parse_proxy_state(
            "\r\n    ProxyEnable    REG_DWORD    0x1\r\n    ProxyServer    REG_SZ    http=127.0.0.1:7890;https=127.0.0.1:7897\r\n",
        );
        assert!(on);
        assert_eq!(srv.as_deref(), Some("http=127.0.0.1:7890;https=127.0.0.1:7897"));
        // 没有 ProxyServer 行
        let (on, srv) = parse_proxy_state("\r\n    ProxyEnable    REG_DWORD    0x0\r\n");
        assert!(!on);
        assert!(srv.is_none());
    }

    /// 本机代理探活：分号形态逐个试，全死 = false。
    #[test]
    fn local_proxy_alive_checks_each_candidate() {
        // 起一个真监听当代理端口
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(local_proxy_alive(&format!("127.0.0.1:{port}")));
        // 分号形态：一个活一个死，任一活即活
        assert!(local_proxy_alive(&format!("http=127.0.0.1:1;https=127.0.0.1:{port}")));
        // 全死
        assert!(!local_proxy_alive("127.0.0.1:1"));
        // 非法形态不 panic
        assert!(!local_proxy_alive("localhost"));
    }

    /// host_of / same_site：agent 跨站点导航是否开新标签的判据（纯函数）。
    #[test]
    fn host_and_same_site_edges() {
        assert_eq!(host_of("https://www.deepseek.com/"), "deepseek.com");
        assert_eq!(host_of("https://deepseek.com/en/index.html"), "deepseek.com");
        assert_eq!(host_of("http://127.0.0.1:5173/menu.html?x=1"), "127.0.0.1:5173");
        assert_eq!(host_of("about:blank"), "about:blank"); // 无 :// 的原样返回（调用方对 about:blank 有专门分支）
        assert_eq!(host_of(""), "");
        // 同站点（含 www 显式前缀差异、路径/查询差异）
        assert!(same_site("https://deepseek.com/", "https://www.deepseek.com/en"));
        assert!(same_site("https://a.com/x?y=1", "https://a.com/z"));
        // 跨站点
        assert!(!same_site("https://www.deepseek.com/", "https://www.google.com"));
        assert!(!same_site("https://deepseek.com/", "https://chat.deepseek.com/"));
        // 空 URL 不算同站点（调用方据此直接用空白 agent 页）
        assert!(!same_site("about:blank", "https://deepseek.com/"));
    }

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

    /// 能力总闸：停用名单里有 browser 就全线拒动，没有就放行。
    ///
    /// 回归 2026-10-06 用户实锤：插件开关拨到停用，agent 用 shell curl 照样能驱动
    /// 内置浏览器——因为停用只收回技能，command() 和 /api/browser/* 都没有关卡。
    /// 这个测试钉住「名单 → 判定」这一跳（判定 put 在 mod.rs 顶部那段注释旁边）。
    #[test]
    fn capability_follows_disabled_list() {
        // 存档现名单，测完还原——DISABLED 是进程级静态量，别把别人的测试带沟里
        let saved = crate::plugin::ui::disabled();
        let had_browser = saved.iter().any(|d| d == BROWSER_PLUGIN);
        let mut off = saved.clone();
        off.retain(|d| d != BROWSER_PLUGIN);
        off.push(BROWSER_PLUGIN.to_string());
        crate::plugin::ui::set_disabled(&off);
        assert!(!capability_on(), "停用名单里有 browser 时必须判定为关");
        assert!(
            BROWSER_OFF_MSG.contains("设置→插件"),
            "拒信要说人去哪儿打开: {BROWSER_OFF_MSG}"
        );

        let mut on = saved.clone();
        on.retain(|d| d != BROWSER_PLUGIN);
        crate::plugin::ui::set_disabled(&on);
        assert!(capability_on(), "没停用时必须判定为开");

        // 还原（含原本就没停用的情形）
        let mut restore = saved.clone();
        if !had_browser {
            restore.retain(|d| d != BROWSER_PLUGIN);
        } else if !restore.iter().any(|d| d == BROWSER_PLUGIN) {
            restore.push(BROWSER_PLUGIN.to_string());
        }
        crate::plugin::ui::set_disabled(&restore);
    }
}

/// 2026-10-08 修：agent 工具侧不再放行 file://——navigate 判只读 Safe，
/// read-only 档也零审批，`file:///C:/.../id_rsa` + snapshot 即绕过 read_file
/// 的工作区禁锢读任意本地文件。面板地址栏（用户自主）仍放行。
#[test]
fn agent_side_navigate_blocks_file_url_but_panel_allows() {
    assert!(check_url("https://example.com/").is_ok());
    assert!(check_url("http://example.com/").is_ok());
    assert!(check_url("about:blank").is_ok());
    let e = check_url("file:///C:/Users/test/.ssh/id_rsa").unwrap_err();
    assert_eq!(e.code, ErrorCode::NavigationBlocked, "{e:?}");
    assert!(e.message.contains("面板"), "错误要指路：{e}", e = e.message);
    // 面板侧：用户手输本地文件放行
    assert!(check_panel_url("file:///C:/report.html").is_ok());
    assert!(check_panel_url("https://example.com/").is_ok());
    assert!(check_panel_url("javascript:alert(1)").is_err());
}
