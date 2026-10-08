//! 会话列表/元信息/分叉/批量/打开/清空/HANDOFF 端点（P19 自 server.rs 按域拆出）

use super::*;
use super::workspace::{apply_workspace, canon_workspace, same_workspace};

/// GET /api/sessions —— 会话列表（新→旧）+ 项目名（P6.1 多会话）。
/// `?scope=workspace` 只返回当前工作区的会话（composer @ 上下文「选对话加入」用；
/// 工作区比对与时间线同一把尺子 same_workspace，别在前端拿裸字符串比）。
pub(super) async fn list_sessions(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let project = crate::tools::workspace_root()
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let scope_workspace = q.get("scope").map(|s| s == "workspace").unwrap_or(false);
    let list = if scope_workspace {
        let cur = crate::tools::workspace_root().display().to_string();
        state
            .session
            .list()
            .into_iter()
            .filter(|m| same_workspace(&m.workspace, &cur))
            .collect::<Vec<_>>()
    } else {
        state.session.list()
    };
    Json(json!({ "project": project, "sessions": list }))
}

/// GET /api/sessions/transcript?id=xx —— 只读转写（composer @ 上下文「选对话加入」用）。
/// 不切当前会话、不带工具事件，只取 user/assistant 正文；总量超预算从**最早**的消息
/// 开始丢（保尾部——引用对话时最近的往来信息量最大），丢了就带 omitted=true。
pub(super) async fn session_transcript(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    const MAX_TOTAL: usize = 16_000;
    const MAX_MSG: usize = 2_500;
    let Some(id) = q.get("id").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 id" })));
    };
    let Some(f) = state.session.get(&id) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "会话不存在" })));
    };
    let mut msgs: Vec<(String, String)> = Vec::new();
    for m in &f.messages {
        let Some(role) = m.get("role").and_then(Value::as_str) else { continue };
        if role != "user" && role != "assistant" {
            continue;
        }
        let text = match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(|p| {
                    if p.get("type").and_then(Value::as_str) == Some("text") {
                        p.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let mut text = text.to_string();
        if text.chars().count() > MAX_MSG {
            text = format!("{}…", text.chars().take(MAX_MSG).collect::<String>());
        }
        msgs.push((role.to_string(), text));
    }
    // 预算：从尾部往前收，装不下的早期消息丢弃
    let mut used = 0usize;
    let mut kept: Vec<(String, String)> = Vec::new();
    for (role, text) in msgs.into_iter().rev() {
        let cost = text.chars().count() + 8;
        if used + cost > MAX_TOTAL {
            break;
        }
        used += cost;
        kept.push((role, text));
    }
    kept.reverse();
    let omitted = kept.len() < f.messages.len();
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "id": f.id,
            "title": f.title,
            "updated_at": f.updated_at,
            "omitted": omitted,
            "messages": kept.iter().map(|(r, t)| json!({ "role": r, "content": t })).collect::<Vec<_>>(),
        })),
    )
}

#[derive(Debug, Deserialize)]
pub struct SessionMetaUpdate {
    pub id: String,
    /// Some=重命名会话标题
    #[serde(default)]
    pub title: Option<String>,
    /// Some=置顶/取消置顶
    #[serde(default)]
    pub pinned: Option<bool>,
    /// Some=归档/取消归档
    #[serde(default)]
    pub archived: Option<bool>,
}

