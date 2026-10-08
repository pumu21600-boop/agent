//! 系统约定读写 + 记忆库/语义检索端点（自 server.rs 按域拆出）

use super::*;
use super::chat::find_channel;

/// 系统约定（prompts/system.md）当前内容：每轮任务实时读盘，保存即生效；
/// 文件缺失/读不到时回落到启动时载入的提示词。
pub(super) fn read_system_prompt(state: &AppState) -> String {
    std::fs::read_to_string(&*state.prompt_path).unwrap_or_else(|_| (*state.system_prompt).clone())
}

/// 保存系统约定：写 prompts/system.md（目录不存在则建）。
pub(super) fn save_system_prompt(state: &AppState, content: &str) -> anyhow::Result<PathBuf> {
    let path = (*state.prompt_path).clone();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::fsx::write_atomic_str(&path, content)?;
    Ok(path)
}

/// GET /api/memory —— 记忆层状态 + 三类约定文件原文（编辑器数据源，每次调用读盘 = 显示文件实时内容）。
pub(super) async fn get_memory(State(state): State<AppState>) -> Json<Value> {
    let mem = state.memory.read().unwrap_or_else(|e| e.into_inner()).clone();
    let projects_registered = mem.load_index().projects.len();
    Json(json!({
        "enabled": mem.enabled,
        "global_dir": mem.global_dir().display().to_string(),
        "project_dir": mem.project_dir().display().to_string(),
        "events_lines": mem.events_lines(),
        "projects_registered": projects_registered,
        "system_path": state.prompt_path.display().to_string(),
        "global_agents_path": mem.global_agents_path().display().to_string(),
        "project_agents_path": mem.project_agents_path().display().to_string(),
        "system_prompt": read_system_prompt(&state),
        "agents_global": mem.agents_raw("global"),
        "agents_project": mem.agents_raw("project"),
    }))
}

#[derive(Debug, Deserialize)]
pub struct SaveAgentsRequest {
    /// "system"（prompts/system.md）| "global"（~/.agent/AGENTS.md）| "project"（工作区根 AGENTS.md）
    pub scope: String,
    pub content: String,
}

/// POST /api/memory/agents —— 保存用户编辑的约定文件（system 走实时读盘，保存即生效；
/// 两层 AGENTS.md 下次任务注入自动生效）。
pub(super) async fn save_memory_agents(
    State(state): State<AppState>,
    Json(req): Json<SaveAgentsRequest>,
) -> (StatusCode, Json<Value>) {
    if !matches!(req.scope.as_str(), "system" | "global" | "project") {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "scope 须为 system / global / project" })));
    }
    let mem = state.memory.read().unwrap_or_else(|e| e.into_inner()).clone();
    let saved = if req.scope == "system" {
        save_system_prompt(&state, &req.content)
    } else {
        mem.save_agents(&req.scope, &req.content)
    };
    match saved {
        Ok(path) => {
            observability::event(
                EventType::State,
                json!({ "kind": "memory.agents_saved", "scope": req.scope, "path": path.display().to_string() }),
            );
            (StatusCode::OK, Json(json!({ "ok": true, "path": path.display().to_string() })))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() }))),
    }
}

// —— 记忆库（星云视图）：会话级记忆索引 + 向量检索 ——

/// 解析 EMBEDDING 角色渠道（B3 顺序：settings.roles.embedding > config.toml [roles].embedding）。
/// 嵌入**不走主渠道兜底**：主模型多半没有 /embeddings 端点，未配置时用本地哈希向量更可靠。
fn resolve_embedder<'a>(state: &'a AppState, s: &Settings) -> Option<crate::memory::library::Embedder<'a>> {
    // features.roles=false（「功能桥接」关）= 不用专家嵌入渠道，直接本地哈希向量
    if !s.features.roles {
        return None;
    }
    let name = s
        .roles
        .as_ref()
        .and_then(|r| r.embedding.as_deref())
        .or_else(|| state.roles_cfg.embedding.as_deref())?;
    let ch = find_channel(s, name)?;
    if ch.api_key.trim().is_empty() {
        return None;
    }
    Some(crate::memory::library::Embedder {
        gw: &state.gateway,
        base_url: ch.base_url,
        model: ch.model,
        api_key: ch.api_key,
    })
}

/// 增量同步索引 → 记忆库 JSON（光点列表 + 项目聚合 + 索引状态）。
pub(crate) async fn build_library(state: &AppState, force: bool) -> Value {
    use crate::memory::library as lb;
    let settings = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    let embedder = resolve_embedder(state, &settings);
    let report = lb::sync(&state.session, &state.log_dir, embedder, force).await;
    let lib = lb::load(&state.log_dir);
    let items: Vec<Value> = lib
        .entries
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "title": e.title,
                "project": e.project,
                "workspace": e.workspace,
                "created_at": e.created_at,
                "updated_at": e.updated_at,
                "count": e.count,
                "chars": e.chars,
                "pinned": e.pinned,
                "archived": e.archived,
                "snippet": e.snippet,
                "indexed": !e.vec.is_empty(),
            })
        })
        .collect();
    let mut proj: std::collections::BTreeMap<String, (String, usize)> = std::collections::BTreeMap::new();
    for e in &lib.entries {
        let slot = proj.entry(e.project.clone()).or_insert_with(|| (e.workspace.clone(), 0));
        slot.1 += 1;
    }
    let projects: Vec<Value> = proj
        .iter()
        .map(|(name, (ws, n))| json!({ "name": name, "workspace": ws, "count": n }))
        .collect();
    let indexed = lib.entries.iter().filter(|e| !e.vec.is_empty()).count();
    let mem = state.memory.read().unwrap_or_else(|e| e.into_inner()).clone();
    json!({
        "items": items,
        "projects": projects,
        "stats": {
            "sessions": lib.entries.len(),
            "projects": projects.len(),
            "indexed": indexed,
            "pending": lib.entries.len() - indexed,
            "model": lib.model,
            "mode": report.mode,
        },
        "sync": report,
        "events_lines": mem.events_lines(),
        "notice": report.error,
    })
}

