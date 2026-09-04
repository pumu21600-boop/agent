//! 进程拉起与收割 —— 施工单 004 / 任务 D
//!
//! 流程：受限 token + Job 就绪 → CreateProcessAsUserW(CREATE_SUSPENDED) → AssignProcessToJobObject
//! → ResumeThread（挂起期入 Job，竞态窗口为零）→ 匿名管道收 stdout/stderr → 超时整树击杀。
//! 环境不继承父进程：只注入白名单最小集（SystemRoot/COMSPEC/PATH/TEMP…），密钥永不进子进程。
//! 同步阻塞实现：P4 工具层接入时用 tokio::task::spawn_blocking 包裹。

use std::{
    ffi::c_void,
    mem::{self, size_of},
    path::{Path, PathBuf},
    ptr,
    thread::{self, JoinHandle},
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, GENERIC_READ, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation},
    Security::SECURITY_ATTRIBUTES,
    Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile},
    System::{
        Pipes::CreatePipe,
        SystemInformation::GetSystemDirectoryW,
        Threading::{
            CreateProcessAsUserW, CreateProcessW, GetExitCodeProcess, ResumeThread, WaitForSingleObject,
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
            STARTF_USESTDHANDLES, STARTUPINFOW,
        },
    },
};

use anyhow::{anyhow, bail};

use super::{job::JobGuard, token, SandboxOutput, SandboxSpec};
use crate::observability::{self, EventType};

use serde_json::json;

const WAIT_OBJECT_0: u32 = 0;

/// 子进程环境白名单（探针与 /api/sandbox/selftest 共用；PROMPT 是 cmd 启动时自注入的）。
pub(crate) const SANDBOX_ENV_VARS: &[&str] = &[
    "APPDATA", "COMPUTERNAME", "COMSPEC", "HOMEDRIVE", "HOMEPATH", "LOCALAPPDATA",
    "NUMBER_OF_PROCESSORS", "PATH", "PATHEXT", "PROCESSOR_ARCHITECTURE", "PROMPT", "SystemRoot",
    "TEMP", "TMP", "USERPROFILE", "windir",
];

/// 单流输出上限：超出部分只丢弃不累积（防一条 `yes` 撑爆内存；上下文截断是 P5 的事）。
const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;

/// Windows 系统目录（一般为 C:\Windows\System32）。
pub(crate) fn system_dir() -> PathBuf {
    let mut buf = [0u16; 512];
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), 512) };
    if n == 0 || n as usize >= 512 {
        return PathBuf::from("C:\\Windows\\System32");
    }
    PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]))
}

/// 拉起进程。失败即无进程可谈（句柄全部就地回收）。
/// restricted=true：受限 token + 白名单环境（权限笼）；false：完整用户 token + 继承父环境（本机直跑）。
/// 两种模式都保留 Job 生命周期保险丝（kill-on-close、超时整树击杀）。
pub(crate) fn spawn(spec: &SandboxSpec) -> anyhow::Result<SandboxChild> {
    // P12 019：AppContainer 模式——LowBox token 由 SECURITY_CAPABILITIES 启动属性创建
    if spec.appcontainer {
        return spawn_appcontainer_impl(spec);
    }
    let token = if spec.restricted {
        token::create_restricted_token()?
    } else {
        token::full_token()?
    };
    let job = JobGuard::new(&spec.limits, spec.restricted)?;

    if !spec.working_dir.exists() {
        bail!("工作目录不存在: {}", spec.working_dir.display());
    }

    unsafe {
        // stdout/stderr 匿名管道；父端读句柄禁继承，子端拿写句柄
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };
        let (out_r, out_w) = create_pipe(&sa)?;
        let (err_r, err_w) = create_pipe(&sa)?;
        SetHandleInformation(out_r, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(err_r, HANDLE_FLAG_INHERIT, 0);

        // stdin 指向 NUL：不给交互输入，读它永远是 EOF
        let nul = CreateFileW(
            wide("NUL").as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &sa,
            OPEN_EXISTING,
            0,
            0, // hTemplateFile
        );
        if nul == 0 || nul == -1 {
            return Err(anyhow!("打开 NUL 失败: {}", GetLastError()));
        }
        let mut si: STARTUPINFOW = mem::zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESTDHANDLES;
        si.hStdInput = nul;
        si.hStdOutput = out_w;
        si.hStdError = err_w;

        let app = wide(spec.application.to_str().ok_or_else(|| anyhow!("application 路径非 UTF-8"))?);
        // CreateProcess 可能临时改写命令行缓冲，故给 mut
        let mut cmd_line = wide(&spec.command_line);
        let cwd = wide(spec.working_dir.to_str().ok_or_else(|| anyhow!("working_dir 非 UTF-8"))?);
        // 受限模式注入白名单环境；本机直跑传 NULL 继承父进程完整环境
        let env = if spec.restricted { build_env_block() } else { Vec::new() };

        let mut pi: PROCESS_INFORMATION = mem::zeroed();
        if CreateProcessAsUserW(
            token.handle(),
            app.as_ptr(),
            cmd_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1, // bInheritHandles：只继承我们标了 inherit 的三个句柄
            CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            if spec.restricted { env.as_ptr() as *const c_void } else { ptr::null() },
            cwd.as_ptr(),
            &si,
            &mut pi,
        ) == 0
        {
            let e = GetLastError();
            CloseHandle(out_r);
            CloseHandle(err_r);
            return Err(anyhow!("CreateProcessAsUserW 失败: {e}（检查 exe 是否存在/可执行）"));
        }
        // 父端这几份用完即关：子进程持有自己的继承副本，管道才能正常 EOF
        CloseHandle(out_w);
        CloseHandle(err_w);
        CloseHandle(nul);

        job.assign(pi.hProcess)?;
        ResumeThread(pi.hThread);
        CloseHandle(pi.hThread);

        let readers = (spawn_reader(out_r), spawn_reader(err_r));

        observability::event(
            EventType::State,
            json!({
                "kind": "sandbox.spawn",
                "pid": pi.dwProcessId,
                "app": spec.application.display().to_string(),
                "working_dir": spec.working_dir.display().to_string(),
                "restricted": spec.restricted,
                "timeout_ms": spec.limits.timeout_ms,
                "memory_mb": spec.limits.memory_mb,
                "active_processes": spec.limits.active_processes,
            }),
        );

        Ok(SandboxChild { job, process: pi.hProcess, pid: pi.dwProcessId, readers: Some(readers), closed: false })
    }
}

