//! eval 评测体系（施工单 015，P10-B）
//!
//! **HTTP 黑盒驱动**：对运行中的服务发真实对话（/api/chat SSE），断言文件系统与
//! SSE 事件流。主服务代码零改动——本模块只被 `src/bin/eval.rs`（CLI 壳）引用。
//!
//! 断言 5 种（留口扩展）：file_exists / file_not_exists / file_contains /
//! no_new_files / sse_event。
//!
//! 任务定义（evals/tasks/*.json）：
//! `name / prompt / plan_mode / approval_mode / timeout_secs / setup_files / assertions`

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::Context;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

// —— 任务定义 ——

#[derive(Debug, Clone, Deserialize)]
pub struct EvalTask {
    pub name: String,
    pub prompt: String,
    #[serde(default)]
    pub plan_mode: bool,
    #[serde(default = "default_approval")]
    pub approval_mode: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// 相对路径 → 文件内容（跑前写入临时工作区）
    #[serde(default)]
    pub setup_files: BTreeMap<String, String>,
    #[serde(default)]
    pub assertions: Vec<Assertion>,
}
fn default_approval() -> String {
    "auto".into()
}
fn default_timeout() -> u64 {
    180
}

#[derive(Debug, Clone, Deserialize)]
pub struct Assertion {
    /// file_exists | file_not_exists | file_contains | no_new_files | sse_event
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub needle: String,
    #[serde(default)]
    pub event: String,
}

impl Assertion {
    /// 断言可读描述（报告用）。
    pub fn describe(&self) -> String {
        match self.kind.as_str() {
            "file_exists" => format!("文件存在: {}", self.path),
            "file_not_exists" => format!("文件不存在: {}", self.path),
            "file_contains" => format!("{} 包含 {:?}", self.path, self.needle),
            "no_new_files" => "无新增文件".into(),
            "sse_event" => format!("SSE 出现事件: {}", self.event),
            other => format!("未知断言类型: {other}"),
        }
    }
}

