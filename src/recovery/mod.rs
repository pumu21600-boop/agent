//! 错误恢复 —— 总架构 §14 / 施工单 007（P6）
//!
//! 「假设一定会崩，把能恢复做成默认」：
//! - checkpoint：把工作区文件提交进影子仓库 `<workspace>/.agent/shadow`
//!   （独立 git dir，work-tree 指向 workspace）。git 工作区只跟踪真仓库 `git ls-files`
//!   的文件；非 git 工作区整目录扫描兜底（2026-10-02 产物面板实锤后加），均套排除规则，
//!   防 IO 爆炸；失败只记日志，绝不阻断工具执行。
//! - restore：影子仓库 `reset --hard` 回指定检查点（含删除其后新建的已跟踪文件）；
//!   shell 副作用不可回滚，靠沙箱兜底。
//! - HANDOFF：交接文档落 `<workspace>/HANDOFF.md`，头部强制项目名/remote/commit/时间戳。
//!
//! 所有 git 调用都是阻塞的，调用方（核心循环/server）包 spawn_blocking。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, anyhow, bail};
use chrono::{SecondsFormat, Utc};

/// 单个检查点（id 为影子仓库完整 commit 哈希）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Checkpoint {
    pub id: String,
    pub ts: String,
    pub reason: String,
    /// 拍点时所属会话 id（旧检查点无此信息，缺省空串）。
    /// 有了归属，切回某条会话时才能只显示它自己的改动、只回滚它自己的基线。
    #[serde(default)]
    pub session: String,
}

/// 真仓库 origin remote url（无 remote/非仓库返回 None）。记忆系统项目身份用。
pub fn remote_url(ws: &Path) -> Option<String> {
    let bin = git_bin().ok()?;
    git_ok(&bin, None, ws, &["remote", "get-url", "origin"]).ok().filter(|s| !s.is_empty())
}

/// 真仓库根目录（非 git 仓库返回 None）。
pub fn root_path(ws: &Path) -> Option<PathBuf> {
    let bin = git_bin().ok()?;
    let out = git_ok(&bin, None, ws, &["rev-parse", "--show-toplevel"]).ok()?;
    if out.is_empty() { None } else { Some(PathBuf::from(out)) }
}

/// 影子仓库 git dir（workspace/.agent/shadow/.git）。
fn shadow_gitdir(ws: &Path) -> PathBuf {
    ws.join(".agent").join("shadow").join(".git")
}

/// 审查面板的数据源（对着 ZCode 审查面板的 source 选择器：未暂存 / 上一轮…）。
/// - `Shadow`：影子仓库相对某个检查点 = 「本任务改动」（默认，产物/审查一直用这条）
/// - `Workspace`：工作区自己的真仓库相对 HEAD = 「未提交改动」
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoOrigin {
    Shadow,
    Workspace,
}

impl RepoOrigin {
    /// 查询串 → 枚举（认 "workspace"/"work"，其余一律影子仓库，保持旧行为）。
    pub fn from_query(s: Option<&str>) -> Self {
        match s.map(|x| x.trim()) {
            Some("workspace") | Some("work") => Self::Workspace,
            _ => Self::Shadow,
        }
    }

    /// 对应的 git dir。None = 让 git 从工作区自己发现真仓库（与 `status()` 同一条路）。
    fn gitdir(&self, ws: &Path) -> Option<PathBuf> {
        match self {
            Self::Shadow => Some(shadow_gitdir(ws)),
            Self::Workspace => None,
        }
    }

    /// 基线形态校验。真仓库这一档额外放行 "HEAD"（工作树自身的最新提交）。
    fn check_baseline(&self, baseline: &str) -> anyhow::Result<()> {
        let bare = baseline.trim();
        if bare.is_empty() {
            bail!("缺少 baseline");
        }
        if *self == Self::Workspace && bare.eq_ignore_ascii_case("HEAD") {
            return Ok(());
        }
        if !bare.chars().all(|c| c.is_ascii_hexdigit()) || !(7..=40).contains(&bare.len()) {
            bail!("非法基线 id: {bare}");
        }
        Ok(())
    }
}

/// 排除目录：影子仓库永不跟踪（真仓库 ls-files 之外的第二道防线，防 IO 爆炸）。
/// .attachments 是粘贴媒体的中转目录（turn.rs 会把它写进 .gitignore），同样不进影子仓库。
const EXCLUDED_DIRS: &[&str] = &["node_modules", "target", "dist", "logs", ".agent", ".git", ".attachments"];
/// 单文件体积上限，超过直接跳过（大二进制不走影子仓库）。
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

fn git_bin() -> anyhow::Result<PathBuf> {
    crate::tools::git_exe().map_err(|e| anyhow!(e))
}

fn git(bin: &Path, gitdir: Option<&Path>, ws: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new(bin);
    // 后台无窗口运行时，控制台子进程会闪黑框：一律不创建新控制台窗口
    cmd.creation_flags(0x0800_0000);
    match gitdir {
        Some(g) => {
            cmd.arg("--git-dir").arg(g);
            cmd.arg("--work-tree").arg(ws);
        }
        None => {
            cmd.arg("-C").arg(ws);
        }
    }
    cmd.args(args).current_dir(ws).stdin(Stdio::null());
    // 父环境可能带 GIT_DIR/GIT_WORK_TREE 等变量，一律剥掉防串仓库
    for k in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_COMMON_DIR"] {
        cmd.env_remove(k);
    }
    cmd.output().context("启动 git 失败")
}

