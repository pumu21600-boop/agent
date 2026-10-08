//! MCP 接入 + 插件清单/draft 隔离区/命名空间存储/静态资源服务（自 server.rs 按域拆出）

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
}

/// POST /api/mcp —— 配置管理与生命周期动作（全部落 settings.json 并同步管理器）。
pub(super) async fn post_mcp(
    State(state): State<AppState>,
    Json(req): Json<McpActionRequest>,
) -> (StatusCode, Json<Value>) {
    let err = |m: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": m })));
    // 2026-10-08 修：settings 写互斥——读-改-写整条链路持锁，防并发写者互覆盖。
    // 临界区只包同步段（clone→改→save→换内存→同步）；refresh 的 spawn_blocking
    // await 在锁外（std 守卫跨 await 会让 handler 的 future 非 Send，axum 不收）。
    {
        let _w = state.settings_write.lock().unwrap_or_else(|e| e.into_inner());
        let mut s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
        match req.action.as_str() {
            "save" => {
                let Some(srv) = req.server.clone() else {
                    return err("缺少 server 配置".into());
                };
                let cfg: crate::config::settings::McpServerCfg = match serde_json::from_value(srv) {
                    Ok(c) => c,
                    Err(e) => return err(format!("server 配置不合法: {e}")),
                };
                // transport 合法性 + 必需字段 + 名称规则：与 settings 工具的整表替换
                // 共用同一份校验（mcp::validate_server_cfg）
                if let Err(e) = crate::mcp::validate_server_cfg(&cfg) {
                    return err(e);
                }
                s.mcp_servers.retain(|c| c.name != cfg.name);
                s.mcp_servers.push(cfg);
            }
            "remove" => {
                let Some(name) = &req.name else { return err("缺少 name".into()) };
                s.mcp_servers.retain(|c| &c.name != name);
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
            // OAuth：发起授权（后台线程跑 discovery→授权→换 token，立即返回快照）
            "oauth_authorize" => {
                let Some(name) = &req.name else { return err("缺少 name".into()) };
                let Some(c) = s.mcp_servers.iter().find(|c| &c.name == name) else {
                    return err(format!("未知 server: {name}"));
                };
                match crate::mcp::oauth::start_authorize(name, c) {
                    Ok(snap) => return (StatusCode::OK, Json(json!({ "ok": true, "tx": snap }))),
                    Err(e) => return err(e),
                }
            }
            // OAuth：撤销授权（删 token + 强制下线；下次调用回「未授权」人话）
            "oauth_revoke" => {
                let Some(name) = &req.name else { return err("缺少 name".into()) };
                if !s.mcp_servers.iter().any(|c| &c.name == name) {
                    return err(format!("未知 server: {name}"));
                }
                crate::mcp::oauth::revoke(name);
                crate::mcp::force_offline(name);
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
        // 存储一门/契约（027）：settings 变更即同步停用名单到工具插件（拒新调用）
        crate::plugin::wasm::set_disabled(&s.disabled_plugins);
        crate::plugin::ui::set_disabled(&s.disabled_plugins); // 生态轮：插件技能根同步停用
        crate::mcp::sync_configs(&s.mcp_servers);
    }

    // refresh：同步后拉起并重新发现工具（阻塞 IO 丢 blocking 池）
    crate::plugin::wasm::rescan(); // 017：顺带重扫 WASM 插件
    if req.action == "refresh" {
        let name = req.name.clone().unwrap_or_default();
        let r = tokio::task::spawn_blocking(move || crate::mcp::ensure_online_by_name(&name)).await;
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

/// env/headers 脱敏（2026-10-08 修）：这两个映射里是第三方服务的密钥/token，
/// 明文回显等于把它们递给任何能调本机端口的调用方——与 settings 对 api_key
/// 只给尾 4 位（key_hint）的纪律不一致，也违反 McpServerCfg.headers 字段
/// 自述的「值绝不进日志」。前端编辑走 McpTab 自己的表单 textarea，不消费这里
/// 的值；留键名 + 尾 2 位只够确认「存没存」。
fn mask_secret_map(m: &std::collections::BTreeMap<String, String>) -> Value {
    m.iter()
        .map(|(k, v)| {
            let n = v.chars().count();
            let hint = if n > 4 {
                let tail: String = v.chars().skip(n - 2).collect();
                format!("••••{tail}")
            } else if n > 0 {
                "••••".to_string()
            } else {
                String::new()
            };
            json!({ "key": k, "hint": hint })
        })
        .collect()
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
        crate::mcp::status()
            .into_iter()
            .map(|(n, o, t)| (n, (o, t)))
            .collect();
    // resources / prompts 清单（声明了能力的 server 才有内容）
    let catalogs = crate::mcp::catalogs();
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
                "env": mask_secret_map(&c.env), "url": c.url, "headers": mask_secret_map(&c.headers),
                "enabled": c.enabled,
                "online": online,
                "tools": tools.iter().map(|(n, d)| json!({"name": n, "desc": d})).collect::<Vec<_>>(),
                "resources": res,
                "prompts": prompts,
                // OAuth 配置 + 授权状态（tx=事务快照，oauth=token 态）
                "oauth": c.oauth,
                "oauth_status": crate::mcp::oauth::tx_status(&c.name),
            })
        })
        .collect();
    Json(json!({ "servers": servers, "wasm": wasm_list }))
}

/// GET /api/plugins —— 组件总线 v0：UI 扩展清单（应用根内置 + 当前工作区自带）。
pub(super) async fn list_plugins(State(state): State<AppState>) -> Json<Value> {
    let disabled = state
        .settings
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .disabled_plugins
        .clone();
    // 二合一（027）：面板插件 + 工具插件（wasm 桥）+ 内存 draft，一份清单
    let extensions = crate::plugin::ui::scan_all(&disabled);
    Json(json!({
        "plugins": extensions,
        "dir": crate::plugin::ui::DIR,
        "app_dir": crate::config::app_root().join(crate::plugin::ui::DIR).display().to_string(),
        "workspace_dir": crate::tools::workspace_root().join(crate::plugin::ui::DIR).display().to_string(),
    }))
}

/// 新建插件的起始 index.html：自包含、无外联、无动态执行——静态审核（review）干净
/// 通过；深浅色主题变量对齐其它内置面板。
const STARTER_INDEX_HTML: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1.0" />
<title>新插件</title>
<style>
  :root { color-scheme: dark; }
  body {
    --bg: #17171a; --fg: #e8e8ea; --dim: #9a9aa2; --line: #2c2c31;
    margin: 0; padding: 16px 18px;
    font: 13px/1.7 "Segoe UI", "Microsoft YaHei", system-ui, sans-serif;
    background: var(--bg); color: var(--fg);
  }
  body.light {
    --bg: #ffffff; --fg: #1a1a1c; --dim: #6b6b74; --line: #e4e4e8;
    color-scheme: light;
  }
  h3 { margin: 0 0 8px; font-size: 14px; font-weight: 650; }
  p { margin: 6px 0; }
  .dim { color: var(--dim); font-size: 12px; }
  code {
    background: var(--line); border-radius: 4px; padding: 1px 5px;
    font-family: Consolas, monospace; font-size: 12px;
  }
</style>
</head>
<body>
  <h3>新插件已就绪</h3>
  <p>改插件目录下的 <code>index.html</code> 与 <code>manifest.json</code>，这个页面就是插件本体。</p>
  <p class="dim">插件跑在沙箱 iframe 里，碰不到宿主页面；要数据走 postMessage 桥，写法参考内置面板。</p>
</body>
</html>
"#;

/// POST /api/plugins/create —— 用户 2026-10-05：插件页「新建插件」脚手架。在工作区
/// plugins/<name>/ 下生成 manifest.json + 起始 index.html。**创建不等于放行**：新插件
/// 与手工放置的插件走同一条信任/审核链路，仍要用户点「信任」才加载（模型自产的 draft
/// 隔离区不适用于用户主动创建——来源是 user、权限该自己声明）。
pub(super) async fn create_plugin(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let name = req.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let description = req
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let err = |m: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": m })));
    if !crate::plugin::draft::safe_name(&name) {
        return err("插件名非法（仅字母数字与 - _ .，且不能是 . 或 ..）".into());
    }
    if description.len() > 200 {
        return err("描述太长（200 字以内）".into());
    }
    // 重名即拒：内置 / 工作区 / 内存 draft / 工具注册表全查（避免「同名覆盖内置」的迷惑性）
    if crate::plugin::ui::scan_all(&[]).iter().any(|e| e.name == name)
        || crate::plugin::wasm::has_plugin(&name)
    {
        return err(format!("已存在同名插件（{name}），换一个名字"));
    }
    let dir = crate::tools::workspace_root().join(crate::plugin::ui::DIR).join(&name);
    if dir.exists() && std::fs::read_dir(&dir).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return err(format!("目录 {name} 已存在且非空：换一个名字或先手工清理"));
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return err(format!("建插件目录失败: {e}"));
    }
    let manifest = json!({
        "apiVersion": 1,
        "source": "user",
        "title": name,
        "description": description,
        "version": "0.1.0",
        "entry": "index.html",
    });
    if let Err(e) = crate::fsx::write_atomic_str(
        &dir.join("manifest.json"),
        &serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    ) {
        return err(format!("写 manifest.json 失败: {e}"));
    }
    if let Err(e) = crate::fsx::write_atomic_str(&dir.join("index.html"), STARTER_INDEX_HTML) {
        return err(format!("写 index.html 失败: {e}"));
    }
    let disabled = state
        .settings
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .disabled_plugins
        .clone();
    let list = crate::plugin::ui::scan_all(&disabled);
    observability::event(
        EventType::State,
        json!({ "kind": "plugin.create", "name": name, "dir": dir.display().to_string() }),
    );
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "plugins": list, "dir": dir.display().to_string() })),
    )
}