/// AppContainer 模式 spawn（P12 019）：profile/SID/ACE → STARTUPINFOEX(SECURITY_CAPABILITIES)
/// → CreateProcessW（挂起）→ Job → Resume。管道/Job/读取线程与 restricted 模式共用。
/// 白名单环境照用（AppContainer 不继承父环境）；默认断网（能力 SID 白名单空）。
fn spawn_appcontainer_impl(spec: &SandboxSpec) -> anyhow::Result<SandboxChild> {
    use super::appcontainer;
    use windows_sys::Win32::System::Threading::{
        InitializeProcThreadAttributeList, UpdateProcThreadAttribute, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, STARTUPINFOEXW,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    };
    use windows_sys::Win32::Security::SECURITY_CAPABILITIES;

    unsafe {
        // 1) profile + 派生 SID（allow_network 决定是否带 INTERNET_CLIENT 能力）
        let sid_guard = appcontainer::ensure_sid(spec.allow_network)
            .map_err(|e| anyhow!("AppContainer profile 失败: {e}"))?;
        // 2) 工作区目录 ACE（AppContainer 默认对任意目录无权限；失败仅告警不阻断）
        if let Some(sid_str) = appcontainer::sid_to_string(sid_guard.sid) {
            if let Err(e) = appcontainer::grant_dir_access(&spec.working_dir, &sid_str) {
                observability::event(
                    EventType::State,
                    json!({ "kind": "sandbox.appcontainer.warn", "message": e }),
                );
            }
        }
        // 3) 属性列表 + SECURITY_CAPABILITIES（指向 sid_guard 内的 SID，须保活到 CreateProcess 后）
        let mut attr_size = 0usize;
        let _ = InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut attr_size);
        let mut attr_buf = vec![0u8; attr_size];
        let attr_list = attr_buf.as_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) == 0 {
            bail!("InitializeProcThreadAttributeList 失败");
        }
        let mut sec_caps: SECURITY_CAPABILITIES = mem::zeroed();
        sec_caps.AppContainerSid = sid_guard.sid;
        if UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            &mut sec_caps as *mut SECURITY_CAPABILITIES as *mut c_void,
            size_of::<SECURITY_CAPABILITIES>(),
            ptr::null_mut(),
            ptr::null_mut(),
        ) == 0
        {
            bail!("UpdateProcThreadAttribute(SECURITY_CAPABILITIES) 失败");
        }

        // 4) 管道骨架（与 restricted 模式一致）
        let job = JobGuard::new(&spec.limits, true)?;
        if !spec.working_dir.exists() {
            bail!("工作目录不存在: {}", spec.working_dir.display());
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: 1,
        };
        let (out_r, out_w) = create_pipe(&sa)?;
        let (err_r, err_w) = create_pipe(&sa)?;
        SetHandleInformation(out_r, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(err_r, HANDLE_FLAG_INHERIT, 0);
        let nul = CreateFileW(
            wide("NUL").as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &sa,
            OPEN_EXISTING,
            0,
            0,
        );
        if nul == 0 || nul == -1 {
            return Err(anyhow!("打开 NUL 失败: {}", GetLastError()));
        }

        // 5) STARTUPINFOEXW + CreateProcessW（AppContainer 走 W 版 + 扩展属性）
        let mut six: STARTUPINFOEXW = mem::zeroed();
        six.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        six.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        six.StartupInfo.hStdInput = nul;
        six.StartupInfo.hStdOutput = out_w;
        six.StartupInfo.hStdError = err_w;
        six.lpAttributeList = attr_list;

        let app = wide(spec.application.to_str().ok_or_else(|| anyhow!("application 路径非 UTF-8"))?);
        let mut cmd_line = wide(&spec.command_line);
        let cwd = wide(spec.working_dir.to_str().ok_or_else(|| anyhow!("working_dir 非 UTF-8"))?);
        let env = build_env_block();

        let mut pi: PROCESS_INFORMATION = mem::zeroed();
        if CreateProcessW(
            app.as_ptr(),
            cmd_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            env.as_ptr() as *const c_void,
            cwd.as_ptr(),
            &mut six.StartupInfo,
            &mut pi,
        ) == 0
        {
            DeleteProcThreadAttributeList(attr_list);
            CloseHandle(out_r);
            CloseHandle(err_r);
            CloseHandle(nul);
            let e = GetLastError();
            return Err(anyhow!(
                "AppContainer CreateProcessW 失败: {e}（LowBox 下进程可能初始化失败，可切回 mode=restricted）"
            ));
        }
        DeleteProcThreadAttributeList(attr_list);
        drop(attr_buf);
        CloseHandle(out_w);
        CloseHandle(err_w);
        CloseHandle(nul);

        job.assign(pi.hProcess)?;
        ResumeThread(pi.hThread);
        CloseHandle(pi.hThread);

        let readers = (spawn_reader(out_r), spawn_reader(err_r));

        observability::event(
            EventType::State,
            json!({
                "kind": "sandbox.spawn",
                "pid": pi.dwProcessId,
                "app": spec.application.display().to_string(),
                "working_dir": spec.working_dir.display().to_string(),
                "appcontainer": true,
                "allow_network": spec.allow_network,
                "restricted": true,
                "timeout_ms": spec.limits.timeout_ms,
                "memory_mb": spec.limits.memory_mb,
                "active_processes": spec.limits.active_processes,
            }),
        );

        Ok(SandboxChild { job, process: pi.hProcess, pid: pi.dwProcessId, readers: Some(readers), closed: false })
    }
}

