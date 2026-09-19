//! 技能/版本/trace/沙箱自检等杂项端点（P19 自 server.rs 按域拆出）

use super::*;

/// GET /api/skills —— 技能列表（项目级 + 全局，含全文供编辑器回显）
pub(super) async fn list_skills() -> Json<Value> {
    Json(json!({ "skills": crate::skills::list() }))
}

#[derive(Deserialize)]
pub struct SkillReq {
    /// "save" | "delete" | "read"
    pub action: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub content: Option<String>,
}

/// POST /api/skills —— 技能保存 / 删除 / 读取
pub(super) async fn post_skills(Json(req): Json<SkillReq>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let err = |e: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": e })));
    match req.action.as_str() {
        "save" => {
            let name = req.name.clone().unwrap_or_default();
            match crate::skills::save(&name, req.description.as_deref().unwrap_or(""), req.content.as_deref().unwrap_or(""))
            {
                Ok(dir) => Ok(Json(json!({ "ok": true, "dir": dir }))),
                Err(e) => Err(err(e)),
            }
        }
        "delete" => {
            let name = req.name.clone().unwrap_or_default();
            match crate::skills::delete(&name) {
                Ok(()) => Ok(Json(json!({ "ok": true }))),
                Err(e) => Err(err(e)),
            }
        }
        "read" => {
            let name = req.name.clone().unwrap_or_default();
            match crate::skills::read(&name) {
                Ok(content) => Ok(Json(json!({ "ok": true, "content": content }))),
                Err(e) => Err(err(e)),
            }
        }
        other => Err(err(format!("未知 action: {other}"))),
    }
}

#[derive(Debug, Deserialize)]
pub struct TraceQuery {
    /// 最近任务数（1–50，默认 5）
    #[serde(default = "default_trace_limit")]
    pub limit: usize,
}

fn default_trace_limit() -> usize {
    5
}

// —— P10.5 设置面板「记忆」分区 ——

/// GET /api/version —— 版本与运行形态（界面「关于」卡片用）。
/// root 是**运行时认定的应用根**（打包形态=exe 同级，开发形态=源码目录），
/// 是排查「配置/扩展/前端产物读到哪一份」的第一手线索。
pub(super) async fn get_version() -> Json<Value> {
    Json(json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "root": crate::config::app_root().display().to_string(),
        "exe": std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
    }))
}

/// GET /api/trace?limit=N —— 最近 N 个任务的事件流（时间线面板数据源）。
/// **只读解析** JSONL（payload 原样透传，6 类 schema 不动）；连续 model_chunk 聚合；
/// 坏行跳过；文件不存在/为空返回空 tasks 而非报错。
pub(super) async fn get_trace(Query(q): Query<TraceQuery>) -> (StatusCode, Json<Value>) {
    let Some(path) = observability::log_path() else {
        return (StatusCode::OK, Json(json!({ "tasks": [], "note": "logger 未初始化" })));
    };
    let limit = q.limit.clamp(1, 50);
    let path = path.to_path_buf();
    let res = tokio::task::spawn_blocking(move || observability::read_tasks(&path, limit)).await;
    match res {
        Ok(Ok(tasks)) => {
            let list: Vec<Value> = tasks
                .iter()
                .map(|t| {
                    json!({
                        "start_ts": t.start_ts,
                        "end_ts": t.end_ts,
                        "duration_ms": t.duration_ms(),
                        "events": observability::trace::render_events(t),
                    })
                })
                .collect();
            (StatusCode::OK, Json(json!({ "tasks": list })))
        }
        Ok(Err(e)) => (StatusCode::OK, Json(json!({ "tasks": [], "note": format!("读取失败: {e}") }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// GET /api/sandbox/selftest —— 跑快速探针（真实拉起受限进程），逐项返回结果。
/// 阻塞在 spawn_blocking 里做；kill-tree / 内存上限探针较慢，只在 cargo test 跑。
pub(super) async fn sandbox_selftest(State(state): State<AppState>) -> Json<Value> {
    let cfg = (*state.sandbox_cfg).clone();
    let results = tokio::task::spawn_blocking(move || crate::sandbox::probes::run_fast_suite(&cfg))
        .await
        .unwrap_or_default();
    observability::event(
        EventType::State,
        json!({ "kind": "sandbox.selftest", "ok": results.iter().all(|r| r.ok) }),
    );
    Json(json!({
        "ok": results.iter().all(|r| r.ok),
        "results": results
            .iter()
            .map(|r| json!({ "name": r.name, "ok": r.ok, "detail": r.detail }))
            .collect::<Vec<Value>>(),
    }))
}