/// POST /api/plugins/remove {name} —— 删除插件（2026-10-06 插件重整：补上「新建插件
/// 只建不删」的缺口，与 create_plugin 对偶）。只让删 user/draft 来源，built_in 拒删
/// 并说人话（内核见 plugin::ui::remove，agent 的 plugin 工具 action=remove 走同一份）。
/// 删除类界面侧先弹 ConfirmModal（delete-class-confirm 纪律）。
pub(super) async fn remove_plugin(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let name = req.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 name" })));
    }
    match crate::plugin::ui::remove(&name) {
        Ok(dir) => {
            // 目录没了 → 工具注册表重扫（它可能同时是个工具插件）+ 前端重拉清单
            crate::plugin::wasm::rescan();
            emit_plugin_settings_changed("");
            let disabled = state
                .settings
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .disabled_plugins
                .clone();
            let list = crate::plugin::ui::scan_all(&disabled);
            (StatusCode::OK, Json(json!({ "ok": true, "dir": dir, "plugins": list })))
        }
        Err((code, msg)) => (code, Json(json!({ "error": msg }))),
    }
}

/// POST /api/plugins/toggle —— 启用/停用扩展（落 settings.json 的停用名单）。
pub(super) async fn toggle_plugin(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let name = req.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 name" })));
    }
    let enabled = req.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    // 2026-10-08 修：settings 写互斥（读-改-写整条链路持锁，防并发写者互覆盖）
    let _w = state.settings_write.lock().unwrap_or_else(|e| e.into_inner());
    let mut s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    s.disabled_plugins.retain(|d| d != &name);
    if !enabled {
        s.disabled_plugins.push(name.clone());
    }
    // 权限放开轮（用户 2026-10-02 拍板）：manifest 声明了 feature 的插件，它的启用开关
    // 就是那个功能的总开关——host 同步写 settings.features，一个能力只留一个开关
    // （否则「插件停了但功能还在跑」或反过来，用户看到的和实际发生的对不上）。
    let mut synced_feature = String::new();
    if let Some(e) = crate::plugin::ui::scan_all(&s.disabled_plugins).into_iter().find(|e| e.name == name)
        && !e.feature.is_empty()
    {
        synced_feature = e.feature.clone();
        if let Err(err) = s.set_feature(&e.feature, enabled) {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": err })));
        }
    }
    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    // 存储一门/契约（027）：settings 变更即同步停用名单到工具插件（拒新调用）
    crate::plugin::wasm::set_disabled(&s.disabled_plugins);
    crate::plugin::ui::set_disabled(&s.disabled_plugins); // 生态轮：停用的插件不贡献技能
    emit_plugin_settings_changed(&synced_feature);
    observability::event(
        EventType::State,
        json!({ "kind": "plugin.toggle", "name": name, "enabled": enabled, "synced_feature": synced_feature }),
    );
    // 二合一：回包清单含工具插件
    let list = crate::plugin::ui::scan_all(&s.disabled_plugins);
    (StatusCode::OK, Json(json!({ "ok": true, "plugins": list })))
}

