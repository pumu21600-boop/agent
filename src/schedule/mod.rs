//! P22-B 定时任务：按 cron 唤醒无头会话跑任务，完成留痕可查
//!
//! 形态（最薄上层）：settings.json 里存任务清单（`schedules`），服务内一个 20s
//! 心跳的循环到点唤醒（走 P22-A 无头会话同一条路），结果写回任务状态 + 全局
//! 通知列表（GET /api/schedules 一并带出，设置→定时 页签展示）。
//!
//! 语义定死：
//! - **到点才跑**：next_after(last_run_ts 或 created_ts) ≤ now 才触发；同一任务
//!   上一轮没跑完（RUNNING 守卫）本轮跳过，绝不并发双跑；
//! - **错过补跑一次**：服务停机期间过点的任务，重启后第一次心跳立即补跑一次
//!   （base 取 last_run/created，天然成立），不会连补多轮；
//! - **立即运行不动账**：手动跑不改 last_run_ts（不打乱排期），只更新状态字段；
//! - **会话两模式**：new（缺省）= 每轮新会话；fixed = 复用同一会话（在跑则 409，
//!   记进 last_error，不重试不连坐）。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::server::AppState;

mod cron;

pub use cron::parse as parse_cron;

fn yes() -> bool {
    true
}

/// 一条定时任务（settings.json `schedules` 数组元素）。
/// 新建时客户端只需 name/cron/message：id 与 created_ts 由服务端补，其余字段有缺省。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleTask {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// 5 段 cron（分 时 日 月 周，本地时区）；保存时校验可解析。
    /// once_ts > 0 时本字段不参与调度（单次任务没有周期表达式）。
    #[serde(default)]
    pub cron: String,
    /// 单次任务的目标时间（unix 秒；0 = 周期任务，按 cron 调度）。
    /// 语义：到点跑一次，跑完自动停用（enabled=false）；停机期间错过的，
    /// 重启后第一次心跳补跑一次。
    #[serde(default)]
    pub once_ts: i64,
    /// 到点唤醒无头会话的消息内容（kind=memory 时忽略，内容由记忆检索生成）
    #[serde(default)]
    pub message: String,
    /// 任务类型："message"（缺省，固定消息）| "memory"（P22-D 主动记忆提醒：
    /// 到点先检索最近 memory_days 天的对话记录，生成回顾指令再唤醒）
    #[serde(default)]
    pub kind: String,
    /// memory 类型的回顾窗口（天，1–90，缺省 3）
    #[serde(default)]
    pub memory_days: u32,
    /// 会话模式："new"（缺省，每轮新会话）| "fixed"（复用 session_id）
    #[serde(default)]
    pub session_mode: String,
    #[serde(default)]
    pub session_id: String,
    /// 任务执行所在工作区（目录绝对路径；空 = 跟随触发时的当前工作区）。
    /// 非空时整轮切到该目录跑（完成会话也记在这个项目下），跑完还原全局根。
    #[serde(default)]
    pub workspace: String,
    /// 审批档位（透传无头会话；空 = 用 config 默认档）
    #[serde(default)]
    pub approval_mode: String,
    /// 高危操作策略：deny（缺省）| approve
    #[serde(default)]
    pub on_approval: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// 整轮超时（秒）；0 = 用默认 1800
    #[serde(default)]
    pub timeout_secs: u64,
    #[serde(default)]
    pub created_ts: i64,
    /// 上次**自动**触发时间（排期基准；手动运行不动它）
    #[serde(default)]
    pub last_run_ts: Option<i64>,
    /// 上次手动运行时间（仅展示）
    #[serde(default)]
    pub last_manual_ts: Option<i64>,
    #[serde(default)]
    pub last_status: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// 上一轮实际用的会话 id（fixed 模式恒为 session_id；new 模式每轮不同）
    #[serde(default)]
    pub last_session_id: Option<String>,
}

impl ScheduleTask {
    /// 生效的会话模式（缺省 new；fixed 才复用）。
    pub fn fixed_session(&self) -> Option<&str> {
        if self.session_mode == "fixed" && !self.session_id.is_empty() {
            Some(&self.session_id)
        } else {
            None
        }
    }

