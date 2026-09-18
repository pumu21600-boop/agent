//! P10-A（施工单 014）：trace 时间线的只读解析。
//!
//! 数据源是 logs/agent.jsonl（固定 4 字段：ts / session_id / event_type / payload，
//! schema 见 docs/jsonl-schema.md——本模块**只读**，写端 schema 一律不碰）。
//!
//! 任务切分：以 `state kind:"task.start"` 为边界（P10 补记的事件）；
//! 旧日志没有该事件，兜底用「tool_call 且 step==0」近似切分。
//! `model_chunk` 为流式分片（单任务可上千条），按**连续段聚合**成一条
//! `{count, first_ts, last_ts, chars}`，时间线展示不逐分片渲染。

use std::{fs::File, io::{BufRead, BufReader}, path::Path};

use anyhow::Context;
use serde_json::{json, Value};

/// 单条时间线事件：payload 原样透传（不改写不删字段），附加展平的元信息。
#[derive(Debug, Clone)]
pub struct TraceEvent {
    pub ts: String,
    /// 事件类型（6 类之一：thought/tool_call/tool_result/model_chunk/state/error）
    pub etype: String,
    pub payload: Value,
}

/// 一个任务的完整事件流。
#[derive(Debug, Clone)]
pub struct TaskTrace {
    pub start_ts: String,
    pub end_ts: String,
    pub events: Vec<TraceEvent>,
}

impl TaskTrace {
    /// 时长毫秒（解析失败返回 0）。
    pub fn duration_ms(&self) -> i64 {
        ts_diff_ms(&self.start_ts, &self.end_ts)
    }
}

/// 读取最近 `limit` 个任务（倒序返回：最新在前）。
/// 坏行跳过不报错（崩不掉时间线面板）。
pub fn read_tasks(path: &Path, limit: usize) -> anyhow::Result<Vec<TaskTrace>> {
    let file = File::open(path).with_context(|| format!("打开 JSONL 失败: {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut tasks: Vec<TaskTrace> = Vec::new();
    let mut cur: Option<TaskTrace> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue, // IO 抖动行跳过
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // 损坏行跳过
        };
        let ts = record.get("ts").and_then(Value::as_str).unwrap_or("").to_string();
        let etype = record.get("event_type").and_then(Value::as_str).unwrap_or("").to_string();
        let payload = record.get("payload").cloned().unwrap_or(Value::Null);

        let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("");
        // 任务边界 1（正规）：state kind:"task.start"
        let is_task_start = etype == "state" && kind == "task.start";
        // 任务边界 2（旧日志兜底）：tool_call 且 step==0，且当前无任务
        // （task.start 已开任务时不再切，避免每个任务双切）
        let is_step0_tool = etype == "tool_call"
            && payload.get("step").and_then(Value::as_u64) == Some(0)
            && cur.is_none();
        // 任务收尾（旧日志兜底用）：loop.finish / cancel 后关闭任务，
        // 让下一个 step0 tool_call 正确开新任务
        let is_task_end = etype == "state" && (kind == "loop.finish" || kind == "cancel");

        if is_task_start {
            if let Some(t) = cur.take() {
                tasks.push(t);
            }
            if cur.is_none() {
                cur = Some(TaskTrace { start_ts: ts.clone(), end_ts: ts.clone(), events: Vec::new() });
            }
        } else if is_step0_tool {
            // cur 此时必为 None（条件里已保证），直接开新任务
            cur = Some(TaskTrace { start_ts: ts.clone(), end_ts: ts.clone(), events: Vec::new() });
        }

        if let Some(t) = cur.as_mut() {
            t.end_ts = ts.clone();
            t.events.push(TraceEvent { ts: ts.clone(), etype, payload });
        }
        if is_task_end
            && let Some(t) = cur.take() {
                tasks.push(t);
            }
        // 边界之前的孤儿事件（进程启动/config.changed 等）不归属任何任务，丢弃
    }
    if let Some(t) = cur.take() {
        tasks.push(t);
    }

    // 尾部截断：最近 limit 个，倒序（最新在前）
    let start = tasks.len().saturating_sub(limit);
    let mut picked: Vec<TaskTrace> = tasks.drain(start..).collect();
    picked.reverse();
    Ok(picked)
}

