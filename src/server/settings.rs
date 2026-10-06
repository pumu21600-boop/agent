//! 供应商预设 / 设置读写 / 模型列表 / 配置管理端点（P19 自 server.rs 按域拆出）

use super::*;

#[derive(Debug, Deserialize)]
pub struct ModelsRequest {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
}

/// 设置更新请求。B1 清除语义（施工单 012 固化，前后端任何一方改动必须对照本条）：
///
/// - `0` = 清除长度类（context_length / max_output_tokens）
/// - 负数 = 清除采样类（temperature / top_p）
/// - 空串 / auto = 清除思考类（thinking / thinking_style）
/// - clear_key=true = 清 key
///
/// 其余字段：null / 缺省 = 不动。
#[derive(Debug, Deserialize, Default)]
pub struct SettingsUpdate {
    /// 配置名称（改当前激活配置的名）
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// 弹窗里勾选保存的模型列表（多选）；Some 则整体替换
    #[serde(default)]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub clear_key: Option<bool>,
    /// 当前模型上下文长度（tokens；0=清除恢复全局默认）
    #[serde(default)]
    pub context_length: Option<u64>,
    /// 当前模型最大输出长度（tokens；0=清除）
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    /// 采样参数（P8；null/缺省=不动，0 或 -1=清除走渠道默认）
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    /// 思考开关（P9）："on"/"off"/档位（low/medium/high/xhigh/max/minimal）/""（空=清除走渠道默认）
    #[serde(default)]
    pub thinking: Option<String>,
    /// 思考参数形态覆盖（P9，OpenClaw thinkingFormat 词表）："auto"/""=端点探测；
    /// none/openai/deepseek/zai/qwen/qwen-chat-template；P8.3 旧值自动映射
    #[serde(default)]
    pub thinking_style: Option<String>,
    /// 模型能力标记（工具调用/图片/视频/音频解析）。Some = 用户手动勾选并保存
    #[serde(default)]
    pub capabilities: Option<crate::config::settings::ModelCapabilities>,
    /// 角色路由映射（摘要/标题/多模态旁路/降级链）
    #[serde(default)]
    pub roles: Option<crate::config::settings::RolesSettings>,
    /// 内置功能开关（功能桥接/css-ui注入/记忆库）。Some = 整体替换
    #[serde(default)]
    pub features: Option<crate::config::settings::FeatureSettings>,
}

/// 供应商预设列表（数据驱动自 providers.json）。
pub(super) async fn list_providers() -> Json<Value> {
    let list: Vec<Value> = gateway::providers::list()
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "name": p.name,
                "default_base_url": p.default_base_url,
                "default_model": p.default_model,
                // P9：预设的思考形态覆盖（多数官方域名可探测，仅豆包/混元带值）
                "thinking_format": p.thinking_format.map(|f| f.as_str()),
                "key_optional": p.key_optional,
            })
        })
        .collect();
    Json(json!({ "providers": list }))
}

/// 已保存 key 的尾 4 位提示（完整 key 不出内存、不落日志）。
fn key_hint_of(key: Option<String>) -> String {
    key.as_ref()
        .map(|k| {
            let n = k.chars().count();
            if n > 4 { k.chars().skip(n - 4).collect::<String>() } else { k.clone() }
        })
        .unwrap_or_default()
}

/// GET /api/settings/schema — settable-path manifest (paths/types/enums/defaults/risks/descs).
/// Read-only local endpoint; the settings UI and the agent share this one table.
pub(super) async fn get_settings_schema() -> Json<Value> {
    Json(crate::config::settings_schema::manifest_json())
}


pub(super) async fn get_settings(State(state): State<AppState>) -> Json<Value> {
    let s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    let mut v = config_state_json(&s);
    v["env_key"] = json!(std::env::var("DEEPSEEK_API_KEY").is_ok());
    Json(v)
}

