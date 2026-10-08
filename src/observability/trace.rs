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
    /// 任务归属的**聊天会话** id：优先 task.start 的 `session`（新日志），
    /// 回落任务内第一条 `session.saved` 的 `session`（旧日志）。空 = 推不出归属。
    /// 注意日志顶层 `session_id` 是**进程级**日志会话（进程重启即换），与工作区无关——
    /// 归属必须走这里，别再拿顶层字段（真机实锤过：按顶层过滤 = 时间线全空）。
    pub session: String,
    /// 起点事件缺失（task.start 被日志轮转吃掉），本任务的开头是**近似**的：
    /// start_ts 是兜底边界那条事件的时刻，不是任务真正的开始。时间线据此提示用户。
    pub lost_start: bool,
    pub events: Vec<TraceEvent>,
}

impl TaskTrace {
    /// 时长毫秒（解析失败返回 0）。
    pub fn duration_ms(&self) -> i64 {
        ts_diff_ms(&self.start_ts, &self.end_ts)
    }
}

/// 读取最近 `limit` 个任务（倒序返回：最新在前）。坏行跳过不报错（崩不掉时间线面板）。
/// `allowed` 为 Some 时只保留**归属这些聊天会话**的任务（「时间线只展示所处工作区」用：
/// 工作区→会话映射由调用方从会话文件算好传进来）；`Some(空集)` = 该工作区还没有任何
/// 会话 → 空时间线，不是报错。过滤发生在**截断之前**，所以拿到的是最近 limit 个
/// 「命中」任务，而不是「最近 limit 个再看剩几个」。
/// 单文件便捷入口（现仅测试用；服务端走 multi 版本读全量轮转）。
#[cfg(test)]
pub fn read_tasks_scoped(
    path: &Path,
    limit: usize,
    allowed: Option<&std::collections::HashSet<String>>,
) -> anyhow::Result<Vec<TaskTrace>> {
    read_tasks_scoped_multi(std::iter::once(path), limit, allowed)
}