    /// 保存前校验（人话错误，直接给设置面板）。
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("任务名称不能为空".into());
        }
        if self.once_ts > 0 {
            // 单次任务：cron 不参与调度；启用的任务时间必须在未来
            // （已跑完自动停用的任务 enabled=false，改个名字再保存不受限）
            if self.enabled && self.once_ts <= chrono::Local::now().timestamp() {
                return Err("单次任务的时间要设在将来".into());
            }
        } else {
            cron::parse(&self.cron).map_err(|e| format!("cron 表达式：{e}"))?;
        }
        match self.kind.as_str() {
            "" | "message" => {
                if self.message.trim().is_empty() {
                    return Err("唤醒消息不能为空".into());
                }
            }
            "memory" => {
                if self.memory_days == 0 || self.memory_days > 90 {
                    return Err("回顾天数要在 1–90 之间".into());
                }
            }
            other => return Err(format!("未知任务类型: {other}")),
        }
        if self.session_mode == "fixed" && self.session_id.trim().is_empty() {
            return Err("固定会话模式要填会话 id".into());
        }
        if !self.workspace.trim().is_empty() && !std::path::Path::new(self.workspace.trim()).is_dir() {
            return Err(format!("工作区目录不存在: {}", self.workspace.trim()));
        }
        Ok(())
    }

    /// 生效的任务类型（缺省 message）。
    pub fn kind(&self) -> &str {
        if self.kind == "memory" { "memory" } else { "message" }
    }

    /// 生效的回顾窗口（memory 类型；越界值兜底 3）。
    pub fn memory_days(&self) -> u32 {
        if self.memory_days == 0 || self.memory_days > 90 { 3 } else { self.memory_days }
    }
}

/// 完成通知（进程内环形缓冲，重启即失；持久结论看任务状态与会话文件）。
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleNotice {
    pub ts: String,
    /// "auto"（到点触发）| "manual"（立即运行）
    pub trigger: String,
    pub task_id: String,
    pub task_name: String,
    /// done / cancelled / timeout / error
    pub status: String,
    /// 状态人话
    pub title: String,
    /// 回答开头或错误（截断）
    pub detail: String,
    pub session_id: String,
    pub duration_ms: u128,
}

fn notices_slot() -> &'static Mutex<std::collections::VecDeque<ScheduleNotice>> {
    static NOTICES: OnceLock<Mutex<std::collections::VecDeque<ScheduleNotice>>> = OnceLock::new();
    NOTICES.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}

/// 最近通知（新→旧，最多 50 条）。
pub fn notices() -> Vec<ScheduleNotice> {
    notices_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

fn push_notice(n: ScheduleNotice) {
    let mut q = notices_slot().lock().unwrap_or_else(|e| e.into_inner());
    q.push_front(n);
    while q.len() > 50 {
        q.pop_back();
    }
}

fn running_slot() -> &'static Mutex<HashSet<String>> {
    static RUNNING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 心跳间隔：20s（cron 最小粒度是分钟，20s 足够及时且开销可忽略）。
const TICK_SECS: u64 = 20;
/// 整轮默认超时（秒）。
const DEFAULT_TIMEOUT_SECS: u64 = 1800;

/// 启动调度循环（main.rs 在服务就绪后调用；没有任务时每跳只读一次设置）。
pub fn start(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(TICK_SECS));
        loop {
            tick.tick().await;
            tick_all(&state);
        }
    });
}

/// 单次心跳：扫全部启用任务，到点的发起唤醒。
fn tick_all(state: &AppState) {
    let now = chrono::Local::now().timestamp();
    let tasks: Vec<ScheduleTask> = match state.settings.read() {
        Ok(s) => s.schedules.clone(),
        Err(e) => e.into_inner().schedules.clone(),
    };
    for t in tasks {
        if !t.enabled {
            continue;
        }
        if running_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&t.id)
        {
            continue; // 上一轮没跑完：跳过本轮，不并发双跑
        }
        // 到点判断：单次任务 = 设定时间已过且没跑过（停机错过的重启补跑一次）；
        // 周期任务 = cron 的 next_after(last_run/created) ≤ now
        let due = if t.once_ts > 0 {
            t.last_run_ts.is_none() && now >= t.once_ts
        } else {
            let Ok(spec) = cron::parse(&t.cron) else {
                continue; // 保存时已校验；运行中改坏的旧任务不跑
            };
            let base = t.last_run_ts.unwrap_or(t.created_ts);
            let Some(from) = DateTime::from_timestamp(base, 0).map(|d| d.with_timezone(&Local))
            else {
                continue;
            };
            let Some(next) = spec.next_after(from) else {
                continue;
            };
            now >= next.timestamp()
        };
        if !due {
            continue; // 没到点
        }
        crate::observability::event(
            crate::observability::EventType::State,
            serde_json::json!({
                "kind": "schedule.fired", "task": t.id, "name": t.name, "trigger": "auto",
            }),
        );
        fire(state, t, "auto", false);
    }
}

