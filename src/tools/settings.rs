//! settings 工具（2026-10-03 agent 自改设置轮）：让模型读/改应用自身的运行时设置。
//!
//! 走「正门」而非直接写 settings.json：set 复用 HTTP 端点同一套字段语义
//! （`server::settings::apply_settings_update`）与落盘后的运行时同步（`sync_runtime`），
//! 改完即时生效。分级判定在 `core::rules`：
//! - get 只读 Safe；set 普通键（思考参数/角色路由/features/MCP/插件技能启停/定时任务）
//!   判 Normal（auto 档静默放行）；
//! - set 红线键（base_url/model/provider_id/active_config_id，渠道指向 = 换自己的大脑）
//!   判 Dangerous，且经 `rules::red_line_hold` 在**任何审批档位**（含完全访问）强制弹卡。
//!
//! 刻意不在范围内（防自提权/防 key 入日志）：api_key 与 clear_key（key 永不经模型之手，
//! 换 key 请用户在设置页操作）、workspace（自己挪自己的笼子）、cost（价格表 = 钱）、
//! trusted_plugins（准入审核归用户）。config.toml 的字段（花费上限/审批默认档/钩子等）
//! 不在本工具内——想改走文件编辑（属红线，写前必问）且需重启生效。

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::settings::{
    Settings,
    {self, FeatureSettings, McpServerCfg, ModelCapabilities, RolesSettings},
};
use crate::schedule::ScheduleTask;
use crate::server::settings::SettingsUpdate;

/// set 动作的补丁：只列要改的键。未知键直接报错（防手误写成静默不生效）。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsPatch {
    // —— 红线：渠道指向（换大脑，任何档位弹卡必问）——
    /// 切换激活配置（写 settings.json 里某套配置的 id，先 get 看清单）
    pub active_config_id: Option<String>,
    pub provider_id: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    // —— 普通：渠道周边（name 只是显示名；models 是模型下拉缓存）——
    pub name: Option<String>,
    pub models: Option<Vec<String>>,
    // —— 普通：模型行为参数（0=清除长度类，负数=清除采样类，空串=清除思考类）——
    pub context_length: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub thinking: Option<String>,
    pub thinking_style: Option<String>,
    pub capabilities: Option<ModelCapabilities>,
    // —— 普通：子系统配置 ——
    pub roles: Option<RolesSettings>,
    pub features: Option<FeatureSettings>,
    pub sandbox_cpu_percent: Option<u32>,
    /// 整表替换（先 get 拿现状再改）。新 server / 配置有改动的 server 会被强制回
    /// 「未确认」——工具清单要用户在界面上重新确认后才放行，模型不能自装自确认。
    pub mcp_servers: Option<Vec<McpServerCfg>>,
    pub disabled_plugins: Option<Vec<String>>,
    pub disabled_skills: Option<Vec<String>>,
    /// 整表替换（先 get 拿现状再改）。新增任务 id 留空自动生成；带 id 改已有任务时
    /// 创建时间与运行态（上次运行/状态/错误）以旧值为准。
    pub schedules: Option<Vec<ScheduleTask>>,
}

