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
use tauri_runtime::ResizeDirection;

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
/// 主 webview 的身份标记（P27）：给本壳窗口里所有 `/api/browser/*` 请求打上
/// `X-Agent-Client: shell`。服务端据此分辨「壳里那个客户端」和「用户在普通浏览器里
/// 开的第二个客户端」——浏览器客户端量的 x/y 是它自己窗口的坐标，不认出来的话会被
/// 当成壳窗口坐标去挪壳里的 child webview，页面整体漂到错误位置（用户实锤「界面卡位」）。
/// **为什么走壳注入而不是前端字段**：前端可以只升级壳这一个页面，用户在浏览器里开着的
/// 那个页面还是旧版、不带标记，靠它自觉申报是拦不住的；壳注入对任何版本的前端都生效。
/// 幂等（页面刷新后重新 eval 一次即可，故按周期补打）。
const SHELL_CLIENT_JS: &str = r#"(function () {
  if (window.__agentShellClient) return;
  window.__agentShellClient = true;
  var hit = function (u) { try { return String(u).indexOf('/api/browser/') >= 0; } catch (e) { return false; } };
  var of = window.fetch;
  if (of) {
    window.fetch = function (u, o) {
      try {
        if (hit(u)) {
          o = o || {};
          var h = o.headers;
          if (h && typeof h.set === 'function') h.set('X-Agent-Client', 'shell');
          else o.headers = Object.assign({}, h, { 'X-Agent-Client': 'shell' });
        }
      } catch (e) {}
      return of.apply(this, arguments);
    };
  }
  var oo = XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open = function (m, u) {
    this.__agentApi = hit(u);
    return oo.apply(this, arguments);
  };
  var os = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send = function () {
    try { if (this.__agentApi) this.setRequestHeader('X-Agent-Client', 'shell'); } catch (e) {}
    return os.apply(this, arguments);
  };
})();"#;
/// 「⋯」菜单浮层的 webview label（与 src/browser/shell.rs 的 MENU_LABEL 一致）
const MENU_LABEL: &str = "wv-menu";

/// 窗口控制 API 注入（无边框窗，对齐 ZCode：原生黑条撤掉，─/▢/✕ 做进内容行）。
/// 与 SHELL_CLIENT_JS 同一周期补打机制——前端不依赖 npm 包，壳注入即用；
/// 浏览器里打开的第二个客户端没有这个 API，前端据此隐藏窗口按钮（不显示按了没反应的按钮）。
const WIN_CTRL_JS: &str = r#"(function () {
  if (window.agentWinApi) return;
  var inv = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;
  if (!inv) return;
  try {
    window.agentWinApi = {
      minimize: function () { return inv('win_minimize', {}); },
      toggleMax: function () { return inv('win_toggle_maximize', {}); },
      hide: function () { return inv('win_hide', {}); },
      startResize: function (dir) { return inv('win_start_resize', { dir: dir }); },
      isMaximized: function () { return inv('win_is_maximized', {}); }
    };
    window.dispatchEvent(new Event('agentwinready'));
  } catch (e) {}
})();"#;

#[tauri::command]
fn win_minimize(win: tauri::Window) {
    let r = win.minimize();
    slog(&format!("win_minimize: result={:?}", r));
}

#[tauri::command]
fn win_toggle_maximize(win: tauri::Window) {
    let was_max = win.is_maximized().unwrap_or(false);
    let r = if was_max { win.unmaximize() } else { win.maximize() };
    slog(&format!(
        "win_toggle_maximize: label={} was_max={} size={:?} result={:?}",
        win.label(),
        was_max,
        win.outer_size().map(|s| (s.width, s.height)).ok(),
        r
    ));
}

/// 关闭 = 藏到托盘（与 CloseRequested 的处理同一语义：壳是 webview 宿主，一销毁
/// 浏览器宿主连同 CDP 端口一起没；服务独立生命周期，退出壳不动服务）。
#[tauri::command]
fn win_hide(win: tauri::Window) {
    let _ = win.hide();
}

/// 当前是否最大化（前端据此把按钮切成还原图标，对齐 ZCode 全屏态）
#[tauri::command]
fn win_is_maximized(win: tauri::Window) -> bool {
    let m = win.is_maximized().unwrap_or(false);
    slog(&format!("win_is_maximized: {m}"));
    m
}

