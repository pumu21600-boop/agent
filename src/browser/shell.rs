//! 桌面壳 webview 控制通道（P22）—— 服务端是指令方，壳是执行方。
//!
//! 内置浏览器搬到 Tauri 壳的 child webview 后，**标签页生命周期的唯一 owner 仍是
//! 本服务**（agent 工具与网页面板共享同一份 tab 状态）。壳通过一条环回 TCP 通道
//! （127.0.0.1:9225，换行分隔 JSON）连进来，服务向下发五类指令：
//!
//! | op | 语义 |
//! |---|---|
//! | `create` | 建 child webview（label/url/矩形/初始显隐） |
//! | `close` | 关 child webview |
//! | `rect` | 移动+缩放（跟随网页面板内容区） |
//! | `visible` | 显示/隐藏（面板切走即隐藏，免得浮层盖住 UI） |
//! | `devtools` | 开/关真 DevTools（child webview 支持，补上无头时代的缺口） |
//! | `menu` | 开「⋯」菜单：建透明置顶的菜单 webview 浮在网页上（页面不动） |
//! | `menu_close` | 关菜单 webview |
//!
//! 设计约束：
//! - **单向**：壳只收指令不回 ack——create 的确认靠 /json/list 里出现新 target
//!   （CDP 端口 9223，见 `SHELL_CDP_PORT`），其余指令幂等。
//! - **单连接**：只认第一个连进来的壳（单实例守卫下只有一个）；第二个连接直接关掉，
//!   防止两个壳互相顶替。
//! - 通道断连只意味着"指挥不动壳了"：`available()` 转假，浏览器会话回落到无头模式，
//!   不影响其他任何功能。

use std::sync::{Mutex, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use serde_json::{Value, json};

/// 壳 WebView2 环境的 CDP 端口（tauri.conf.json 的 additionalBrowserArguments 同值）。
pub const SHELL_CDP_PORT: u16 = 9223;
/// 控制通道监听端口（壳来连）。
pub const SHELL_CTL_PORT: u16 = 9225;

struct Conn {
    tx: mpsc::UnboundedSender<String>,
}

static CONN: OnceLock<Mutex<Option<Conn>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Conn>> {
    CONN.get_or_init(|| Mutex::new(None))
}

/// 壳是否连着（能指挥 webview 的前提）。
pub fn available() -> bool {
    slot().lock().map(|g| g.is_some()).unwrap_or(false)
}

/// 手动禁用 shell 模式（CDP 探测失败等自愈路径；下次壳重连自动恢复）。
pub fn disable() {
    if let Ok(mut g) = slot().lock() {
        *g = None;
    }
}

/// 起控制通道监听（服务启动时调一次；端口被占只记日志，不影响主服务）。
///
/// 单连接语义：`slot()` 的 compare-and-set 在锁内完成——已有时新连接直接放掉，
/// 不会出现两个壳互相顶替；读任务断开时注销，壳重连自然恢复。
pub fn start_listener() {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(("127.0.0.1", SHELL_CTL_PORT)).await {
            Ok(l) => l,
            Err(e) => {
                crate::observability::debug(&format!(
                    "[shell] 控制通道监听失败 {SHELL_CTL_PORT}: {e}（内置浏览器回退无头模式）"
                ));
                return;
            }
        };
        crate::observability::debug(&format!("[shell] 控制通道监听 127.0.0.1:{SHELL_CTL_PORT}"));
        loop {
            let Ok((sock, _)) = listener.accept().await else { continue };
            let (tx, mut rx) = mpsc::unbounded_channel::<String>();
            {
                let mut g = slot().lock().unwrap_or_else(|e| e.into_inner());
                if g.is_some() {
                    continue; // 已有一个壳连着，放掉新的（防互相顶替）
                }
                *g = Some(Conn { tx });
            }
            crate::observability::debug("[shell] 壳已接入控制通道");
            let (rd, mut wr) = tokio::io::split(sock);
            // 写任务：指令队列 → 套接字
            tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    if wr.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
            // 读任务：只消费 hello/探活，断开即注销
            tokio::spawn(async move {
                let mut r = BufReader::new(rd);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            if let Ok(v) = serde_json::from_str::<Value>(line.trim())
                                && v.get("op").and_then(Value::as_str) == Some("hello")
                            {
                                crate::observability::debug(&format!("[shell] 壳 hello: {}", line.trim()));
                            }
                        }
                        Err(_) => break,
                    }
                }
                disable();
                crate::observability::debug("[shell] 壳断开控制通道");
            });
        }
    });
}

/// 发一条指令（壳没连时静默丢弃——调用方按无头模式走）。
fn send(op: &Value) {
    let tx = {
        let g = slot().lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().map(|c| c.tx.clone())
    };
    if let Some(tx) = tx {
        let _ = tx.send(format!("{op}\n"));
    }
}

/// 建 child webview。label 由服务端生成（tab 身份的一部分）。
pub fn create(label: &str, url: &str, x: f64, y: f64, w: f64, h: f64, visible: bool) {
    send(&json!({
        "op": "create", "label": label, "url": url,
        "x": x, "y": y, "w": w, "h": h, "visible": visible
    }));
}

/// 清空壳里全部 child webview（服务端会话重建时调用——壳不重启时上一个会话的
/// webview 还活着，不清会一直泄漏常驻）。
pub fn reset() {
    send(&json!({ "op": "reset" }));
}

pub fn close(label: &str) {
    send(&json!({ "op": "close", "label": label }));
}

/// 开「⋯」菜单：壳建一个**透明置顶**的 child webview（label 固定 wv-menu，页面由服务
/// 同源提供 http://127.0.0.1:5173/menu.html），铺在面板矩形上——它压在下层内容 webview
/// 之上，菜单因此能浮在网页上面（页面纹丝不动，对齐 ZCode）。anchor 是「⋯」按钮相对面板
/// 左上角的矩形，tab 是当前可见标签（菜单项要操作的那一页）。按需创建=最新的子窗口=
/// 天然置顶。URL 由壳用自己的 SERVICE_URL 拼（不依赖服务的端口配置）。
pub fn menu(x: f64, y: f64, w: f64, h: f64, anchor: (f64, f64, f64, f64), tab: &str) {
    send(&json!({
        "op": "menu", "label": MENU_LABEL,
        "x": x, "y": y, "w": w, "h": h,
        "ax": anchor.0, "ay": anchor.1, "aw": anchor.2, "ah": anchor.3,
        "tab": tab
    }));
}

/// 关菜单 webview（点了菜单项/backdrop/切走面板）。
pub fn menu_close() {
    send(&json!({ "op": "menu_close", "label": MENU_LABEL }));
}

/// 菜单浮层的 webview label（壳侧同名常量）。
pub const MENU_LABEL: &str = "wv-menu";

pub fn rect(label: &str, x: f64, y: f64, w: f64, h: f64) {
    send(&json!({ "op": "rect", "label": label, "x": x, "y": y, "w": w, "h": h }));
}

pub fn visible(label: &str, visible: bool) {
    send(&json!({ "op": "visible", "label": label, "visible": visible }));
}

pub fn devtools(label: &str, open: bool) {
    send(&json!({ "op": "devtools", "label": label, "open": open }));
}