/// GET /api/memory/library —— 记忆库光点数据（每次调用做一次有界增量同步）。
pub(super) async fn get_memory_library(State(state): State<AppState>) -> Json<Value> {
    Json(build_library(&state, false).await)
}

/// 定时任务「记忆提醒」到点前的记忆库同步（有界增量；配了嵌入渠道就走语义向量，
/// 没配走本地词法——与记忆库面板同一路径，只是没人看着时也要保新鲜）。
pub(crate) async fn sync_library_for_schedule(state: &AppState) {
    let settings = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    let embedder = resolve_embedder(state, &settings);
    let _ = crate::memory::library::sync(&state.session, &state.log_dir, embedder, false).await;
}

/// POST /api/memory/library/reindex —— 强制重建向量（忽略指纹），返回同款光点数据。
pub(super) async fn reindex_memory_library(State(state): State<AppState>) -> Json<Value> {
    observability::event(EventType::State, json!({ "kind": "memory.reindex" }));
    Json(build_library(&state, true).await)
}

#[derive(Debug, Deserialize)]
pub struct MemorySearchRequest {
    #[serde(default)]
    pub query: String,
    /// 「相似会话」：以该会话为源找相近的其它会话（用索引里已存的向量，不调外部渠道）
    #[serde(default)]
    pub like: Option<String>,
    #[serde(default)]
    pub top_k: Option<usize>,
    /// 匹配下限（0..1，越小越宽）；搜索缺省 0.18、相似缺省 0.3
    #[serde(default)]
    pub min_score: Option<f32>,
}

/// POST /api/memory/search —— 向量检索（混合排序），返回命中的会话 id 供光点过滤。
pub(super) async fn search_memory(State(state): State<AppState>, Json(req): Json<MemorySearchRequest>) -> Json<Value> {
    use crate::memory::library as lb;
    let lib = lb::load(&state.log_dir);
    let top_k = req.top_k.unwrap_or(40).clamp(1, 200);
    if let Some(id) = req.like.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let results = lb::similar(&lib, id, top_k, req.min_score.unwrap_or(0.3).clamp(0.0, 1.0));
        observability::event(EventType::State, json!({ "kind": "memory.similar", "id": id, "hits": results.len() }));
        return Json(json!({
            "results": results,
            "total": lib.entries.len(),
            "mode": "similar",
            "model": lib.model,
            "notice": Value::Null,
        }));
    }
    let query = req.query.trim().to_string();
    if query.is_empty() {
        return Json(json!({ "results": [], "total": 0, "mode": "none", "notice": "查询为空" }));
    }
    let min_score = req.min_score.unwrap_or(0.18).clamp(0.0, 1.0);
    // 查询向量必须与索引同口径：本地索引直接本地算；外部索引调嵌入席位，
    // 失败/未配置则退化为纯词法（qvec 为空，search 内部自动降级）。
    let mut notice: Option<String> = None;
    let qvec = if lib.model.is_empty() || lib.model == lb::LOCAL_MODEL {
        lb::local_vec(&query)
    } else {
        let settings = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
        match resolve_embedder(&state, &settings) {
            Some(emb) => {
                match emb.gw.embed(&emb.base_url, &emb.model, &emb.api_key, std::slice::from_ref(&query)).await {
                    Ok(vs) if !vs.is_empty() => lb::norm_vec(&vs[0]),
                    Ok(_) => {
                        notice = Some("嵌入渠道未返回向量，本次只用关键词匹配".into());
                        Vec::new()
                    }
                    Err(e) => {
                        notice = Some(format!("嵌入渠道不可用（{e}），本次只用关键词匹配"));
                        Vec::new()
                    }
                }
            }
            None => {
                notice = Some("嵌入席位未配置或 key 缺失，本次只用关键词匹配".into());
                Vec::new()
            }
        }
    };
    let results = lb::search(&lib, &qvec, &query, top_k, min_score);
    let mode = if qvec.is_empty() {
        "lexical"
    } else if lib.model == lb::LOCAL_MODEL {
        "local"
    } else {
        "external"
    };
    observability::event(
        EventType::State,
        json!({ "kind": "memory.search", "query": query, "hits": results.len(), "mode": mode }),
    );
    Json(json!({
        "results": results,
        "total": lib.entries.len(),
        "mode": mode,
        "model": lib.model,
        "notice": notice,
    }))
}

// —— （施工单 016）：MCP 插件 ——