/// 多文件版本：日志会按体积轮转成 `<file>.1`（最新）… `<file>.N`（最旧），
/// **task.start 落在被轮转走的归档里、工具事件落在当前文件里**是真机实锤的常态
/// （20MB 上限，一轮长对话就能跨文件）。单文件读法会把跨文件任务撕成
/// 「归档里一个完整任务 + 当前文件里一群没头没尾的孤儿事件（全部丢弃）」，
/// 时间线看起来就是「怎么都刷不出来」。调用方按**时间正序**（最旧文件在前）
/// 传入，归属状态跨文件延续。
pub fn read_tasks_scoped_multi<'a, I>(
    paths: I,
    limit: usize,
    allowed: Option<&std::collections::HashSet<String>>,
) -> anyhow::Result<Vec<TaskTrace>>
where
    I: IntoIterator<Item = &'a Path>,
{
    let mut tasks: Vec<TaskTrace> = Vec::new();
    let mut cur: Option<TaskTrace> = None;
    let mut any = false;

    for path in paths {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => continue, // 归档缺失（keep=0/被清）不算错，跳过
        };
        any = true;
        let reader = BufReader::new(file);
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
            // 让下一个 step0 tool_call 正确开新任务。
            // 2026-10-08 修：多会话并发时（单进程共用一份 agent.jsonl）B 的
            // loop.finish/cancel 会把 A 尚未收尾的开放任务当场闭合，A 后续的 tool
            // 事件只能按「孤儿活动」开降级碎片、或被 allowed 过滤整条丢掉。收尾事件
            // 带 session 时按归属核对（两边都有 session 且不一致 → 不是我的收尾）；
            // 缺字段（旧日志）保持宽容，照旧闭合。
            let end_session = payload.get("session").and_then(Value::as_str).unwrap_or("");
            let end_mine = end_session.is_empty()
                || cur.as_ref().is_none_or(|t| t.session.is_empty() || t.session == end_session);
            let is_task_end = etype == "state" && (kind == "loop.finish" || kind == "cancel") && end_mine;
            // 任务归属（时间线按工作区过滤用）：task.start 新带的 session 优先，
            // 旧日志回落任务内第一条 session.saved 的 session（实测 46 个任务里 45 个可归属）
            let ev_session = payload
                .get("session")
                .and_then(Value::as_str)
                .filter(|_| is_task_start || kind == "session.saved")
                .unwrap_or("")
                .to_string();
            // 降级任务的收尾：真机日志里 loop.finish 压根不存在（实测 0 条），只靠
            // session.saved。它每轮至少两条（任务起点一次、回复落地一次），所以
            // 「本任务已经干过活（有 tool_result）之后来的那条」才是这一轮的结束——
            // 起点那条在 tool_call 之前，不会误闭合。同样按 session 归属核对：
            // 多会话并发时 B 的 session.saved 不该闭合 A 的降级任务。
            let is_partial_end = kind == "session.saved"
                && cur.as_ref().is_some_and(|t| t.lost_start && t.events.iter().any(|e| e.etype == "tool_result"))
                && (ev_session.is_empty()
                    || cur.as_ref().is_none_or(|t| t.session.is_empty() || t.session == ev_session));
            // 边界 3（轮转兜底）：当前没有任务，却出现了**只有任务里才会有**的活动事件。
            // task.start 被日志轮转吃掉（keep_archives=3、单归档 ~100MB）是真机常态：
            // 长任务的起点落在更早的归档里被删，后续 tool_call 还留在仅存的几份中。
            // 旧代码把这些事件当孤儿全丢 → 时间线整块空白。这里开一个「起点缺失」的
            // 降级任务，至少让用户看见这个任务干过什么，而不是什么都没有。
            let is_orphan_activity = cur.is_none()
                && !is_task_start
                && matches!(etype.as_str(), "tool_call" | "tool_result")
                && (etype != "tool_call" || !is_step0_tool);

            if is_task_start {
                if let Some(t) = cur.take() {
                    tasks.push(t);
                }
                if cur.is_none() {
                    cur = Some(TaskTrace { start_ts: ts.clone(), end_ts: ts.clone(), session: ev_session.clone(), events: Vec::new(), lost_start: false });
                }
            } else if is_step0_tool || is_orphan_activity {
                // cur 此时必为 None（条件里已保证），直接开新任务
                cur = Some(TaskTrace {
                    start_ts: ts.clone(),
                    end_ts: ts.clone(),
                    session: String::new(),
                    events: Vec::new(),
                    // step0 是旧日志的近似切分，起点时间同样是近似值
                    lost_start: is_orphan_activity,
                });
            }
            if let Some(t) = cur.as_mut() {
                t.end_ts = ts.clone();
                if t.session.is_empty() && !ev_session.is_empty() {
                    t.session = ev_session.clone();
                }
                t.events.push(TraceEvent { ts: ts.clone(), etype, payload });
            }
            if (is_task_end || is_partial_end)
                && let Some(t) = cur.take() {
                    tasks.push(t);
                }
            // 边界之前的孤儿事件（首个文件开头处：进程启动/config.changed 等）不归属任何任务，丢弃
        }
    }
    if !any {
        anyhow::bail!("打开 JSONL 失败: 无可用日志文件（当前与归档都不存在）");
    }
    if let Some(t) = cur.take() {
        tasks.push(t);
    }

    // 工作区过滤：按任务归属的聊天会话筛（先筛后截断——拿到的是最近 limit 个命中任务）；
    // 推不出归属的任务（日志里没有会话线索）在过滤视图下一律不出现
    if let Some(set) = allowed {
        tasks.retain(|t| !t.session.is_empty() && set.contains(&t.session));
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

    fn rec_s(ts: &str, etype: &str, payload: serde_json::Value, sid: &str) -> String {
        serde_json::json!({"ts": ts, "session_id": sid, "event_type": etype, "payload": payload}).to_string()
    }

    #[test]
    fn scoped_read_keeps_only_allowed_sessions() {
        // 任务归属走 payload.session（task.start 新字段 / 任务内 session.saved 旧日志）；
        // 顶层 session_id（进程级日志会话）不参与过滤——按它过滤 = 时间线全空（真机实锤）
        let lines = vec![
            rec_s("2026-09-04T00:00:00Z", "state", serde_json::json!({"kind":"task.start","session":"sa"}), "proc-1"),
            rec_s("2026-09-04T00:00:01Z", "thought", serde_json::json!({"text":"a"}), "proc-1"),
            rec_s("2026-09-04T00:00:02Z", "state", serde_json::json!({"kind":"loop.finish"}), "proc-1"),
            // 旧日志形态：task.start 不带 session，靠任务内 session.saved 归属
            rec_s("2026-09-04T00:00:03Z", "state", serde_json::json!({"kind":"task.start"}), "proc-1"),
            rec_s("2026-09-04T00:00:03.5Z", "state", serde_json::json!({"kind":"session.saved","session":"sb"}), "proc-1"),
            rec_s("2026-09-04T00:00:04Z", "thought", serde_json::json!({"text":"b"}), "proc-1"),
            rec_s("2026-09-04T00:00:05Z", "state", serde_json::json!({"kind":"loop.finish"}), "proc-1"),
            // 推不出归属的任务：过滤视图下一律不出现
            rec_s("2026-09-04T00:00:06Z", "state", serde_json::json!({"kind":"task.start"}), "proc-1"),
            rec_s("2026-09-04T00:00:07Z", "state", serde_json::json!({"kind":"loop.finish"}), "proc-1"),
        ];
        let (_g, path) = write_tmp("scoped", &lines);

        let only_a = std::collections::HashSet::from(["sa".to_string()]);
        let t = read_tasks_scoped(&path, 10, Some(&only_a)).unwrap();
        assert_eq!(t.len(), 1, "只留 sa 的任务");
        assert_eq!(t[0].session, "sa");
        assert_eq!(read_tasks_scoped(&path, 1, Some(&only_a)).unwrap().len(), 1, "先筛后截断：limit=1 也能拿到命中任务");

        let only_b = std::collections::HashSet::from(["sb".to_string()]);
        let t = read_tasks_scoped(&path, 10, Some(&only_b)).unwrap();
        assert_eq!(t.len(), 1, "旧日志任务靠 session.saved 归属");
        assert!(t[0].events.iter().any(|e| e.payload.get("text").and_then(|x| x.as_str()) == Some("b")));

        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(read_tasks_scoped(&path, 10, Some(&empty)).unwrap().is_empty(), "空集 = 空时间线（工作区还没有会话）");
        assert_eq!(read_tasks_scoped(&path, 10, None).unwrap().len(), 3, "None = 不过滤");
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
        let tasks = read_tasks_scoped(&path, 10, None).unwrap();
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
        let tasks = read_tasks_scoped(&path, 2, None).unwrap();
        assert_eq!(tasks.len(), 2, "坏行跳过；limit=2 只留最近 2 个任务");
        assert_eq!(tasks[0].start_ts, "2026-09-04T00:00:04Z", "最新在前");
    }

    #[test]
    fn task_spanning_rotated_files_stays_whole() {
        // 真机实锤（2026-10-02）：task.start 写进归档 <file>.1，工具事件写在当前文件——
        // 只读当前文件时这些事件成了孤儿被丢弃，时间线「怎么都刷不出来」。
        let (_g, old) = write_tmp(
            "span-old",
            &[rec_s(
                "2026-10-02T04:55:28Z",
                "state",
                serde_json::json!({"kind":"task.start","session":"sa"}),
                "proc-1",
            )],
        );
        let (_g2, cur) = write_tmp(
            "span-cur",
            &[
                rec_s("2026-10-02T04:55:40Z", "tool_call", serde_json::json!({"step":1,"name":"shell"}), "proc-1"),
                rec_s("2026-10-02T04:55:50Z", "state", serde_json::json!({"kind":"loop.finish"}), "proc-1"),
            ],
        );
        // 顺序 = 时间正序（最旧在前）：归档在先、当前文件在后
        let only_a = std::collections::HashSet::from(["sa".to_string()]);
        let tasks = read_tasks_scoped_multi([&old, &cur].iter().map(|p| p.as_path()), 10, Some(&only_a)).unwrap();
        assert_eq!(tasks.len(), 1, "跨文件是一个任务，不是 0 个或 2 个");
        assert_eq!(tasks[0].session, "sa", "归属跟着 task.start 走");
        assert_eq!(tasks[0].events.len(), 3, "task.start + tool_call + loop.finish 拼全");
        assert!(tasks[0].duration_ms() >= 22_000, "时长跨文件累计：{}", tasks[0].duration_ms());
    }

    /// 轮转兜底：task.start 被归档吃掉后，剩下的工具事件不能当孤儿全丢。
    ///
    /// 回归 2026-10-06 用户实锤「时间线刷不出来」：真机上长任务的 task.start 落在
    /// 更早的归档里被删（keep_archives=3），后续 tool_call 还留在仅存的几份中，
    /// 旧代码把它们全丢 → 时间线整块空白。现在要开一个 lost_start 的降级任务。
    #[test]
    fn orphan_tool_events_after_rotation_become_a_partial_task() {
        // 没有 task.start；step 也不是 0（真机 step 是进程级累加计数器，永远不回 0）
        let lines = vec![
            rec("2026-10-06T08:00:00Z", "state", serde_json::json!({"kind":"startup"})),
            rec("2026-10-06T08:07:09Z", "tool_call", serde_json::json!({"step":7768,"name":"shell","arguments":"del x"})),
            rec("2026-10-06T08:07:20Z", "tool_result", serde_json::json!({"step":7768,"ok":true})),
            rec("2026-10-06T08:07:31Z", "tool_call", serde_json::json!({"step":7769,"name":"shell","arguments":"del y"})),
        ];
        let (_g, path) = write_tmp("orphan", &lines);
        let tasks = read_tasks_scoped(&path, 10, None).unwrap();
        assert_eq!(tasks.len(), 1, "孤儿活动要落成一个降级任务，不是被丢弃");
        let t = &tasks[0];
        assert!(t.lost_start, "缺 task.start 的任务要标记起点缺失");
        assert_eq!(t.events.len(), 3, "三条工具事件都要在");
        // start_ts 用兜底边界那条的时刻，且必须照实反映「这不是真起点」
        assert_eq!(t.start_ts, "2026-10-06T08:07:09Z");
        // 进程启动的 state 不该被算进任务
        assert!(t.events.iter().all(|e| e.payload.get("kind").and_then(Value::as_str) != Some("startup")));
    }

    /// 正常任务（有 task.start）不能被误标成起点缺失。
    #[test]
    fn normal_task_is_not_marked_lost_start() {
        let lines = vec![
            rec("2026-10-06T08:00:00Z", "state", serde_json::json!({"kind":"task.start","session":"s1"})),
            rec("2026-10-06T08:00:02Z", "tool_call", serde_json::json!({"step":5,"name":"shell"})),
            rec("2026-10-06T08:00:03Z", "tool_result", serde_json::json!({"step":5,"ok":true})),
        ];
        let (_g, path) = write_tmp("normal", &lines);
        let tasks = read_tasks_scoped(&path, 10, None).unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(!tasks[0].lost_start, "有 task.start 就不是缺失");
        assert_eq!(tasks[0].events.len(), 3);
    }

    /// 降级任务要**按轮收尾**，不能把所有孤儿事件并成一个巨无霸。
    ///
    /// 真机日志里 `loop.finish` 实测 0 条，唯一可用的轮边界是 session.saved；它每轮
    /// 至少两条（任务起点一次、回复落地一次），所以判据是「本任务已有 tool_result
    /// 之后来的那条」——起点那条在 tool_call 之前，不会误闭合。
    #[test]
    fn partial_tasks_close_per_turn_via_session_saved() {
        let lines = vec![
            // 第一轮：起点快照 → 干活 → 回复落地
            rec("2026-10-06T08:00:00Z", "state", serde_json::json!({"kind":"session.saved","session":"s1","messages":1})),
            rec("2026-10-06T08:00:02Z", "tool_call", serde_json::json!({"step":7768,"name":"shell"})),
            rec("2026-10-06T08:00:03Z", "tool_result", serde_json::json!({"step":7768,"ok":true})),
            rec("2026-10-06T08:00:04Z", "state", serde_json::json!({"kind":"session.saved","session":"s1","messages":5})),
            // 第二轮：新的孤儿活动，结尾再来一次落地
            rec("2026-10-06T08:10:00Z", "state", serde_json::json!({"kind":"session.saved","session":"s1","messages":6})),
            rec("2026-10-06T08:10:02Z", "tool_call", serde_json::json!({"step":7800,"name":"git"})),
            rec("2026-10-06T08:10:03Z", "tool_result", serde_json::json!({"step":7800,"ok":true})),
            rec("2026-10-06T08:10:04Z", "state", serde_json::json!({"kind":"session.saved","session":"s1","messages":9})),
        ];
        let (_g, path) = write_tmp("partial-turn", &lines);
        let tasks = read_tasks_scoped(&path, 10, None).unwrap();
        assert_eq!(tasks.len(), 2, "两轮要切成两个任务，不是并成一个: {}", tasks.len());
        assert!(tasks.iter().all(|t| t.lost_start), "都没有 task.start，都该标起点缺失");
        // 任务从第一条 tool_call 起算（在那之前的 session.saved 是孤儿，丢弃）；
        // 每轮 = tool_call + tool_result + 闭合用的那条 session.saved（它就是本轮的
        // 收尾事件，计入比丢掉更诚实）
        assert_eq!(tasks[1].events.len(), 3, "第一轮：tool_call + tool_result + 收尾落盘");
        assert_eq!(tasks[0].events.len(), 3, "第二轮：tool_call + tool_result + 收尾落盘");
        // 归属照样能从闭合那条 session.saved 推出来
        assert_eq!(tasks[0].session, "s1");
    }
}
