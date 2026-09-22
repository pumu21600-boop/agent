//! Agent 桌面壳（P13 → P22）—— 窗口 + 托盘 + 服务保活 + **child webview 宿主**
//!
//! 设计（窄而刚 v1）：
//! - WebView 直接加载 http://127.0.0.1:5173（前端零改动，agent.exe 是唯一真源）；
//! - 启动时探测 5173，未起则 spawn `<repo>/target/release/agent.exe`（start.bat 内置化）；
//! - 托盘：左键显示/聚焦窗口，右键菜单（打开主窗口 / 退出壳）；
//! - 壳退出**不杀**服务（服务独立生命周期）；
//! - agent.exe 路径按开发机布局解析（desktop/ 的上两级 target/release）；
//!   未来打包分发时改为同级目录探测（留口）。
//!
//! P22 内置浏览器改造：浏览器不再由服务另起一个无头 Edge 进程，而是**把网页变成
//! 壳窗口里的 child webview**（真页面、真交互、复用 App 自己的 WebView2 运行时）。
//! - 主窗口 WebView2 环境带 `--remote-debugging-port`（tauri.conf.json 的
//!   additionalBrowserArguments），child webview 带同一份 args 以复用同一环境——
//!   服务端据此用页面级 CDP 驱动这些 webview（快照/点击/输入/截图协议不变）。
//! - 壳**不决策**标签页生命周期：服务是唯一 owner，通过 127.0.0.1:9225 的
//!   控制通道（换行分隔 JSON）下发 create/close/rect/visible/devtools 五类指令；
//!   壳只负责执行（create 从控制线程发起——add_child 内部 run_on_main_thread +
//!   recv，主线程调用会死锁）。
//! - 单实例守卫：两个壳会抢 9223 的 CDP 端口，第二个壳的 webview 将不可见。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use tauri::{LogicalPosition, LogicalSize, Manager, WebviewBuilder, WebviewUrl};

const SERVICE_URL: &str = "127.0.0.1:5173";
/// 壳 WebView2 环境的 CDP 端口（与 src/browser/shell.rs 的 SHELL_CDP_PORT 保持一致）
const CDP_PORT: u16 = 9223;
/// 主窗口与 child webview 共用的 browser args（与 tauri.conf.json 的
/// additionalBrowserArguments 保持一致——环境已存在时 options 以首次创建为准，
/// child 带同一份是为防 WebView2 按 options 差异另起环境）。
/// 只带调试端口与地址，**不带** --force-device-scale-factor 之类会改变 App 自身
/// 渲染缩放的 flag（外部方案踩过的坑）。
const BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
 --remote-debugging-port=9223 --remote-debugging-address=127.0.0.1";
/// 服务端 webview 控制通道（agent.exe 监听，壳来连）
const CTL_ADDR: (&str, u16) = ("127.0.0.1", 9225);
/// 单实例守卫端口（第二个壳直接退出，不抢 CDP 端口）
const GUARD_PORT: u16 = 9224;
/// child webview 弹窗防护：window.open 直接掐掉（on_new_window 再兜一层）
const POPUP_GUARD_JS: &str = "window.open = function () { return null; };";

/// 服务 exe 路径：desktop/ 的上两级 target\release\agent.exe（开发机布局）。
fn service_exe() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|repo| repo.join("target").join("release").join("agent.exe"))
        .unwrap_or_else(|| PathBuf::from("target/release/agent.exe"))
}

fn service_alive() -> bool {
    std::net::TcpStream::connect(SERVICE_URL).is_ok()
}

