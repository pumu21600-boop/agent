//! MCP 接入 + 插件清单/draft 隔离区/命名空间存储/静态资源服务（P19 自 server.rs 按域拆出）

use super::*;

/// GET /api/mcp —— server 列表 + WASM 插件（配置/在线状态/工具清单）。
pub(super) async fn get_mcp(State(state): State<AppState>) -> Json<Value> {
    get_mcp_inner(&state)
}

#[derive(Debug, Deserialize)]
pub struct McpActionRequest {
    /// save（新增/更新）| remove | confirm（首装确认）| refresh（重连发现）| toggle（启停）
    pub action: String,
    #[serde(default)]
    pub server: Option<Value>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub risk: Option<String>,
}

/// POST /api/mcp —— 配置管理与生命周期动作（全部落 settings.json 并同步管理器）。
pub(super) async fn post_mcp(
    State(state): State<AppState>,
    Json(req): Json<McpActionRequest>,
) -> (StatusCode, Json<Value>) {
    let mut s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    let err = |m: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": m })));
    match req.action.as_str() {
        "save" => {
            let Some(srv) = req.server.clone() else {
                return err("缺少 server 配置".into());
            };
            let cfg: crate::config::settings::McpServerCfg = match serde_json::from_value(srv) {
                Ok(c) => c,
                Err(e) => return err(format!("server 配置不合法: {e}")),
            };
            if !crate::plugin::mcp::valid_name(&cfg.name) {
                return err("名称非法（非空、仅字母数字-_、禁止包含 __）".into());
            }
            // transport 合法性 + 该 transport 的必需字段（早失败好过调用时才报错）
            match cfg.transport.as_str() {
                "http" | "HTTP" | "Http" | "sse" | "SSE" | "Sse" => {
                    if !cfg.url.trim().starts_with("http://") && !cfg.url.trim().starts_with("https://") {
                        return err("http/sse transport 需要 http(s):// 开头的 url".into());
                    }
                }
                "stdio" | "STDIO" | "Stdio" => {
                    if cfg.command.trim().is_empty() {
                        return err("stdio transport 需要 command".into());
                    }
                }
                other => return err(format!("未知 transport: {other}（stdio | http | sse）")),
            }
            s.mcp_servers.retain(|c| c.name != cfg.name);
            s.mcp_servers.push(cfg);
        }
        "remove" => {
            let Some(name) = &req.name else { return err("缺少 name".into()) };
            s.mcp_servers.retain(|c| &c.name != name);
        }
        "confirm" => {
            let Some(name) = &req.name else { return err("缺少 name".into()) };
            let Some(c) = s.mcp_servers.iter_mut().find(|c| &c.name == name) else {
                return err(format!("未知 server: {name}"));
            };
            c.confirmed = true;
            if let Some(r) = &req.risk
                && (r == "dangerous" || r == "normal") {
                    c.risk = r.clone();
                }
        }
        "toggle" => {
            let Some(name) = &req.name else { return err("缺少 name".into()) };
            let Some(c) = s.mcp_servers.iter_mut().find(|c| &c.name == name) else {
                return err(format!("未知 server: {name}"));
            };
            c.enabled = !c.enabled;
        }
        "refresh" => {
            let Some(name) = &req.name else { return err("缺少 name".into()) };
            if !s.mcp_servers.iter().any(|c| &c.name == name) {
                return err(format!("未知 server: {name}"));
            }
            // 只重连+重新发现；配置不动（ensure_online 用管理器内已同步的 cfg）
        }
        // P24 OAuth：发起授权（后台线程跑 discovery→授权→换 token，立即返回快照）
        "oauth_authorize" => {
            let Some(name) = &req.name else { return err("缺少 name".into()) };
            let Some(c) = s.mcp_servers.iter().find(|c| &c.name == name) else {
                return err(format!("未知 server: {name}"));
            };
            match crate::plugin::mcp_oauth::start_authorize(name, c) {
                Ok(snap) => return (StatusCode::OK, Json(json!({ "ok": true, "tx": snap }))),
                Err(e) => return err(e),
            }
        }
        // P24 OAuth：撤销授权（删 token + 强制下线；下次调用回「未授权」人话）
        "oauth_revoke" => {
            let Some(name) = &req.name else { return err("缺少 name".into()) };
            if !s.mcp_servers.iter().any(|c| &c.name == name) {
                return err(format!("未知 server: {name}"));
            }
            crate::plugin::mcp_oauth::revoke(name);
            crate::plugin::mcp::force_offline(name);
            observability::event(
                observability::EventType::State,
                json!({ "kind": "mcp.oauth.revoked", "server": name }),
            );
        }
        other => return err(format!("未知操作: {other}")),
    }

    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    // P18 存储一门/契约（027）：settings 变更即同步停用名单到工具插件（拒新调用）
    crate::plugin::wasm::set_disabled(&s.disabled_extensions);
    crate::plugin::mcp::sync_configs(&s.mcp_servers);

    // refresh：同步后拉起并重新发现工具（阻塞 IO 丢 blocking 池）
    crate::plugin::wasm::rescan(); // 017：顺带重扫 WASM 插件
    if req.action == "refresh" {
        let name = req.name.clone().unwrap_or_default();
        let r = tokio::task::spawn_blocking(move || crate::plugin::mcp::ensure_online_by_name(&name)).await;
        match r {
            Ok(Ok(n)) => {
                return (StatusCode::OK, Json(json!({ "ok": true, "tools_found": n })));
            }
            Ok(Err(e)) => {
                return (StatusCode::OK, Json(json!({ "ok": false, "error": e })));
            }
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
            }
        }
    }

    (StatusCode::OK, get_mcp_inner(&state))
}