/// 受限进程树句柄：Drop 必团灭（哪怕没等结果直接丢）。
pub struct SandboxChild {
    job: JobGuard,
    process: HANDLE,
    pid: u32,
    readers: Option<(JoinHandle<String>, JoinHandle<String>)>,
    closed: bool,
}

impl SandboxChild {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// 等待退出；timeout_ms 到点整树击杀并标记 timed_out。消费自身，句柄就地关闭。
    pub fn wait(mut self, timeout_ms: u32) -> anyhow::Result<SandboxOutput> {
        let (exit_code, timed_out) = unsafe {
            let w = WaitForSingleObject(self.process, timeout_ms);
            if w == WAIT_OBJECT_0 {
                let mut code = 0u32;
                GetExitCodeProcess(self.process, &mut code);
                (Some(code), false)
            } else {
                self.job.terminate();
                WaitForSingleObject(self.process, 10_000);
                let mut code = 0u32;
                GetExitCodeProcess(self.process, &mut code);
                (Some(code), true)
            }
        };
        let (stdout, stderr) = self.join_readers_bounded();
        self.close_handles();
        observability::event(
            EventType::State,
            json!({
                "kind": "sandbox.exit",
                "pid": self.pid,
                "exit_code": exit_code,
                "timed_out": timed_out,
                "stdout_chars": stdout.chars().count(),
                "stderr_chars": stderr.chars().count(),
            }),
        );
        Ok(SandboxOutput { exit_code, timed_out, stdout, stderr })
    }

