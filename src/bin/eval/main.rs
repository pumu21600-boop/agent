//! eval 评测 CLI —— 施工单 015（P10-B）
//!
//! 对运行中的服务（默认 http://127.0.0.1:5173）做 HTTP 黑盒驱动评测：
//! 每个任务在独立临时工作区跑一轮真实对话，按任务定义断言文件系统与 SSE 事件。
//!
//! 用法（项目根目录）：
//!   cargo run --bin eval                      跑全部任务
//!   cargo run --bin eval -- hello-file        只跑指定任务
//!   cargo run --bin eval -- --keep            保留临时工作区（诊断用）
//!   cargo run --bin eval -- --base URL        指定服务地址
//!
//! 退出码：全部通过 0，任何失败 1（CI 友好）。跑完自动恢复原工作区。

mod eval_core;

use eval_core::{load_tasks, run_assertion, snapshot, drive_chat, drive_turn, TaskReport};
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let mut base = "http://127.0.0.1:5173".to_string();
    let mut keep = false;
    let mut filter: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--base" => base = args.next().unwrap_or_else(|| base.clone()),
            "--keep" => keep = true,
            other => filter.push(other.to_string()),
        }
    }

    let tasks_dir = std::env::current_dir().unwrap_or_default().join("evals").join("tasks");
    let all = match load_tasks(&tasks_dir) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[eval] {e:#}");
            std::process::exit(1);
        }
    };
    let tasks: Vec<_> = all
        .into_iter()
        .filter(|t| filter.is_empty() || filter.contains(&t.name))
        .collect();
    if tasks.is_empty() {
        eprintln!("[eval] 没有匹配的任务（目录 {}）", tasks_dir.display());
        std::process::exit(1);
    }

    // 原工作区记录：跑完恢复
    let client = reqwest::Client::new();
    let original_ws: Option<String> = match client
        .get(format!("{base}/api/workspace"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(r) => r
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("current").and_then(|c| c.as_str()).map(|s| s.to_string())),
        Err(_) => None,
    };
    println!("[eval] 服务 {base}，当前工作区 {:?}", original_ws.as_deref().unwrap_or("(未知)"));

    let mut reports: Vec<TaskReport> = Vec::new();
    for task in &tasks {
        let t0 = std::time::Instant::now();
        println!("[eval] ▶ 任务 {}（{} 条断言）", task.name, task.assertions.len());

        // 1) 独立临时工作区 + setup_files
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let ws: PathBuf = std::env::temp_dir().join(format!("agent_eval_ws_{}_{}", task.name, stamp));
        if let Err(e) = std::fs::create_dir_all(&ws) {
            reports.push(TaskReport {
                name: task.name.clone(), passed: 0, failed: task.assertions.len(),
                duration_ms: t0.elapsed().as_millis() as u64,
                drive_error: Some(format!("建临时工作区失败: {e}")),
            });
            continue;
        }
        for (rel, content) in &task.setup_files {
            let full = ws.join(rel);
            if let Some(parent) = full.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(full, content) {
                eprintln!("[eval] 写 setup 文件 {rel} 失败: {e}");
            }
        }
        let files_before = snapshot(&ws);

        // 2) 切换服务工作区
        let switch = client
            .post(format!("{base}/api/workspace"))
            .json(&serde_json::json!({ "path": ws.to_string_lossy() }))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;
        if let Err(e) = switch {
            reports.push(TaskReport {
                name: task.name.clone(), passed: 0, failed: task.assertions.len(),
                duration_ms: t0.elapsed().as_millis() as u64,
                drive_error: Some(format!("切换工作区失败: {e}")),
            });
            continue;
        }

        // 3) 真实对话 + 收流（多轮任务：第二轮模拟「批准后执行」）
        let session_id = format!("eval-{}-{stamp}", task.name);
        let mut drive = drive_chat(&base, task, &session_id).await;
        if let Some(fu) = task.followup_prompt.clone() {
            println!("[eval]   ↳ 第二轮（followup，plan_mode={}）", task.followup_plan_mode);
            // 第二轮要带完整历史（含第一轮的 assistant 文本）——服务端以 messages 为消息源，
            // 只发一句「计划已批准」会丢计划上下文（真机踩过：模型答「任务信息不足」）
            let mut history: Vec<serde_json::Value> = vec![serde_json::json!({ "role": "user", "content": task.prompt })];
            if let Ok(first) = &drive
                && !first.text.trim().is_empty() {
                    history.push(serde_json::json!({ "role": "assistant", "content": first.text }));
                }
            history.push(serde_json::json!({ "role": "user", "content": fu }));
            let first_events = drive.as_ref().map(|d| d.sse_events.clone()).unwrap_or_default();
            let first_timed_out = drive.as_ref().map(|d| d.timed_out).unwrap_or(false);
            let second = drive_turn(
                &base,
                &history,
                task.followup_plan_mode,
                &task.approval_mode,
                task.timeout_secs,
                &session_id,
            )
            .await;
            drive = match (drive, second) {
                (Ok(mut a), Ok(b)) => {
                    a.sse_events.extend(first_events);
                    a.sse_events.extend(b.sse_events);
                    a.done = b.done;
                    a.timed_out = first_timed_out || b.timed_out;
                    a.error = b.error;
                    Ok(a)
                }
                (Ok(a), Err(e)) => Err(anyhow::anyhow!("第二轮失败: {e:#}（第一轮 {a:?}）")),
                (Err(e), _) => Err(e),
            };
        }
        let (sse, d_err) = match drive {
            Ok(d) => {
                if let Some(err) = &d.error {
                    eprintln!("[eval]   服务端报错: {err}");
                }
                if d.timed_out {
                    eprintln!("[eval]   整体超时（{}s）", task.timeout_secs);
                }
                (d.sse_events, d.error.clone().or(if d.timed_out { Some("整体超时".into()) } else { None }))
            }
            Err(e) => (Default::default(), Some(format!("{e:#}"))),
        };
        let drive_error = d_err;

        // 4) 断言
        let mut details: Vec<(String, bool)> = Vec::new();
        for a in &task.assertions {
            let (ok, desc) = run_assertion(a, &ws, &files_before, &sse);
            details.push((desc, ok));
        }
        let passed = details.iter().filter(|(_, ok)| *ok).count();
        let failed = details.len() - passed;
        for (desc, ok) in &details {
            println!("[eval]   {} {}", if *ok { "✓" } else { "✕" }, desc);
        }
        reports.push(TaskReport {
            name: task.name.clone(),
            passed,
            failed,
            duration_ms: t0.elapsed().as_millis() as u64,
            drive_error,
        });

        // 5) 清理临时工作区（--keep 保留诊断）
        if keep {
            println!("[eval]   保留工作区: {}", ws.display());
        } else {
            let _ = std::fs::remove_dir_all(&ws);
        }
    }

    // 6) 恢复原工作区
    if let Some(orig) = &original_ws {
        let _ = client
            .post(format!("{base}/api/workspace"))
            .json(&serde_json::json!({ "path": orig }))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;
        println!("[eval] 已恢复原工作区 {orig}");
    }

    // 7) 报告
    println!("\n========== eval 报告 ==========");
    let mut all_ok = true;
    for r in &reports {
        let verdict = if r.drive_error.is_some() {
            all_ok = false;
            format!("驱动失败（{}）", r.drive_error.as_deref().unwrap_or(""))
        } else if r.failed == 0 {
            format!("通过 {}/{}", r.passed, r.passed + r.failed)
        } else {
            all_ok = false;
            format!("通过 {}/{}（有失败）", r.passed, r.passed + r.failed)
        };
        println!("{:<20} {:>8}  {}ms", r.name, verdict, r.duration_ms);
    }
    let total_pass: usize = reports.iter().map(|r| r.passed).sum();
    let total_fail: usize = reports.iter().map(|r| r.failed).sum();
    let drive_fail = reports.iter().filter(|r| r.drive_error.is_some()).count();
    println!("--------------------------------");
    println!(
        "合计: 通过 {total_pass} / 失败 {total_fail} / 驱动失败 {drive_fail} —— {}",
        if all_ok { "ALL PASS ✅" } else { "FAILED ❌" }
    );
    std::process::exit(if all_ok { 0 } else { 1 });
}
