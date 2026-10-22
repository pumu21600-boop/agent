//! git 工具:git.exe 发现链、内置运行时懒解压与沙箱内 git 执行。

use super::*;

/// git.exe 发现链（开箱即用）：AGENT_GIT_PATH 环境变量 → 父进程 PATH（where.exe）→
/// 注册表（Git for Windows 安装时登记的 InstallPath）→ 常见安装位置 → 随分发捆绑的 tools/git。
/// 找到后把相关目录登记进沙箱 PATH（shell 里按名调 git / git 内部 helper 才可用）。
pub(crate) fn git_exe() -> Result<PathBuf, String> {
    static GIT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    GIT.get_or_init(|| {
        // 1) 显式环境变量
        if let Some(p) = crate::config::env_pawch("GIT_PATH") {
            let pb = PathBuf::from(p);
            if pb.is_file() {
                register_git_dirs(&pb);
                return Ok(pb);
            }
        }
        // 2) 父进程 PATH
        if let Some(p) = where_git() {
            register_git_dirs(&p);
            return Ok(p);
        }
        // 3) 注册表：Git for Windows 的 InstallPath（HKLM / HKCU，含 Wow6432Node 兜底）
        use std::os::windows::process::CommandExt;
        for hive in [
            "HKLM\\SOFTWARE\\GitForWindows",
            "HKLM\\SOFTWARE\\WOW6432Node\\GitForWindows",
            "HKCU\\SOFTWARE\\GitForWindows",
        ] {
            if let Ok(out) = std::process::Command::new("reg.exe")
                .args(["query", hive, "/v", "InstallPath"])
                .creation_flags(0x0800_0000) // 不闪控制台窗口
                .stdin(std::process::Stdio::null())
                .output()
            {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some(line) = text.lines().find(|l| l.contains("InstallPath"))
                    && let Some(dir) = line.split("REG_SZ").last().map(|s| s.trim()) {
                        for sub in ["cmd\\git.exe", "bin\\git.exe"] {
                            let pb = PathBuf::from(dir).join(sub);
                            if pb.is_file() {
                                register_git_dirs(&pb);
                                return Ok(pb);
                            }
                        }
                    }
            }
        }
        // 4) 常见安装位置（Git for Windows / PortableGit 习惯路径）
        let program_files = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into());
        let program_files_x86 =
            std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".into());
        let local_appdata = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let mut candidates: Vec<PathBuf> = Vec::new();
        for base in [&program_files, &program_files_x86, &local_appdata] {
            if base.is_empty() {
                continue;
            }
            for rel in [
                "Git\\cmd\\git.exe",
                "Git\\bin\\git.exe",
                "PortableGit\\cmd\\git.exe",
                "PortableGit\\bin\\git.exe",
            ] {
                candidates.push(PathBuf::from(base).join(rel));
            }
        }
        // 4.5) 已知版本化便携布局：~/.workbuddy/binaries/PortableGit/versions/<最高版>/cmd/git.exe
        //      与 scoop shim（current 链接在部分 API 下不可穿透，取最高版本号目录）
        if let Ok(up) = std::env::var("USERPROFILE") {
            let vroot = PathBuf::from(&up).join(".workbuddy\\binaries\\PortableGit\\versions");
            if let Ok(rd) = std::fs::read_dir(&vroot) {
                let mut best: Option<(Vec<u32>, PathBuf)> = None;
                for e in rd.filter_map(|e| e.ok()) {
                    let fname = e.file_name().to_string_lossy().to_string();
                    if !e.path().is_dir() {
                        continue;
                    }
                    let ver: Vec<u32> = fname.split('.').filter_map(|x| x.parse::<u32>().ok()).collect();
                    if ver.is_empty() {
                        continue;
                    }
                    if best.as_ref().map(|(v, _)| ver > *v).unwrap_or(true) {
                        best = Some((ver, e.path()));
                    }
                }
                if let Some((_, dir)) = best {
                    candidates.push(dir.join("cmd\\git.exe"));
                    candidates.push(dir.join("bin\\git.exe"));
                }
            }
            candidates.push(PathBuf::from(&up).join("scoop\\apps\\git\\current\\cmd\\git.exe"));
        }
        // 5) 随分发捆绑：exe 同级 tools/git/（放入 PortableGit 即开箱可用）
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent() {
                for rel in ["tools\\git\\cmd\\git.exe", "tools\\git\\bin\\git.exe"] {
                    candidates.push(dir.join(rel));
                }
            }
        for p in candidates {
            if p.is_file() {
                register_git_dirs(&p);
                return Ok(p);
            }
        }
        // 6) 内置运行时（懒解压）：以上都没有（用户机器上没装 git）时，把随包携带的 MinGit
        //    压缩包释放到用户目录再用——不联网、不需要管理员；用户自己装了 git 就永远走不到这里。
        if let Some(p) = ensure_bundled_git() {
            register_git_dirs(&p);
            return Ok(p);
        }
        Err("未找到 git.exe：请安装 Git for Windows（https://git-scm.com/downloads），或设置环境变量 AGENT_GIT_PATH 指向 git.exe 后重启服务".into())
    })
    .clone()
}