    /// 收割读管道线程。主进程退出后写端可能仍被残留孙进程握着（EOF 不来），
    /// 有限等待后整树击杀兜底——绝不无限等，这是「工具结束后卡死」的防线上。
    fn join_readers_bounded(&mut self) -> (String, String) {
        use std::time::{Duration, Instant};
        let Some((so, se)) = self.readers.take() else {
            return (String::new(), String::new());
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while (!so.is_finished() || !se.is_finished()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        if !so.is_finished() || !se.is_finished() {
            self.job.terminate();
        }
        (so.join().unwrap_or_default(), se.join().unwrap_or_default())
    }

    fn join_readers(&mut self) -> (String, String) {
        self.join_readers_bounded()
    }

    fn close_handles(&mut self) {
        if !self.closed {
            self.closed = true;
            unsafe { CloseHandle(self.process) };
            self.process = 0;
        }
    }
}

impl Drop for SandboxChild {
    fn drop(&mut self) {
        if !self.closed {
            // 没等完就丢：整树击杀再回收，读管道线程随 EOF 自然退出
            self.job.terminate();
            unsafe { WaitForSingleObject(self.process, 5_000) };
            let _ = self.join_readers();
            self.close_handles();
        }
    }
}

/// 独立线程读管道到 EOF（child 退出/被杀后写端关闭）；超限部分丢弃不累积。
fn spawn_reader(handle: HANDLE) -> JoinHandle<String> {
    thread::spawn(move || unsafe {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let mut got = 0u32;
            if ReadFile(
                handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut got,
                ptr::null_mut(),
            ) == 0
                || got == 0
            {
                break;
            }
            if out.len() < MAX_CAPTURE_BYTES {
                out.extend_from_slice(&buf[..got as usize]);
            }
        }
        CloseHandle(handle);
        String::from_utf8_lossy(&out).into_owned()
    })
}

fn create_pipe(sa: &SECURITY_ATTRIBUTES) -> anyhow::Result<(HANDLE, HANDLE)> {
    let mut r: HANDLE = 0;
    let mut w: HANDLE = 0;
    unsafe {
        if CreatePipe(&mut r, &mut w, sa, 0) == 0 {
            bail!("CreatePipe 失败: {}", GetLastError());
        }
    }
    Ok((r, w))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 环境白名单块（不继承父环境，密钥进不来）：NAME=VALUE\0 … \0。
fn build_env_block() -> Vec<u16> {
    let sys_root = std::env::var("SystemRoot")
        .unwrap_or_else(|_| system_dir().parent().map(Path::display).map(|p| p.to_string()).unwrap_or_else(|| "C:\\Windows".into()));
    let user_profile = std::env::var("USERPROFILE").unwrap_or_default();
    let temp = std::env::var("TEMP")
        .or_else(|_| std::env::var("TMP"))
        .unwrap_or_else(|_| std::env::temp_dir().display().to_string());
    let mut vars: Vec<(String, String)> = vec![
        ("SystemRoot".into(), sys_root.clone()),
        ("windir".into(), sys_root.clone()),
        ("COMSPEC".into(), format!("{sys_root}\\System32\\cmd.exe")),
        // cmd 启动会自注入 PROMPT；先占位，cmd 看到已存在就不覆盖
        ("PROMPT".into(), "$P$G".into()),
        (
            "PATH".into(),
            // P4 起 shell/git 工具要能用用户装的工具链（git/cargo/node…），
            // PATH 不是密钥，继承父进程；父进程没有再退回系统目录集
            std::env::var("PATH").unwrap_or_else(|_| {
                format!(
                    "{sys_root}\\System32;{sys_root};{sys_root}\\System32\\Wbem;{sys_root}\\System32\\WindowsPowerShell\\v1.0;{sys_root}\\System32\\downlevel"
                )
            }),
        ),
        ("PATHEXT".into(), ".COM;.EXE;.BAT;.CMD;.VBS;.JS;.WSF;.MSC".into()),
        ("NUMBER_OF_PROCESSORS".into(), std::env::var("NUMBER_OF_PROCESSORS").unwrap_or_else(|_| "1".into())),
        ("PROCESSOR_ARCHITECTURE".into(), std::env::var("PROCESSOR_ARCHITECTURE").unwrap_or_else(|_| "AMD64".into())),
        ("TEMP".into(), temp.clone()),
        ("TMP".into(), temp),
        ("COMPUTERNAME".into(), std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into())),
    ];
    if !user_profile.is_empty() {
        vars.push(("USERPROFILE".into(), user_profile.clone()));
        vars.push(("HOMEDRIVE".into(), std::env::var("HOMEDRIVE").unwrap_or_default()));
        vars.push(("HOMEPATH".into(), std::env::var("HOMEPATH").unwrap_or_default()));
        vars.push(("LOCALAPPDATA".into(), std::env::var("LOCALAPPDATA").unwrap_or_default()));
        vars.push(("APPDATA".into(), std::env::var("APPDATA").unwrap_or_default()));
    }
    vars.retain(|(_, v)| !v.is_empty());
    vars.sort_by_key(|(k, _)| k.to_uppercase());

    let mut block: Vec<u16> = Vec::new();
    for (k, v) in &vars {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}