/// 立即运行一次（手动；不改 last_run_ts）。
pub fn run_now(state: &AppState, id: &str) -> Result<(), String> {
    let t = state
        .settings
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .schedules
        .iter()
        .find(|x| x.id == id)
        .cloned()
        .ok_or_else(|| "任务不存在".to_string())?;
    if running_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&t.id)
    {
        return Err("该任务正在运行中，等这一轮结束再跑".into());
    }
    fire(state, t, "manual", true);
    Ok(())
}

/// 发起一次唤醒：占守卫 → 无头会话 → 回写状态 + 通知。
fn fire(state: &AppState, task: ScheduleTask, trigger: &'static str, manual: bool) {
    {
        let mut running = running_slot().lock().unwrap_or_else(|e| e.into_inner());
        if !running.insert(task.id.clone()) {
            return; // 双保险：并发入口只允许一个在跑
        }
    }
    let state = state.clone();
    tokio::spawn(async move {
        // P22-D：记忆提醒类型——到点先同步记忆库（本地索引，零网络）并生成回顾指令；
        // 窗口内没有可回顾的对话就跳过本轮（标记已处理，不每跳重复到点、不白烧模型调用）
        let message = if task.kind() == "memory" {
            // 先同步记忆库（有界增量，配了嵌入渠道走语义向量），再检索回顾窗口内的对话
            crate::server::memory::sync_library_for_schedule(&state).await;
            let lib = crate::memory::library::load(&state.log_dir);
            match build_memory_message(&lib.entries, task.memory_days(), chrono::Local::now().timestamp()) {
                Some(m) => m,
                None => {
                    let now = chrono::Local::now().timestamp();
                    write_back(&state, &task.id, !manual, now, "skipped", Some("回顾窗口内没有可回顾的对话"), None);
                    push_notice(ScheduleNotice {
                        ts: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
                        trigger: trigger.to_string(),
                        task_id: task.id.clone(),
                        task_name: task.name.clone(),
                        status: "skipped".into(),
                        title: format!("定时任务「{}」跳过：没有可回顾的对话", task.name),
                        detail: String::new(),
                        session_id: String::new(),
                        duration_ms: 0,
                    });
                    running_slot()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&task.id);
                    return;
                }
            }
        } else {
            task.message.clone()
        };
        let req = crate::server::HeadlessRequest {
            message,
            session_id: task.fixed_session().unwrap_or("").to_string(),
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            approval_mode: task.approval_mode.clone(),
            plan_mode: false,
            on_approval: task.on_approval.clone(),
                timeout_secs: if task.timeout_secs == 0 {
                    DEFAULT_TIMEOUT_SECS
                } else {
                    task.timeout_secs
                },
                purpose: "schedule".into(),
                parent_session: String::new(),
                workspace: task.workspace.clone(),
            };
        let result = crate::server::run_headless_inner(state.clone(), req).await;
        let (status, session_id, error, duration_ms) = match result {
            Ok(r) => (
                r.status.clone(),
                r.session_id.clone(),
                r.error.clone().unwrap_or_default(),
                r.duration_ms,
            ),
            Err((_code, msg)) => ("error".to_string(), String::new(), msg, 0),
        };
        // 通知不贴回答内容（去会话里看）；只有出错才带一句原因
        let detail = if status == "done" { String::new() } else { error.clone() };
        // 回写任务状态（自动触发才动 last_run_ts——排期基准不能被手动跑打乱）
        write_back(
            &state,
            &task.id,
            !manual,
            chrono::Local::now().timestamp(),
            &status,
            Some(error.as_str()),
            Some(session_id.as_str()),
        );
        push_notice(ScheduleNotice {
            ts: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            trigger: trigger.to_string(),
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            title: format!(
                "定时任务「{}」{}",
                task.name,
                match status.as_str() {
                    "done" => "已完成",
                    "cancelled" => "已取消",
                    "timeout" => "超时取消",
                    _ => "出错",
                }
            ),
            status,
            detail,
            session_id,
            duration_ms,
        });
        crate::observability::event(
            crate::observability::EventType::State,
            serde_json::json!({
                "kind": "schedule.done", "task": task.id, "name": task.name, "trigger": trigger,
            }),
        );
        running_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&task.id);
    });
}