/// 无边框窗边缘拉伸（Windows 没有原生 resize 边框，边缘热区按下时从这里起拖）。
#[tauri::command]
fn win_start_resize(win: tauri::Window, dir: String) {
    use ResizeDirection::*;
    let d = match dir.as_str() {
        "East" => East,
        "West" => West,
        "North" => North,
        "South" => South,
        "NorthEast" => NorthEast,
        "NorthWest" => NorthWest,
        "SouthEast" => SouthEast,
        "SouthWest" => SouthWest,
        _ => return,
    };
    let _ = win.start_resize_dragging(d);
}

/// 服务 exe 路径：desktop/ 的上一级就是仓库根 → `<repo>/target/release/agent.exe`
/// （开发机布局；P13 原稿多退了一级指向 Desktop\target\release，壳从来没能自己拉起过
/// 服务——以前服务总是手动起着的，bug 一直被掩盖。start.bat 改靠壳保活后必须修）。
/// 打包分发时改为同级目录探测（留口）。
fn service_exe() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(repo) = manifest.parent() {
        let p = repo.join("target").join("release").join("agent.exe");
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("target/release/agent.exe")
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
            // 2026-10-08 修：cwd 固定到 exe 所在目录（target/release），与服务自身
            // spawn_detached 的 current_dir(exe.parent()) 一致——原来多跳一级到
            // target/，两个拉起面的工作目录语义分叉（app_root 按 exe 路径兜住了主要
            // 资源，但任何显式读 cwd 的路径会拿错地方）。
            .current_dir(exe.parent().unwrap_or(Path::new(".")))
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

/// 控制指令的取值助手（省掉每个分支里重复的 op.get().and_then().unwrap_or()）。
fn op_str<'a>(op: &'a Value, key: &str, default: &'a str) -> &'a str {
    op.get(key).and_then(Value::as_str).unwrap_or(default)
}
fn op_f64(op: &Value, key: &str, default: f64) -> f64 {
    op.get(key).and_then(Value::as_f64).unwrap_or(default)
}
fn op_bool(op: &Value, key: &str, default: bool) -> bool {
    op.get(key).and_then(Value::as_bool).unwrap_or(default)
}
/// 尺寸字段（宽高有下限，防 0 尺寸的 webview）。
fn op_size(op: &Value) -> (f64, f64) {
    (
        op_f64(op, "w", 900.0).max(64.0),
        op_f64(op, "h", 600.0).max(64.0),
    )
}

/// 执行一条控制指令。create 由本（非主）线程发起：add_child 内部
/// run_on_main_thread + recv，在主线程调用会死锁。
fn dispatch(app: &tauri::AppHandle, op: &Value) {
    let kind = op_str(op, "op", "");
    let label = op_str(op, "label", "").to_string();
    if !matches!(kind, "hello" | "reset") && label.is_empty() {
        return;
    }
    let host = app.state::<WebviewHost>();
    match kind {
        "create" => {
            let url = op_str(op, "url", "about:blank");
            let (x, y) = (op_f64(op, "x", 0.0), op_f64(op, "y", 0.0));
            let (w, h) = op_size(op);
            let visible = op_bool(op, "visible", false);
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
            let (x, y) = (op_f64(op, "x", 0.0), op_f64(op, "y", 0.0));
            let (w, h) = op_size(op);
            if let Some(wv) = host.get(&label) {
                let _ = wv.set_position(LogicalPosition::new(x, y));
                let _ = wv.set_size(LogicalSize::new(w, h));
                slog(&format!("rect {label} {x},{y} {w}x{h}"));
            }
        }
        "visible" => {
            let visible = op_bool(op, "visible", false);
            if let Some(wv) = host.get(&label) {
                if let Err(e) = if visible { wv.show() } else { wv.hide() } {
                    slog(&format!("visible {label}={visible} 失败: {e}"));
                } else {
                    slog(&format!("visible {label}={visible} ok"));
                }
            }
        }
        "devtools" => {
            let open = op_bool(op, "open", true);
            if let Some(wv) = host.get(&label) {
                if open {
                    wv.open_devtools();
                } else {
                    wv.close_devtools();
                }
                slog(&format!("devtools {label} open={open}"));
            }
        }
        "menu" => {
            // 「⋯」菜单浮层（P20.4 补丁四）：透明置顶的 child webview 铺在面板矩形上，
            // 菜单页由服务同源提供（URL query 带锚点与 tab）。它压在下层内容 webview
            // 之上——菜单因此浮在网页上面，网页纹丝不动（HTML 菜单做不到：HWND 永远
            // 盖在主界面之上）。按需创建 = 最新的子窗口 = 天然置顶；同名先关旧的。
            let Some(win) = app.get_window("main") else {
                slog("menu 失败：没有 main 窗口");
                return;
            };
            if let Some(old) = host.remove(MENU_LABEL) {
                let _ = old.close();
            }
            let (x, y) = (op_f64(op, "x", 0.0), op_f64(op, "y", 0.0));
            let (w, h) = op_size(op);
            // 锚点换算成相对面板左上角（webview 内坐标）
            let (ax, ay) = (op_f64(op, "ax", 0.0) - x, op_f64(op, "ay", 0.0) - y);
            let (aw, ah) = (op_f64(op, "aw", 26.0), op_f64(op, "ah", 26.0));
            let tab = op_str(op, "tab", "");
            let url = format!(
                "http://{SERVICE_URL}/menu.html?ax={ax}&ay={ay}&aw={aw}&ah={ah}&tab={tab}"
            );
            let Ok(parsed) = url.parse::<tauri::Url>() else {
                slog("menu 失败：URL 解析不了");
                return;
            };
            let builder = WebviewBuilder::new(MENU_LABEL, WebviewUrl::External(parsed))
                .additional_browser_args(BROWSER_ARGS)
                .transparent(true)
                .initialization_script(POPUP_GUARD_JS)
                .on_new_window(|_url, _features| tauri::webview::NewWindowResponse::Deny);
            match win.add_child(builder, LogicalPosition::new(x, y), LogicalSize::new(w, h)) {
                Ok(wv) => {
                    let _ = wv.set_focus();
                    host.insert(MENU_LABEL.to_string(), wv);
                    slog(&format!("menu {MENU_LABEL} {x},{y} {w}x{h} anchor={ax},{ay}"));
                }
                Err(e) => slog(&format!("menu {MENU_LABEL} 失败: {e}")),
            }
        }
        "menu_close" => {
            if let Some(wv) = host.remove(MENU_LABEL) {
                let _ = wv.close();
                slog(&format!("menu_close {MENU_LABEL}"));
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
        .invoke_handler(tauri::generate_handler![
            win_minimize,
            win_toggle_maximize,
            win_hide,
            win_start_resize,
            win_is_maximized
        ])
        .setup(|app| {
            app.manage(WebviewHost::default());
            // 关窗 = 藏到托盘（不退出）：壳进程是 child webview 的宿主，窗口一销毁
            // 浏览器宿主连同 CDP 端口一起没——用户习惯点 X，不能一关就没了
            if let Some(win) = app.get_webview_window("main") {
                let w = win.clone();
                win.on_window_event(move |ev| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = ev {
                        api.prevent_close();
                        let _ = w.hide();
                    }
                });
                // 身份标记补打（P27）：壳窗口里的前端页面是启动时加载的，用户可能在浏览器里
                // 还开着一个旧版页面——两者都往服务端报面板矩形，服务端只认带壳标记的那个。
                // 页面刷新会丢掉注入，故按周期重打（脚本幂等，重复执行零副作用）。
                let marker = win.clone();
                std::thread::spawn(move || loop {
                    let _ = marker.eval(SHELL_CLIENT_JS);
                    let _ = marker.eval(WIN_CTRL_JS);
                    std::thread::sleep(std::time::Duration::from_secs(2));
                });
            }
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

            // 服务守护：服务进程没了（stop.bat / 强杀 / 崩溃）→ 壳优雅退出。
            // 强杀壳会给 Windows 托盘留幽灵图标—— explorer 一直显示到鼠标划过才清
            // （2026-09-29 用户实锤「每次重启服务托盘多出很多个 agent」）。优雅退出
            // 让 Tauri 自己 NIM_DELETE 托盘图标，一个都不留。5s 宽限期：容纳快速
            // stop→start 的重启（服务只是短暂不在）；真没了就退，start.bat 会带新的来。
            {
                let guard_app = app.handle().clone();
                std::thread::spawn(move || {
                    let mut dead_since: Option<std::time::Instant> = None;
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        if service_alive() {
                            dead_since = None;
                            continue;
                        }
                        let since = *dead_since.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() >= std::time::Duration::from_secs(5) {
                            slog("服务已停止超过 5s，壳优雅退出（清理托盘图标）");
                            guard_app.exit(0);
                            return;
                        }
                    }
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Agent 桌面壳启动失败");
}