/// 把 SettingsUpdate 应用到当前激活配置（put_settings 的主体逻辑，独立成纯函数便于单测；
/// pub(crate)：agent 的 settings 工具复用同一套字段语义，见 tools/settings.rs）。
/// 返回 (url_changed, provider_id, effective_model)。
pub(crate) fn apply_settings_update(s: &mut Settings, req: &SettingsUpdate) -> (bool, String, String) {
    s.ensure_active_config();

    if let Some(v) = &req.name {
        s.set_active_name(v);
    }

    let mut result = (false, "deepseek".to_string(), "deepseek-chat".to_string());
    if let Some(c) = s.active_mut() {
        let url_before = crate::config::settings::normalize_base(&c.effective_base_url());
        if let Some(v) = &req.provider_id
            && !v.trim().is_empty() {
                c.provider_id = v.trim().to_string();
            }
        if let Some(v) = &req.base_url {
            c.base_url = v.trim().to_string();
        }
        if let Some(v) = &req.model {
            let v = v.trim().to_string();
            if c.model != v {
                // 能力标记归用户手动勾选，切换模型不改写（原「按模型名推导初值」已移除）
                c.model = v;
            }
        }
        if let Some(v) = &req.models {
            let mut list: Vec<String> =
                v.iter().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect();
            list.sort();
            list.dedup();
            c.models = list;
        }
        if let Some(v) = req.context_length {
            c.context_length = if v == 0 { None } else { Some(v) };
        }
        if let Some(v) = req.max_output_tokens {
            c.max_output_tokens = if v == 0 { None } else { Some(v) };
        }
        // P8 采样参数：temperature 0.0 是合法值（确定性输出），清除用负数
        if let Some(v) = req.temperature {
            c.temperature = if v < 0.0 { None } else { Some(v) };
        }
        if let Some(v) = req.top_p {
            c.top_p = if v < 0.0 { None } else { Some(v) };
        }
        if let Some(v) = req.thinking.as_deref() {
            let v = v.trim().to_lowercase();
            c.thinking = match v.as_str() {
                "on" | "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => {
                    Some(v.clone())
                }
                _ => None,
            };
        }
        if let Some(v) = req.thinking_style.as_deref() {
            let v = v.trim().to_lowercase();
            c.thinking_style = if v.is_empty() || v == "auto" { None } else { Some(v.to_string()) };
        }
        if let Some(caps) = &req.capabilities {
            c.capabilities = caps.clone();
            // 能力标记归用户所有：手动勾选即时保存
            c.capabilities_user_set = true;
        }

        let url_after = crate::config::settings::normalize_base(&c.effective_base_url());
        let changed = url_before != url_after;
        if changed {
            c.models.clear();
            c.model.clear();
        }
        result = (changed, c.provider_id.clone(), c.effective_model());
    }
    result
}

/// 保存设置。api_key 非空则 DPAPI 加密持久化到当前激活配置；clear_key=true 则清除。
pub(super) async fn put_settings(
    State(state): State<AppState>,
    Json(req): Json<SettingsUpdate>,
) -> (StatusCode, Json<Value>) {
    let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner()).clone();
    let (url_changed, pid, eff_model) = apply_settings_update(&mut s, &req);

    if url_changed {
        observability::event(
            EventType::State,
            json!({ "kind": "settings.channel_changed", "base_url": s.effective_base_url() }),
        );
    }
    if req.clear_key == Some(true) {
        if let Err(e) = s.clear_key() {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
        }
    } else if let Some(k) = &req.api_key
        && !k.trim().is_empty()
            && let Err(e) = s.set_key(k.trim()) {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
            }

    if let Some(roles) = req.roles {
        s.roles = Some(roles);
    }

    if let Some(features) = req.features {
        s.features = features;
    }

    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    sync_runtime(&s);
    observability::event(
        EventType::State,
        json!({ "kind": "settings.updated", "provider_id": pid, "model": eff_model }),
    );
    (StatusCode::OK, Json(config_state_json(&s)))
}