/// 插件开关落定后推给前端的 settings_changed。
///
/// 这个端点一直在写 settings（disabled_plugins，联动命中时还有 features），却曾是唯一
/// 不推 settings_changed 的写入面：前端 toggleExt 只把回包的插件清单塞进 extList，
/// settings 派生的界面状态就停在旧值。2026-10-06 用户实锤——关掉星云插件，
/// 左下角「记忆库」按钮还在（它看的是 features.galaxy，那是联动写进去的）。
///
/// 前端 settings_changed 的 run 是 loadExt + loadSettings，所以事件一到，插件清单与
/// features 一起重拉，入口该收就收。keys 带上联动命中的功能键：现在只作排查线索，
/// 哪天前端要按 keys 做增量刷新，形状已经在这儿了。
fn emit_plugin_settings_changed(synced_feature: &str) {
    let mut keys = vec!["disabled_plugins".to_string()];
    if !synced_feature.is_empty() {
        keys.push(synced_feature.to_string());
    }
    crate::server::ui_events::emit(json!({ "kind": "settings_changed", "keys": keys }));
}

/// POST /api/plugins/draft —— 模型自产插件提交进 draft 隔离区（027）。
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
                .disabled_plugins
                .clone();
            let list = crate::plugin::ui::scan_all(&disabled);
            (StatusCode::OK, Json(json!({ "ok": true, "plugins": list })))
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
    }
}

