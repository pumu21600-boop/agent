//! Git 状态/提交/检查点端点（P19 自 server.rs 按域拆出）

use super::*;

/// GET /api/changes?baseline=xx[&origin=shadow|workspace] —— 本任务相对基线的改动文件清单
/// （「产物」/「审查」标签数据源）files: [{ path, status, adds, dels }]；未跟踪文件也在内
/// （agent 新建的文件必现）。origin=workspace 且不传 baseline = 真仓库相对 HEAD（「未提交改动」档）。
pub(super) async fn list_changes(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let err = |e: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    let origin = crate::recovery::RepoOrigin::from_query(params.get("origin").map(|s| s.as_str()));
    let mut baseline = params.get("baseline").map(|s| s.trim()).unwrap_or("").to_string();
    if baseline.is_empty() {
        if origin == crate::recovery::RepoOrigin::Workspace {
            baseline = "HEAD".to_string();
        } else {
            return Err(err("缺少 baseline".into()));
        }
    }
    let ws = crate::tools::workspace_root().to_path_buf();
    match tokio::task::spawn_blocking(move || crate::recovery::changed_files(&ws, origin, &baseline)).await {
        Ok(Ok(files)) => Ok(Json(json!({ "ok": true, "files": files }))),
        Ok(Err(e)) => Err(err(e.to_string())),
        Err(e) => Err(err(e.to_string())),
    }
}

/// GET /api/review/diff?baseline=xx[&path=xx][&origin=shadow|workspace] —— 相对基线的统一 diff 文本
/// （审查面板逐文件看具体改动用；path 缺省 = 全量）。
pub(super) async fn review_diff(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let err = |e: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    let origin = crate::recovery::RepoOrigin::from_query(params.get("origin").map(|s| s.as_str()));
    let mut baseline = params.get("baseline").map(|s| s.trim()).unwrap_or("").to_string();
    if baseline.is_empty() {
        if origin == crate::recovery::RepoOrigin::Workspace {
            baseline = "HEAD".to_string();
        } else {
            return Err(err("缺少 baseline".into()));
        }
    }
    let ws = crate::tools::workspace_root().to_path_buf();
    let path = params.get("path").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    match tokio::task::spawn_blocking(move || {
        crate::recovery::diff_text(&ws, origin, &baseline, path.as_deref())
    })
    .await
    {
        Ok(Ok(text)) => Ok(Json(json!({ "ok": true, "diff": text }))),
        Ok(Err(e)) => Err(err(e.to_string())),
        Err(e) => Err(err(e.to_string())),
    }
}

/// GET /api/git/status —— 工作区 git 概览（分支/HEAD/脏文件数/上游领先落后/未提交明细）。
/// 非 git 仓库返回 is_repo=false，前端隐藏该信息。
pub(super) async fn git_status(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let _ = &state;
    let ws = crate::tools::workspace_root().to_path_buf();
    match tokio::task::spawn_blocking(move || crate::recovery::status(&ws)).await {
        Ok(s) => (StatusCode::OK, Json(serde_json::to_value(s).unwrap_or_else(|_| json!({ "is_repo": false })))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// GET /api/git/info —— git 运行时状态（设置面板「插件」分区：可用性/版本/来源/内置包）。
pub(super) async fn git_runtime_info() -> Json<Value> {
    Json(crate::tools::git_info())
}

#[derive(Debug, Deserialize)]
pub struct GitCommitRequest {
    pub message: String,
    /// 要提交的路径；缺省/空数组 = 全部改动（add -A）
    #[serde(default)]
    pub files: Vec<String>,
}

/// POST /api/git/commit —— 用户手动提交：按选中路径 stage 后 commit，返回短哈希。
pub(super) async fn git_commit(
    State(state): State<AppState>,
    Json(req): Json<GitCommitRequest>,
) -> (StatusCode, Json<Value>) {
    let _ = &state;
    let ws = crate::tools::workspace_root().to_path_buf();
    let msg = req.message.clone();
    let files = req.files.clone();
    let res = tokio::task::spawn_blocking(move || crate::recovery::commit(&ws, &msg, &files)).await;
    match res {
        Ok(Ok(id)) => {
            observability::event(
                EventType::State,
                json!({ "kind": "git.committed", "id": id, "files": req.files.len(), "manual": true }),
            );
            (StatusCode::OK, Json(json!({ "ok": true, "id": id })))
        }
        // git 自己的报错（如「没有可提交的改动」）直接给用户看，比翻译成中文更有用
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// GET /api/checkpoints[?session=<id>] —— 影子仓库检查点列表（新→旧，最多 20 个）。
/// 带 session 时额外返回 `mine`：该会话自己的检查点（改动条基线优先取它）。
pub(super) async fn list_checkpoints(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let _ = &state;
    let want = q.get("session").map(|s| s.trim().to_string()).unwrap_or_default();
    let ws = crate::tools::workspace_root().to_path_buf();
    match tokio::task::spawn_blocking(move || crate::recovery::list(&ws, 20)).await {
        Ok(Ok(list)) => {
            let mine: Vec<_> = list.iter().filter(|c| !want.is_empty() && c.session == want).cloned().collect();
            (StatusCode::OK, Json(json!({ "checkpoints": list, "mine": mine })))
        }
        // 仓库还没有任何提交（git log fatal: does not have any commits yet）不是错误，
        // 是空态——空清单 + 人话备注，别 500 糊前端一脸
        Ok(Err(e)) => {
            let msg = e.to_string();
            if msg.contains("does not have any commits yet") || msg.contains("not a git repository") {
                return (StatusCode::OK, Json(json!({ "checkpoints": [], "mine": [], "note": "工作区 git 仓库还没有提交，暂无检查点" })));
            }
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": msg })))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// POST /api/checkpoints/restore —— 工作树回滚到指定检查点（shell 副作用不可回滚）。
pub(super) async fn restore_checkpoint(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let _ = &state;
    let Some(id) = req.get("id").and_then(Value::as_str) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 id" })));
    };
    let ws = crate::tools::workspace_root().to_path_buf();
    let id = id.to_string();
    match tokio::task::spawn_blocking(move || crate::recovery::restore(&ws, &id)).await {
        Ok(Ok(())) => {
            observability::event(
                EventType::State,
                json!({ "kind": "checkpoint.restored", "id": req.get("id").and_then(Value::as_str).unwrap_or_default() }),
            );
            (StatusCode::OK, Json(json!({ "ok": true })))
        }
        Ok(Err(e)) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}
