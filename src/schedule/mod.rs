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
    /// 5 段 cron（分 时 日 月 周，本地时区）；保存时校验可解析
    #[serde(default)]
    pub cron: String,
    /// 到点唤醒无头会话的消息内容
    #[serde(default)]
    pub message: String,
    /// 会话模式："new"（缺省，每轮新会话）| "fixed"（复用 session_id）
    #[serde(default)]
    pub session_mode: String,
    #[serde(default)]
    pub session_id: String,
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
        if self.message.trim().is_empty() {
            return Err("唤醒消息不能为空".into());
        }
        cron::parse(&self.cron).map_err(|e| format!("cron 表达式：{e}"))?;
        if self.session_mode == "fixed" && self.session_id.trim().is_empty() {
            return Err("固定会话模式要填会话 id".into());
        }
        Ok(())
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
        let Ok(spec) = cron::parse(&t.cron) else {
            continue; // 保存时已校验；运行中改坏的旧任务不跑
        };
        let base = t.last_run_ts.unwrap_or(t.created_ts);
        let Some(from) = DateTime::from_timestamp(base, 0).map(|d| d.with_timezone(&Local)) else {
            continue;
        };
        let Some(next) = spec.next_after(from) else {
            continue;
        };
        if now < next.timestamp() {
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
        let req = crate::server::HeadlessRequest {
            message: task.message.clone(),
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
        };
        let result = crate::server::run_headless_inner(state.clone(), req).await;
        let (status, detail, session_id, error, duration_ms) = match result {
            Ok(r) => (
                r.status.clone(),
                preview(&r.answer),
                r.session_id.clone(),
                r.error.clone().unwrap_or_default(),
                r.duration_ms,
            ),
            Err((_code, msg)) => ("error".to_string(), String::new(), String::new(), msg, 0),
        };
        let now = chrono::Local::now().timestamp();
        // 回写任务状态（自动触发才动 last_run_ts——排期基准不能被手动跑打乱）
        {
            let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner());
            if let Some(t) = s.schedules.iter_mut().find(|x| x.id == task.id) {
                if !manual {
                    t.last_run_ts = Some(now);
                } else {
                    t.last_manual_ts = Some(now);
                }
                t.last_status = Some(status.clone());
                t.last_error = if error.is_empty() { None } else { Some(error.clone()) };
                if !session_id.is_empty() {
                    t.last_session_id = Some(session_id.clone());
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

/// 回答开头截断（通知详情用）。
fn preview(s: &str) -> String {
    let flat: String = s.chars().take(120).collect();
    flat.trim().to_string()
}

/// 计算某任务的下次触发时间（unix 秒；解析失败/无下一次返回 None）。
pub fn next_run_ts(task: &ScheduleTask) -> Option<i64> {
    let spec = cron::parse(&task.cron).ok()?;
    let base = task.last_run_ts.unwrap_or(task.created_ts);
    let from = DateTime::from_timestamp(base, 0)?.with_timezone(&Local);
    spec.next_after(from).map(|d| d.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(cron: &str, mode: &str, sid: &str) -> ScheduleTask {
        ScheduleTask {
            id: "t1".into(),
            name: "巡检".into(),
            cron: cron.into(),
            message: "跑一遍构建".into(),
            session_mode: mode.into(),
            session_id: sid.into(),
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
