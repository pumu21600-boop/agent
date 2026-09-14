//! 服务自管理（P13 前置）：双击即用 + 后台常驻 + 开机自启，不再依赖 start/stop.bat。
//!
//! 行为：
//! - 无参数双击：5173 已在跑 → 直接打开浏览器；未跑 → 自己以后台方式再起一个（隐藏窗口，
//!   关终端不影响），然后打开浏览器；已是后台子进程则直接跑服务。
//! - `agent.exe start`：后台启动服务（已跑则只开浏览器），与双击等价但不阻塞终端。
//! - `agent.exe stop`：结束其他 agent.exe 服务进程（不杀自己）。
//! - `agent.exe status`：打印 5173 监听状态与进程信息。
//! - `agent.exe --install`：写 HKCU Run 开机自启（最小化启动）；`--uninstall` 移除。
//! - `agent.exe --minimized`：后台子进程入口（隐藏窗口跑服务）。

use std::path::PathBuf;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "AgentLocalBuild";

fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("agent.exe"))
}

/// 5173 是否已有监听（TCP 连接探针，200ms 超时）。
pub fn port_open(host: &str, port: u16) -> bool {
    let addr = format!("{host}:{port}");
    std::net::TcpStream::connect_timeout(
        &addr.parse().unwrap_or(std::net::SocketAddr::from(([127, 0, 0, 1], port))),
        std::time::Duration::from_millis(200),
    )
    .is_ok()
}

/// 本 exe 的其他服务进程（除自己外正在跑的 agent.exe）。
#[cfg(windows)]
fn sibling_pids() -> Vec<u32> {
    use std::os::windows::process::CommandExt;
    let me = std::process::id();
    let mut out = Vec::new();
    // 用 tasklist 兜底枚举（零新依赖；沙箱探针里 tasklist 受限，但自管理跑在用户干净环境）
    let list = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq agent.exe", "/FO", "CSV", "/NH"])
        .creation_flags(0x0800_0000) // 不闪控制台窗口
        .output();
    let Ok(list) = list else { return out };
    let text = String::from_utf8_lossy(&list.stdout);
    for line in text.lines() {
        // "agent.exe","1234","Console","1","17,280 K"
        let cols: Vec<&str> = line.split("\",\"").collect();
        if cols.len() >= 2 {
            if let Ok(pid) = cols[1].trim_matches('"').parse::<u32>() {
                if pid != me {
                    out.push(pid);
                }
            }
        }
    }
    out
}

#[cfg(not(windows))]
fn sibling_pids() -> Vec<u32> {
    Vec::new()
}

/// 用系统默认浏览器打开 url（Windows 用 ShellExecute，零新依赖）。
#[cfg(windows)]
fn open_browser(url: &str) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let op: Vec<u16> = std::ffi::OsStr::new("open").encode_wide().chain([0]).collect();
    let file: Vec<u16> = std::ffi::OsStr::new(url).encode_wide().chain([0]).collect();
    unsafe {
        ShellExecuteW(0, op.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
    }
}

#[cfg(not(windows))]
fn open_browser(_url: &str) {}

/// 以隐藏窗口方式再起一个自己（后台常驻，关终端不影响）。
#[cfg(windows)]
fn spawn_detached(extra_args: &[&str]) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const DETACHED_PROCESS: u32 = 0x00000008;
    let exe = exe_path();
    let workdir = exe.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    // 工作目录固定到 exe 所在目录：config.toml / web/dist / prompts 全是相对路径
    std::process::Command::new(&exe)
        .args(extra_args)
        .current_dir(&workdir)
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("后台启动失败: {e}"))
}

#[cfg(not(windows))]
fn spawn_detached(_extra_args: &[&str]) -> anyhow::Result<()> {
    anyhow::bail!("当前平台不支持后台启动")
}