pub(crate) fn where_git() -> Option<PathBuf> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    Command::new("where.exe")
        .arg("git")
        .stdin(Stdio::null())
        .creation_flags(0x0800_0000) // 不闪控制台窗口
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(String::from))
        .map(PathBuf::from)
        .filter(|p| p.exists())
}

/// 设置面板「插件」分区的 git 运行时状态：可用性 / 版本 / 来源 / 内置包情况。
/// 每次调用实时取（git_exe 的结果本身是进程内缓存：新装 git 需重启服务才会重新检测）。
pub(crate) fn git_info() -> Value {
    let rt = bundled_runtime_dir();
    let zip = bundled_zip_candidates().into_iter().find(|z| z.is_file());
    let runtime_dir = rt.as_ref().map(|d| d.display().to_string());
    let runtime_ready = rt
        .as_ref()
        .map(|d| d.join("cmd\\git.exe").is_file())
        .unwrap_or(false);
    match git_exe() {
        Ok(p) => {
            let path = p.display().to_string();
            let source = if rt.as_ref().map(|d| path_is_under(&p, d)).unwrap_or(false) {
                "bundled" // 内置 zip 解压出来的
            } else if path.replace('/', "\\").contains("\\tools\\git\\") {
                "portable" // 手动放在 exe 同级 tools\git\
            } else {
                "system" // 用户自己装的
            };
            json!({
                "ok": true,
                "path": path,
                "version": git_version(&p),
                "source": source,
                "runtime_dir": runtime_dir,
                "runtime_ready": runtime_ready,
                "zip_available": zip.is_some(),
            })
        }
        Err(e) => json!({
            "ok": false,
            "error": e,
            "runtime_dir": runtime_dir,
            "runtime_ready": runtime_ready,
            "zip_available": zip.is_some(),
        }),
    }
}

/// 路径是否位于某目录下（Windows 大小写不敏感，分隔符归一）。
pub(crate) fn path_is_under(child: &Path, parent: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().to_lowercase().replace('/', "\\");
    norm(child).starts_with(&norm(parent))
}

/// `git --version` → 版本号字符串（失败返回 "unknown"）。
pub(crate) fn git_version(git: &Path) -> String {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(git)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .creation_flags(0x0800_0000) // 不闪控制台窗口
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .map(|s| s.trim_start_matches("git version ").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// 内置 git 运行时目录（懒解压目标）：`%LOCALAPPDATA%\pawch-agent\runtime\git`。
/// 放用户目录而不是程序目录：程序可能装在只读位置（Program Files），且卸载重装不丢。
/// 2026-10-22 改名迁移轮：`agent` → `pawch-agent`。旧目录（`%LOCALAPPDATA%\agent\...`，
/// 已解压的缓存）兼容一个版本——有就直接用，没有才解压新的；纯缓存，搬不搬都不丢数据。
pub(crate) fn bundled_runtime_dir() -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok().filter(|s| !s.is_empty())?;
    let base = PathBuf::from(local);
    let legacy = base.join("agent").join("runtime").join("git");
    if legacy.is_dir() {
        return Some(legacy);
    }
    Some(base.join("pawch-agent").join("runtime").join("git"))
}

/// 随包携带的 MinGit 压缩包候选位置：
/// `PAWCH_AGENT_GIT_ZIP`（旧名 `AGENT_GIT_ZIP` 兼容一个版本）环境变量 → exe 同级
/// `tools\git-runtime.zip`（打包分发布局）
/// → exe 上两级 `runtime\git-runtime.zip`（开发布局：target\release\pawch-agent.exe → 仓库根）。
pub(crate) fn bundled_zip_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(p) = crate::config::env_pawch("GIT_ZIP")
        && !p.to_string_lossy().trim().is_empty() {
            v.push(PathBuf::from(p));
        }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent() {
            v.push(dir.join("tools").join("git-runtime.zip"));
            if let Some(root) = dir.parent().and_then(Path::parent) {
                v.push(root.join("runtime").join("git-runtime.zip"));
            }
        }
    v
}

