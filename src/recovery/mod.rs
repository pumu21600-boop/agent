//! 错误恢复 —— 总架构 §14 / 施工单 007（P6）
//!
//! 「假设一定会崩，把能恢复做成默认」：
//! - checkpoint：文件改动前把真仓库已跟踪文件提交进影子仓库 `<workspace>/.agent/shadow`
//!   （独立 git dir，work-tree 指向 workspace），只跟踪真仓库 `git ls-files` 的文件
//!   加排除规则，防 IO 爆炸；失败只记日志，绝不阻断工具执行。
//! - restore：影子仓库 `reset --hard` 回指定检查点（含删除其后新建的已跟踪文件）；
//!   shell 副作用不可回滚，靠沙箱兜底。
//! - HANDOFF：交接文档落 `<workspace>/HANDOFF.md`，头部强制项目名/remote/commit/时间戳。
//!
//! 所有 git 调用都是阻塞的，调用方（核心循环/server）包 spawn_blocking。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, anyhow, bail};
use chrono::{Local, SecondsFormat, Utc};
use serde_json::Value;

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

/// 排除目录：影子仓库永不跟踪（真仓库 ls-files 之外的第二道防线，防 IO 爆炸）。
const EXCLUDED_DIRS: &[&str] = &["node_modules", "target", "dist", "logs", ".agent", ".git"];
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

