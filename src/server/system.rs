//! 技能/版本/trace/沙箱自检等杂项端点（P19 自 server.rs 按域拆出）

use super::*;

/// GET /api/skills —— 技能列表（项目级 + 全局，含全文供编辑器回显）
pub(super) async fn list_skills() -> Json<Value> {
    Json(json!({ "skills": crate::skills::list_ui() })) // list_ui：带启用位（设置页行内 switch）
}

#[derive(Deserialize)]
pub struct SkillReq {
    /// "save" | "delete" | "read" | "toggle"
    pub action: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub content: Option<String>,
    /// toggle：目标启用位（true=启用 / false=停用）
    pub enabled: Option<bool>,
}

/// POST /api/skills —— 技能保存 / 删除 / 读取 / 启停
pub(super) async fn post_skills(
    State(state): State<AppState>,
    Json(req): Json<SkillReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
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
        // 启停：写 settings.disabled_skills 并即时重灌进程内名单（索引/菜单下一轮即生效）
        "toggle" => {
            let name = req.name.clone().unwrap_or_default();
            let enabled = req.enabled.unwrap_or(true);
            if name.trim().is_empty() {
                return Err(err("缺少技能名".into()));
            }
            let now = {
                let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner());
                s.disabled_skills.retain(|n| n != &name);
                if !enabled {
                    s.disabled_skills.push(name.clone());
                }
                if let Err(e) = s.save() {
                    return Err(err(e.to_string()));
                }
                s.disabled_skills.clone()
            };
            crate::skills::set_disabled(&now);
            observability::event(
                EventType::State,
                json!({ "kind": "skill.toggled", "skill": name, "enabled": enabled }),
            );
            Ok(Json(json!({ "ok": true, "enabled": enabled })))
        }
        other => Err(err(format!("未知 action: {other}"))),
    }
}

#[derive(Debug, Deserialize)]
pub struct TraceQuery {
    /// 最近任务数（1–50，默认 5）
    #[serde(default = "default_trace_limit")]
    pub limit: usize,
    /// 当前会话 id（宿主带入兜底）：刚建的会话可能还没落盘，按工作区映射会漏掉它
    #[serde(default)]
    pub session: Option<String>,
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
/// **只展示当前工作区**的任务（用户 2026-10-01 要求）：日志行只有 session_id，
/// 归属靠会话文件里的 workspace 映射；当前会话可能还没落盘，`session` 参数兜底。
/// 每个任务带 `session`/`session_title`（归属会话 id + 标题）：面板要能看出
/// 「这确实是整个工作区的任务、不是当前会话一个的」（用户 2026-10-07 实锤看不出来）。
/// **只读解析** JSONL（payload 原样透传，6 类 schema 不动）；连续 model_chunk 聚合；
/// 坏行跳过；文件不存在/为空返回空 tasks 而非报错。
pub(super) async fn get_trace(
    State(state): State<AppState>,
    Query(q): Query<TraceQuery>,
) -> (StatusCode, Json<Value>) {
    let Some(path) = observability::log_path() else {
        return (StatusCode::OK, Json(json!({ "tasks": [], "note": "logger 未初始化" })));
    };
    let limit = q.limit.clamp(1, 50);
    let cur_ws = crate::tools::workspace_root().display().to_string();
    // 工作区 → 会话映射一张表两用：既算过滤集，也当 id→标题 查表回给面板
    // （用户 2026-10-07 实锤「时间线像只加载当前会话」：任务行看不出归属会话，
    // 单会话内容就像被按会话过滤了。响应带上 session/session_title，范围可见）。
    // 标题口径与侧栏一致（list() 已做「新会话」占位的首条用户消息回落）。
    let titles: std::collections::HashMap<String, String> = state
        .session
        .list()
        .iter()
        .filter(|m| super::workspace::same_workspace(&m.workspace, &cur_ws))
        .map(|m| (m.id.clone(), m.title.clone()))
        .collect();
    let mut allowed: std::collections::HashSet<String> = titles.keys().cloned().collect();
    if let Some(sid) = q.session.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        allowed.insert(sid.to_string());
    }
    // 日志按体积轮转（<file>.1 最新 … <file>.N 最旧）：长对话的 task.start 落在归档、
    // 后续事件落在当前文件是真机常态——只读当前文件会把跨文件任务撕成孤儿事件
    // （时间线「怎么都刷不出来」的实锤根因）。按时间正序（最旧在前）喂给解析器，
    // 归属状态跨文件延续；上限探 32 份归档，缺的跳过。
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for i in (1..=32usize).rev() {
        let rot = std::path::PathBuf::from(format!("{}.{}", path.display(), i));
        if rot.is_file() {
            files.push(rot);
        }
    }
    files.push(path.to_path_buf());
    let res = tokio::task::spawn_blocking(move || {
        observability::read_tasks_scoped_multi(files.iter().map(|p| p.as_path()), limit, Some(&allowed))
    })
    .await;
    match res {
        Ok(Ok(tasks)) => {
            let list: Vec<Value> = tasks
                .iter()
                .map(|t| {
                    json!({
                        "start_ts": t.start_ts,
                        "end_ts": t.end_ts,
                        "duration_ms": t.duration_ms(),
                        // 归属会话：id + 标题（标题查不到 = 刚建还没落盘的兜底会话，
                        // 前端显示「新会话」）；工作区范围的任务都带得出归属
                        "session": t.session,
                        "session_title": titles.get(&t.session).cloned().unwrap_or_default(),
                        // 起点被日志轮转吃掉时为 true：start_ts 是近似值（兜底边界那条
                        // 事件的时刻），面板要照实说，别让用户以为那就是任务开始时间
                        "lost_start": t.lost_start,
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

#[derive(Debug, Deserialize)]
pub struct SandboxInfoQuery {
    /// 1=强制重探（UI「重新检测」）；默认取缓存（进程内只探一次）
    #[serde(default)]
    pub refresh: u8,
}

/// GET /api/sandbox/info —— 当前沙箱档位（P20 沙箱统一：档位探测链 + 生效档缓存）。
/// 返回配置档位、生效档位（含 AppContainer→受限 自动回退与原因）、隔离面说明、探测链明细。
pub(super) async fn sandbox_info(
    State(state): State<AppState>,
    Query(q): Query<SandboxInfoQuery>,
) -> Json<Value> {
    let cfg = (*state.sandbox_cfg).clone();
    let allow_unsandboxed = state.agent_cfg.allow_unsandboxed;
    let refresh = q.refresh == 1;
    let report = tokio::task::spawn_blocking(move || {
        crate::sandbox::probes::cached_tier_report(&cfg, false, refresh)
    })
    .await
    .unwrap_or_else(|e| crate::sandbox::probes::TierReport {
        requested: "unknown".into(),
        effective: "restricted".into(),
        label: "探测失败".into(),
        describe: format!("档位探测任务失败: {e}"),
        fallback_reason: None,
        probes: vec![],
        probed_at_ms: 0,
    });
    let mut v = report.to_json();
    v["allow_unsandboxed"] = json!(allow_unsandboxed);
    v["mode_configured"] = json!(report.requested);
    Json(v)
}