fn get_mcp_inner(state: &AppState) -> Json<Value> {
    let wasm_list: Vec<Value> = crate::plugin::wasm::status()
        .into_iter()
        .map(|(stem, ok, tools, err)| {
            json!({
                "stem": stem, "ok": ok, "load_error": err,
                "tools": tools.iter().map(|(n, d)| json!({"name": n, "desc": d})).collect::<Vec<_>>(),
            })
        })
        .collect();
    let s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    let status: std::collections::HashMap<String, (bool, Vec<(String, String)>)> =
        crate::plugin::mcp::status()
            .into_iter()
            .map(|(n, o, t)| (n, (o, t)))
            .collect();
    // P17：resources / prompts 清单（声明了能力的 server 才有内容）
    let catalogs = crate::plugin::mcp::catalogs();
    let servers: Vec<Value> = s
        .mcp_servers
        .iter()
        .map(|c| {
            let (online, tools) = status
                .get(&c.name)
                .cloned()
                .unwrap_or((false, Vec::new()));
            let (res, prompts) = catalogs.get(&c.name).cloned().unwrap_or_default();
            json!({
                "name": c.name, "transport": c.transport, "command": c.command, "args": c.args,
                "env": c.env, "url": c.url, "headers": c.headers,
                "enabled": c.enabled, "confirmed": c.confirmed, "risk": c.risk,
                "online": online,
                "tools": tools.iter().map(|(n, d)| json!({"name": n, "desc": d})).collect::<Vec<_>>(),
                "resources": res,
                "prompts": prompts,
                // P24：OAuth 配置 + 授权状态（tx=事务快照，oauth=token 态）
                "oauth": c.oauth,
                "oauth_status": crate::plugin::mcp_oauth::tx_status(&c.name),
            })
        })
        .collect();
    Json(json!({ "servers": servers, "wasm": wasm_list }))
}

/// GET /api/ext —— 组件总线 v0：UI 扩展清单（应用根内置 + 当前工作区自带）。
pub(super) async fn list_extensions(State(state): State<AppState>) -> Json<Value> {
    let disabled = state
        .settings
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .disabled_extensions
        .clone();
    // P18 二合一（027）：面板插件 + 工具插件（wasm 桥）+ 内存 draft，一份清单
    let mut extensions = crate::plugin::ui::scan(&disabled);
    extensions.extend(crate::plugin::wasm::scan_plugins(&disabled));
    Json(json!({
        "extensions": extensions,
        "dir": crate::plugin::ui::DIR,
        "app_dir": crate::config::app_root().join(crate::plugin::ui::DIR).display().to_string(),
        "workspace_dir": crate::tools::workspace_root().join(crate::plugin::ui::DIR).display().to_string(),
    }))
}

/// POST /api/ext/toggle —— 启用/停用扩展（P17，落 settings.json 的停用名单）。
pub(super) async fn toggle_extension(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let name = req.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 name" })));
    }
    let enabled = req.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner()).clone();
    s.disabled_extensions.retain(|d| d != &name);
    if !enabled {
        s.disabled_extensions.push(name.clone());
    }
    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    // P18 存储一门/契约（027）：settings 变更即同步停用名单到工具插件（拒新调用）
    crate::plugin::wasm::set_disabled(&s.disabled_extensions);
    observability::event(
        EventType::State,
        json!({ "kind": "ext.toggle", "name": name, "enabled": enabled }),
    );
    // P18 二合一：回包清单含工具插件
    let mut list = crate::plugin::ui::scan(&s.disabled_extensions);
    list.extend(crate::plugin::wasm::scan_plugins(&s.disabled_extensions));
    (StatusCode::OK, Json(json!({ "ok": true, "extensions": list })))
}

