//! 沙箱探针 —— 施工单 004 / 任务 E
//!
//! 每个探针真实拉起一个受限进程，验证隔离面。cargo test 与 GET /api/sandbox/selftest 共用。
//! 探针即验收：输出捕获 / 环境白名单 / 特权剥离 / 工作目录可达 / 超时团灭 / kill-tree / 内存上限。

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use super::{run, SandboxLimits, SandboxSpec};
// 仅测试路径（probe_memory_cap）用到，放这里避免非测试构建的 unused import 告警
#[cfg(test)]
use super::SandboxOutput;
use crate::{config::SandboxConfig, tools::workspace_root};

/// 探针结果（SSE/HTTP 直出 JSON）。
pub struct ProbeResult {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// 单测里串行化 ping 类探针，避免进程计数互相污染。
#[cfg(test)]
pub(crate) static PING_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 快速探针套件（/api/sandbox/selftest 用；kill-tree 与内存上限探针较慢，只在 cargo test 跑）。
pub fn run_fast_suite(cfg: &SandboxConfig) -> Vec<ProbeResult> {
    let limits = limits_of(cfg);
    vec![
        probe_echo(limits),
        probe_env_trim(limits),
        probe_privilege_strip(limits),
        probe_workspace_read(limits),
        probe_timeout_kill(limits),
    ]
}

pub(crate) fn limits_of(cfg: &SandboxConfig) -> SandboxLimits {
    SandboxLimits {
        timeout_ms: cfg.timeout_ms,
        memory_mb: cfg.memory_mb,
        active_processes: cfg.active_processes,
    }
}

fn probe_workdir() -> PathBuf {
    let dir = std::env::temp_dir().join("agent-p3-sandbox");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn cmd_spec(command: &str, working_dir: &Path, limits: SandboxLimits) -> SandboxSpec {
    let cmd = super::system_cmd();
    SandboxSpec {
        application: cmd.clone(),
        command_line: format!("\"{}\" {command}", cmd.display()),
        working_dir: working_dir.to_path_buf(),
        limits,
        restricted: true,
        appcontainer: false,
        allow_network: false,
    }
}

/// 输出捕获：cmd /C echo 的内容原样带回。
pub fn probe_echo(limits: SandboxLimits) -> ProbeResult {
    let out = run(&cmd_spec("/C echo hello-sandbox-42", &probe_workdir(), limits));
    match out {
        Ok(o) if o.stdout.contains("hello-sandbox-42") => {
            ProbeResult { name: "echo", ok: true, detail: "输出捕获正常".into() }
        }
        Ok(o) => ProbeResult {
            name: "echo",
            ok: false,
            detail: format!("stdout 未包含标记: {:?}", o.stdout),
        },
        Err(e) => ProbeResult { name: "echo", ok: false, detail: e.to_string() },
    }
}

/// 环境白名单：子进程 set 里只允许出现白名单变量，密钥（DEEPSEEK_API_KEY 等）进不来。
pub fn probe_env_trim(limits: SandboxLimits) -> ProbeResult {
    let out = run(&cmd_spec("/C set", &probe_workdir(), limits));
    match out {
        Ok(o) => {
            let strangers: Vec<&str> = o
                .stdout
                .lines()
                .filter_map(|l| l.split_once('=').map(|(k, _)| k))
                .filter(|k| !super::proc::SANDBOX_ENV_VARS.iter().any(|v| v.eq_ignore_ascii_case(k)))
                .collect();
            if strangers.is_empty() {
                ProbeResult { name: "env_trim", ok: true, detail: "子进程环境为纯白名单".into() }
            } else {
                ProbeResult {
                    name: "env_trim",
                    ok: false,
                    detail: format!("环境出现白名单外变量: {strangers:?}"),
                }
            }
        }
        Err(e) => ProbeResult { name: "env_trim", ok: false, detail: e.to_string() },
    }
}

/// 特权剥离差分断言：内省交给 CreateProcessAsUserW 的那个 token，
/// 启用特权必须只剩白名单（SeChangeNotifyPrivilege 是系统固定保留，无害）。
/// 不走 whoami：DISABLE_MAX_PRIVILEGE 只把特权置禁用不删名字，`whoami /priv`
/// 仍列出该行，且中文系统输出 GBK 无法可靠解析状态列。
pub fn probe_privilege_strip(_limits: SandboxLimits) -> ProbeResult {
    use super::token;
    let (base, restricted) = (token::base_token(), token::create_restricted_token());
    match (base, restricted) {
        (Ok(b), Ok(r)) => {
            let control = token::enabled_privilege_names(b.handle());
            let sandbox = token::enabled_privilege_names(r.handle());
            match (control, sandbox) {
                (Ok(c), Ok(s)) => {
                    // 系统固定保留项（CreateRestrictedToken 既有行为）：
                    // 遍历检查绕过本身无安全面，保留它不算未剥光
                    const PINNED: &[&str] = &["SeChangeNotifyPrivilege"];
                    let survivors: Vec<&String> =
                        s.iter().filter(|n| !PINNED.contains(&n.as_str())).collect();
                    let ok = survivors.is_empty() && c.iter().any(|n| n == "SeChangeNotifyPrivilege");
                    ProbeResult {
                        name: "privilege_strip",
                        ok,
                        detail: format!(
                            "沙箱启用特权 {s:?}（对照组 {c:?}），白名单外 {} 个",
                            survivors.len()
                        ),
                    }
                }
                (a, b2) => ProbeResult {
                    name: "privilege_strip",
                    ok: false,
                    detail: format!("token 内省失败: control={a:?} sandbox={b2:?}"),
                },
            }
        }
        (a, b2) => ProbeResult {
            name: "privilege_strip",
            ok: false,
            detail: format!("打开 token 失败: control={a:?} restricted={b2:?}"),
        },
    }
}

/// 工作目录可达：沙箱内进程能在工作目录里读到宿主刚写的文件（隔离不能过头，
/// 正常读面必须保留）。
///
/// 探针**自带标记文件**，不依赖工作区里恰好存在某个仓库文件——早先写死读 `Cargo.toml`，
/// 一旦按打包形态分发（exe 同级没有源码）就必然假失败（真机抓到）。
pub fn probe_workspace_read(limits: SandboxLimits) -> ProbeResult {
    const TOKEN: &str = "agent-sandbox-read-probe";
    // 标记名带进程号 + 自增序号：并发自检（测试并行 / 界面同时点两次）互不踩——
    // 固定名曾出现同进程两个探针互相删标记的「file not found」假失败。
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let marker_name = format!(".agent_probe_read_{}_{seq}.txt", std::process::id());
    let root = workspace_root();
    let marker = root.join(&marker_name);
    if let Err(e) = std::fs::write(&marker, TOKEN) {
        return ProbeResult {
            name: "workspace_read",
            ok: false,
            detail: format!("写标记文件失败（工作区不可写）: {e}"),
        };
    }
    let out = run(&cmd_spec(&format!("/C type {marker_name}"), &root, limits));
    let _ = std::fs::remove_file(&marker);
    match out {
        Ok(o) if o.stdout.contains(TOKEN) => {
            ProbeResult { name: "workspace_read", ok: true, detail: "工作目录内文件可读".into() }
        }
        Ok(o) => ProbeResult {
            name: "workspace_read",
            ok: false,
            detail: format!("type {marker_name} 异常: exit={:?} out={:?}", o.exit_code, o.stderr),
        },
        Err(e) => ProbeResult { name: "workspace_read", ok: false, detail: e.to_string() },
    }
}

/// 超时团灭：墙钟超时到点整树击杀并标记 timed_out。
pub fn probe_timeout_kill(limits: SandboxLimits) -> ProbeResult {
    let tight = SandboxLimits { timeout_ms: 1_000, ..limits };
    let start = Instant::now();
    let out = run(&cmd_spec("/C ping -n 30 127.0.0.1", &probe_workdir(), tight));
    let elapsed = start.elapsed();
    match out {
        Ok(o) if o.timed_out && elapsed < Duration::from_millis(10_000) => ProbeResult {
            name: "timeout_kill",
            ok: true,
            detail: format!("超时击杀生效，耗时 {}ms", elapsed.as_millis()),
        },
        Ok(o) => ProbeResult {
            name: "timeout_kill",
            ok: false,
            detail: format!("timed_out={} 耗时={}ms", o.timed_out, elapsed.as_millis()),
        },
        Err(e) => ProbeResult { name: "timeout_kill", ok: false, detail: e.to_string() },
    }
}

/// kill-tree：cmd 里 start 出孙进程，SandboxChild Drop 后孙进程必须归零。
/// 供 cargo test 调用（要数进程，较慢）——快速自检 run_fast_suite 不含它。
#[cfg(test)]
pub fn probe_kill_tree(limits: SandboxLimits) -> ProbeResult {
    let count_pings = || {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq ping.exe", "/FO", "CSV", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| l.to_lowercase().contains("ping.exe"))
                    .count()
            })
            .unwrap_or(0)
    };