/// settings.json 变更后的运行时同步（幂等）：settings 落盘后把停用名单/MCP 配置/
/// 价格表灌回各子系统。HTTP 端点（put_settings / manage_configs / post_mcp）与
/// agent 的 settings 工具共用这一道，保证「改设置即生效」只有一条路。
pub(crate) fn sync_runtime(s: &Settings) {
    // P18 存储一门/契约（027）：settings 变更即同步停用名单到工具插件（拒新调用）
    crate::plugin::wasm::set_disabled(&s.disabled_plugins);
    crate::plugin::ui::set_disabled(&s.disabled_plugins); // 生态轮：同名单贯通插件技能根
    crate::mcp::sync_configs(&s.mcp_servers); // P11：MCP 配置变更即同步管理器（幂等）
    crate::skills::set_disabled(&s.disabled_skills); // 2026-10-02：技能启停名单贯通
    // P21：价格表同步刷新内存副本
    crate::usage::install_prices(&s.cost.prices);
}

/// 磁盘 settings.json 的外部改动监听（2026-10-05 用户实锤「前端也没有立马看得到」）。
///
/// 场景：agent 拿文件工具（或用户在记事本里）**直接改 settings.json**，不走任何
/// HTTP 端点/工具。此前这条路径改完：服务内存里那份不刷新、界面也不推事件——
/// 用户看着还是旧值，重启才好。v0.32.18 只堵住了「走接口」这条路，漏了这条。
///
/// 做法：500ms 轮询 mtime+长度（便宜），变了就重新解析并与内存逐字节比对：
/// - 内容真不同才接管：换内存、sync_runtime、推 settings_changed（界面即时跟上）；
/// - 自家端点/工具写盘后内容一致 → 空转，不重复推事件；
/// - 解析失败 / 文件正被写（空或截断）→ 保持现状等下一次轮询，绝不拿坏文件覆盖内存；
/// - 用规范化序列化后再比：键序/缩进/尾随换行不构成变化，只有值真变了才触发。
pub(crate) fn start_external_reload(state: &AppState) {
    use std::time::{Duration, SystemTime};
    let settings = state.settings.clone();
    let path = Settings::path_pub();
    std::thread::spawn(move || {
        let mut last_sig: Option<(SystemTime, u64)> = None;
        let mut last_text: Option<String> = None;
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            let sig = (mtime, meta.len());
            if Some(sig) == last_sig {
                continue;
            }
            last_sig = Some(sig);
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            // 正被写盘（编辑器 truncate-then-write）：空文件一律不接
            if text.trim().is_empty() {
                continue;
            }
            // mtime 变了但内容没变（另存为同内容）
            if last_text.as_deref() == Some(text.as_str()) {
                continue;
            }
            let Ok(fresh) = crate::config::settings::settings_from_text(&text) else {
                observability::debug("[settings] 外部改动解析失败，保持内存现状（等下一次轮询）");
                continue;
            };
            let cur = settings.read().unwrap_or_else(|e| e.into_inner()).clone();
            let same = serde_json::to_string(&cur).ok().as_deref() == serde_json::to_string(&fresh).ok().as_deref();
            if same {
                last_text = Some(text);
                continue; // 自家端点/工具刚写盘的，内容一致
            }
            *settings.write().unwrap_or_else(|e| e.into_inner()) = fresh.clone();
            last_text = Some(text);
            sync_runtime(&fresh);
            crate::server::ui_events::emit(json!({ "kind": "settings_changed", "keys": ["external"] }));
            observability::event(EventType::State, json!({ "kind": "settings.external_reloaded" }));
        }
    });
}