/// POST /api/ext/draft —— 模型自产插件提交进 draft 隔离区（P18 027）。
/// 规范化（source=model_generated、权限清空）在 draft::submit 里强制执行。
pub(super) async fn submit_draft(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let name = req.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let manifest = req.get("manifest").cloned().unwrap_or(json!({}));
    let mut files = std::collections::BTreeMap::new();
    if let Some(obj) = req.get("files").and_then(Value::as_object) {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                files.insert(k.clone(), s.to_string());
            }
        }
    }
    match crate::plugin::draft::submit(&name, &manifest, &files) {
        Ok(()) => {
            let disabled = state
                .settings
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .disabled_extensions
                .clone();
            let mut list = crate::plugin::ui::scan(&disabled);
            list.extend(crate::plugin::wasm::scan_plugins(&disabled));
            (StatusCode::OK, Json(json!({ "ok": true, "extensions": list })))
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
    }
}

/// POST /api/ext/promote —— draft 一键转正（落盘持久化、来源改 user、重扫注册表）。
/// P21 红线3：转正是用户显式动作 = 信任决定，落盘后**自动 pin** 内容指纹（不 pin 就会被
/// 自己的验签门拦下，转正等于白做）。
pub(super) async fn promote_draft(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let name = req.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    match crate::plugin::draft::promote(&name) {
        Ok(paths) => {
            // 自动 pin：从落盘的 manifest 路径推导插件目录与 digest
            let pins = crate::plugin::trust::pin_from_manifest_paths(&paths);
            if !pins.is_empty() {
                let mut s = state
                    .settings
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                for (k, d) in &pins {
                    s.trusted_plugins.insert(k.clone(), d.clone());
                    crate::plugin::trust::audit_pinned(k, d, "promote");
                }
                if let Err(e) = s.save() {
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
                }
                crate::plugin::trust::install(&s.trusted_plugins);
                *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
            }
            let disabled = state
                .settings
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .disabled_extensions
                .clone();
            let mut list = crate::plugin::ui::scan(&disabled);
            list.extend(crate::plugin::wasm::scan_plugins(&disabled));
            (StatusCode::OK, Json(json!({ "ok": true, "written": paths, "pinned": pins.len(), "extensions": list })))
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
    }
}

/// POST /api/ext/trust {key, action} —— 第三方插件验签的 pin / unpin（P21 红线3）。
/// key 形如 `ui-ext:<name>` / `plugins:<name>`；pin 时按当前位置重算 digest。
pub(super) async fn trust_plugin(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let key = req.get("key").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let action = req.get("action").and_then(Value::as_str).unwrap_or("pin").trim().to_string();
    if key.is_empty() || !key.contains(':') {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 key（形如 ui-ext:<name>）" })));
    }
    let err = |m: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": m })));
    let mut s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    match action.as_str() {
        "pin" => {
            let Some(path) = crate::plugin::trust::resolve(&key) else {
                return err(format!("找不到插件 {key}（目录不存在或名字非法）"));
            };
            let digest = match crate::plugin::trust::digest_of_path(&path) {
                Ok(d) => d,
                Err(e) => return err(e),
            };
            s.trusted_plugins.insert(key.clone(), digest.clone());
            crate::plugin::trust::audit_pinned(&key, &digest, "user");
        }
        "unpin" => {
            s.trusted_plugins.remove(&key);
            observability::event(
                EventType::State,
                json!({ "kind": "plugin.trust_unpinned", "key": key, "by": "user" }),
            );
        }
        other => return err(format!("未知操作: {other}（pin | unpin）")),
    }
    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    crate::plugin::trust::install(&s.trusted_plugins);
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    // pin/unpin 会改变工具插件的可加载性 → 重扫注册表，清单与工具表同步
    crate::plugin::wasm::rescan();
    let disabled = s.disabled_extensions.clone();
    let mut list = crate::plugin::ui::scan(&disabled);
    list.extend(crate::plugin::wasm::scan_plugins(&disabled));
    (StatusCode::OK, Json(json!({ "ok": true, "extensions": list })))
}

/// DELETE /api/ext/draft?name= —— 丢弃 draft（内存即清，WASM 注册表同步回收）。
pub(super) async fn discard_draft(
    State(state): State<AppState>,
    Query(q): Query<PluginDataQueryName>,
) -> (StatusCode, Json<Value>) {
    if crate::plugin::draft::discard(&q.name) {
        let disabled = state
            .settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .disabled_extensions
            .clone();
        let mut list = crate::plugin::ui::scan(&disabled);
        list.extend(crate::plugin::wasm::scan_plugins(&disabled));
        (StatusCode::OK, Json(json!({ "ok": true, "extensions": list })))
    } else {
        (StatusCode::NOT_FOUND, Json(json!({ "error": format!("没有名为 {} 的 draft", q.name) })))
    }
}

#[derive(Deserialize)]
pub(super) struct PluginDataQueryName {
    name: String,
}

#[derive(Deserialize)]
pub(super) struct PluginDataQuery {
    name: String,
    key: String,
}

/// GET /api/plugin/data?name=&key= —— 插件命名空间读（存储一门的插件侧 API）。
pub(super) async fn read_plugin_data(Query(q): Query<PluginDataQuery>) -> Response {
    match crate::storage::plugin_read(&q.name, &q.key) {
        Ok(Some(v)) => (StatusCode::OK, Json(json!({ "ok": true, "value": v }))).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "键不存在").into_response(),
        Err(e) => (StatusCode::FORBIDDEN, e).into_response(),
    }
}

