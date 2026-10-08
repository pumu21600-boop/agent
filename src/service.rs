//! 服务自管理（P13 前置）：双击即用 + 后台常驻 + 开机自启，不再依赖 start/stop.bat。
//!
//! 行为：
//! - 无参数双击：5173 已在跑 → 直接打开浏览器；未跑 → 自己以后台方式再起一个（隐藏窗口，
//!   关终端不影响），然后打开浏览器；已是后台子进程则直接跑服务。
//! - `agent.exe start`：后台启动服务（已跑则只开浏览器），与双击等价但不阻塞终端。
//! - `agent.exe stop`：结束其他 agent.exe 服务进程（不杀自己）。
//! - `agent.exe status`：打印 5173 监听状态与进程信息。
//! - `agent.exe run <消息> [选项]`：P22-A 无头会话一次性唤醒——打本地服务跑完整轮，
//!   把最终回答打印到终端（程序化唤醒的 CLI 入口；选项见 parse_run_args）。
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
fn sibling_pids() -> Vec<u32> {
    use std::os::windows::process::CommandExt;
    // 用 tasklist 兜底枚举（零新依赖；沙箱探针里 tasklist 受限，但自管理跑在用户干净环境）
    let list = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq agent.exe", "/FO", "CSV", "/NH"])
        .creation_flags(0x0800_0000) // 不闪控制台窗口
        .output();
    let Ok(list) = list else { return Vec::new() };
    parse_pids(&String::from_utf8_lossy(&list.stdout), std::process::id())
}

/// 解析 `tasklist /FO CSV /NH` 输出里的 PID（排除自己）。纯函数（可单测）：
/// 停止/启动判断全靠它，解析错了就会误杀或误判「没在跑」。
fn parse_pids(text: &str, me: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for line in text.lines() {
        // "agent.exe","1234","Console","1","17,280 K"
        let cols: Vec<&str> = line.split("\",\"").collect();
        if cols.len() >= 2
            && let Ok(pid) = cols[1].trim_matches('"').parse::<u32>()
            && pid != me
        {
            out.push(pid);
        }
    }
    out
}

/// 用系统默认浏览器打开 url（ShellExecute，零新依赖）。
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

/// 以隐藏窗口方式再起一个自己（后台常驻，关终端不影响）。
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

/// HKCU Run 开机自启：登录后最小化启动（后台跑服务，不弹窗口）。
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
        "run" => {
            // P22-A 无头会话 CLI：在独立线程里跑阻塞 HTTP（reqwest 阻塞客户端不进
            // tokio 运行时），run_cli 所有出口都直接退进程，join 只是兜底。
            let rest: Vec<String> = argv[2..].to_vec();
            let host = host.to_string();
            let worker = std::thread::spawn(move || run_cli(&host, port, &rest));
            let _ = worker.join();
            true
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
            eprintln!("[agent] unknown arg: {arg1} (usage: start / stop / status / run <消息> / --install / --uninstall)");
            true
        }
    }
}

/// P22-A `run` 子命令的解析结果。
pub(super) struct RunArgs {
    pub message: String,
    pub session_id: String,
    pub model: String,
    pub approval_mode: String,
    pub timeout_secs: u64,
    /// on_approval=approve：无人值守时对高危操作放行（缺省拒绝）
    pub approve: bool,
    /// 打印完整 JSON（含过程与用量）而不是只打印最终回答
    pub json_out: bool,
}

/// 解析 `run` 子选项（纯函数，可单测）：
/// `agent.exe run <消息...> [--session <id>] [--model <m>] [--mode <档位>] [--timeout <秒>] [--approve] [--json]`
/// 消息可以是多个词（自动拼接），也可以只给一条完整字符串。
pub(super) fn parse_run_args(args: &[String]) -> Result<RunArgs, String> {
    let mut words: Vec<String> = Vec::new();
    let mut session_id = String::new();
    let mut model = String::new();
    let mut approval_mode = String::new();
    let mut timeout_secs = 600u64;
    let mut approve = false;
    let mut json_out = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--session" => {
                i += 1;
                session_id = args.get(i).ok_or("--session 缺少值")?.clone();
            }
            "--model" => {
                i += 1;
                model = args.get(i).ok_or("--model 缺少值")?.clone();
            }
            "--mode" => {
                i += 1;
                approval_mode = args.get(i).ok_or("--mode 缺少值")?.clone();
            }
            "--timeout" => {
                i += 1;
                let raw = args.get(i).ok_or("--timeout 缺少值")?;
                timeout_secs = raw
                    .parse()
                    .map_err(|_| format!("--timeout 必须是秒数: {raw}"))?;
            }
            "--approve" => approve = true,
            "--json" => json_out = true,
            other if other.starts_with("--") => return Err(format!("未知参数: {other}")),
            other => words.push(other.to_string()),
        }
        i += 1;
    }
    let message = words.join(" ").trim().to_string();
    if message.is_empty() {
        return Err("缺少要唤醒的消息内容".into());
    }
    Ok(RunArgs { message, session_id, model, approval_mode, timeout_secs, approve, json_out })
}