/// 拉取渠道模型列表。请求里没给 base_url/key 就用已保存设置。
pub(super) async fn list_models(
    State(state): State<AppState>,
    Json(req): Json<ModelsRequest>,
) -> (StatusCode, Json<Value>) {
    let s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    let base_url = if req.base_url.trim().is_empty() { s.effective_base_url() } else { req.base_url.trim().to_string() };
    // B2 key 回落链（施工单 012 固化）：请求显式 key > 激活配置 DPAPI key > DEEPSEEK_API_KEY 环境变量。
    // key 永不出日志、永不出 JSONL。
    let api_key = if !req.api_key.trim().is_empty() {
        req.api_key.trim().to_string()
    } else {
        s.get_key().or_else(|| std::env::var("DEEPSEEK_API_KEY").ok()).unwrap_or_default()
    };
    if base_url.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "base_url 为空" })));
    }
    if api_key.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "未设置 API key" })));
    }
    match state.gateway.list_models(&base_url, &api_key).await {
        Ok(models) => (StatusCode::OK, Json(json!({ "models": models }))),
        Err(e) => {
            observability::event(EventType::Error, json!({ "message": e.to_string() }));
            (StatusCode::BAD_GATEWAY, Json(json!({ "error": e.to_string() })))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ConfigRequest {
    /// new（新建并激活，起点复制当前配置）| delete | apply（切换激活）
    pub action: String,
    #[serde(default)]
    pub id: Option<String>,
}

/// 配置管理：new / delete / apply。改动即时生效（顶层字段即激活配置镜像）。
pub(super) async fn manage_configs(
    State(state): State<AppState>,
    Json(req): Json<ConfigRequest>,
) -> (StatusCode, Json<Value>) {
    let mut s = state.settings.write().unwrap_or_else(|e| e.into_inner()).clone();
    let err =
        |m: &str| (StatusCode::BAD_REQUEST, Json(json!({ "error": m })));
    match req.action.as_str() {
        "new" => {
            s.new_config();
        }
        "delete" => {
            let Some(id) = req.id else { return err("缺少配置 id") };
            if !s.delete_config(&id) {
                return err("配置不存在");
            }
        }
        "apply" => {
            let Some(id) = req.id else { return err("缺少配置 id") };
            if !s.activate(&id) {
                return err("配置不存在");
            }
        }
        other => return err(&format!("未知操作: {other}")),
    }
    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    let pid = s.provider_id();
    let model = s.effective_model();
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    sync_runtime(&s);
    observability::event(
        EventType::State,
        json!({ "kind": "config.changed", "action": req.action, "provider_id": pid, "model": model }),
    );
    (StatusCode::OK, Json(config_state_json(&s)))
}

/// 配置快照（含各配置 key 状态与角色路由），供前端全量刷新。
fn config_state_json(s: &Settings) -> Value {
    let active = s.active_or_default();
    let preset = gateway::providers::find(&active.provider_id);
    let key = s.get_key();
    json!({
        "ok": true,
        "configs": s.configs.iter().map(|c| {
            let k = s.config_key(c);
            json!({
                "id": c.id,
                "name": c.name.clone(),
                "provider_id": c.provider_id,
                "base_url": c.base_url.clone(),
                "model": c.model.clone(),
                "models": c.models.clone(),
                "has_key": k.is_some(),
                "key_hint": key_hint_of(k),
                "context_length": c.context_length,
                "max_output_tokens": c.max_output_tokens,
                "temperature": c.temperature,
                "top_p": c.top_p,
                "thinking": c.thinking,
                "thinking_style": c.thinking_style,
                "capabilities": c.capabilities,
                "capabilities_user_set": c.capabilities_user_set,
            })
        }).collect::<Vec<Value>>(),
        "active_config_id": s.active_config_id,
        "name": active.name.clone(),
        "provider_id": active.provider_id,
        "base_url": active.base_url,
        "effective_base_url": active.effective_base_url(),
        "model": active.model,
        "models": active.models,
        "context_length": active.context_length,
        "max_output_tokens": active.max_output_tokens,
        "temperature": active.temperature,
        "top_p": active.top_p,
        "thinking": active.thinking,
        "capabilities": active.capabilities,
        "capabilities_user_set": active.capabilities_user_set,
        "has_key": key.is_some(),
        "key_hint": key_hint_of(key),
        // P8.2 工作区 + P9/P8.5 思考参数形态覆盖（前端按它渲染档位菜单与全协议规范）
        "workspace": crate::tools::workspace_root().display().to_string(),
        "recent_workspaces": s.recent_workspaces,
        "workspace_aliases": s.workspace_aliases,
        "thinking_style_override": active.thinking_style,
        "thinking_style": effective_thinking_style(active, preset),
        "roles": s.roles,
        "features": s.features,
    })
}

/// 生效的思考参数形态（P9/P8.5，8大规范词表）：渠道覆盖 > 端点探测。
/// 值：none | openai | deepseek | zai | qwen | claude | moonshot | qwen-chat-template | auto
fn effective_thinking_style(
    c: &crate::config::settings::Config,
    preset: Option<&gateway::providers::ProviderPreset>,
) -> &'static str {
    use gateway::compat::ThinkingFormat as TF;
    match c.thinking_style.as_deref() {
        Some("none") => "none",
        // P23：Anthropic Messages 协议专属形态（开/关 + 自动预算），显式指定或探测判定都归它
        Some("anthropic") => "anthropic",
        // P8.3 旧词表别名 → 新词表
        Some("thinking_object") => TF::Zai.as_str(),
        Some("reasoning_effort") => TF::Openai.as_str(),
        Some("enable_thinking" | "enable_thinking_budget") => TF::Qwen.as_str(),
        Some(v) => TF::parse(v).map(|f| f.as_str()).unwrap_or("auto"),
        // auto：端点探测（预设 thinking_format 是探测认不出时的兜底）
        _ => {
            let compat = gateway::compat::resolve(
                Some(c.provider_id.as_str()),
                Some(&c.effective_base_url()),
                &c.model,
                preset.and_then(|p| p.thinking_format),
            );
            if compat.protocol == gateway::compat::Protocol::AnthropicMessages {
                "anthropic"
            } else {
                compat.thinking_format.as_str()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::chat::build_chat_params;

    /// 测试用配置（custom 中转端点：探测不出规范 → openai 兜底，适合测显式覆盖）。
    fn test_config() -> crate::config::settings::Config {
        crate::config::settings::Config {
            id: "cfg-t".into(),
            name: "test".into(),
            provider_id: "custom".into(),
            base_url: "https://relay.example.com/v1".into(),
            model: "m".into(),
            models: vec![],
            key_cipher: None,
            context_length: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            thinking: None,
            thinking_style: None,
            capabilities: Default::default(),
            capabilities_user_set: false,
        }
    }

    fn test_settings() -> Settings {
        let mut s = Settings::default();
        s.configs.push(test_config());
        s.active_config_id = Some("cfg-t".into());
        s
    }

    #[test]
    fn thinking_style_override() {
        let _ = gateway::providers::init(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("providers.json"),
        );
        // 自定义中转端点探测认不出 → 覆盖为 qwen 布尔形态
        let preset = gateway::providers::find("custom");
        let mut c = crate::config::settings::Config {
            id: "cfg-t".into(),
            name: "test".into(),
            provider_id: "custom".into(),
            base_url: "".into(),
            model: "m".into(),
            models: vec![],
            key_cipher: None,
            context_length: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            thinking: Some("off".into()),
            thinking_style: Some("qwen".into()),
            capabilities: Default::default(),
            capabilities_user_set: false,
        };
        let p = build_chat_params(&c, preset);
        assert_eq!(p.thinking_format, Some(gateway::compat::ThinkingFormat::Qwen));
        // 覆盖后 off 真的会发 enable_thinking:false
        let body = gateway::build_body("m", &json!([]), true, &p, None);
        assert_eq!(body["enable_thinking"], json!(false));
        // P8.3 旧设置值自动映射：thinking_object → zai
        c.thinking_style = Some("thinking_object".into());
        let p = build_chat_params(&c, preset);
        assert_eq!(p.thinking_format, Some(gateway::compat::ThinkingFormat::Zai));
        // none：任何思考参数都不发（none 用 openai 形态但不发档位=零思考字段）
        c.thinking_style = Some("none".into());
        let p = build_chat_params(&c, gateway::providers::find("zhipu"));
        let body = gateway::build_body("glm-4.6", &json!([]), true, &p, None);
        assert!(body.get("thinking").is_none() && body.get("enable_thinking").is_none());
        // auto：端点探测（zhipu 官方域名 → zai 形态）
        c.thinking_style = None;
        c.thinking = Some("on".into());
        let p = build_chat_params(&c, gateway::providers::find("zhipu"));
        let body = gateway::build_body("glm-4.6", &json!([]), true, &p, None);
        assert_eq!(p.thinking_format, None, "auto 不写覆盖，探测说了算");
        assert_eq!(body["thinking"], json!({"type":"enabled"}), "探测出 zai 形态后 on 生效");
    }

    #[test]
    fn effective_thinking_style_resolution() {
        let _ = gateway::providers::init(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("providers.json"),
        );
        let mut c = crate::config::settings::Config {
            id: "cfg-t".into(),
            name: "test".into(),
            provider_id: "custom".into(),
            base_url: "".into(),
            model: "m".into(),
            models: vec![],
            key_cipher: None,
            context_length: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            thinking: None,
            thinking_style: None,
            capabilities: Default::default(),
            capabilities_user_set: false,
        };
        // 自定义中转（空地址探测不出）→ openai 形态兜底
        c.provider_id = "custom".into();
        assert_eq!(effective_thinking_style(&c, gateway::providers::find("custom")), "openai");
        // 智谱官方域名探测 → zai
        c.provider_id = "zhipu".into();
        c.base_url = "https://open.bigmodel.cn/api/paas/v4".into();
        assert_eq!(effective_thinking_style(&c, gateway::providers::find("zhipu")), "zai");
        // 覆盖优先，旧词表也映射
        c.thinking_style = Some("enable_thinking_budget".into());
        assert_eq!(effective_thinking_style(&c, gateway::providers::find("zhipu")), "qwen");
        c.thinking_style = Some("none".into());
        assert_eq!(effective_thinking_style(&c, gateway::providers::find("zhipu")), "none");
    }

    #[test]
    fn thinking_accepts_effort_levels() {
        let _ = gateway::providers::init(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("providers.json"),
        );
        let mut c = crate::config::settings::Config {
            id: "cfg-t".into(),
            name: "test".into(),
            provider_id: "openai".into(),
            base_url: "".into(),
            model: "m".into(),
            models: vec![],
            key_cipher: None,
            context_length: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            thinking: None,
            thinking_style: None,
            capabilities: Default::default(),
            capabilities_user_set: false,
        };
        // 输入栏 🧠 菜单发的档位要能持久化（P8.3 期间被 put_settings 吞掉的老问题）
        for lvl in ["low", "medium", "high", "on", "off"] {
            c.thinking = Some(lvl.into());
            let p = build_chat_params(&c, gateway::providers::find("openai"));
            assert_eq!(p.thinking.as_deref(), Some(lvl));
        }
    }

    #[test]
    fn thinking_style_manual_override_end_to_end() {
        // A1（施工单 012）验收路径：custom 中转渠道手动指定 zai 形态 → 商汤式网关 on/off 生效
        let _ = gateway::providers::init(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("providers.json"),
        );
        let mut s = test_settings();
        let req = SettingsUpdate {
            thinking_style: Some("zai".into()),
            ..Default::default()
        };
        apply_settings_update(&mut s, &req);
        assert_eq!(s.active().unwrap().thinking_style.as_deref(), Some("zai"));
        let mut p = build_chat_params(s.active().unwrap(), gateway::providers::find("custom"));
        // 端点探测对中转域名给 openai 兜底；显式覆盖必须赢
        p.thinking = Some("on".into());
        let body = gateway::build_body(
            "deepseek-v4-flash",
            &json!([{ "role": "user", "content": "hi" }]),
            true,
            &p,
            None,
        );
        assert_eq!(body["thinking"], json!({"type":"enabled"}), "覆盖 zai 后 on 发 thinking 对象");
        p.thinking = Some("off".into());
        let body = gateway::build_body(
            "deepseek-v4-flash",
            &json!([{ "role": "user", "content": "hi" }]),
            true,
            &p,
            None,
        );
        assert_eq!(body["thinking"], json!({"type":"disabled"}), "覆盖 zai 后 off 生效");
        // auto/空串 = 清除显式覆盖，回到端点探测
        let req = SettingsUpdate {
            thinking_style: Some("auto".into()),
            ..Default::default()
        };
        apply_settings_update(&mut s, &req);
        assert!(s.active().unwrap().thinking_style.is_none(), "auto=清除覆盖");
    }

    #[test]
    fn capabilities_user_mark_survives_model_switch() {
        // 能力标记归用户所有：手动勾选后，任何模型切换都不得改写（推导机制已移除）
        let mut s = test_settings();
        // 模拟用户勾选「图片解析」→ capabilities_user_set 置位
        let req = SettingsUpdate {
            capabilities: Some(crate::config::settings::ModelCapabilities {
                tools: true,
                vision: true,
                video: false,
                audio: false,
            }),
            ..Default::default()
        };
        apply_settings_update(&mut s, &req);
        assert!(s.active().unwrap().capabilities_user_set);
        assert!(s.active().unwrap().capabilities.vision);
        // 切到别的模型：手动标记原样保留
        let req = SettingsUpdate {
            model: Some("qwen3.8-flash".into()),
            ..Default::default()
        };
        apply_settings_update(&mut s, &req);
        assert!(
            s.active().unwrap().capabilities.vision,
            "用户手动标记的 vision 不得被模型切换冲掉"
        );
        assert!(s.active().unwrap().capabilities_user_set);
        // 从未被标记过的配置切模型：能力标记同样保持不变（不再按模型名推导）
        let mut s2 = test_settings();
        let req = SettingsUpdate {
            model: Some("qwen-vl-max".into()),
            ..Default::default()
        };
        apply_settings_update(&mut s2, &req);
        let c = s2.active().unwrap();
        assert!(
            !c.capabilities.vision,
            "无用户标记时切模型也不再推导，保持默认（仅工具调用）"
        );
        assert!(c.capabilities.tools, "默认工具调用能力不受影响");
    }

    #[test]
    fn capabilities_tools_gate_wires_to_params() {
        // A3（施工单 012）：勾掉「工具调用」→ ChatParams.tools_enabled=false → 请求体无 tools
        let mut s = test_settings();
        let req = SettingsUpdate {
            capabilities: Some(crate::config::settings::ModelCapabilities {
                tools: false,
                vision: false,
                video: false,
                audio: false,
            }),
            ..Default::default()
        };
        apply_settings_update(&mut s, &req);
        let p = build_chat_params(s.active().unwrap(), gateway::providers::find("custom"));
        assert!(!p.tools_enabled);
        let body = gateway::build_body(
            "m",
            &json!([{ "role": "user", "content": "hi" }]),
            true,
            &p,
            None,
        );
        assert!(body.get("tools").is_none(), "tools=false 时请求体不得携带 tools");
        // 对照：默认（不勾掉）→ tools_enabled=true，agent 循环携带 tools
        let p = build_chat_params(&test_config(), gateway::providers::find("custom"));
        assert!(p.tools_enabled);
    }

    /// GET /api/settings/schema：全表 manifest 原样返回（条数/字段/红线/表外提示）。
    #[tokio::test]
    async fn settings_schema_endpoint_returns_full_manifest() {
        let Json(v) = get_settings_schema().await;
        // paths 数组长度与 schema 表一致
        assert_eq!(
            v["paths"].as_array().unwrap().len(),
            crate::config::settings_schema::SCHEMA.len(),
            "paths 条数应等于 SCHEMA.len()"
        );
        // 表外路径提示含 api_key（key 永不外泄）
        assert!(
            v["out_of_scope"].as_str().unwrap().contains("api_key"),
            "out_of_scope 应声明 api_key 不在表内"
        );
        // 每条路径五个必备字段（Rust 侧 SchemaEntry.ty 在 JSON 里键名为 type）
        assert!(
            v["paths"].as_array().unwrap().iter().all(|p| {
                p.get("path").is_some()
                    && p.get("label").is_some()
                    && p.get("type").is_some()
                    && p.get("risk").is_some()
                    && p.get("desc").is_some()
            }),
            "paths 每条都要有 path/label/type/risk/desc"
        );
        // 红线集合 2026-10-04 放开权限轮后应为空（审批档位不在 settings.json 表内，
        // api_key 本就表外）——settings.json 里不再有 red_line 行
        let red: Vec<String> = v["paths"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["risk"] == "red_line")
            .map(|p| p["path"].as_str().unwrap().to_string())
            .collect();
        assert!(red.is_empty(), "渠道键已放开，settings.json 表内不应再有 red_line: {red:?}");
        // 放开轮新增的 mcp_servers 行在表内且为 normal
        let mcp = v["paths"].as_array().unwrap().iter().find(|p| p["path"] == "mcp_servers").expect("mcp_servers 应入表");
        assert_eq!(mcp["risk"], "normal", "mcp_servers 应为 normal");
    }
}