/// 用系统 tar.exe（Win10 1803+ 自带 bsdtar）解压 zip：零新依赖，不引入 zip crate。
pub(crate) fn extract_zip(zip: &Path, dest: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    std::fs::create_dir_all(dest).map_err(|e| format!("建目录失败: {e}"))?;
    let tar = std::env::var("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32").join("tar.exe"))
        .unwrap_or_else(|_| PathBuf::from("tar.exe"));
    let out = std::process::Command::new(&tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(dest)
        .stdin(std::process::Stdio::null())
        .creation_flags(0x0800_0000) // 不闪控制台窗口
        .output()
        .map_err(|e| format!("tar 启动失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "解压失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// 第 6 步：确保内置 git 可用——已解压过直接用；否则解压随包压缩包（一次性，只发生在没装 git 的机器上）。
pub(crate) fn ensure_bundled_git() -> Option<PathBuf> {
    let rt = bundled_runtime_dir()?;
    ensure_bundled_git_at(&rt, &bundled_zip_candidates())
}

/// 纯逻辑（可单测）：先看运行时目录是否已有 git；没有则用第一个存在的 zip 解压；返回可用的 git.exe。
pub(crate) fn ensure_bundled_git_at(rt: &Path, zips: &[PathBuf]) -> Option<PathBuf> {
    const RELS: [&str; 3] = ["cmd\\git.exe", "bin\\git.exe", "mingw64\\bin\\git.exe"];
    for rel in RELS {
        let p = rt.join(rel);
        if p.is_file() {
            return Some(p);
        }
    }
    let zip = zips.iter().find(|z| z.is_file())?;
    match extract_zip(zip, rt) {
        Ok(()) => {
            let found = RELS.iter().map(|rel| rt.join(rel)).find(|p| p.is_file());
            crate::observability::try_event(
                crate::observability::EventType::State,
                json!({
                    "kind": "git.runtime_extracted",
                    "zip": zip.display().to_string(),
                    "dest": rt.display().to_string(),
                    "ok": found.is_some(),
                }),
            );
            found
        }
        Err(e) => {
            crate::observability::try_event(
                crate::observability::EventType::State,
                json!({
                    "kind": "git.runtime_extract_failed",
                    "zip": zip.display().to_string(),
                    "error": e,
                }),
            );
            None
        }
    }
}

/// 把 git.exe 相关目录登记进沙箱 PATH（cmd 为主，bin/mingw64 为 helper 兜底）。
pub(crate) fn register_git_dirs(git_exe: &Path) {
    let cmd_dir = git_exe.parent().map(Path::to_path_buf);
    if let Some(d) = &cmd_dir {
        crate::sandbox::register_path_dir(d);
    }
    if let Some(root) = git_exe.parent().and_then(Path::parent) {
        for rel in ["bin", "mingw64\\bin", "usr\\bin"] {
            crate::sandbox::register_path_dir(&root.join(rel));
        }
    }
}

pub(crate) async fn git(args: &str, cfg: &ExecConfig) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let gargs = v.get("args").and_then(Value::as_str).ok_or("缺少 args 参数")?;
    if gargs.trim().is_empty() {
        return Err("args 为空".to_string());
    }
    // push / clean / rebase 这类破坏性操作不在工具层硬禁——那样只堵住 git 工具，
    // 从 shell 跑 git 照样能绕过去，反而造成两套标准。统一交给风险引擎判高危 +
    // 用户逐次审批（见 core/rules.rs risk_git），两条路径语义一致。
    let timeout_ms = cfg.sandbox.timeout_ms.clamp(1, 300_000);
    let git = git_exe()?;
    let command_line = format!("\"{}\" {gargs}", git.display());
    run_in_sandbox(cfg, git, command_line, timeout_ms).await
}