/// POST /api/plugins/promote —— draft 一键转正（落盘持久化、来源改 user、重扫注册表）。
/// 红线3：转正是用户显式动作 = 信任决定，落盘后**自动 pin** 内容指纹（不 pin 就会被
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
                // 2026-10-08 修：settings 写互斥（pin 表读-改-写整条链路持锁）
                let _w = state.settings_write.lock().unwrap_or_else(|e| e.into_inner());
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
                .disabled_plugins
                .clone();
            let list = crate::plugin::ui::scan_all(&disabled);
            (StatusCode::OK, Json(json!({ "ok": true, "written": paths, "pinned": pins.len(), "plugins": list })))
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
    }
}

/// POST /api/plugins/trust {key, action} —— 第三方插件验签的 pin / unpin（红线3）。
/// key 形如 `plugins:<name>`；pin 时按当前位置重算 digest。
///
/// **准入审核（用户 2026-10-02 拍板）**：pin 是加载的前置条件，所以 pin 前必须先过
/// 静态审核——有 blocker 直接拒 pin（审核不过 → 装不进来 → 跑不起来）。pin 表记的是
/// 内容指纹，天然把「放行」绑定到「审过的这一份」：内容一变 → 验签打回 → 重新走审核。
pub(super) async fn trust_plugin(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let key = req.get("key").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let action = req.get("action").and_then(Value::as_str).unwrap_or("pin").trim().to_string();
    if key.is_empty() || !key.contains(':') {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 key（形如 plugins:<name>）" })));
    }
    let err = |m: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": m })));
    // 2026-10-08 修：settings 写互斥（pin 表读-改-写整条链路持锁，防与其它写面互覆盖）
    let _w = state.settings_write.lock().unwrap_or_else(|e| e.into_inner());
    let mut s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    match action.as_str() {
        "pin" => {
            let Some(path) = crate::plugin::trust::resolve(&key) else {
                return err(format!("找不到插件 {key}（目录不存在或名字非法）"));
            };
            if !path.is_dir() {
                return err(format!("{key} 不是目录形态（裸文件旧形态不支持审核，请改用插件目录）"));
            }
            // 准入审核：blocker 一律拒 pin；warn 放行但随响应带回亮旗
            let report = match crate::plugin::review::review_key(&key) {
                Ok(r) => r,
                Err(e) => return err(e),
            };
            if report.verdict == crate::plugin::review::Verdict::Blocked {
                let reasons = crate::plugin::review::block_reasons(&report);
                observability::event(
                    EventType::State,
                    json!({ "kind": "plugin.review_blocked", "key": key, "reasons": reasons }),
                );
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "error": "审核未通过，不能信任：",
                        "review_blocked": reasons,
                        "report": report,
                    })),
                );
            }
            observability::event(
                EventType::State,
                json!({ "kind": "plugin.review_passed", "key": key, "warns": report.findings.len() }),
            );
            let digest = report.digest.unwrap_or_default();
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
    let disabled = s.disabled_plugins.clone();
    let list = crate::plugin::ui::scan_all(&disabled);
    (StatusCode::OK, Json(json!({ "ok": true, "plugins": list })))
}