/// 真仓库已跟踪文件（workspace 相对路径），套排除规则与体积上限。
fn tracked_files(bin: &Path, ws: &Path) -> anyhow::Result<Vec<PathBuf>> {
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

/// 拍检查点：add + 有变更才 commit。返回 None 表示没有需要落的变化。
/// 任何失败都由调用方决定降级（核心循环只记日志不阻断）。
/// `session_id` 写进提交正文的 trailer，供改动条/撤销按会话对齐。
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

/// 工作树相对基线 commit 的改动统计：(文件数, 增行, 删行)。
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

/// 工作树相对基线 commit 的改动文件清单（`diff --numstat` 第三列）。
pub fn changed_files(ws: &Path, baseline: &str) -> anyhow::Result<Vec<String>> {
    if !baseline.chars().all(|c| c.is_ascii_hexdigit()) || !(7..=40).contains(&baseline.len()) {
        bail!("非法基线 id: {baseline}");
    }
    let bin = git_bin()?;
    let gitdir = shadow_gitdir(ws);
    if !gitdir.exists() {
        bail!("影子仓库不存在");
    }
    let out = git_ok(&bin, Some(&gitdir), ws, &["diff", "--numstat", baseline])?;
    let mut files = Vec::new();
    for line in out.lines() {
        let mut it = line.splitn(3, '\t');
        let (_a, _d) = (it.next(), it.next());
        if let Some(p) = it.next() {
            files.push(p.to_string());
        }
    }
    Ok(files)
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

/// 从消息数组里取纯文本（string / null / 分段数组都兜住）。
fn content_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// HANDOFF 输入：触发说明 + 会话消息（无会话时 None）。
pub struct HandoffInput {
    /// 如「手动」「自动：上下文占用 83%（阈值 80%）」
    pub trigger: String,
    pub session_id: String,
    pub messages: Option<Vec<Value>>,
}

/// 生成交接文档 `<workspace>/HANDOFF.md`。
/// 头部四要素（项目名/git remote/commit/时间戳）照总架构 §9 强制写入；正文 v1 从简，不调模型。
pub fn write_handoff(ws: &Path, input: &HandoffInput) -> anyhow::Result<PathBuf> {
    let bin = crate::tools::git_exe().ok();
    let (remote, commit) = match &bin {
        Some(b) => (
            git_ok(b, None, ws, &["remote", "get-url", "origin"])
                .map(|s| if s.is_empty() { "（未配置 origin）".into() } else { s })
                .unwrap_or_else(|_| "（未配置 origin）".to_string()),
            git_ok(b, None, ws, &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|_| "（非 git 仓库）".to_string()),
        ),
        None => ("（未配置 origin）".into(), "（git 不可用）".into()),
    };
    let project = ws
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| ws.to_string_lossy().to_string());

    let mut md = String::new();
    md.push_str(&format!("# HANDOFF — {project}\n\n"));
    md.push_str("> 本文件由 agent 自动生成（P6 恢复与续命），供下一会话接续。崩溃/换会话后先读本文件。\n\n");
    md.push_str("| 项 | 值 |\n|---|---|\n");
    md.push_str(&format!("| 项目名 | {project} |\n"));
    md.push_str(&format!("| git remote | {remote} |\n"));
    md.push_str(&format!("| HEAD commit | {commit} |\n"));
    md.push_str(&format!(
        "| 时间戳 | {}（UTC {}） |\n",
        Local::now().format("%Y-%m-%d %H:%M:%S %z"),
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    ));
    md.push_str(&format!("| session | {} |\n", input.session_id));
    md.push_str(&format!("| 触发 | {} |\n\n", input.trigger));

    if let Some(msgs) = &input.messages {
        // 最近用户请求（≤5，每条压成一行）
        let users: Vec<String> = msgs
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .filter_map(|m| {
                let t = content_text(m.get("content"));
                let t = one_line(&t, 200);
                (!t.is_empty()).then_some(t)
            })
            .collect();
        md.push_str("## 最近用户请求\n\n");
        if users.is_empty() {
            md.push_str("（无）\n\n");
        } else {
            for (i, u) in users.iter().rev().take(5).rev().enumerate() {
                md.push_str(&format!("{}. {u}\n", i + 1));
            }
            md.push('\n');
        }

        // 最后一次助手回答（前 500 字）
        let last_asst = msgs
            .iter()
            .rev()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
            .filter_map(|m| {
                let t = content_text(m.get("content"));
                (!t.trim().is_empty()).then_some(t)
            })
            .next();
        md.push_str("## 最后一次助手回答（前 500 字）\n\n");
        match last_asst {
            Some(t) => md.push_str(&format!("{}\n\n", t.chars().take(500).collect::<String>())),
            None => md.push_str("（无）\n\n"),
        }

        // 本会话工具调用计数
        let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
        for m in msgs {
            if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    if let Some(n) = c.pointer("/function/name").and_then(Value::as_str) {
                        *counts.entry(n.to_string()).or_default() += 1;
                    }
                }
            }
        }
        md.push_str("## 本会话工具调用\n\n");
        if counts.is_empty() {
            md.push_str("（无）\n\n");
        } else {
            let line = counts
                .into_iter()
                .map(|(k, v)| format!("{k} ×{v}"))
                .collect::<Vec<_>>()
                .join("、");
            md.push_str(&format!("{line}\n\n"));
        }
    }

    md.push_str("## 边界说明\n\n");
    md.push_str("- 文件改动可用检查点回滚（GET /api/checkpoints 列表、POST /api/checkpoints/restore 恢复）；shell 副作用不可回滚。\n");
    md.push_str("- 会话消息在 `<日志目录>/session.json`，新会话用 DELETE /api/session。\n");

    let path = ws.join("HANDOFF.md");
    std::fs::write(&path, md).with_context(|| format!("写 HANDOFF 失败: {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    #[test]
    fn handoff_header_fields() {
        if !git_have() {
            return;
        }
        let ws = temp_ws("handoff");
        let w = ws.path();
        ws.git(&["init", "--quiet"]);
        ws.git(&["config", "user.name", "test"]);
        ws.git(&["config", "user.email", "test@local"]);
        std::fs::write(w.join("x.txt"), "x\n").unwrap();
        ws.git(&["add", "x.txt"]);
        ws.git(&["commit", "--quiet", "-m", "base"]);

        let path = write_handoff(
            w,
            &HandoffInput {
                trigger: "自动：上下文占用 83%（阈值 80%）".into(),
                session_id: "sid-test".into(),
                messages: Some(vec![
                    json!({"role": "user", "content": "帮我修编译错误"}),
                    json!({"role": "assistant", "content": "正在看", "tool_calls": [
                        {"function": {"name": "read_file"}},
                        {"function": {"name": "read_file"}},
                        {"function": {"name": "shell"}}
                    ]}),
                    json!({"role": "tool", "content": "…"}),
                    json!({"role": "assistant", "content": "修好了，原因是 xxx"}),
                ]),
            },
        )
        .unwrap();
        assert_eq!(path, w.join("HANDOFF.md"));
        let md = std::fs::read_to_string(&path).unwrap();
        assert!(md.contains("# HANDOFF — "), "项目名: {md}");
        assert!(md.contains("agent-p6-handoff"), "用目录名当项目名: {md}");
        assert!(md.contains("git remote"), "remote 字段: {md}");
        assert!(md.contains("（未配置 origin）"), "无 remote 时标注: {md}");
        assert!(md.contains("HEAD commit"), "commit 字段: {md}");
        assert!(!md.contains("（非 git 仓库）"), "真仓库应有 commit 哈希: {md}");
        assert!(md.contains("时间戳"), "时间戳字段: {md}");
        assert!(md.contains("sid-test"), "session id: {md}");
        assert!(md.contains("83%"), "触发原因: {md}");
        assert!(md.contains("帮我修编译错误"), "最近请求: {md}");
        assert!(md.contains("修好了"), "最后回答: {md}");
        assert!(md.contains("read_file ×2"), "工具计数: {md}");
        assert!(md.contains("shell ×1"), "工具计数: {md}");
    }

    #[test]
    fn content_text_variants() {
        assert_eq!(content_text(Some(&json!("abc"))), "abc");
        assert_eq!(content_text(Some(&Value::Null)), "");
        assert_eq!(content_text(Some(&json!([{"text": "a"}, {"text": "b"}]))), "ab");
        assert_eq!(content_text(None), "");
    }
}
