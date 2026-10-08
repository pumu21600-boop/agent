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
                // 2026-10-08 修：settings 写互斥（disabled_skills 读-改-写持锁，
                // 锁序：settings_write → settings RwLock）
                let _w = state.settings_write.lock().unwrap_or_else(|e| e.into_inner());
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
    /// 当前会话 id（宿主带入）：**时间线只显示当前对话**——这个参数就是过滤范围
    /// 本身（用户 2026-10-07 拍板）。空 = 没有当前会话（新对话还没落 id）→ 空时间线。
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

/// GET /api/trace?limit=N&session=<当前会话id> —— 最近 N 个任务的事件流（时间线面板数据源）。
/// **只展示当前对话**的任务（用户 2026-10-07 拍板，推翻 10-01 的「按工作区范围」）：
/// `session` 参数就是范围本身，严格只放行该会话（10-01 时它是工作区映射之外的
/// 兜底并集，现在当前会话即范围）。没有当前会话（新对话还没落 id）→ 空时间线：
/// 新对话本来没有任务，别把别的会话的任务漏进来。
/// 每个任务带 `session`/`session_title`（归属会话 id + 标题）。**只读解析** JSONL
/// （payload 原样透传，6 类 schema 不动）；连续 model_chunk 聚合；坏行跳过；
/// 文件不存在/为空返回空 tasks 而非报错。
pub(super) async fn get_trace(
    State(state): State<AppState>,
    Query(q): Query<TraceQuery>,
) -> (StatusCode, Json<Value>) {
    let Some(path) = observability::log_path() else {
        return (StatusCode::OK, Json(json!({ "tasks": [], "scope": "session", "note": "logger 未初始化" })));
    };
    let limit = q.limit.clamp(1, 50);
    // 严格当前会话范围；没有当前会话 = 新对话还没落 id → 空时间线（照实说，
    // 不漏别会话的任务进来）。标题查表顺带做：会话文件里拿标题给面板展示
    // （查不到 = 刚建还没落盘，面板显示「新会话」）。
    let allowed: std::collections::HashSet<String> = match q.session.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(sid) => std::iter::once(sid.to_string()).collect(),
        None => {
            return (StatusCode::OK, Json(json!({ "tasks": [], "scope": "session", "note": "当前对话还没有任务记录" })));
        }
    };
    let titles: std::collections::HashMap<String, String> = state
        .session
        .list()
        .iter()
        .map(|m| (m.id.clone(), m.title.clone()))
        .collect();
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
                        // 归属会话：id + 标题（标题查不到 = 刚建还没落盘的新对话，
                        // 面板显示「新会话」）
                        "session": t.session,
                        "session_title": titles.get(&t.session).cloned().unwrap_or_default(),
                        // 起点被日志轮转吃掉时为 true：start_ts 是近似值（兜底边界那条
                        // 事件的时刻），面板要照实说，别让用户以为那就是任务开始时间
                        "lost_start": t.lost_start,
                        "events": observability::trace::render_events(t),
                    })
                })
                .collect();
            (StatusCode::OK, Json(json!({ "tasks": list, "scope": "session" })))
        }
        Ok(Err(e)) => (StatusCode::OK, Json(json!({ "tasks": [], "note": format!("读取失败: {e}") }))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

/// GET /api/sandbox/selftest —— 跑快速探针（真实拉起受限进程），逐项返回结果。
/// 阻塞在 spawn_blocking 里做；kill-tree / 内存上限探针较慢，只在 cargo test 跑。
pub(super) async fn sandbox_selftest(State(state): State<AppState>) -> Json<Value> {
    let cfg = (*state.sandbox_cfg).clone();
    // 2026-10-08 修：JoinError（探针任务 panic）时 unwrap_or_default 给出空 Vec，
    // 而 `iter().all()` 空真算出 ok=true——界面显示「自检通过」而一个探针都没跑。
    // 探针必须诚实：跑不起来就是失败，原因照实带回。
    let results = match tokio::task::spawn_blocking(move || crate::sandbox::probes::run_fast_suite(&cfg)).await {
        Ok(r) => r,
        Err(e) => {
            observability::event(EventType::Error, json!({ "kind": "sandbox.selftest_panicked", "error": e.to_string() }));
            return Json(json!({
                "ok": false,
                "error": format!("自检任务崩溃（探针 panic）: {e}"),
                "results": []
            }));
        }
    };
    observability::event(
        EventType::State,
        json!({ "kind": "sandbox.selftest", "ok": results.iter().all(|r| r.ok), "count": results.len() }),
    );
    Json(json!({
        "ok": !results.is_empty() && results.iter().all(|r| r.ok),
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