/// POST /api/plugins/review {key} —— 只审不信（界面「审核」按钮用）：跑静态审查，
/// 返回完整报告（结论 + 逐条发现）。不落任何状态——放行只在 pin 时发生一次。
pub(super) async fn review_plugin(Json(req): Json<Value>) -> (StatusCode, Json<Value>) {
    let key = req.get("key").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if key.is_empty() || !key.contains(':') {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "缺少 key（形如 plugins:<name>）" })));
    }
    match crate::plugin::review::review_key(&key) {
        Ok(report) => (StatusCode::OK, Json(json!({ "ok": true, "report": report }))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))),
    }
}

/// DELETE /api/plugins/draft?name= —— 丢弃 draft（内存即清，WASM 注册表同步回收）。
pub(super) async fn discard_draft(
    State(state): State<AppState>,
    Query(q): Query<PluginDataQueryName>,
) -> (StatusCode, Json<Value>) {
    if crate::plugin::draft::discard(&q.name) {
        let disabled = state
            .settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .disabled_plugins
            .clone();
        let list = crate::plugin::ui::scan_all(&disabled);
        (StatusCode::OK, Json(json!({ "ok": true, "plugins": list })))
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
        // 2026-10-08 修：404 原是纯文本「键不存在」——该端点其余分支与全部其它 API
        // 错误都是 JSON，插件前端对 404 直接 resp.json() 会抛异常而不是走到空值分支。
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({ "ok": false, "error": "键不存在" }))).into_response(),
        Err(e) => (StatusCode::FORBIDDEN, Json(json!({ "error": e }))).into_response(),
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

/// GET /plugins/{name} —— 扩展入口页（entry 由 manifest 指定）。
pub(super) async fn plugin_entry(State(state): State<AppState>, AxumPath(name): AxumPath<String>) -> Response {
    serve_plugin(&state, &name, "")
}

/// GET /plugins/{name}/{*path} —— 扩展目录内的静态资源。
pub(super) async fn plugin_asset(
    State(state): State<AppState>,
    AxumPath((name, path)): AxumPath<(String, String)>,
) -> Response {
    serve_plugin(&state, &name, &path)
}

