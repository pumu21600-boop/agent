//! P22-B 定时任务接口：清单（含下次运行/通知）/ 新建改 / 删 / 立即运行
//!
//! 数据落 settings.json `schedules`（用户数据，随设置持久化）；运行时态
//! （上次状态/通知）由服务维护。档位/高危策略透传给无头会话，语义与 P22-A 一致。

use super::*;
use crate::schedule::{self, ScheduleTask};

#[derive(Debug, Deserialize)]
pub(super) struct ScheduleIdQuery {
    #[serde(default)]
    pub id: String,
}

/// GET /api/schedules —— 任务清单（带下次运行时间与人话描述）+ 最近完成通知。
pub(super) async fn list_schedules(State(state): State<AppState>) -> Json<Value> {
    let schedules: Vec<Value> = state
        .settings
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .schedules
        .iter()
        .map(|t| {
            let next = schedule::next_run_ts(t);
            json!({
                "id": t.id,
                "name": t.name,
                "cron": t.cron,
                "cron_text": schedule::parse_cron(&t.cron).map(|c| c.describe()).unwrap_or_default(),
                "message": t.message,
                "session_mode": if t.session_mode.is_empty() { "new".to_string() } else { t.session_mode.clone() },
                "session_id": t.session_id,
                "approval_mode": t.approval_mode,
                "on_approval": if t.on_approval.is_empty() { "deny".to_string() } else { t.on_approval.clone() },
                "enabled": t.enabled,
                "timeout_secs": t.timeout_secs,
                "created_ts": t.created_ts,
                "last_run_ts": t.last_run_ts,
                "last_manual_ts": t.last_manual_ts,
                "last_status": t.last_status,
                "last_error": t.last_error,
                "last_session_id": t.last_session_id,
                "next_run_ts": next,
            })
        })
        .collect();
    Json(json!({
        "schedules": schedules,
        "notices": schedule::notices(),
    }))
}

/// POST /api/schedules —— 新建（id 空）或修改（带 id）。校验不过返回人话错误。
pub(super) async fn save_schedule(
    State(state): State<AppState>,
    Json(mut req): Json<ScheduleTask>,
) -> (StatusCode, Json<Value>) {
    req.name = req.name.trim().to_string();
    req.cron = req.cron.trim().to_string();
    req.message = req.message.trim().to_string();
    req.session_id = req.session_id.trim().to_string();
    if let Err(e) = req.validate() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    }
    let is_new = req.id.trim().is_empty();
    if is_new {
        req.id = uuid::Uuid::new_v4().to_string();
        req.created_ts = chrono::Local::now().timestamp();
    }
    {
        let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner());
        if is_new {
            s.schedules.push(req.clone());
        } else if let Some(t) = s.schedules.iter_mut().find(|x| x.id == req.id) {
            // created_ts / 运行态以旧值为准（防客户端篡改排期基准）
            let created = t.created_ts;
            let last_run = t.last_run_ts;
            let last_manual = t.last_manual_ts;
            let last_status = t.last_status.clone();
            let last_error = t.last_error.clone();
            let last_session = t.last_session_id.clone();
            *t = req.clone();
            t.created_ts = created;
            t.last_run_ts = last_run;
            t.last_manual_ts = last_manual;
            t.last_status = last_status;
            t.last_error = last_error;
            t.last_session_id = last_session;
        } else {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("任务不存在: {}", req.id) })),
            );
        }
        let snapshot = s.clone();
        drop(s);
        if let Err(e) = snapshot.save() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("保存失败: {e}") })),
            );
        }
    }
    observability::event(
        EventType::State,
        json!({ "kind": "schedule.saved", "task": req.id, "name": req.name, "new": is_new }),
    );
    (StatusCode::OK, Json(json!({ "ok": true, "id": req.id })))
}

/// DELETE /api/schedules?id= —— 删除任务（正在跑的照常跑完，只是不再触发）。
pub(super) async fn delete_schedule(
    State(state): State<AppState>,
    Query(q): Query<ScheduleIdQuery>,
) -> (StatusCode, Json<Value>) {
    let id = q.id.trim().to_string();
    if id.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 id" })));
    }
    let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner());
    let before = s.schedules.len();
    s.schedules.retain(|t| t.id != id);
    let removed = s.schedules.len() != before;
    let snapshot = s.clone();
    drop(s);
    if !removed {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "任务不存在" })));
    }
    if let Err(e) = snapshot.save() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("删除落盘失败: {e}") })),
        );
    }
    observability::event(EventType::State, json!({ "kind": "schedule.deleted", "task": id }));
    (StatusCode::OK, Json(json!({ "ok": true })))
}

/// POST /api/schedules/run?id= —— 立即运行一次（不改排期基准；结果走通知）。
pub(super) async fn run_schedule_now(
    State(state): State<AppState>,
    Query(q): Query<ScheduleIdQuery>,
) -> (StatusCode, Json<Value>) {
    let id = q.id.trim().to_string();
    match schedule::run_now(&state, &id) {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true, "fired": true }))),
        Err(e) => (StatusCode::CONFLICT, Json(json!({ "error": e }))),
    }
}