/// 把补丁合并进 Settings（不落盘、不同步——纯函数便于单测）。
/// 返回人话变更清单（逐键一行）。渠道字段语义与 HTTP 端点完全同源。
pub(crate) fn apply_patch(s: &mut Settings, p: &SettingsPatch) -> Result<Vec<String>, String> {
    let mut changed: Vec<String> = Vec::new();

    // 0) 切激活配置要放在所有渠道字段之前——后面改的是「切换后」的激活配置
    if let Some(id) = p.active_config_id.as_deref() {
        if !s.activate(id) {
            return Err(format!("配置不存在: {id}（先 action=get 查看全部配置 id）"));
        }
        changed.push(format!("active_config_id → {id}"));
    }

    // 1) 渠道与参数字段走 HTTP 端点同一套纯函数（0=清长度、负数=清采样、空串=清思考）。
    //    model 单独第二遍应用：改 base_url 会清空模型与模型列表（URL 变了旧列表作废），
    //    同一个补丁里同时给 base_url+model 时第二遍保证模型落得住。
    let upd = SettingsUpdate {
        name: p.name.clone(),
        provider_id: p.provider_id.clone(),
        base_url: p.base_url.clone(),
        models: p.models.clone(),
        context_length: p.context_length,
        max_output_tokens: p.max_output_tokens,
        temperature: p.temperature,
        top_p: p.top_p,
        thinking: p.thinking.clone(),
        thinking_style: p.thinking_style.clone(),
        capabilities: p.capabilities.clone(),
        ..Default::default()
    };
    let (url_changed, _, _) = crate::server::settings::apply_settings_update(s, &upd);
    if url_changed {
        changed.push("base_url（渠道地址变更：模型与模型列表已清空，需重新拉取）".into());
    }
    if p.model.is_some() {
        let upd_model = SettingsUpdate { model: p.model.clone(), ..Default::default() };
        let _ = crate::server::settings::apply_settings_update(s, &upd_model);
        changed.push(format!("model → {}", p.model.as_deref().unwrap_or("")));
    }
    if p.name.is_some() {
        changed.push("name".into());
    }
    if p.provider_id.is_some() {
        changed.push("provider_id".into());
    }
    if p.models.is_some() {
        changed.push(if url_changed { "models（随渠道地址变更已清空）".into() } else { "models".into() });
    }
    for (given, key) in [
        (p.context_length.is_some(), "context_length"),
        (p.max_output_tokens.is_some(), "max_output_tokens"),
        (p.temperature.is_some(), "temperature"),
        (p.top_p.is_some(), "top_p"),
        (p.thinking.is_some(), "thinking"),
        (p.thinking_style.is_some(), "thinking_style"),
        (p.capabilities.is_some(), "capabilities"),
    ] {
        if given {
            changed.push(key.to_string());
        }
    }

    // 2) 角色路由 / 功能开关 / 沙箱 CPU 上限——与 put_settings 同语义
    if let Some(roles) = p.roles.clone() {
        s.roles = Some(roles);
        changed.push("roles".into());
    }
    if let Some(features) = p.features.clone() {
        s.features = features;
        changed.push("features".into());
    }
    if let Some(cpu) = p.sandbox_cpu_percent {
        s.sandbox_cpu_percent = Some(cpu.min(100));
        changed.push("sandbox_cpu_percent".into());
    }

    // 3) MCP 整表替换：配置没变的条目保留用户已确认状态；新条目或配置有改动的条目
    //    强制回未确认——工具清单必须用户重新确认才放行（总架构 §7 tool poisoning 防线）
    if let Some(servers) = &p.mcp_servers {
        let mut next: Vec<McpServerCfg> = Vec::with_capacity(servers.len());
        for srv in servers {
            let mut c = srv.clone();
            let old = s.mcp_servers.iter().find(|o| o.name == c.name);
            let same_config = matches!(&old, Some(o)
                if o.transport == c.transport
                    && o.command == c.command
                    && o.args == c.args
                    && o.env == c.env
                    && o.url == c.url
                    && o.headers == c.headers
                    && o.oauth == c.oauth);
            match (&old, same_config) {
                (Some(o), true) => {
                    c.confirmed = o.confirmed;
                    c.risk = o.risk.clone();
                }
                _ => c.confirmed = false,
            }
            next.push(c);
        }
        s.mcp_servers = next;
        changed.push("mcp_servers".into());
    }

    // 4) 插件 / 技能停用名单（整表替换）
    if let Some(list) = &p.disabled_plugins {
        s.disabled_plugins = list.iter().map(|x| x.trim().to_string()).collect();
        changed.push("disabled_plugins".into());
    }
    if let Some(list) = &p.disabled_skills {
        s.disabled_skills = list.iter().map(|x| x.trim().to_string()).collect();
        changed.push("disabled_skills".into());
    }

    // 5) 定时任务整表替换：新增补 id/创建时间；改动保留运行态（与 HTTP 端点同规）
    if let Some(tasks) = &p.schedules {
        let mut next: Vec<ScheduleTask> = Vec::with_capacity(tasks.len());
        for t in tasks {
            let mut t = t.clone();
            t.name = t.name.trim().to_string();
            t.cron = t.cron.trim().to_string();
            t.message = t.message.trim().to_string();
            t.session_id = t.session_id.trim().to_string();
            t.validate().map_err(|e| format!("定时任务「{}」不合法: {e}", t.name))?;
            if t.id.trim().is_empty() {
                t.id = uuid::Uuid::new_v4().to_string();
                t.created_ts = chrono::Local::now().timestamp();
            } else if let Some(old) = s.schedules.iter().find(|x| x.id == t.id) {
                t.created_ts = old.created_ts;
                t.last_run_ts = old.last_run_ts;
                t.last_manual_ts = old.last_manual_ts;
                t.last_status = old.last_status.clone();
                t.last_error = old.last_error.clone();
                t.last_session_id = old.last_session_id.clone();
            } else {
                return Err(format!("定时任务 id 不存在: {}（新增请把 id 留空）", t.id));
            }
            next.push(t);
        }
        s.schedules = next;
        changed.push("schedules".into());
    }

    if changed.is_empty() {
        return Err("patch 里没有任何要改的键".into());
    }
    Ok(changed)
}