    let long = SandboxLimits { timeout_ms: 60_000, ..limits };
    let child = match super::spawn(&cmd_spec(
        "/C start /b ping -n 60 127.0.0.1 & ping -n 60 127.0.0.1",
        &probe_workdir(),
        long,
    )) {
        Ok(c) => c,
        Err(e) => return ProbeResult { name: "kill_tree", ok: false, detail: e.to_string() },
    };
    std::thread::sleep(Duration::from_millis(1_500));
    let alive = count_pings();
    if alive < 2 {
        drop(child);
        return ProbeResult {
            name: "kill_tree",
            ok: false,
            detail: format!("预期 ≥2 个 ping.exe 实际 {alive}，探针前提不成立"),
        };
    }
    drop(child); // Drop = 团灭
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut remaining = 0usize;
    while Instant::now() < deadline {
        remaining = count_pings();
        if remaining == 0 {
            return ProbeResult { name: "kill_tree", ok: true, detail: format!("Drop 后 {alive} 个 ping.exe 全部归零") };
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    ProbeResult { name: "kill_tree", ok: false, detail: format!("Drop 后仍残留 {remaining} 个 ping.exe") }
}

/// 内存上限：用原生 hog（src/bin/sandbox_hog.rs，无 .NET 依赖）做差分——
/// 对照组小分配宽限额成功，受限组超限分配必须失败。
/// 不用 powershell：受限 token 下 .NET 启动即 0xC0000142，且 -Command 里
/// 分配失败会当非终止错误吞掉继续跑，探针结论不可信。
/// 供 cargo test 调用（要拉起 hog + 60s 限额）——快速自检 run_fast_suite 不含它。
#[cfg(test)]
pub fn probe_memory_cap(limits: SandboxLimits) -> ProbeResult {
    let Some(hog) = find_hog() else {
        return ProbeResult { name: "memory_cap", ok: false, detail: "找不到 sandbox_hog.exe（先 cargo build）".into() };
    };
    let run_hog = |mb: usize, memory_mb: u32| -> Result<SandboxOutput, anyhow::Error> {
        run(&SandboxSpec {
            application: hog.clone(),
            command_line: format!("\"{}\" {mb}", hog.display()),
            working_dir: probe_workdir(),
            limits: SandboxLimits { timeout_ms: 60_000, memory_mb, ..limits },
            restricted: true,
        appcontainer: false,
        allow_network: false,
        })
    };
    let control = run_hog(512, 2048);
    let restricted = run_hog(1024, 256);
    match (control, restricted) {
        (Ok(c), Ok(r)) => {
            let control_ok = c.stdout.contains("HOG-ALLOC-OK");
            let restricted_dead = !r.stdout.contains("HOG-ALLOC-OK");
            if control_ok && restricted_dead {
                ProbeResult { name: "memory_cap", ok: true, detail: "宽限额成功、超限被拒，内存上限生效".into() }
            } else {
                ProbeResult {
                    name: "memory_cap",
                    ok: false,
                    detail: format!(
                        "control_ok={control_ok} restricted_dead={restricted_dead} (exit {:?}/{:?}) stdout尾={:?}/{:?}",
                        c.exit_code,
                        r.exit_code,
                        c.stdout.chars().rev().take(80).collect::<String>(),
                        r.stdout.chars().rev().take(80).collect::<String>(),
                    ),
                }
            }
        }
        (a, b) => ProbeResult {
            name: "memory_cap",
            ok: false,
            detail: format!(
                "探针拉起失败: control={:?} restricted={:?}",
                a.map(|o| o.exit_code),
                b.map(|o| o.exit_code)
            ),
        },
    }
}

/// 定位 hog 可执行文件：测试二进制在 target/debug/deps/，hog 在 target/debug/。
#[cfg(test)]
fn find_hog() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AGENT_HOG_EXE") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    [dir.join("sandbox_hog.exe"), dir.join("..").join("sandbox_hog.exe")]
        .into_iter()
        .find(|cand| cand.exists())
}

/// 宿主 canary（仅测试用）：先真拉起一次受限 cmd /C echo，判断本机到底能不能
/// 启动受限 token 子进程。GitHub 托管 Windows runner 上受限 cmd/git 在执行任何
/// 代码前就死（exit 0xC0000142，与上面 powershell 探针注释记录的是同一签名），
/// 那是宿主 token/会话环境限制而非代码回退。命中该签名返回跳过原因，由各探针
/// 测试自报 skip；其余失败形态一律照常失败——真问题不掩盖。
#[cfg(test)]
pub(crate) fn restricted_host_limitation() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            match run(&cmd_spec(
                "/C echo agent-host-canary-ok",
                &probe_workdir(),
                SandboxLimits { timeout_ms: 10_000, memory_mb: 512, active_processes: 8 },
            )) {
                // 正常执行 → 本机支持，探针全部照跑
                Ok(o) if o.stdout.contains("agent-host-canary-ok") => None,
                // 未执行就死且命中已知宿主限制签名 → 跳过并留原因
                Ok(o) if o.exit_code == Some(0xC000_0142) => Some(
                    "宿主无法启动受限 token 子进程（canary cmd 未执行即 exit 0xC0000142）——托管 runner 的已知环境限制，非代码回退；真机不受影响".into(),
                ),
                // 其它形态（超时/别的退出码/拉起失败）不算宿主限制，交给探针自己报错
                _ => None,
            }
        })
        .clone()
}