/// PUT /api/plugin/data {name, key, value} —— 插件命名空间写（JSON ≤64KB；越权/超限审计拒绝）。
pub(super) async fn write_plugin_data(Json(req): Json<Value>) -> (StatusCode, Json<Value>) {
    let name = req.get("name").and_then(Value::as_str).unwrap_or("");
    let key = req.get("key").and_then(Value::as_str).unwrap_or("");
    let Some(value) = req.get("value") else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 value" })));
    };
    match crate::storage::plugin_write(name, key, value) {
        Ok(bytes) => (StatusCode::OK, Json(json!({ "ok": true, "bytes": bytes }))),
        Err(e) => (StatusCode::FORBIDDEN, Json(json!({ "error": e }))),
    }
}

/// DELETE /api/plugin/data?name=&key= —— 插件命名空间删。
pub(super) async fn delete_plugin_data(Query(q): Query<PluginDataQuery>) -> (StatusCode, Json<Value>) {
    match crate::storage::plugin_delete(&q.name, &q.key) {
        Ok(found) => (StatusCode::OK, Json(json!({ "ok": true, "found": found }))),
        Err(e) => (StatusCode::FORBIDDEN, Json(json!({ "error": e }))),
    }
}

/// GET /ui-ext/{name} —— 扩展入口页（entry 由 manifest 指定）。
pub(super) async fn ui_ext_entry(State(state): State<AppState>, AxumPath(name): AxumPath<String>) -> Response {
    serve_ext(&state, &name, "")
}

/// GET /ui-ext/{name}/{*path} —— 扩展目录内的静态资源。
pub(super) async fn ui_ext_asset(
    State(state): State<AppState>,
    AxumPath((name, path)): AxumPath<(String, String)>,
) -> Response {
    serve_ext(&state, &name, &path)
}

/// 分发扩展资产：越界/缺失一律 404（不区分，避免探测目录结构）；停用、未通过验签的也 404。
fn serve_ext(state: &AppState, name: &str, rel: &str) -> Response {
    let disabled = state
        .settings
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .disabled_extensions
        .clone();
    if disabled.iter().any(|d| d == name) {
        return (StatusCode::NOT_FOUND, "插件已停用").into_response();
    }
    // P21 红线3：第三方验签门与清单拒载同一把尺子——未 pin / 内容已变的插件不服务任何资产
    // （否则「清单里显示未受信任、面板 iframe 却能打开」自相矛盾）
    let list = crate::plugin::ui::scan(&disabled);
    if let Some(e) = list.iter().find(|e| e.name == name)
        && (!e.ok || !e.trusted)
    {
        return (
            StatusCode::NOT_FOUND,
            format!("插件未通过来源验签：{}", e.error.clone().unwrap_or_else(|| "未受信任".into())),
        )
            .into_response();
    }
    // P18（027）：draft 面板资产走内存 overlay——键必须与提交时的文件名完全一致，
    // 不做路径拼接（构造上无穿越）；命中即返回，未命中落回磁盘路径。
    let eff_rel = if rel.trim().is_empty() {
        crate::plugin::draft::entry_file(name)
    } else {
        Some(rel.to_string())
    };
    if let Some(eff) = eff_rel
        && let Some(bytes) = crate::plugin::draft::read_file(name, &eff)
    {
        let ct = crate::plugin::ui::content_type(std::path::Path::new(&eff));
        return ([(CONTENT_TYPE, ct)], bytes).into_response();
    }
    let rel = if rel.trim().is_empty() {
        list.iter()
            .find(|e| e.name == name)
            .map(|e| e.entry.clone())
            .unwrap_or_else(|| "index.html".to_string())
    } else {
        rel.to_string()
    };
    match crate::plugin::ui::resolve_asset(name, &rel) {
        Ok(p) => match std::fs::read(&p) {
            Ok(bytes) => ([(CONTENT_TYPE, crate::plugin::ui::content_type(&p))], bytes).into_response(),
            Err(e) => (StatusCode::NOT_FOUND, format!("读文件失败: {e}")).into_response(),
        },
        Err(e) => (StatusCode::NOT_FOUND, e).into_response(),
    }
}