/// HKCU Run 开机自启：登录后最小化启动（后台跑服务，不弹窗口）。
#[cfg(windows)]
fn set_autostart(enable: bool) -> anyhow::Result<()> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW};
    use std::os::windows::ffi::OsStrExt;
    let key_w: Vec<u16> = std::ffi::OsStr::new(RUN_KEY).encode_wide().chain([0]).collect();
    let val_w: Vec<u16> = std::ffi::OsStr::new(RUN_VALUE).encode_wide().chain([0]).collect();
    unsafe {
        if enable {
            let cmd = format!("\"{}\" --minimized", exe_path().display());
            let cmd_w: Vec<u16> = std::ffi::OsStr::new(&cmd).encode_wide().chain([0]).collect();
            let mut hkey = 0;
            let rc = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                key_w.as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_SET_VALUE,
                std::ptr::null(),
                &mut hkey,
                std::ptr::null_mut(),
            );
            if rc != 0 {
                anyhow::bail!("写开机自启注册表失败: {rc}");
            }
            let bytes = (cmd_w.len() * 2) as u32;
            let rc = RegSetValueExW(hkey, val_w.as_ptr(), 0, REG_SZ, cmd_w.as_ptr() as *const u8, bytes);
            windows_sys::Win32::Foundation::CloseHandle(hkey as _);
            if rc != 0 {
                anyhow::bail!("写开机自启注册表失败: {rc}");
            }
        } else {
            let mut hkey = 0;
            let rc = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                key_w.as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_SET_VALUE,
                std::ptr::null(),
                &mut hkey,
                std::ptr::null_mut(),
            );
            if rc == 0 {
                RegDeleteValueW(hkey, val_w.as_ptr());
                windows_sys::Win32::Foundation::CloseHandle(hkey as _);
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn set_autostart(_enable: bool) -> anyhow::Result<()> {
    anyhow::bail!("当前平台不支持开机自启")
}

/// 自管理命令分发。返回 true = 已接管（main 不再跑服务）；false = 正常跑服务。
pub fn handle(argv: &[String], host: &str, port: u16) -> bool {
    let url = format!("http://{host}:{port}");
    let arg1 = argv.get(1).map(|s| s.as_str()).unwrap_or("");
    match arg1 {
        "status" => {
            let pids = sibling_pids();
            if port_open(host, port) {
                println!("[agent] running: {url} ({} service process)", pids.len());
            } else if pids.is_empty() {
                println!("[agent] stopped: {url} no listener, no service process");
            } else {
                println!("[agent] starting: {} agent.exe alive but {url} not listening yet", pids.len());
            }
            true
        }
        "stop" => {
            let pids = sibling_pids();
            if pids.is_empty() {
                println!("[agent] no running service process");
                return true;
            }
            let mut killed = 0;
            for pid in pids {
                use std::os::windows::process::CommandExt;
                if std::process::Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).creation_flags(0x0800_0000).output().is_ok() {
                    killed += 1;
                }
            }
            println!("[agent] stopped {killed} service process");
            true
        }
        "start" => {
            if port_open(host, port) {
                println!("[agent] already running: {url}");
            } else {
                match spawn_detached(&["--minimized"]) {
                    Ok(()) => println!("[agent] started in background: {url}"),
                    Err(e) => {
                        eprintln!("[agent] {e}");
                        return true;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(1500));
            }
            open_browser(&url);
            true
        }
        "--install" => {
            match set_autostart(true) {
                Ok(()) => println!("[agent] autostart enabled (background service on login)"),
                Err(e) => eprintln!("[agent] {e}"),
            }
            true
        }
        "--uninstall" => {
            match set_autostart(false) {
                Ok(()) => println!("[agent] autostart removed"),
                Err(e) => eprintln!("[agent] {e}"),
            }
            true
        }
        "--minimized" => {
            // 后台子进程入口：隐藏控制台窗口后跑服务
            #[cfg(windows)]
            unsafe {
                use windows_sys::Win32::System::Console::GetConsoleWindow;
                use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
                let hwnd = GetConsoleWindow();
                if hwnd != 0 {
                    ShowWindow(hwnd, SW_HIDE);
                }
            }
            false
        }
        "" => {
            // 双击：已跑→开浏览器；未跑→后台起自己→开浏览器；自己退出（不占终端）
            if port_open(host, port) {
                open_browser(&url);
            } else if sibling_pids().is_empty() {
                match spawn_detached(&["--minimized"]) {
                    Ok(()) => {
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                        open_browser(&url);
                    }
                    Err(e) => eprintln!("{e}"),
                }
            } else {
                // 有兄弟进程但端口未开：正在启动中，直接开浏览器等它就绪
                open_browser(&url);
            }
            true
        }
        _ => {
            eprintln!("[agent] unknown arg: {arg1} (usage: start / stop / status / --install / --uninstall)");
            true
        }
    }
}