/// 扫描目录下全部 *.json 任务（按文件名排序，稳定顺序）。
pub fn load_tasks(dir: &Path) -> anyhow::Result<Vec<EvalTask>> {
    let mut out = Vec::new();
    if !dir.exists() {
        anyhow::bail!("任务目录不存在: {}", dir.display());
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    entries.sort();
    for p in entries {
        let raw = fs::read_to_string(&p).with_context(|| format!("读 {}", p.display()))?;
        let task: EvalTask = serde_json::from_str(&raw).with_context(|| format!("解析 {}", p.display()))?;
        out.push(task);
    }
    Ok(out)
}

// —— 文件系统辅助 ——

/// 递归枚举 root 下全部文件（相对路径、正斜杠、排序）。
pub fn list_files(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    walk(root, root, &mut out);
    out
}
fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(root, &p, out);
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.insert(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

// —— 断言引擎（纯函数；文件断言基于工作区目录，事件断言基于 SSE 类型集合） ——

pub fn run_assertion(
    a: &Assertion,
    ws_dir: &Path,
    files_before: &BTreeSet<String>,
    sse_events: &BTreeSet<String>,
) -> (bool, String) {
    let desc = a.describe();
    let ok = match a.kind.as_str() {
        "file_exists" => ws_dir.join(&a.path).is_file(),
        "file_not_exists" => !ws_dir.join(&a.path).exists(),
        "file_contains" => fs::read_to_string(ws_dir.join(&a.path))
            .map(|c| c.contains(&a.needle))
            .unwrap_or(false),
        "no_new_files" => {
            let now = list_files(ws_dir);
            now.iter().all(|f| {
                files_before.contains(f) || is_runtime_file(f)
            })
        }
        "sse_event" => sse_events.contains(&a.event),
        _ => false, // 未知类型一律失败（防任务集手滑静默通过）
    };
    (ok, desc)
}

/// 服务自身管理的运行时文件（eval 断言应忽略，不算「任务改动」）：
/// .agent/ 记忆层、AGENTS.md 模板、.gitignore（服务会自动建/追加）、.git/、.attachments/。
fn is_runtime_file(rel: &str) -> bool {
    let lower = rel.to_lowercase();
    lower.starts_with(".agent/")
        || lower.starts_with(".git/")
        || lower.starts_with(".attachments/")
        || lower == "agents.md"
        || lower == ".gitignore"
}

// —— SSE 驱动器 ——

/// 一次任务的驱动结果。
#[derive(Debug, Default)]
pub struct DriveResult {
    /// 出现过的顶层 SSE 事件类型（session/content/tool_call/tool_result/notice/plan_proposal/done/…）
    pub sse_events: BTreeSet<String>,
    /// 服务端 error 事件内容（ None = 无）
    pub error: Option<String>,
    /// 是否收到 done（正常终止）
    pub done: bool,
    /// 是否被整体超时打断
    pub timed_out: bool,
}

/// 对服务发一轮 /api/chat 并收流至 done/error/超时。
pub async fn drive_chat(
    base: &str,
    task: &EvalTask,
    session_id: &str,
) -> anyhow::Result<DriveResult> {
    let url = format!("{base}/api/chat");
    let body = json!({
        "messages": [{ "role": "user", "content": task.prompt }],
        "message": task.prompt,
        "plan_mode": task.plan_mode,
        "approval_mode": task.approval_mode,
        "session_id": session_id,
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(task.timeout_secs + 10))
        .send()
        .await
        .context("请求 /api/chat 失败（服务未启动？）")?;
    if !resp.status().is_success() {
        anyhow::bail!("/api/chat HTTP {}", resp.status());
    }

    let mut out = DriveResult::default();
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(task.timeout_secs);

    loop {
        let chunk = tokio::select! {
            c = stream.next() => c,
            _ = tokio::time::sleep_until(deadline) => {
                out.timed_out = true;
                break;
            }
        };
        let bytes = match chunk {
            Some(Ok(b)) => b,
            Some(Err(_)) | None => break, // 连接结束/中断
        };
        buf.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(pos) = buf.find("\n\n") {
            let raw = buf[..pos].to_string();
            buf.drain(..pos + 2);
            for line in raw.split('\n') {
                let Some(data) = line.strip_prefix("data: ") else { continue };
                let Ok(ev) = serde_json::from_str::<Value>(data) else { continue };
                if let Some(obj) = ev.as_object() {
                    if let Some(key) = obj.keys().next() {
                        out.sse_events.insert(key.clone());
                        if key == "error" {
                            out.error = obj
                                .get("error")
                                .and_then(Value::as_str)
                                .map(|s| s.to_string());
                        }
                        if key == "done" {
                            out.done = true;
                        }
                    }
                }
            }
            if out.done || out.error.is_some() {
                break;
            }
        }
        if out.done || out.error.is_some() {
            break;
        }
    }
    Ok(out)
}

/// 任务报告行。
#[derive(Debug)]
pub struct TaskReport {
    pub name: String,
    pub passed: usize,
    pub failed: usize,
    /// (断言描述, 是否通过)
    pub details: Vec<(String, bool)>,
    pub duration_ms: u64,
    /// 驱动层错误（服务不通/HTTP 错/超时）——出现时断言不计分
    pub drive_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws_with(tag: &str, files: &[(&str, &str)]) -> (tempfile_like::Guard, PathBuf) {
        // tag 区分测试用例，避免同进程并行测试共用临时目录竞态
        let dir = std::env::temp_dir().join(format!("agent_eval_test_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        for (p, c) in files {
            let full = dir.join(p);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, c).unwrap();
        }
        (tempfile_like::Guard(dir.clone()), dir)
    }

    /// 极简临时目录清理（不用外部 crate）。
    mod tempfile_like {
        pub struct Guard(pub std::path::PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn a(kind: &str, path: &str, needle: &str, event: &str) -> Assertion {
        Assertion {
            kind: kind.into(),
            path: path.into(),
            needle: needle.into(),
            event: event.into(),
        }
    }

    #[test]
    fn file_assertions() {
        let (_g, ws) = ws_with("file_assertions", &[("hello.txt", "hello eval\n"), ("sub/b.json", "{\"ok\":true}")]);
        let before = list_files(&ws);
        let mut sse = BTreeSet::new();
        sse.insert("done".to_string());

        assert!(run_assertion(&a("file_exists", "hello.txt", "", ""), &ws, &before, &sse).0);
        assert!(run_assertion(&a("file_not_exists", "nope.txt", "", ""), &ws, &before, &sse).0);
        assert!(run_assertion(&a("file_contains", "hello.txt", "hello eval", ""), &ws, &before, &sse).0);
        assert!(!run_assertion(&a("file_contains", "hello.txt", "missing", ""), &ws, &before, &sse).0);
        assert!(run_assertion(&a("file_contains", "sub/b.json", "ok", ""), &ws, &before, &sse).0);
        assert!(run_assertion(&a("no_new_files", "", "", ""), &ws, &before, &sse).0);
        assert!(run_assertion(&a("sse_event", "", "", "done"), &ws, &before, &sse).0);
        assert!(!run_assertion(&a("sse_event", "", "", "plan_proposal"), &ws, &before, &sse).0);
        assert!(!run_assertion(&a("unknown_kind", "", "", ""), &ws, &before, &sse).0, "未知断言类型必须失败");
    }

    #[test]
    fn no_new_files_detects_newcomer_but_skips_runtime_files() {
        let (_g, ws) = ws_with("newcomer", &[("base.txt", "x")]);
        let before = list_files(&ws);
        std::fs::write(ws.join("new.txt"), "y").unwrap();
        assert!(!run_assertion(&a("no_new_files", "", "", ""), &ws, &before, &BTreeSet::new()).0);
        std::fs::remove_file(ws.join("new.txt")).unwrap(); // 移除非法新增后再验证运行时文件豁免
        // 服务运行时文件不算「任务改动」（记忆层/AGENTS.md 模板/.gitignore 等）
        std::fs::create_dir_all(ws.join(".agent")).unwrap();
        std::fs::write(ws.join(".agent").join("index.json"), "{}").unwrap();
        std::fs::write(ws.join("AGENTS.md"), "模板").unwrap();
        std::fs::write(ws.join(".gitignore"), ".attachments/\n").unwrap();
        assert!(run_assertion(&a("no_new_files", "", "", ""), &ws, &before, &BTreeSet::new()).0);
    }

    #[test]
    fn load_tasks_sorted_and_typed() {
        let (_g, dir) = ws_with("load", &[]);
        std::fs::write(
            dir.join("b_second.json"),
            serde_json::json!({
                "name": "task-b", "prompt": "p",
                "plan_mode": true, "timeout_secs": 60,
                "setup_files": {"in.txt": "seed"},
                "assertions": [{"type": "file_contains", "path": "out.txt", "needle": "x"}]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join("a_first.json"),
            serde_json::json!({"name": "task-a", "prompt": "p", "assertions": [{"type": "sse_event", "event": "done"}]})
                .to_string(),
        )
        .unwrap();
        let tasks = load_tasks(&dir).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, "task-a", "按文件名排序");
        assert!(tasks[1].plan_mode, "plan_mode 解析");
        assert_eq!(tasks[1].setup_files.get("in.txt").unwrap(), "seed");
        assert_eq!(tasks[1].assertions[0].kind, "file_contains");
        // 默认值（task-a 未写）：approval_mode=auto、timeout=180；task-b 显式 60 覆盖
        assert_eq!(tasks[0].approval_mode, "auto");
        assert_eq!(tasks[0].timeout_secs, 180);
        assert_eq!(tasks[1].timeout_secs, 60);
    }
}