/// 分发扩展资产：越界/缺失一律 404（不区分，避免探测目录结构）；停用、未通过验签的也 404。
fn serve_plugin(state: &AppState, name: &str, rel: &str) -> Response {
    let disabled = state
        .settings
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .disabled_plugins
        .clone();
    if disabled.iter().any(|d| d == name) {
        return (StatusCode::NOT_FOUND, "插件已停用").into_response();
    }
    // 红线3：第三方验签门与清单拒载同一把尺子——未 pin / 内容已变的插件不服务任何资产
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
    // （027）：draft 面板资产走内存 overlay——键必须与提交时的文件名完全一致，
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 新建插件的脚手架必须干净过静态审核（review 判 Pass）——否则建出来的插件
    /// 点「信任」会被后端拦下，「新建插件」入口就成了死路。改起始页模板先跑这个测试。
    #[test]
    fn scaffold_passes_static_review() {
        let d = std::env::temp_dir().join(format!("agent-plugin-scaffold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // 与 create_plugin 落盘的 manifest 同字段（描述留空也走这条）
        let manifest = json!({
            "apiVersion": 1,
            "source": "user",
            "title": "demo",
            "description": "",
            "version": "0.1.0",
            "entry": "index.html",
        });
        crate::fsx::write_atomic_str(
            &d.join("manifest.json"),
            &serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        crate::fsx::write_atomic_str(&d.join("index.html"), STARTER_INDEX_HTML).unwrap();
        let report = crate::plugin::review::review_dir(&d);
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(report.verdict, crate::plugin::review::Verdict::Pass);
        assert!(report.findings.is_empty(), "脚手架不该亮任何旗: {:?}", report.findings);
    }

    /// 插件开关必须推 settings_changed，且联动命中功能键时把它带在 keys 里。
    ///
    /// 回归的是 2026-10-06 的用户实锤：关掉星云插件，左下角「记忆库」按钮还在。
    /// 后端联动早写对了 settings.features.galaxy（落盘为证），但这个端点是唯一
    /// 不推事件的写入面——前端 toggleExt 只更新 extList，settings 派生的界面状态
    /// （侧栏入口看 features.galaxy）停在旧值，直到重启才对上。
    ///
    /// 事件形状即前端契约：kind=settings_changed（UI_OPS 表里它的 run 是
    /// loadExt + loadSettings）。keys 的顺序与内容是断言对象——换形状前端就刷不到。
    ///
    /// 注：这里钉的是事件本身的形状与「确实推了」；handler 里那次调用靠 code review 兜
    /// （本仓无 HTTP 层测试基座，为一个 emit 造一套 AppState 不划算）。
    #[test]
    fn plugin_toggle_pushes_settings_changed_with_feature_key() {
        // 事件总线是进程级广播，别的测试也可能在推：只认形状完全对上的那条
        fn drain_for_galaxy(rx: &mut tokio::sync::broadcast::Receiver<Value>) -> Option<Value> {
            for _ in 0..64 {
                match rx.try_recv() {
                    Ok(v) => {
                        let keys = v.get("keys").and_then(Value::as_array);
                        let want = keys.is_some_and(|k| {
                            k.iter().filter_map(Value::as_str).eq(["disabled_plugins", "galaxy"])
                        });
                        if v.get("kind").and_then(Value::as_str) == Some("settings_changed") && want {
                            return Some(v);
                        }
                    }
                    Err(_) => break,
                }
            }
            None
        }

        let mut rx = crate::server::ui_events::test_subscribe();
        emit_plugin_settings_changed("galaxy");
        let ev = drain_for_galaxy(&mut rx).expect("必须推 settings_changed：前端靠它重拉 settings（侧栏入口看 features.galaxy）");
        assert_eq!(ev["kind"], "settings_changed");
        assert_eq!(
            ev["keys"].as_array().map(|a| a.len()).unwrap_or(0),
            2,
            "联动命中功能键时要带上它: {ev}"
        );

        // 没声明 feature 的普通插件也推（disabled_plugins 一直在变），只是 keys 只有一项
        let mut plain = crate::server::ui_events::test_subscribe();
        emit_plugin_settings_changed("");
        let ev2 = plain.try_recv().expect("普通插件开关也要推 settings_changed");
        assert_eq!(ev2["keys"].as_array().map(|a| a.len()).unwrap_or(0), 1, "无联动时 keys 只有 disabled_plugins: {ev2}");
    }
}

#[cfg(test)]
mod mask_tests {
    use super::*;

    /// 2026-10-08 修：env/headers 明文回显把第三方密钥递给任何本机调用方。
    /// 脱敏后只剩键名 + 尾 2 位（短值全遮），键名保留供确认「存没存」。
    #[test]
    fn mcp_secret_maps_are_masked() {
        let mut m = std::collections::BTreeMap::new();
        m.insert("API_KEY".to_string(), "sk-super-secret-token-1234".to_string());
        m.insert("SHORT".to_string(), "ab".to_string());
        m.insert("EMPTY".to_string(), String::new());
        let v = mask_secret_map(&m);
        let arr = v.as_array().unwrap();
        let find = |k: &str| arr.iter().find(|e| e["key"] == k).unwrap().clone();
        let api = find("API_KEY");
        let hint = api["hint"].as_str().unwrap();
        assert!(hint.starts_with("••••") && hint.ends_with("34"), "只留尾 2 位: {hint}");
        assert!(!hint.contains("secret"), "正文不得泄露: {hint}");
        assert_eq!(find("SHORT")["hint"].as_str().unwrap(), "••••", "短值全遮");
        assert_eq!(find("EMPTY")["hint"].as_str().unwrap(), "", "空值留空");
        // 空映射 → 空数组（不是 null）
        let empty = mask_secret_map(&std::collections::BTreeMap::new());
        assert_eq!(empty.as_array().unwrap().len(), 0);
    }
}