fn git_ok(bin: &Path, gitdir: Option<&Path>, ws: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = git(bin, gitdir, ws, args)?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {} 失败: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 确保影子仓库就绪（幂等）：git init + 本地 config（user/autocrlf/gpgsign）。
fn ensure_shadow(bin: &Path, ws: &Path) -> anyhow::Result<PathBuf> {
    let gitdir = shadow_gitdir(ws);
    if !gitdir.exists() {
        let shadow_root = ws.join(".agent").join("shadow");
        std::fs::create_dir_all(&shadow_root).with_context(|| format!("建影子仓库目录失败: {}", shadow_root.display()))?;
        git_ok(bin, None, &shadow_root, &["init", "--quiet"])?;
        for cfg in [
            ["user.name", "agent-checkpoint"],
            ["user.email", "agent@local"],
            ["core.autocrlf", "false"],
            ["commit.gpgsign", "false"],
        ] {
            git_ok(bin, Some(&gitdir), ws, &["config", cfg[0], cfg[1]])?;
        }
    }
    Ok(gitdir)
}

/// 影子仓库要跟踪的文件清单。真仓库是 git 仓库时沿用它的 ls-files（老语义：
/// untracked 文件不属于影子仓库，restore 不动它）；工作区**不是 git 仓库**时
/// git ls-files 无从谈起，退化为目录扫描（排除目录 + 体积上限）——产物/检查点
/// 的真机实锤：普通文件夹（Desktop\th 这类）不是 git 仓库，检查点永远拍不出来，
/// 「产物」面板就没有基线可看。两种来源都套同一套排除与体积规则。
fn tracked_files(bin: &Path, ws: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let probe = git(bin, None, ws, &["rev-parse", "--is-inside-work-tree"]);
    let is_git_ws = matches!(&probe, Ok(out) if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true");
    if !is_git_ws {
        return walk_files(ws);
    }
    let out = git(bin, None, ws, &["ls-files", "-z"])?;
    if !out.status.success() {
        bail!("ls-files 失败: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let mut files = Vec::new();
    for entry in out.stdout.split(|b| *b == 0) {
        if entry.is_empty() {
            continue;
        }
        let rel = PathBuf::from(String::from_utf8_lossy(entry).replace('\\', "/"));
        let excluded = rel.components().any(|c| {
            matches!(c, std::path::Component::Normal(_))
                && EXCLUDED_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref())
        });
        if excluded {
            continue;
        }
        let full = ws.join(&rel);
        let Ok(meta) = std::fs::metadata(&full) else { continue };
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            continue;
        }
        files.push(rel);
    }
    Ok(files)
}

/// 非 git 工作区的兜底清单：整棵扫（软链接不进，防环；单文件 >10MB 跳过；
/// 排除目录整枝剪掉）。文件数硬上限 20000——普通项目目录远够，失控目录不拖死检查点。
fn walk_files(ws: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        if files.len() >= 20_000 {
            bail!("工作区文件数超过 20000，影子仓库拒收（用 git 仓库或清理目录后重试）");
        }
        let full = ws.join(&rel);
        let Ok(rd) = std::fs::read_dir(&full) else { continue };
        for entry in rd.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let child = rel.join(&*name);
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_symlink() {
                continue;
            }
            if meta.is_dir() {
                if EXCLUDED_DIRS.contains(&name.as_ref()) || name.starts_with('.') {
                    continue;
                }
                stack.push(child);
            } else if meta.is_file() && meta.len() <= MAX_FILE_BYTES {
                files.push(child);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// 拍检查点：add + 有变更才 commit。返回 None 表示没有需要落的变化。
/// 任何失败都由调用方决定降级（核心循环只记日志不阻断）。
/// `session_id` 写进提交正文的 trailer，供改动条/撤销按会话对齐。
/// 跟踪范围：git 工作区沿用真仓库 ls-files；非 git 工作区整目录扫（见 tracked_files）。
pub fn checkpoint(ws: &Path, reason: &str, session_id: &str) -> anyhow::Result<Option<Checkpoint>> {
    let bin = git_bin()?;
    let gitdir = ensure_shadow(&bin, ws)?;

    // 真仓库本身得是个 git 仓库（ls-files 能跑通）
    let files = tracked_files(&bin, ws)?;
    if files.is_empty() {
        return Ok(None);
    }

    // pathspec 走文件，防 Windows 命令行 8191 字符上限
    let spec = std::env::temp_dir().join(format!("agent-cp-{}.nul", uuid::Uuid::new_v4().simple()));
    let mut buf = Vec::new();
    for f in &files {
        buf.extend_from_slice(f.to_string_lossy().as_bytes());
        buf.push(0);
    }
    std::fs::write(&spec, &buf).context("写 pathspec 文件失败")?;

    let add = git(
        &bin,
        Some(&gitdir),
        ws,
        &["add", "--pathspec-from-file", &spec.to_string_lossy(), "--pathspec-file-nul"],
    );
    let _ = std::fs::remove_file(&spec);
    let out = add?;
    if !out.status.success() {
        return Err(anyhow!("git add 失败: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }

    // 无变更不造空提交
    if git(&bin, Some(&gitdir), ws, &["diff", "--cached", "--quiet"])?.status.success() {
        return Ok(None);
    }

    let sid = session_id.trim();
    let msg = if sid.is_empty() {
        format!("checkpoint: {}", one_line(reason, 120))
    } else {
        // trailer 单独成段：git log %b 可解析，人看也清楚
        format!("checkpoint: {}\n\nAgent-Session: {}", one_line(reason, 120), sid)
    };
    git_ok(&bin, Some(&gitdir), ws, &["commit", "--quiet", "-m", &msg])?;
    let id = git_ok(&bin, Some(&gitdir), ws, &["rev-parse", "HEAD"])?;
    Ok(Some(Checkpoint {
        id,
        ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        reason: one_line(reason, 200),
        session: sid.to_string(),
    }))
}

/// 最近 limit 个检查点（新→旧）。
/// 正文带 `Agent-Session:` trailer 的解析出归属会话；`-z` 让记录以 NUL 分隔，
/// 这样多行正文不会破坏按行切分。
pub fn list(ws: &Path, limit: usize) -> anyhow::Result<Vec<Checkpoint>> {
    let bin = git_bin()?;
    let gitdir = shadow_gitdir(ws);
    if !gitdir.exists() {
        return Ok(vec![]);
    }
    let out = git_ok(
        &bin,
        Some(&gitdir),
        ws,
        &["log", "-n", &limit.to_string(), "-z", "--pretty=%H%x1f%aI%x1f%s%x1f%b"],
    )?;
    Ok(out
        .split('\0')
        .filter_map(|rec| {
            let mut it = rec.splitn(4, '\x1f');
            let id = it.next()?.trim().to_string();
            if id.is_empty() {
                return None;
            }
            let ts = it.next().unwrap_or("").to_string();
            let reason = it.next().unwrap_or("").strip_prefix("checkpoint: ").unwrap_or("").to_string();
            let session = it
                .next()
                .unwrap_or("")
                .lines()
                .find_map(|l| l.trim().strip_prefix("Agent-Session:"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            Some(Checkpoint { id, ts, reason, session })
        })
        .collect())
}

/// 工作区 git 概览：是否仓库、当前分支、HEAD、脏文件数、上游领先/落后、未提交文件清单。
/// 非仓库（或 git 不可用）返回 is_repo=false，不报错——前端据此隐藏 git 信息。
#[derive(Debug, Clone, serde::Serialize)]
pub struct GitStatus {
    pub is_repo: bool,
    pub branch: String,
    pub head: String,
    pub dirty: usize,
    pub upstream: String,
    pub ahead: u64,
    pub behind: u64,
    /// 未提交文件明细（前端「待提交」列表用），最多 `GIT_FILES_CAP` 条
    pub files: Vec<GitFile>,
}

/// 一个未提交文件：`status` 是 git porcelain 的两字符状态码（`M` / `??` / `A` / `D` / `R` …）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct GitFile {
    pub status: String,
    pub path: String,
}

/// 明细列表上限：够用又不至于把 SSE/HTTP 响应撑爆。
const GIT_FILES_CAP: usize = 500;

pub fn status(ws: &Path) -> GitStatus {
    let blank = GitStatus {
        is_repo: false,
        branch: String::new(),
        head: String::new(),
        dirty: 0,
        upstream: String::new(),
        ahead: 0,
        behind: 0,
        files: Vec::new(),
    };
    let Ok(bin) = git_bin() else { return blank };
    match git_ok(&bin, None, ws, &["rev-parse", "--is-inside-work-tree"]) {
        Ok(t) if t.trim() == "true" => {}
        _ => return blank,
    }
    let mut st = GitStatus { is_repo: true, ..blank };
    st.branch = git_ok(&bin, None, ws, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    if st.branch == "HEAD" {
        st.branch = "（分离头指针）".to_string();
    }
    st.head = git_ok(&bin, None, ws, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    // core.quotepath=false：中文/特殊字符路径按原样输出，不转义成 \xxx 八进制
    if let Ok(out) = git_ok(&bin, None, ws, &["-c", "core.quotepath=false", "status", "--porcelain=v1", "-b"]) {
        for line in out.lines() {
            if let Some(rest) = line.strip_prefix("## ") {
                // `## main...origin/main [ahead 1, behind 2]`
                if let Some((_, tail)) = rest.split_once("...") {
                    st.upstream = tail.split_whitespace().next().unwrap_or("").to_string();
                }
                if let Some(idx) = rest.find('[') {
                    let inner = rest[idx + 1..].trim_end_matches(']');
                    for part in inner.split(',') {
                        let p = part.trim();
                        if let Some(v) = p.strip_prefix("ahead ") {
                            st.ahead = v.trim().parse().unwrap_or(0);
                        } else if let Some(v) = p.strip_prefix("behind ") {
                            st.behind = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
                continue;
            }
            if line.trim().is_empty() {
                continue;
            }
            st.dirty += 1;
            if st.files.len() >= GIT_FILES_CAP {
                continue;
            }
            let status: String = line.chars().take(2).collect::<String>().trim().to_string();
            let rest: String = line.chars().skip(2).collect();
            // 重命名形如 `old -> new`，取新名字；路径两侧可能带引号（含特殊字符时）
            let path = rest
                .trim()
                .rsplit(" -> ")
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"')
                .to_string();
            st.files.push(GitFile { status, path });
        }
    }
    st
}

/// 提交工作区改动（用户在界面上手动提交）。`files` 为空 = `add -A` 全部，
/// 否则只 stage 指定路径。返回短哈希；失败（含「没有可提交的改动」）原样上抛 git 的报错。
pub fn commit(ws: &Path, message: &str, files: &[String]) -> anyhow::Result<String> {
    let msg = message.trim();
    if msg.is_empty() {
        bail!("提交信息不能为空");
    }
    let bin = git_bin()?;
    match git_ok(&bin, None, ws, &["rev-parse", "--is-inside-work-tree"]) {
        Ok(t) if t.trim() == "true" => {}
        _ => bail!("当前工作区不是 git 仓库"),
    }
    if files.is_empty() {
        git_ok(&bin, None, ws, &["add", "-A"])?;
    } else {
        let mut args: Vec<&str> = vec!["add", "--"];
        args.extend(files.iter().map(String::as_str));
        git_ok(&bin, None, ws, &args)?;
    }
    git_ok(&bin, None, ws, &["commit", "-m", msg])?;
    git_ok(&bin, None, ws, &["rev-parse", "--short", "HEAD"])
}

/// 工作树恢复到指定检查点。只回退**该检查点之后被影子仓库跟踪且发生变化的文件**
/// （agent 实际碰过的），绝不整树 reset——基线过旧时不会误伤 agent 没碰过的文件
/// （如用户自己已提交/未提交的改动）。只动影子仓库与工作区文件，不碰真仓库 HEAD。
pub fn restore(ws: &Path, id: &str) -> anyhow::Result<()> {
    if !id.chars().all(|c| c.is_ascii_hexdigit()) || !(7..=40).contains(&id.len()) {
        bail!("非法检查点 id: {id}");
    }
    let bin = git_bin()?;
    let gitdir = shadow_gitdir(ws);
    if !gitdir.exists() {
        bail!("影子仓库不存在");
    }
    // 该检查点与当前 HEAD 之间 agent 改了哪些文件（M/D=改/删 → 恢复；A=新增 → 删除）
    let out = git_ok(&bin, Some(&gitdir), ws, &["diff", "--name-status", id, "HEAD"])?;
    let mut touched = 0usize;
    for line in out.lines() {
        let mut it = line.splitn(2, '\t');
        let status = it.next().unwrap_or("").to_string();
        let path = match it.next() {
            Some(p) => p.trim().to_string(),
            None => continue,
        };
        let status0 = status.chars().next().unwrap_or('M');
        if status0 == 'A' {
            // 影子仓库在检查点后新增的文件：直接从工作区删掉
            let full = ws.join(&path);
            let _ = std::fs::remove_file(&full);
            let _ = git_ok(&bin, Some(&gitdir), ws, &["reset", "--quiet", id, "--", &path]);
        } else {
            // M / D：把该文件恢复回检查点时的内容
            git_ok(&bin, Some(&gitdir), ws, &["checkout", id, "--", &path])?;
        }
        touched += 1;
    }
    if touched == 0 {
        bail!("该检查点之后没有需要回退的文件改动");
    }
    Ok(())
}

/// 影子仓库当前 HEAD（最近一次检查点）；空仓库/不存在返回 None。
/// 用作任务起点基线：结束时的改动统计与「撤销」都以它为参照。
pub fn head_id(ws: &Path) -> anyhow::Result<Option<String>> {
    let gitdir = shadow_gitdir(ws);
    if !gitdir.exists() {
        return Ok(None);
    }
    let bin = git_bin()?;
    match git_ok(&bin, Some(&gitdir), ws, &["rev-parse", "HEAD"]) {
        Ok(id) => Ok(Some(id)),
        // 空仓库 rev-parse 失败属正常
        Err(_) => Ok(None),
    }
}

/// 工作树相对基线 commit 的统一 diff 文本（审查面板逐文件看改动用）。
/// 上限 512KB，防长会话 diff 把面板撑爆；超限尾部标省略。
/// `origin` 决定跑影子仓库还是工作区自己的真仓库（「未提交改动」档）。
pub fn diff_text(
    ws: &Path,
    origin: RepoOrigin,
    baseline: &str,
    path: Option<&str>,
) -> anyhow::Result<String> {
    origin.check_baseline(baseline)?;
    let bin = git_bin()?;
    let gitdir = origin.gitdir(ws);
    match (&origin, &gitdir) {
        (RepoOrigin::Shadow, Some(g)) if !g.exists() => bail!("影子仓库不存在"),
        (RepoOrigin::Workspace, _) => {
            let inside = git_ok(&bin, None, ws, &["rev-parse", "--is-inside-work-tree"])
                .map(|s| s.trim() == "true")
                .unwrap_or(false);
            if !inside {
                bail!("工作区不在 git 仓库中");
            }
        }
        _ => {}
    }
    let mut args = vec!["diff".to_string(), baseline.trim().to_string()];
    if let Some(p) = path {
        // 只做形态校验，**不 resolve 到磁盘**——已删除的文件（技能迁移/agent 删的）
        // 工作树里已不存在，但 diff 照样要能出「删除」段（真机实锤：os error 3）。
        // 禁止绝对路径/盘符/.. 穿越。
        let p = p.trim();
        if p.is_empty() || Path::new(p).is_absolute() || p.contains(':') || p.split(['/', '\\']).any(|s| s == "..") {
            bail!("非法路径: {p}");
        }
        args.push("--".to_string());
        args.push(p.replace('\\', "/"));
    }
    let out = git_ok(&bin, gitdir.as_deref(), ws, &args.iter().map(String::as_str).collect::<Vec<_>>())?;
    const MAX: usize = 512 * 1024;
    if out.len() > MAX {
        Ok(format!("{}…（diff 超过 512KB 已截断）", &out[..MAX]))
    } else {
        Ok(out)
    }
}

/// 改动统计（保留原样）：工作树相对基线 commit 的 (文件数, 增行, 删行)。
/// 用 `diff --numstat`（覆盖暂存+未暂存，即当前工作树全量相对基线）。
pub fn diff_stat(ws: &Path, baseline: &str) -> anyhow::Result<(usize, u64, u64)> {
    if !baseline.chars().all(|c| c.is_ascii_hexdigit()) || !(7..=40).contains(&baseline.len()) {
        bail!("非法基线 id: {baseline}");
    }
    let bin = git_bin()?;
    let gitdir = shadow_gitdir(ws);
    if !gitdir.exists() {
        bail!("影子仓库不存在");
    }
    let out = git_ok(&bin, Some(&gitdir), ws, &["diff", "--numstat", baseline])?;
    let mut files = 0usize;
    let (mut adds, mut dels) = (0u64, 0u64);
    for line in out.lines() {
        let mut it = line.splitn(3, '\t');
        let (a, d) = match (it.next(), it.next()) {
            (Some(a), Some(d)) => (a, d),
            _ => continue,
        };
        files += 1;
        adds += a.parse::<u64>().unwrap_or(0); // 二进制文件为 "-"，按 0 计
        dels += d.parse::<u64>().unwrap_or(0);
    }
    Ok((files, adds, dels))
}

/// 一个改动文件（2026-10-02 审查面板/展开列表用，改动条原来只有计数）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChangedFile {
    /// 工作区相对路径
    pub path: String,
    /// git 两字状态码首字母：M 改 / A 增 / D 删；未跟踪为 '?'
    pub status: char,
    /// 新增行数（二进制/未跟踪为 0）
    pub adds: u64,
    /// 删除行数（二进制/未跟踪为 0）
    pub dels: u64,
}

/// 工作树相对基线 commit 的改动文件清单（`diff --numstat` + `--name-status` 合并）。
/// **追加未跟踪文件**（`ls-files --others`）：任务里新建的文件不入 git diff
/// （基线检查点之后 add 过才可见），产物面板「本任务还没有产物」的第二个实锤根因——
/// agent 造出的新文件必须原样出现在清单里。排除目录/大文件与检查点同一套规则。
pub fn changed_files(
    ws: &Path,
    origin: RepoOrigin,
    baseline: &str,
) -> anyhow::Result<Vec<ChangedFile>> {
    origin.check_baseline(baseline)?;
    let bin = git_bin()?;
    let gitdir = origin.gitdir(ws);
    match (&origin, &gitdir) {
        (RepoOrigin::Shadow, Some(g)) if !g.exists() => bail!("影子仓库不存在"),
        (RepoOrigin::Workspace, _) => {
            let inside = git_ok(&bin, None, ws, &["rev-parse", "--is-inside-work-tree"])
                .map(|s| s.trim() == "true")
                .unwrap_or(false);
            if !inside {
                bail!("工作区不在 git 仓库中");
            }
        }
        _ => {}
    }
    let base = baseline.trim().to_string();
    // numstat：每行 "adds\tdels\tpath"
    let numstat = git_ok(&bin, gitdir.as_deref(), ws, &["diff", "--numstat", &base])?;
    // name-status：每行 "M\tpath"（未跟踪不在此列）
    let status = git_ok(&bin, gitdir.as_deref(), ws, &["diff", "--name-status", &base])?;
    let mut status_of = std::collections::HashMap::new();
    for line in status.lines() {
        let mut it = line.splitn(2, '\t');
        if let (Some(s), Some(p)) = (it.next(), it.next()) {
            status_of.insert(p.to_string(), s.chars().next().unwrap_or('M'));
        }
    }
    let mut out: Vec<ChangedFile> = Vec::new();
    for line in numstat.lines() {
        let mut it = line.splitn(3, '\t');
        let a = it.next().unwrap_or("0");
        let d = it.next().unwrap_or("0");
        if let Some(p) = it.next() {
            out.push(ChangedFile {
                path: p.to_string(),
                status: status_of.get(p).copied().unwrap_or('M'),
                adds: a.parse().unwrap_or(0),
                dels: d.parse().unwrap_or(0),
            });
        }
    }
    // 未跟踪：--directory 让整个被排除目录折叠成一条（不逐个走进 node_modules 之类的深渊）；
    // --exclude 与 EXCLUDED_DIRS 同源，非 git 工作区没有 .gitignore，全靠它兜
    let mut others_cmd = vec![
        "ls-files".to_string(),
        "--others".to_string(),
        "-z".to_string(),
        "--directory".to_string(),
        "--exclude-standard".to_string(),
    ];
    for dir in EXCLUDED_DIRS {
        others_cmd.push(format!("--exclude={dir}"));
    }
    let others = git_ok(
        &bin,
        gitdir.as_deref(),
        ws,
        &others_cmd.iter().map(String::as_str).collect::<Vec<_>>(),
    )?;
    for entry in others.split('\0') {
        if entry.is_empty() {
            continue;
        }
        let rel = entry.trim_end_matches('/');
        if rel.is_empty() || out.iter().any(|f| f.path == rel) {
            continue;
        }
        let path = PathBuf::from(rel);
        let excluded = path.components().any(|c| {
            matches!(c, std::path::Component::Normal(_))
                && EXCLUDED_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref())
        });
        if excluded {
            continue;
        }
        let Ok(meta) = std::fs::metadata(ws.join(&path)) else { continue };
        if meta.is_file() && meta.len() <= MAX_FILE_BYTES {
            out.push(ChangedFile {
                path: rel.replace('\\', "/"),
                status: '?',
                adds: 0,
                dels: 0,
            });
        }
    }
    Ok(out)
}

/// 测试与排障用：影子仓库当前跟踪的文件清单。
#[cfg(test)]
pub fn shadow_files(ws: &Path) -> Vec<String> {
    let bin = crate::tools::git_exe().expect("git 可用");
    let gitdir = shadow_gitdir(ws);
    git_ok(&bin, Some(&gitdir), ws, &["ls-files"])
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

fn one_line(s: &str, max_chars: usize) -> String {
    let flat: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    flat.chars().take(max_chars).collect()
}



#[cfg(test)]
mod tests {
    use super::*;

    fn git_have() -> bool {
        crate::tools::git_exe().is_ok()
    }

    /// 建临时工作区：git init + 文件入 index（staged 即算 tracked，无需真 commit）
    fn temp_ws(tag: &str) -> TestWs {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("agent-p6-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TestWs(p)
    }
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    struct TestWs(PathBuf);
    impl TestWs {
        fn path(&self) -> &Path {
            &self.0
        }
        fn git(&self, args: &[&str]) {
            let bin = crate::tools::git_exe().unwrap();
            let out = git(&bin, None, &self.0, args).unwrap();
            assert!(out.status.success(), "git {:?} 失败: {}", args, String::from_utf8_lossy(&out.stderr));
        }
    }
    impl Drop for TestWs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn checkpoint_create_list_restore() {
        if !git_have() {
            return;
        }
        let ws = temp_ws("cp");
        let w = ws.path();
        ws.git(&["init", "--quiet"]);
        // a.txt 入 index（tracked）；new.txt 是 untracked（影子仓库不应跟踪）
        std::fs::write(w.join("a.txt"), "v1\n").unwrap();
        ws.git(&["add", "a.txt"]);
        std::fs::write(w.join("new.txt"), "agent-made\n").unwrap();

        // 第一次检查点：a.txt=v1
        let cp1 = checkpoint(w, "任务 A 开始改 a.txt", "sess-test").unwrap().expect("有 tracked 文件应有检查点");

        // 改 a.txt + 新建 tracked 文件 b.txt
        std::fs::write(w.join("a.txt"), "v2\n").unwrap();
        std::fs::write(w.join("b.txt"), "b\n").unwrap();
        ws.git(&["add", "b.txt"]);
        let cp2 = checkpoint(w, "任务 A 第二步", "sess-test").unwrap().expect("有变更应有检查点");
        assert_ne!(cp1.id, cp2.id);

        // 无变更再拍 → None
        assert!(checkpoint(w, "重复拍", "sess-test").unwrap().is_none(), "无变更不应产出检查点");

        let list = list(w, 10).unwrap();
        assert_eq!(list.len(), 2, "两个检查点: {list:?}");
        assert_eq!(list[0].id, cp2.id, "新→旧");
        assert!(list[1].reason.contains("任务 A 开始"), "reason 保留: {:?}", list[1].reason);

        // restore 到 cp1：a.txt 回 v1；cp2 引入的 b.txt（影子仓库已跟踪）应被删；new.txt 不受影响
        restore(w, &cp1.id).unwrap();
        assert_eq!(std::fs::read_to_string(w.join("a.txt")).unwrap(), "v1\n");
        assert!(!w.join("b.txt").exists(), "cp2 新建的已跟踪文件应被回滚删除");
        assert!(w.join("new.txt").exists(), "untracked 文件不属于影子仓库，不动");

        // 影子仓库只跟踪真仓库 tracked 的文件
        let files = shadow_files(w);
        assert!(files.contains(&"a.txt".to_string()));
        assert!(!files.iter().any(|f| f == "new.txt"), "untracked 不进影子仓库: {files:?}");
    }

    /// 检查点归属：会话 id 写进提交正文，list() 能解析回来；旧检查点（无 trailer）为空串。
    #[test]
    fn checkpoint_records_session_owner() {
        if !git_have() {
            return;
        }
        let ws = temp_ws("owner");
        let w = ws.path();
        ws.git(&["init", "--quiet"]);
        std::fs::write(w.join("a.txt"), "v1\n").unwrap();
        ws.git(&["add", "a.txt"]);

        checkpoint(w, "会话甲的第一步", "sess-aaa").unwrap().expect("应有检查点");
        std::fs::write(w.join("a.txt"), "v2\n").unwrap();
        // 空 session（旧行为/无会话上下文）不应写出 trailer
        checkpoint(w, "无归属", "").unwrap().expect("应有检查点");

        let list = list(w, 10).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].session, "", "空 session 不写 trailer");
        assert_eq!(list[1].session, "sess-aaa", "会话 id 应能从 %b 解析回来");
        assert!(list[1].reason.contains("会话甲"), "reason 不受 trailer 影响: {:?}", list[1].reason);

        // 按会话过滤（server 层 /api/checkpoints?session= 用的就是这条判定）
        let mine: Vec<_> = list.iter().filter(|c| c.session == "sess-aaa").collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].reason, "会话甲的第一步");
    }

    #[test]
    fn checkpoint_excludes_and_size_filter() {
        if !git_have() {
            return;
        }
        let ws = temp_ws("exclude");
        let w = ws.path();
        ws.git(&["init", "--quiet"]);
        std::fs::create_dir_all(w.join("node_modules")).unwrap();
        std::fs::write(w.join("node_modules").join("pkg.js"), "big dep\n").unwrap();
        std::fs::write(w.join("normal.txt"), "ok\n").unwrap();
        ws.git(&["add", "node_modules/pkg.js", "normal.txt"]);

        assert!(checkpoint(w, "排除规则", "sess-test").unwrap().is_some());
        let files = shadow_files(w);
        assert!(files.contains(&"normal.txt".to_string()));
        assert!(!files.iter().any(|f| f.starts_with("node_modules")), "排除目录不进影子仓库: {files:?}");

        // 大文件跳过：影子仓库不跟踪它 → 相对上次检查点无可见变更 → None
        std::fs::write(w.join("huge.bin"), vec![0u8; 11 * 1024 * 1024]).unwrap();
        ws.git(&["add", "huge.bin"]);
        assert!(checkpoint(w, "大文件前", "sess-test").unwrap().is_none(), "被排除的大文件不应触发检查点");
        let files = shadow_files(w);
        assert!(!files.iter().any(|f| f == "huge.bin"), "超 10MB 文件不进影子仓库: {files:?}");
    }

    #[test]
    fn plain_directory_workspace_gets_checkpoints_and_artifacts() {
        // 产物面板实锤（2026-10-02）：用户工作区 Desktop\th 不是 git 仓库，
        // ls-files 无从谈起 → 检查点永远拍不出来 → 产物永远空。非 git 工作区
        // 现在走目录扫描兜底；agent 新建（未跟踪）文件也必须进 changed_files。
        if !git_have() {
            return;
        }
        let ws = temp_ws("plain");
        let w = ws.path();
        // 故意不 git init——普通文件夹
        std::fs::write(w.join("doc.txt"), "v1\n").unwrap();
        let cp1 = checkpoint(w, "普通目录基线", "sess-plain")
            .unwrap()
            .expect("非 git 工作区也应有检查点");

        std::fs::write(w.join("doc.txt"), "v2\n").unwrap();
        std::fs::write(w.join("made-by-agent.txt"), "new\n").unwrap();
        let files = changed_files(w, RepoOrigin::Shadow, &cp1.id).unwrap();
        assert!(files.iter().any(|f| f.path == "doc.txt"), "修改的文件要列出: {files:?}");
        assert!(files.iter().any(|f| f.path == "made-by-agent.txt"), "新建文件要列出: {files:?}");

        // 收尾检查点（真实流程里任务结束会再拍一张），restore 才有「检查点之后」的提交可回退
        let cp2 = checkpoint(w, "普通目录收尾", "sess-plain").unwrap().expect("有变更应有检查点");
        assert_ne!(cp1.id, cp2.id);
        restore(w, &cp1.id).unwrap();
        assert_eq!(std::fs::read_to_string(w.join("doc.txt")).unwrap(), "v1\n");
        assert!(!w.join("made-by-agent.txt").exists(), "检查点后新建的文件已被影子仓库跟踪，回滚应删除");
    }

    #[test]
    fn workspace_origin_reads_real_repo_against_head() {
        // 审查面板「未提交改动」档（origin=workspace）：跑的是工作区自己的真仓库，
        // 基线是 HEAD，不是影子仓库检查点。非 git 工作区要给「不在 git 仓库中」。
        if !git_have() {
            return;
        }
        let ws = temp_ws("wsorigin");
        let w = ws.path();
        ws.git(&["init", "--quiet"]);
        std::fs::write(w.join("tracked.txt"), "v1\n").unwrap();
        ws.git(&["add", "tracked.txt"]);
        ws.git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", "base"]);

        // 工作树改动 + 未跟踪新文件
        std::fs::write(w.join("tracked.txt"), "v2\n").unwrap();
        std::fs::write(w.join("fresh.txt"), "new\n").unwrap();

        let files = changed_files(w, RepoOrigin::Workspace, "HEAD").unwrap();
        assert!(files.iter().any(|f| f.path == "tracked.txt"), "相对 HEAD 的改动要列出: {files:?}");
        assert!(files.iter().any(|f| f.path == "fresh.txt" && f.status == '?'), "真仓库未跟踪文件也要列出: {files:?}");

        let d = diff_text(w, RepoOrigin::Workspace, "HEAD", Some("tracked.txt")).unwrap();
        assert!(d.contains("-v1") && d.contains("+v2"), "真仓库 diff 要出改动行: {d}");

        // 非 git 工作区：不给影子仓库兜底，明确报「不在 git 仓库中」
        let plain = temp_ws("wsorigin-plain");
        let e = changed_files(plain.path(), RepoOrigin::Workspace, "HEAD").unwrap_err().to_string();
        assert!(e.contains("不在 git 仓库"), "应说人话: {e}");
        // 影子仓库这一档仍然只要检查点 id
        assert!(RepoOrigin::from_query(Some("shadow")).eq(&RepoOrigin::Shadow));
        assert!(RepoOrigin::from_query(Some("work")).eq(&RepoOrigin::Workspace));
        assert!(RepoOrigin::from_query(None).eq(&RepoOrigin::Shadow));
    }

    #[test]
    fn head_id_and_diff_stat() {
        if !git_have() {
            return;
        }
        let ws = temp_ws("diffstat");
        let w = ws.path();
        ws.git(&["init", "--quiet"]);
        assert!(head_id(w).unwrap().is_none(), "空影子仓库无 HEAD");

        std::fs::write(w.join("a.txt"), "v1\n").unwrap();
        ws.git(&["add", "a.txt"]);
        let cp = checkpoint(w, "基线", "sess-test").unwrap().expect("应有检查点");
        assert_eq!(head_id(w).unwrap().as_deref(), Some(cp.id.as_str()));

        // 检查点之后改 2 个文件：结束快照提交进影子仓库后，diff_stat 相对基线应为 2 文件 +7 -1
        std::fs::write(w.join("a.txt"), "v2\nline2\nline3\nline4\n").unwrap();
        std::fs::write(w.join("b.txt"), "1\n2\n3\n4\n").unwrap();
        ws.git(&["add", "b.txt"]);
        checkpoint(w, "任务结束快照", "sess-test").unwrap().expect("有变更应提交");
        let (files, adds, dels) = diff_stat(w, &cp.id).unwrap();
        assert_eq!(files, 2, "numstat 两行: {files}");
        assert_eq!((adds, dels), (8, 1), "a.txt +4/-1，b.txt 新文件 +4/-0: ({adds},{dels})");

        // 撤销语义验证：reset 回基线 → 改动还原、任务内新建文件被删、diff 归零
        restore(w, &cp.id).unwrap();
        assert!(!w.join("b.txt").exists(), "结束快照已跟踪的新文件应被回滚删除");
        assert_eq!(diff_stat(w, &cp.id).unwrap(), (0, 0, 0));
    }

    #[test]
    fn restore_only_touches_agent_changed_files() {
        if !git_have() {
            return;
        }
        let ws = temp_ws("restore-scope");
        let w = ws.path();
        ws.git(&["init", "--quiet"]);
        std::fs::write(w.join("a.txt"), "v1\n").unwrap();
        std::fs::write(w.join("b.txt"), "b1\n").unwrap();
        ws.git(&["add", "a.txt", "b.txt"]);
        let cp1 = checkpoint(w, "基线", "sess-test").unwrap().expect("应有检查点");

        // agent 只改 a.txt；b.txt 在快照里没变
        std::fs::write(w.join("a.txt"), "v2\n").unwrap();
        checkpoint(w, "结束快照", "sess-test").unwrap().expect("应有提交");

        // 用户在快照之后自己改了 b.txt（影子仓库没见过这次改动）
        std::fs::write(w.join("b.txt"), "b2-user\n").unwrap();

        restore(w, &cp1.id).unwrap();
        assert_eq!(std::fs::read_to_string(w.join("a.txt")).unwrap(), "v1\n", "agent 改的文件应回退");
        assert_eq!(
            std::fs::read_to_string(w.join("b.txt")).unwrap(),
            "b2-user\n",
            "用户自己的改动不得被回滚误伤"
        );
    }

    #[test]
    fn restore_rejects_bad_id() {
        if !git_have() {
            return;
        }
        let ws = temp_ws("badid");
        let err = restore(ws.path(), "../../etc").unwrap_err();
        assert!(err.to_string().contains("非法"));
    }


}