/// 无头会话 CLI 主体：打本地服务 `/api/headless/run`，打印最终回答。
/// 所有出口都退进程：done=0，其余（出错/超时/取消/连接失败）=1，参数错=2。
fn run_cli(host: &str, port: u16, args: &[String]) -> ! {
    let ra = match parse_run_args(args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[agent] {e}");
            eprintln!(
                "usage: agent.exe run <消息> [--session <id>] [--model <m>] [--mode read-only|auto|full-access] [--timeout <秒>] [--approve] [--json]"
            );
            std::process::exit(2);
        }
    };
    let url = format!("http://{host}:{port}/api/headless/run");
    if !port_open(host, port) {
        eprintln!("[agent] 服务未在 http://{host}:{port} 监听：先 agent.exe start（或双击 agent.exe）");
        std::process::exit(1);
    }
    // 坑 17：reqwest 在 Windows 上会读系统代理注册表，环回请求被代理接走返 502——
    // 打本机服务必须 no_proxy。
    let client = match reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(ra.timeout_secs.max(60) + 30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[agent] 创建 HTTP 客户端失败: {e}");
            std::process::exit(1);
        }
    };
    let body = serde_json::json!({
        "message": ra.message,
        "session_id": ra.session_id,
        "model": ra.model,
        "approval_mode": ra.approval_mode,
        "on_approval": if ra.approve { "approve" } else { "deny" },
        "timeout_secs": ra.timeout_secs,
        "purpose": "cli",
    });
    let resp = client.post(&url).json(&body).send();
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[agent] 请求失败: {e}");
            std::process::exit(1);
        }
    };
    let http_ok = resp.status().is_success();
    let parsed: serde_json::Value = match resp.json() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[agent] 响应解析失败: {e}");
            std::process::exit(1);
        }
    };
    if ra.json_out {
        println!("{}", serde_json::to_string_pretty(&parsed).unwrap_or_default());
    } else {
        for n in parsed
            .get("notices")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(s) = n.as_str() {
                eprintln!("[提示] {s}");
            }
        }
        if let Some(ans) = parsed.get("answer").and_then(serde_json::Value::as_str)
            && !ans.is_empty()
        {
            println!("{ans}");
        }
        if let Some(err) = parsed.get("error").and_then(serde_json::Value::as_str) {
            eprintln!("[agent] {err}");
        }
    }
    let status = parsed
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let has_error = parsed.get("error").is_some();
    if http_ok && status == "done" && !has_error {
        std::process::exit(0);
    }
    if !http_ok {
        std::process::exit(1);
    }
    eprintln!("[agent] 无头会话状态: {status}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P22-A：`run` 子选项解析——消息多词拼接、各 flags、缺省值（安全侧）。
    #[test]
    fn parse_run_args_covers_words_flags_and_defaults() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // 多词消息自动拼接；缺省 = 拒绝高危 / 600s 超时 / 不打印 JSON
        let r = parse_run_args(&a(&["帮我看下", "构建为什么挂了"])).unwrap();
        assert_eq!(r.message, "帮我看下 构建为什么挂了");
        assert!(!r.approve, "缺省必须拒绝高危操作");
        assert_eq!(r.timeout_secs, 600);
        assert!(!r.json_out);
        assert!(r.session_id.is_empty());
        // 全套 flags
        let r = parse_run_args(&a(&[
            "跑一下",
            "测试",
            "--session",
            "abc-123",
            "--model",
            "step-5-preview",
            "--mode",
            "read-only",
            "--timeout",
            "90",
            "--approve",
            "--json",
        ]))
        .unwrap();
        assert_eq!(r.message, "跑一下 测试");
        assert_eq!(r.session_id, "abc-123");
        assert_eq!(r.model, "step-5-preview");
        assert_eq!(r.approval_mode, "read-only");
        assert_eq!(r.timeout_secs, 90);
        assert!(r.approve);
        assert!(r.json_out);
        // 出错路径：空消息 / 缺值 / 未知 flag / 非数字超时
        assert!(parse_run_args(&a(&[])).is_err());
        assert!(parse_run_args(&a(&["--session"])).is_err());
        assert!(parse_run_args(&a(&["hi", "--oops"])).is_err());
        assert!(parse_run_args(&a(&["hi", "--timeout", "abc"])).is_err());
    }

    #[test]
    fn parse_pids_reads_csv_and_skips_self() {
        let text = "\"agent.exe\",\"1234\",\"Console\",\"1\",\"17,280 K\"\r\n\
                    \"agent.exe\",\"4321\",\"Console\",\"1\",\"20,000 K\"\r\n\
                    \r\n";
        // 自己（1234）必须被排除，否则 stop 会把自己也杀掉
        assert_eq!(parse_pids(text, 1234), vec![4321]);
        assert_eq!(parse_pids(text, 9999), vec![1234, 4321]);
        // tasklist 查无结果时的固定文案不产生 PID
        assert!(parse_pids("INFO: No tasks are running which match the specified criteria.", 1).is_empty());
        assert!(parse_pids("", 1).is_empty());
        // 只有一列（异常输出）不 panic、不产 PID
        assert!(parse_pids("\"agent.exe\"", 1).is_empty());
    }

    #[test]
    fn port_open_detects_listener() {
        // 起一个真监听：探针必须报 true
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(port_open("127.0.0.1", port), "有监听时应为 true");
        drop(l);
        // 未监听的端口：报 false（超时 200ms 内返回，不会卡住 status/start 判断）
        let free = {
            let t = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let p = t.local_addr().unwrap().port();
            drop(t);
            p
        };
        assert!(!port_open("127.0.0.1", free), "无监听时应为 false");
    }

    /// status 分支是只读的（打印 + 探针），可以在测试里安全跑通，确认没有 panic。
    #[test]
    fn status_command_takes_over_without_side_effects() {
        let argv = vec!["agent.exe".to_string(), "status".to_string()];
        assert!(handle(&argv, "127.0.0.1", 1), "status 应接管并立刻返回");
    }
}
