use super::*;

#[derive(Debug, Deserialize)]
pub struct CancelQuery {
    /// 目标会话（可空：不带 = 停全部在跑任务，命令行客户端与内置页的兼容语义）
    #[serde(default)]
    pub session: String,
}

/// POST /cancel[?session=<id>] —— 会话级停止：只停指定会话当前这一轮；
/// 不带 session 时停全部在跑任务。
pub(crate) async fn cancel(State(state): State<AppState>, Query(q): Query<CancelQuery>) -> Json<Value> {
    let session = q.session.trim();
    if session.is_empty() {
        let count = state.cancel.cancel_all();
        observability::event(
            EventType::State,
            json!({ "kind": "cancel", "scope": "all", "count": count }),
        );
        Json(json!({ "cancelled": true, "scope": "all", "count": count }))
    } else {
        let hit = state.cancel.cancel(session);
        // 父任务被停 → 级联停它的全部活跃子代理（子代理跑在独立会话，标志互不相通）
        let cascaded = subagent::cancel_children(session);
        observability::event(
            EventType::State,
            json!({ "kind": "cancel", "scope": "session", "session": session, "active": hit, "subagents_cancelled": cascaded }),
        );
        Json(json!({ "cancelled": true, "scope": "session", "session": session, "active": hit, "subagents_cancelled": cascaded }))
    }
}

/// GET /api/cancel[?session=<id>] —— 带 session 查该会话；不带查「是否有在跑会话被取消」。
pub(crate) async fn cancel_status(State(state): State<AppState>, Query(q): Query<CancelQuery>) -> Json<Value> {
    let session = q.session.trim();
    if session.is_empty() {
        Json(json!({
            "cancelled": state.cancel.any_cancelled(),
            "active": state.cancel.active_count(),
        }))
    } else {
        Json(json!({
            "cancelled": state.cancel.is_set(session),
            "running": state.cancel.is_active(session),
        }))
    }
}

// —— （施工单 014）：trace 时间线 ——