/// POST /api/sessions/meta —— DSH 式侧边栏：置顶/取消置顶、归档/取消归档、重命名。
pub(super) async fn session_meta(
    State(state): State<AppState>,
    Json(req): Json<SessionMetaUpdate>,
) -> (StatusCode, Json<Value>) {
    let sess = state.session.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        if let Some(t) = &req.title {
            sess.rename(&req.id, t.trim())?;
        }
        if req.pinned.is_some() || req.archived.is_some() {
            sess.set_flags(&req.id, req.pinned, req.archived)?;
        }
        Ok(())
    })
    .await;
    match res {
        Ok(Ok(())) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

#[derive(Debug, Deserialize)]
pub struct ForkRequest {
    pub id: String,
}

/// POST /api/sessions/fork —— DSH 式「分叉会话」：复制消息/摘要/工作区为新会话。
pub(super) async fn fork_session(
    State(state): State<AppState>,
    Json(req): Json<ForkRequest>,
) -> (StatusCode, Json<Value>) {
    let sess = state.session.clone();
    let id = req.id.clone();
    match tokio::task::spawn_blocking(move || sess.fork(&id)).await {
        Ok(Ok(new_id)) => (StatusCode::OK, Json(json!({ "ok": true, "id": new_id }))),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

#[derive(Debug, Deserialize)]
pub struct SessionBatchRequest {
    pub ids: Vec<String>,
    /// Some=批量置顶/取消置顶
    #[serde(default)]
    pub pinned: Option<bool>,
    /// Some=批量归档/取消归档（管理模式主用）
    #[serde(default)]
    pub archived: Option<bool>,
}

/// POST /api/sessions/batch —— 管理模式批量操作（归档/取消归档/置顶）。
pub(super) async fn batch_sessions(
    State(state): State<AppState>,
    Json(req): Json<SessionBatchRequest>,
) -> (StatusCode, Json<Value>) {
    let sess = state.session.clone();
    let ids = req.ids.clone();
    let (pinned, archived) = (req.pinned, req.archived);
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
        let mut n = 0;
        for id in &ids {
            if sess.set_flags(id, pinned, archived).is_ok() {
                n += 1;
            }
        }
        Ok(n)
    })
    .await;
    match res {
        Ok(Ok(n)) => (StatusCode::OK, Json(json!({ "ok": true, "updated": n }))),
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// GET /api/session?id=xxx —— 恢复指定会话；缺省返回最近更新的一条。
pub(super) async fn get_session(State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>) -> (StatusCode, Json<Value>) {
    let f = match q.get("id") {
        Some(id) => state.session.get(id),
        None => state.session.latest(),
    };
    match f {
        Some(f) => (StatusCode::OK, Json(json!({ "id": f.id, "title": f.title, "updated_at": f.updated_at, "summary": f.summary, "workspace": f.workspace, "messages": f.messages }))),
        None => (StatusCode::OK, Json(json!({ "messages": [] }))),
    }
}

#[derive(Debug, Deserialize)]
pub struct SessionOpenRequest {
    pub id: String,
}

/// POST /api/sessions/new —— 新建一条空会话并立即落盘（当前项目下），
/// 侧栏马上能见到该会话；首条消息随后写入同一条。同工作区已有空会话则复用。
pub(super) async fn new_session(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let ws = crate::tools::workspace_root().display().to_string();
    match state.session.create_empty(&ws, "新会话") {
        Ok(f) => {
            observability::event(
                EventType::State,
                json!({ "kind": "session.created", "session": f.id, "workspace": ws }),
            );
            (
                StatusCode::OK,
                Json(json!({ "ok": true, "id": f.id, "title": f.title, "workspace": ws, "created_at": f.created_at })),
            )
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// POST /api/sessions/open —— 打开会话：其所属工作区与当前不同时先切过去，再返回消息。
/// 「切会话 = 回到那个项目」的闭环入口；目录已不存在时不硬失败，降级只读打开并回 warning。
pub(super) async fn open_session(
    State(state): State<AppState>,
    Json(req): Json<SessionOpenRequest>,
) -> (StatusCode, Json<Value>) {
    let Some(f) = state.session.get(&req.id) else {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "会话不存在" })));
    };
    let current = crate::tools::workspace_root().display().to_string();
    let mut switched = false;
    let mut warning: Option<String> = None;
    let target = f.workspace.trim();
    if !target.is_empty() && !same_workspace(&current, target) {
        let p = std::path::PathBuf::from(target);
        if p.is_dir() {
            let canon = canon_workspace(p);
            match apply_workspace(&state, &canon) {
                Ok(()) => switched = true,
                Err(e) => warning = Some(format!("切换项目失败：{e}")),
            }
        } else {
            warning = Some(format!("原项目目录已不存在，仍在当前项目里打开该会话：{target}"));
        }
    }
    (
        StatusCode::OK,
        Json(json!({
            "id": f.id,
            "title": f.title,
            "updated_at": f.updated_at,
            "summary": f.summary,
            "messages": f.messages,
            "workspace": f.workspace,
            // 用量快照：重启/重开后恢复圆环与缓存行（无快照的旧会话为 null，前端按零兜底）
            "usage": f.usage,
            "current_workspace": crate::tools::workspace_root().display().to_string(),
            "switched": switched,
            "warning": warning,
        })),
    )
}

/// DELETE /api/session?id=xxx —— 删除一条会话（其余不受影响）。
pub(super) async fn clear_session(State(state): State<AppState>, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>) -> (StatusCode, Json<Value>) {
    let Some(id) = q.get("id") else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 id" })));
    };
    match state.session.delete(id) {
        Ok(()) => {
            // P9 A3：删会话即撤销其「会话内同类允许」记忆（仅内存，从不落盘）
            if let Ok(mut m) = state.approved_scopes.lock() {
                m.remove(id);
            }
            // 2026-10-08 修：档位表也只插不删——每个历史会话永久留一条，
            // 长期运行无上限增长。删会话时一并收掉（session 字段不校验存在性，
            // 直接 curl 循环打 /api/approval-mode 也能灌，这里至少让正常路径闭合）。
            if let Ok(mut m) = state.session_approval.write() {
                m.remove(id);
            }
            observability::event(EventType::State, json!({ "kind": "session.cleared", "session": id }));
            (StatusCode::OK, Json(json!({ "ok": true })))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

// 注：P6 交接文档生成（/api/handoff 手动 + 核心循环 80% 自动）已按用户要求移除
// （2026-10-01）——会话落盘与滚动摘要已保障跨任务续命，不再另生 HANDOFF.md。