/// 工具执行入口：action=get 返回脱敏快照；action=set 应用补丁并落盘+同步。
pub(crate) async fn run(args: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let handle =
        settings::shared_settings().ok_or_else(|| "设置子系统尚未初始化（共享句柄缺失）".to_string())?;
    match v.get("action").and_then(|x| x.as_str()) {
        Some("get") => {
            let s = handle.read().unwrap_or_else(|e| e.into_inner()).clone();
            Ok(crate::context::compress(&snapshot_json(&s), 8000))
        }
        Some("set") => {
            let patch: SettingsPatch =
                serde_json::from_value(v.get("patch").cloned().unwrap_or(Value::Null))
                    .map_err(|e| format!("patch 不合法（未知键或值类型不对）: {e}；可改键见本工具说明"))?;
            let mut s = handle.read().unwrap_or_else(|e| e.into_inner()).clone();
            let changed = apply_patch(&mut s, &patch)?;
            s.save().map_err(|e| format!("保存 settings.json 失败: {e}"))?;
            *handle.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
            crate::server::settings::sync_runtime(&s);
            crate::observability::event(
                crate::observability::EventType::State,
                json!({ "kind": "settings.agent_updated", "keys": changed }),
            );
            Ok(format!("已保存并即时生效（{}）", changed.join("、")))
        }
        other => Err(format!("未知 action: {other:?}（只支持 get / set）")),
    }
}

/// 尾 4 位提示（完整 key 不出内存、不落日志，与 HTTP 端点同一口径）。
fn key_hint_of(key: Option<String>) -> String {
    key.as_ref()
        .map(|k| {
            let n = k.chars().count();
            if n > 4 { k.chars().skip(n - 4).collect::<String>() } else { k.clone() }
        })
        .unwrap_or_default()
}