/// 探测服务，未起则后台拉起（CREATE_NO_WINDOW，不随壳退出）。
fn ensure_service() -> Option<u32> {
    if service_alive() {
        println!("[desktop] 服务已在运行");
        return None;
    }
    let exe = service_exe();
    if !exe.exists() {
        eprintln!(
            "[desktop] 服务未运行且未找到 {}（先在项目根 cargo build --release）",
            exe.display()
        );
        return None;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        match Command::new(&exe)
            .current_dir(exe.parent().map(|p| p.parent().unwrap_or(p)).unwrap_or(Path::new(".")))
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn()
        {
            Ok(child) => {
                println!("[desktop] 已拉起服务 pid={}", child.id());
                Some(child.id())
            }
            Err(e) => {
                eprintln!("[desktop] 拉起服务失败: {e}");
                None
            }
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

use std::path::Path;

/// 壳日志（release 下无控制台，写文件兜底；只记 webview 宿主的诊断信息）。
fn slog(msg: &str) {
    let path = std::env::temp_dir().join("agent-shell.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{now}] {msg}");
    }
}

/// 单实例守卫：占住环回端口即视为本机唯一壳。
/// 端口被占时读 PID 锁判断「占用者是不是另一个壳」：是 → 本实例退出（两个壳会抢
/// 9223 CDP 端口，第二个的 webview 不可见也不可驱动）；不是（锁是死的/根本没有锁，
/// 9224 被无关程序占了）→ **放弃守卫继续启动**——不能因为一个无关端口把壳拦死。
/// 返回的 listener 必须保活（drop 即释放）。
fn instance_guard() -> Option<TcpListener> {
    let lock = std::env::temp_dir().join("agent-shell.lock");
    // 先读后写：写在自己 bind 成功之后，否则第二个壳会把第一个的 PID 覆盖掉、把自己
    // 误判成"没有别的壳在跑"
    let other_pid = std::fs::read_to_string(&lock)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|p| *p != std::process::id());
    let addr = ("127.0.0.1", GUARD_PORT);
    if let Ok(l) = TcpListener::bind(addr) {
        let _ = std::fs::write(&lock, std::process::id().to_string());
        return Some(l);
    }
    if other_pid.map(pid_alive).unwrap_or(false) {
        slog("另一个 Agent 壳已在运行，本实例退出");
        println!("[desktop] 另一个 Agent 壳已在运行");
        std::process::exit(0);
    }
    slog(&format!("单实例端口 {GUARD_PORT} 被非本程序占用，放弃守卫继续启动"));
    TcpListener::bind(addr).ok()
}

/// PID 是否活着（tasklist 探活；只在第二个壳启动的罕见路径上跑）。
fn pid_alive(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .creation_flags(0x0800_0000)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(true)
}

/// child webview 注册表（label → webview 句柄）。
#[derive(Default)]
struct WebviewHost {
    tabs: Mutex<HashMap<String, tauri::Webview<tauri::Wry>>>,
}

impl WebviewHost {
    fn get(&self, label: &str) -> Option<tauri::Webview<tauri::Wry>> {
        self.tabs.lock().ok()?.get(label).cloned()
    }
    fn insert(&self, label: String, wv: tauri::Webview<tauri::Wry>) {
        if let Ok(mut t) = self.tabs.lock() {
            t.insert(label, wv);
        }
    }
    fn remove(&self, label: &str) -> Option<tauri::Webview<tauri::Wry>> {
        self.tabs.lock().ok()?.remove(label)
    }
}

/// 控制通道客户端：连上服务端 9225，按行收 JSON 指令并执行；断线 1s 重连。
fn spawn_ctl_client(app: tauri::AppHandle) {
    thread::spawn(move || {
        let (host, port) = CTL_ADDR;
        loop {
            if let Ok(sock) = TcpStream::connect((host, port)) {
                slog(&format!("控制通道已连接 {host}:{port}"));
                let mut w = sock.try_clone().ok();
                if let Some(w) = w.as_mut() {
                    let hello = json!({ "op": "hello", "pid": std::process::id(), "cdp": CDP_PORT });
                    let _ = w.write_all(format!("{hello}\n").as_bytes());
                }
                let mut r = BufReader::new(sock);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line) {
                        Ok(0) => break, // 服务端关了通道
                        Ok(_) => {
                            let t = line.trim();
                            if t.is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<Value>(t) {
                                Ok(op) => dispatch(&app, &op),
                                Err(e) => slog(&format!("指令解析失败: {e} <- {t}")),
                            }
                        }
                        Err(_) => break,
                    }
                }
                slog("控制通道断开，1s 后重连");
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
}

/// 执行一条控制指令。create 由本（非主）线程发起：add_child 内部
/// run_on_main_thread + recv，在主线程调用会死锁。
fn dispatch(app: &tauri::AppHandle, op: &Value) {
    let kind = op.get("op").and_then(Value::as_str).unwrap_or("");
    let label = op.get("label").and_then(Value::as_str).unwrap_or("").to_string();
    if !matches!(kind, "hello" | "reset") && label.is_empty() {
        return;
    }
    let host = app.state::<WebviewHost>();
    match kind {
        "create" => {
            let url = op.get("url").and_then(Value::as_str).unwrap_or("about:blank");
            let x = op.get("x").and_then(Value::as_f64).unwrap_or(0.0);
            let y = op.get("y").and_then(Value::as_f64).unwrap_or(0.0);
            let w = op.get("w").and_then(Value::as_f64).unwrap_or(900.0).max(64.0);
            let h = op.get("h").and_then(Value::as_f64).unwrap_or(600.0).max(64.0);
            let visible = op.get("visible").and_then(Value::as_bool).unwrap_or(false);
            let Some(win) = app.get_window("main") else {
                slog("create 失败：没有 main 窗口");
                return;
            };
            // 同名 label 先关旧的（服务侧重试时会出现）
            if let Some(old) = host.remove(&label) {
                let _ = old.close();
            }
            let Ok(parsed) = url.parse::<tauri::Url>() else {
                slog(&format!("create 失败：URL 解析不了 {url}"));
                return;
            };
            let builder = WebviewBuilder::new(&label, WebviewUrl::External(parsed))
                .additional_browser_args(BROWSER_ARGS)
                .initialization_script(POPUP_GUARD_JS)
                .on_new_window(|_url, _features| tauri::webview::NewWindowResponse::Deny);
            match win.add_child(builder, LogicalPosition::new(x, y), LogicalSize::new(w, h)) {
                Ok(wv) => {
                    if !visible {
                        let _ = wv.hide();
                    }
                    host.insert(label.clone(), wv);
                    slog(&format!("create {label} {url} {w}x{h} visible={visible}"));
                }
                Err(e) => slog(&format!("create {label} 失败: {e}")),
            }
        }
        "reset" => {
            // 服务端会话重建：清掉全部残留 child webview（上一个会话的，不清会常驻泄漏）
            let labels: Vec<String> = {
                let g = host.tabs.lock().unwrap_or_else(|e| e.into_inner());
                g.keys().cloned().collect()
            };
            let n = labels.len();
            for l in labels {
                if let Some(wv) = host.remove(&l) {
                    let _ = wv.close();
                }
            }
            slog(&format!("reset：关了 {n} 个残留 webview"));
        }
        "close" => {
            if let Some(wv) = host.remove(&label) {
                let _ = wv.close();
                slog(&format!("close {label}"));
            }
        }
        "rect" => {
            let x = op.get("x").and_then(Value::as_f64).unwrap_or(0.0);
            let y = op.get("y").and_then(Value::as_f64).unwrap_or(0.0);
            let w = op.get("w").and_then(Value::as_f64).unwrap_or(900.0).max(64.0);
            let h = op.get("h").and_then(Value::as_f64).unwrap_or(600.0).max(64.0);
            if let Some(wv) = host.get(&label) {
                let _ = wv.set_position(LogicalPosition::new(x, y));
                let _ = wv.set_size(LogicalSize::new(w, h));
                slog(&format!("rect {label} {x},{y} {w}x{h}"));
            }
        }
        "visible" => {
            let visible = op.get("visible").and_then(Value::as_bool).unwrap_or(false);
            if let Some(wv) = host.get(&label) {
                let r = if visible { wv.show() } else { wv.hide() };
                slog(&format!("visible {label}={visible} ok={}", r.is_ok()));
                if let Err(e) = r {
                    slog(&format!("visible {label}={visible} 失败: {e}"));
                }
            }
        }
        "devtools" => {
            let open = op.get("open").and_then(Value::as_bool).unwrap_or(true);
            if let Some(wv) = host.get(&label) {
                if open {
                    wv.open_devtools();
                } else {
                    wv.close_devtools();
                }
                slog(&format!("devtools {label} open={open}"));
            }
        }
        "hello" => {}
        other => slog(&format!("未知指令: {other}")),
    }
}

fn main() {
    // 单实例：两个壳会抢 9223，第二个直接退出（listener 需保活）
    let _guard = instance_guard();

    // 服务保活先于窗口（窗口加载 5173，服务没起会白屏）
    ensure_service();

    tauri::Builder::default()
        .setup(|app| {
            app.manage(WebviewHost::default());
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

            let open = MenuItem::with_id(app, "open", "打开主窗口", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出壳（服务保持运行）", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;

            let _tray = TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().expect("缺窗口图标").clone())
                .tooltip("Agent（127.0.0.1:5173）")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| match ev.id.as_ref() {
                    "open" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = ev
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(app)?;

            // 控制通道客户端（连上服务端 9225；服务没起时每秒重试）
            spawn_ctl_client(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Agent 桌面壳启动失败");
}