/// 回写任务状态到 settings.json（自动触发才动 last_run_ts——排期基准不能被手动跑打乱）。
/// `session` 为空 = 不更新会话 id（如跳过轮没有会话）。
fn write_back(
    state: &AppState,
    task_id: &str,
    auto: bool,
    now: i64,
    status: &str,
    error: Option<&str>,
    session: Option<&str>,
) {
    let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner());
    if let Some(t) = s.schedules.iter_mut().find(|x| x.id == task_id) {
        if auto {
            t.last_run_ts = Some(now);
        } else {
            t.last_manual_ts = Some(now);
        }
        // 单次任务：跑过一次就到头——记 last_run_ts（防重复触发）+ 自动停用。
        // 手动"立即运行"同样消耗掉这一次（单次语义不区分触发来源）。
        if t.once_ts > 0 {
            t.last_run_ts = Some(now);
            t.enabled = false;
        }
        t.last_status = Some(status.to_string());
        t.last_error = error.filter(|e| !e.is_empty()).map(|e| e.to_string());
        if let Some(sid) = session.filter(|s| !s.is_empty()) {
            t.last_session_id = Some(sid.to_string());
        }
    }
    let snapshot = s.clone();
    drop(s);
    if let Err(e) = snapshot.save() {
        crate::observability::event(
            crate::observability::EventType::Error,
            serde_json::json!({ "message": format!("定时任务状态落盘失败: {e}") }),
        );
    }
}

/// 计算某任务的下次触发时间（unix 秒；解析失败/无下一次返回 None）。
pub fn next_run_ts(task: &ScheduleTask) -> Option<i64> {
    if task.once_ts > 0 {
        // 单次：没跑过且启用才有"下次"；跑完（last_run 已记/已停用）= 没有
        return if task.enabled && task.last_run_ts.is_none() {
            Some(task.once_ts)
        } else {
            None
        };
    }
    let spec = cron::parse(&task.cron).ok()?;
    let base = task.last_run_ts.unwrap_or(task.created_ts);
    let from = DateTime::from_timestamp(base, 0)?.with_timezone(&Local);
    spec.next_after(from).map(|d| d.timestamp())
}

/// P22-D：从记忆库条目生成「主动记忆提醒」的回顾指令（纯函数，可单测）。
/// 口径：最近 `days` 天内有更新、非归档、至少 2 条消息的真对话；按更新时间倒序取 12 条。
/// 没有可回顾的返回 None（调度器据此跳过本轮，不白烧一次模型调用）。
pub fn build_memory_message(
    entries: &[crate::memory::library::Entry],
    days: u32,
    now_ts: i64,
) -> Option<String> {
    let cutoff = now_ts - (days as i64) * 86_400;
    let mut picked: Vec<&crate::memory::library::Entry> = entries
        .iter()
        .filter(|e| {
            !e.archived
                && e.count >= 2
                && parse_rfc3339(&e.updated_at).is_some_and(|t| t >= cutoff)
        })
        .collect();
    // RFC3339（UTC 定长）字典序即时间序，倒序 = 最近更新在前
    picked.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    picked.truncate(12);
    if picked.is_empty() {
        return None;
    }
    let lines: Vec<String> = picked
        .iter()
        .map(|e| {
            let day: String = e.updated_at.chars().take(10).collect();
            let head: String = e.digest.chars().take(160).collect();
            format!("- 《{}》（{}，更新于 {}）：{}", e.title, e.project, day, head.trim())
        })
        .collect();
    Some(format!(
        "【主动记忆提醒】以下是最近 {days} 天里有更新的对话记录（最多 12 条）：\n{}\n\n请从这些记录里找出用户提到「要跟进 / 待办 / 之后再做 / 计划中 / 约好了」的事项，逐条列出并给出建议动作；没有任何值得提醒的就明确说「没有需要提醒的事」，不要编造不存在的事项。",
        lines.join("\n")
    ))
}