/// 任务内事件列表的展示形态转换：连续 model_chunk 聚合、其余原样。
/// 返回 JSON 数组（前端直接渲染）：
/// - 普通事件：`{ts, offset_ms, etype, payload}`
/// - model_chunk 聚合条：`{ts, offset_ms, etype:"model_chunk", payload:{aggregated:true, count, chars, first_ts, last_ts}}`
pub fn render_events(task: &TaskTrace) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut i = 0usize;
    while i < task.events.len() {
        let ev = &task.events[i];
        if ev.etype == "model_chunk" {
            let first_ts = ev.ts.clone();
            let mut count = 0usize;
            let mut last_ts = ev.ts.clone();
            while i < task.events.len() && task.events[i].etype == "model_chunk" {
                count += 1;
                last_ts = task.events[i].ts.clone();
                i += 1;
            }
            out.push(json!({
                "ts": first_ts, "offset_ms": ts_diff_ms(&task.start_ts, &first_ts),
                "etype": "model_chunk",
                "payload": { "aggregated": true, "count": count, "first_ts": first_ts, "last_ts": last_ts },
            }));
        } else {
            out.push(json!({
                "ts": ev.ts, "offset_ms": ts_diff_ms(&task.start_ts, &ev.ts),
                "etype": ev.etype, "payload": ev.payload,
            }));
            i += 1;
        }
    }
    out
}

/// RFC3339 时间差（毫秒）；解析失败返回 0。
pub fn ts_diff_ms(from: &str, to: &str) -> i64 {
    let f = chrono::DateTime::parse_from_rfc3339(from);
    let t = chrono::DateTime::parse_from_rfc3339(to);
    match (f, t) {
        (Ok(f), Ok(t)) => (t - f).num_milliseconds().max(0),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(tag: &str, lines: &[String]) -> (tempfile_like::Guard, std::path::PathBuf) {
        // tag 区分测试用例，避免同进程并行测试共用临时目录竞态
        let dir = std::env::temp_dir().join(format!("agent_trace_test_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        (tempfile_like::Guard(dir.clone()), path)
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

    fn rec(ts: &str, etype: &str, payload: serde_json::Value) -> String {
        serde_json::json!({"ts": ts, "session_id": "s", "event_type": etype, "payload": payload}).to_string()
    }

    #[test]
    fn splits_tasks_by_task_start_and_aggregates_chunks() {
        let lines = vec![
            rec("2026-09-04T00:00:00Z", "state", serde_json::json!({"kind":"startup"})),
            rec("2026-09-04T00:00:01Z", "state", serde_json::json!({"kind":"task.start"})),
            rec("2026-09-04T00:00:01.5Z", "model_chunk", serde_json::json!({"data":"a"})),
            rec("2026-09-04T00:00:01.6Z", "model_chunk", serde_json::json!({"data":"bb"})),
            rec("2026-09-04T00:00:02Z", "tool_call", serde_json::json!({"step":0,"name":"shell","arguments":"dir"})),
            rec("2026-09-04T00:00:03Z", "tool_result", serde_json::json!({"step":0,"ok":true})),
            rec("2026-09-04T00:00:04Z", "state", serde_json::json!({"kind":"loop.finish","step":1})),
            rec("2026-09-04T00:00:10Z", "tool_call", serde_json::json!({"step":0,"name":"git","arguments":"status"})),
        ];
        let (_g, path) = write_tmp("splits", &lines);
        let tasks = read_tasks(&path, 10).unwrap();
        assert_eq!(tasks.len(), 2, "task.start 与 step0 兜底各切一个任务");
        let t1 = &tasks[0]; // 最新在前：第二个任务（step0 git）
        assert_eq!(t1.events.len(), 1);
        let t2 = &tasks[1];
        assert_eq!(t2.duration_ms(), 3000);
        let rendered = render_events(t2);
        let agg = rendered.iter().find(|e| e["etype"] == "model_chunk").unwrap();
        assert_eq!(agg["payload"]["count"], 2, "连续 model_chunk 聚合成一条");
        assert_eq!(rendered.iter().filter(|e| e["etype"] == "tool_call").count(), 1);
    }

    #[test]
    fn bad_lines_are_skipped_and_limit_trims() {
        let lines = vec![
            "这不是JSON".into(),
            "".into(),
            rec("2026-09-04T00:00:00Z", "state", serde_json::json!({"kind":"task.start"})),
            rec("2026-09-04T00:00:02Z", "state", serde_json::json!({"kind":"task.start"})),
            rec("2026-09-04T00:00:04Z", "state", serde_json::json!({"kind":"task.start"})),
        ];
        let (_g, path) = write_tmp("badlines", &lines);
        let tasks = read_tasks(&path, 2).unwrap();
        assert_eq!(tasks.len(), 2, "坏行跳过；limit=2 只留最近 2 个任务");
        assert_eq!(tasks[0].start_ts, "2026-09-04T00:00:04Z", "最新在前");
    }
}