/// 测试内 logger 初始化（sandbox 事件落 JSONL，探针依赖它）。
#[cfg(test)]
fn init_logger() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = std::env::temp_dir().join("agent-p3-test-logs");
        let _ = crate::observability::init(&dir, "test.jsonl", format!("test-{}", uuid::Uuid::new_v4()));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SandboxConfig;

    /// P12 019：AppContainer 真机探针（ignored，手工跑：cargo test appcontainer_smoke -- --ignored --nocapture）
    #[test]
    #[ignore]
    fn appcontainer_smoke() {
        use super::*;
        let _ = crate::observability::init(
            &std::env::temp_dir().join("agent_ac_probe_logs"),
            "ac-probe.jsonl",
            "ac-probe".into(),
        );
        let ws = crate::tools::workspace_root();
        let spec = SandboxSpec {
            application: crate::sandbox::system_cmd(),
            command_line: format!(r#""{}" /c echo hi-from-appcontainer"#, crate::sandbox::system_cmd().display()),
            working_dir: ws,
            limits: SandboxLimits { timeout_ms: 15_000, memory_mb: 1024, active_processes: 32 },
            restricted: true,
            appcontainer: true,
            allow_network: false, // 默认断网（NULL capabilities 分支）
        };
        let out = crate::sandbox::run(&spec).expect("AppContainer spawn 失败");
        println!("[appcontainer] exit={:?} timed_out={} stdout={:?}", out.exit_code, out.timed_out, out.stdout.trim());
        assert!(!out.timed_out);
        assert!(out.stdout.contains("hi-from-appcontainer"), "输出应包含回显");
    }

    fn cfg() -> SandboxConfig {
        SandboxConfig { timeout_ms: 10_000, memory_mb: 1024, active_processes: 64, mode: "restricted".into(), appcontainer: false, allow_network: false }
    }

    fn probe(name: &str, r: ProbeResult) -> ProbeResult {
        assert!(r.ok, "{name} 未过: {}", r.detail);
        r
    }

    #[test]
    fn echo_capture() {
        init_logger();
        if let Some(reason) = restricted_host_limitation() {
            eprintln!("[skip] echo_capture：{reason}");
            return;
        }
        probe("echo", probe_echo(limits_of(&cfg())));
    }

    #[test]
    fn env_whitelist_trim() {
        init_logger();
        // 差分前提：父进程环境里塞一个密钥，沙箱子进程绝不能看见
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-should-not-leak-42") };
        let r = probe("env_trim", probe_env_trim(limits_of(&cfg())));
        assert!(!r.detail.contains("should-not-leak"));
    }

    #[test]
    fn privilege_strip() {
        init_logger();
        probe("privilege_strip", probe_privilege_strip(limits_of(&cfg())));
    }

    #[test]
    fn workspace_read() {
        init_logger();
        if let Some(reason) = restricted_host_limitation() {
            eprintln!("[skip] workspace_read：{reason}");
            return;
        }
        probe("workspace_read", probe_workspace_read(limits_of(&cfg())));
    }

    #[test]
    fn timeout_kills_tree() {
        init_logger();
        if let Some(reason) = restricted_host_limitation() {
            eprintln!("[skip] timeout_kills_tree：{reason}");
            return;
        }
        let _g = PING_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        probe("timeout_kill", probe_timeout_kill(limits_of(&cfg())));
    }

    #[test]
    fn kill_tree_orphans() {
        init_logger();
        if let Some(reason) = restricted_host_limitation() {
            eprintln!("[skip] kill_tree_orphans：{reason}");
            return;
        }
        let _g = PING_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        probe("kill_tree", probe_kill_tree(limits_of(&cfg())));
    }

    #[test]
    fn memory_cap_enforced() {
        init_logger();
        // runner 上即使 cargo build --bins 产出了 hog，受限拉起也会命中宿主限制签名
        if let Some(reason) = restricted_host_limitation() {
            eprintln!("[skip] memory_cap_enforced：{reason}");
            return;
        }
        probe("memory_cap", probe_memory_cap(limits_of(&cfg())));
    }

    #[test]
    fn selftest_suite_shape() {
        if let Some(reason) = restricted_host_limitation() {
            eprintln!("[skip] selftest_suite_shape：{reason}");
            return;
        }
        let results = run_fast_suite(&cfg());
        let names: Vec<&str> = results.iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["echo", "env_trim", "privilege_strip", "workspace_read", "timeout_kill"]);
        for r in &results {
            crate::observability::debug(&format!(
                "[selftest] {}: {} ({})",
                r.name,
                if r.ok { "OK" } else { "FAIL" },
                r.detail
            ));
        }
        assert!(results.iter().all(|r| r.ok), "fast suite 未全绿: {:?}", results.iter().filter(|r| !r.ok).map(|r| (r.name, r.detail.clone())).collect::<Vec<_>>());
    }
}