/// 当前设置快照（脱敏）：key 只给 has_key/尾号；MCP 的 env/headers/oauth 细节不吐
/// （可能带机密，与「值只在内存与 settings.json，绝不进日志」的既有约束一致）。
fn snapshot_json(s: &settings::Settings) -> String {
    json!({
        "active_config_id": s.active_config_id,
        "configs": s.configs.iter().map(|c| {
            let k = s.config_key(c);
            json!({
                "id": c.id,
                "name": c.name,
                "provider_id": c.provider_id,
                "base_url": c.base_url,
                "model": c.model,
                "models": c.models,
                "has_key": k.is_some(),
                "key_hint": key_hint_of(k),
                "context_length": c.context_length,
                "max_output_tokens": c.max_output_tokens,
                "temperature": c.temperature,
                "top_p": c.top_p,
                "thinking": c.thinking,
                "capabilities": c.capabilities,
            })
        }).collect::<Vec<Value>>(),
        "roles": s.roles,
        "features": s.features,
        "sandbox_cpu_percent": s.sandbox_cpu_percent,
        "mcp_servers": s.mcp_servers.iter().map(|c| json!({
            "name": c.name,
            "transport": c.transport,
            "command": c.command,
            "args": c.args,
            "url": c.url,
            "enabled": c.enabled,
            "confirmed": c.confirmed,
            "risk": c.risk,
            "has_oauth": c.oauth.is_some(),
        })).collect::<Vec<Value>>(),
        "disabled_plugins": s.disabled_plugins,
        "disabled_skills": s.disabled_skills,
        "schedules": s.schedules,
        "red_line_keys": ["base_url", "model", "provider_id", "active_config_id"],
        "out_of_scope": "api_key/工作区/价格表/信任插件不可经本工具修改；config.toml（花费上限/审批默认档/钩子）改文件且需重启，写前必经用户确认",
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::{Config, Settings};

    fn config(id: &str, name: &str, base_url: &str, model: &str) -> Config {
        Config {
            id: id.into(),
            name: name.into(),
            provider_id: "custom".into(),
            base_url: base_url.into(),
            model: model.into(),
            models: vec![model.into()],
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
        s.configs.push(config("cfg-a", "主力", "https://relay.example.com/v1", "m-a"));
        s.configs.push(config("cfg-b", "备用", "https://backup.example.com/v1", "m-b"));
        s.active_config_id = Some("cfg-a".into());
        s
    }

    fn patch(json: Value) -> SettingsPatch {
        serde_json::from_value(json).expect("patch 应可解析")
    }

    #[test]
    fn base_url_and_model_in_one_patch_both_land() {
        // 改渠道地址会清空模型（URL 变了旧模型作废），但同一补丁里的 model 要落得住
        let mut s = test_settings();
        let p = patch(json!({"base_url": "https://new.example.com/v1", "model": "glm-5.3-flash"}));
        let changed = apply_patch(&mut s, &p).unwrap();
        let c = s.active().unwrap();
        assert_eq!(c.base_url, "https://new.example.com/v1");
        assert_eq!(c.model, "glm-5.3-flash", "同补丁的 model 不得被 url 变更清掉");
        assert!(c.models.is_empty(), "旧模型列表应被清空");
        assert!(changed.iter().any(|x| x.starts_with("base_url")));
        assert!(changed.iter().any(|x| x.starts_with("model")));
    }

    #[test]
    fn thinking_and_features_apply() {
        let mut s = test_settings();
        let p = patch(json!({"thinking": "high", "features": {"roles": false}}));
        apply_patch(&mut s, &p).unwrap();
        assert_eq!(s.active().unwrap().thinking.as_deref(), Some("high"));
        assert!(!s.features.roles);
    }

    #[test]
    fn activate_switches_target_config() {
        let mut s = test_settings();
        let p = patch(json!({"active_config_id": "cfg-b", "thinking": "low"}));
        apply_patch(&mut s, &p).unwrap();
        assert_eq!(s.active_config_id.as_deref(), Some("cfg-b"));
        assert_eq!(s.active().unwrap().base_url, "https://backup.example.com/v1");
        assert_eq!(s.active().unwrap().thinking.as_deref(), Some("low"), "参数应落到切换后的激活配置");
        // 不存在的 id 报错
        let mut s2 = test_settings();
        let p = patch(json!({"active_config_id": "nope"}));
        assert!(apply_patch(&mut s2, &p).is_err());
    }

    #[test]
    fn empty_and_unknown_patches_rejected() {
        let mut s = test_settings();
        // 范围外键（cost/工作区/trusted_plugins 等）在解析层就被拒
        assert!(
            serde_json::from_value::<SettingsPatch>(json!({"cost": {"currency": "CNY"}})).is_err(),
            "未知/范围外键必须在解析层报错，不得静默忽略"
        );
        // 空补丁：没有任何改动
        let p = patch(json!({}));
        assert!(apply_patch(&mut s, &p).is_err());
        // 正常补丁仍可用
        let p = patch(json!({"thinking": "high"}));
        assert!(apply_patch(&mut s, &p).is_ok());
    }

    #[test]
    fn mcp_new_or_changed_server_requires_reconfirm() {
        use crate::config::settings::McpServerCfg;
        let mk = |cmd: &str| McpServerCfg {
            name: "fs".into(),
            transport: "stdio".into(),
            command: cmd.into(),
            args: vec![],
            env: Default::default(),
            url: String::new(),
            headers: Default::default(),
            oauth: None,
            enabled: true,
            confirmed: true, // 模型传 true 也要被守卫改写
            risk: "normal".into(),
        };

        // 原样带回（比如只是想改 enabled）→ 保留已确认
        let mut s = test_settings();
        s.mcp_servers.push(mk("node old.js"));
        let mut keep = mk("node old.js");
        keep.enabled = false;
        let p = SettingsPatch { mcp_servers: Some(vec![keep]), ..Default::default() };
        apply_patch(&mut s, &p).unwrap();
        assert!(s.mcp_servers[0].confirmed, "配置未变的 server 保留确认态");
        assert!(!s.mcp_servers[0].enabled);

        // 命令变了 → 强制回未确认（哪怕模型传 confirmed=true）
        let mut s2 = test_settings();
        s2.mcp_servers.push(mk("node old.js"));
        let p = SettingsPatch { mcp_servers: Some(vec![mk("node NEW.js")]), ..Default::default() };
        apply_patch(&mut s2, &p).unwrap();
        assert!(!s2.mcp_servers[0].confirmed, "配置有改动的 server 必须重新确认");

        // 全新 server → 未确认
        let mut s3 = test_settings();
        let p = SettingsPatch { mcp_servers: Some(vec![mk("node new.js")]), ..Default::default() };
        apply_patch(&mut s3, &p).unwrap();
        assert!(!s3.mcp_servers[0].confirmed, "新 server 不得由模型自确认");
    }

    #[test]
    fn schedules_add_and_update_preserve_runtime() {
        let task = |v: Value| -> ScheduleTask { serde_json::from_value(v).unwrap() };
        let mut s = test_settings();
        s.schedules.push(task(json!({
            "id": "t1", "name": "早报", "cron": "0 9 * * *", "message": "早安",
            "created_ts": 1700000000, "last_run_ts": 1700000100, "last_status": "ok"
        })));

        // 整表替换：改已有任务（换 cron）+ 同时新增一个（id 留空自动生成）
        let upd = task(json!({"id": "t1", "name": "早报", "cron": "0 8 * * *", "message": "早!"}));
        let fresh = task(json!({"name": "晚报", "cron": "0 21 * * *", "message": "晚安"}));
        let p = SettingsPatch { schedules: Some(vec![upd, fresh]), ..Default::default() };
        apply_patch(&mut s, &p).unwrap();
        assert_eq!(s.schedules.len(), 2);
        assert_eq!(s.schedules[0].cron, "0 8 * * *");
        assert_eq!(s.schedules[0].created_ts, 1700000000, "创建时间以旧值为准");
        assert_eq!(s.schedules[0].last_run_ts, Some(1700000100), "运行态以旧值为准");
        assert_eq!(s.schedules[0].last_status.as_deref(), Some("ok"));
        assert!(!s.schedules[1].id.is_empty(), "新增任务应自动生成 id");
        assert!(s.schedules[1].created_ts > 0);

        // 整表替换漏带已有任务 = 删除（语义如此；工具说明要求先 get 再改）
        let only_fresh = task(json!({"name": "夜报", "cron": "0 23 * * *", "message": "睡了"}));
        let p = SettingsPatch { schedules: Some(vec![only_fresh]), ..Default::default() };
        apply_patch(&mut s, &p).unwrap();
        assert_eq!(s.schedules.len(), 1);

        // 编造 id → 报错
        let bogus = task(json!({"id": "nope", "name": "x", "cron": "0 9 * * *", "message": "y"}));
        let p = SettingsPatch { schedules: Some(vec![bogus]), ..Default::default() };
        assert!(apply_patch(&mut s, &p).is_err());
    }

    #[test]
    fn disabled_lists_replace() {
        let mut s = test_settings();
        s.disabled_skills = vec!["旧技能".into()];
        let p = patch(json!({"disabled_skills": ["git-helper"], "disabled_plugins": [" css-ui "]}));
        apply_patch(&mut s, &p).unwrap();
        assert_eq!(s.disabled_skills, vec!["git-helper".to_string()]);
        assert_eq!(s.disabled_plugins, vec!["css-ui".to_string()], "名单条目去首尾空白");
    }
}