fn parse_rfc3339(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(cron: &str, mode: &str, sid: &str) -> ScheduleTask {
        ScheduleTask {
            id: "t1".into(),
            name: "巡检".into(),
            cron: cron.into(),
            once_ts: 0,
            message: "跑一遍构建".into(),
            kind: String::new(),
            memory_days: 0,
            session_mode: mode.into(),
            session_id: sid.into(),
            workspace: String::new(),
            approval_mode: String::new(),
            on_approval: String::new(),
            enabled: true,
            timeout_secs: 0,
            created_ts: 0,
            last_run_ts: None,
            last_manual_ts: None,
            last_status: None,
            last_error: None,
            last_session_id: None,
        }
    }

    #[test]
    fn validate_rejects_bad_tasks() {
        let mut t = task("0 9 * * *", "new", "");
        assert!(t.validate().is_ok());
        t.name = "  ".into();
        assert!(t.validate().is_err());
        let mut t = task("0 9 * * *", "new", "");
        t.message = String::new();
        assert!(t.validate().is_err());
        let t = task("bad cron", "new", "");
        assert!(t.validate().unwrap_err().contains("cron"));
        let t = task("0 9 * * *", "fixed", "");
        assert!(t.validate().is_err(), "fixed 模式必须带会话 id");
        let t = task("0 9 * * *", "fixed", "sess-1");
        assert!(t.validate().is_ok());
        // P22-D：memory 类型不要求 message，但 days 要在 1–90
        let mut t = task("0 9 * * *", "new", "");
        t.kind = "memory".into();
        t.message = String::new();
        t.memory_days = 3;
        assert!(t.validate().is_ok(), "memory 类型不要求固定消息");
        t.memory_days = 0;
        assert!(t.validate().is_err(), "days=0 拒绝（kind() 兜底另算）");
        t.memory_days = 91;
        assert!(t.validate().is_err());
        let mut t = task("0 9 * * *", "new", "");
        t.kind = "weird".into();
        assert!(t.validate().unwrap_err().contains("未知任务类型"));
    }

    /// 单次任务（once_ts）：不要求 cron；启用的必须设在未来；
    /// 待跑有"下次"，跑过/停用即无。
    #[test]
    fn once_task_validate_and_next_run() {
        let now = chrono::Local::now().timestamp();
        let mut t = task("", "new", "");
        t.once_ts = now + 3600;
        assert!(t.validate().is_ok(), "单次任务不要求 cron，时间在未来即可");
        t.once_ts = now - 60;
        assert!(t.validate().is_err(), "启用的单次任务时间要在将来");
        // 已跑完自动停用的（enabled=false、时间已过）：改名等编辑不受限
        t.enabled = false;
        assert!(t.validate().is_ok());
        // 周期语义不受影响：cron 任务 once_ts=0 仍走 cron 校验
        let t = task("bad cron", "new", "");
        assert!(t.validate().unwrap_err().contains("cron"));
        // 下次触发：待跑 = once_ts；跑过 / 停用 = 没有
        let mut t = task("", "new", "");
        t.once_ts = now + 3600;
        assert_eq!(next_run_ts(&t), Some(t.once_ts));
        t.last_run_ts = Some(now);
        assert_eq!(next_run_ts(&t), None, "跑过的单次任务没有下次");
        t.last_run_ts = None;
        t.enabled = false;
        assert_eq!(next_run_ts(&t), None, "停用的单次任务没有下次");
    }

    /// 工作区：指定了就必须是存在的目录；空 = 跟随当前，不校验。
    #[test]
    fn workspace_must_exist_when_set() {
        let mut t = task("0 9 * * *", "new", "");
        t.workspace = "./definitely_missing_ws_dir_42".into();
        assert!(t.validate().unwrap_err().contains("工作区目录不存在"));
        t.workspace = String::new();
        assert!(t.validate().is_ok());
    }

    /// P22-D：类型与回顾窗口的生效值（缺省 message / days 越界兜底 3）。
    #[test]
    fn kind_and_days_defaults() {
        let t = task("0 9 * * *", "new", "");
        assert_eq!(t.kind(), "message");
        assert_eq!(t.memory_days(), 3);
        let mut t = task("0 9 * * *", "new", "");
        t.kind = "memory".into();
        t.memory_days = 7;
        assert_eq!(t.kind(), "memory");
        assert_eq!(t.memory_days(), 7);
        // 越界值：validate 已拦，kind()/memory_days() 兜底不 panic
        let mut t = task("0 9 * * *", "new", "");
        t.kind = "memory".into();
        t.memory_days = 999;
        assert_eq!(t.memory_days(), 3);
    }

    /// P22-D：回顾指令生成——窗口过滤 / 归档与空会话排除 / 倒序取 12 / 空窗 None。
    #[test]
    fn build_memory_message_filters_and_composes() {
        use crate::memory::library::Entry;
        let now = 1_800_000_000i64; // 2027-01-15 前后
        let mk = |id: &str, upd: &str, count: usize, archived: bool| Entry {
            id: id.into(),
            title: format!("会话{id}"),
            workspace: "/ws".into(),
            project: "proj".into(),
            created_at: upd.into(),
            updated_at: upd.into(),
            count,
            chars: 100,
            pinned: false,
            archived,
            digest: format!("{id} 的摘要内容"),
            snippet: String::new(),
            fp: String::new(),
            vec: Vec::new(),
        };
        // 窗口内（1 天前）/ 窗口外（10 天前）/ 归档 / 空会话各一条
        let in_win = chrono::DateTime::from_timestamp(now - 86_400, 0).unwrap().to_rfc3339();
        let out_win = chrono::DateTime::from_timestamp(now - 10 * 86_400, 0).unwrap().to_rfc3339();
        let entries = vec![
            mk("a", &in_win, 4, false),
            mk("b", &out_win, 6, false),
            mk("c", &in_win, 5, true),
            mk("d", &in_win, 1, false),
        ];
        let msg = build_memory_message(&entries, 3, now).expect("应生成");
        assert!(msg.contains("会话a"), "窗口内的真对话要在");
        assert!(!msg.contains("会话b"), "窗口外的不要");
        assert!(!msg.contains("会话c"), "归档的不要");
        assert!(!msg.contains("会话d"), "空会话不要");
        assert!(msg.contains("最近 3 天"), "{msg}");
        // 全部不合格 → None（调度器据此跳过，不白烧模型调用）
        assert!(build_memory_message(&entries[1..], 3, now).is_none());
        assert!(build_memory_message(&[], 3, now).is_none());
    }

    #[test]
    fn fixed_session_only_when_configured() {
        assert_eq!(task("0 9 * * *", "new", "").fixed_session(), None);
        assert_eq!(task("0 9 * * *", "fixed", "s1").fixed_session(), Some("s1"));
        // fixed 但没填 id：退化成每轮新会话（validate 已在保存时拦下）
        assert_eq!(task("0 9 * * *", "fixed", "").fixed_session(), None);
    }

    #[test]
    fn next_run_uses_last_run_as_base() {
        // 从没跑过：基准 = created_ts（0 = 1970-01-01 00:00 UTC = 本地 08:00）
        // → 当天本地 09:00 = 1970-01-01 01:00 UTC = 3600 秒
        let t = task("0 9 * * *", "new", "");
        assert_eq!(next_run_ts(&t), Some(3600));
        // 跑过：基准 = last_run_ts，下一次严格大于它
        let mut t = task("0 9 * * *", "new", "");
        let base = DateTime::from_timestamp(1_800_000_000, 0).unwrap(); // 2027-01-15 前后
        t.last_run_ts = Some(base.timestamp());
        let next = next_run_ts(&t).expect("应有下一次");
        assert!(next > base.timestamp());
        // cron 坏了的旧任务：None（不跑也不 panic）
        let t = task("not a cron", "new", "");
        assert_eq!(next_run_ts(&t), None);
    }
}
