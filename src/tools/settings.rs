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
    /// 整表替换（先 get 拿现状再改）。2026-10-04 起装了就能用、无首装确认
    pub mcp_servers: Option<Vec<McpServerCfg>>,
    pub disabled_plugins: Option<Vec<String>>,
    pub disabled_skills: Option<Vec<String>>,
    /// 整表替换（先 get 拿现状再改）。新增任务 id 留空自动生成；带 id 改已有任务时
    /// 创建时间与运行态（上次运行/状态/错误）以旧值为准。
    pub schedules: Option<Vec<ScheduleTask>>,
    /// 2026-10-03 schema round: dot sub-paths (features.<key>/roles.<field>) collected apart.
    #[serde(skip)]
    pub dot_paths: Vec<(String, Value)>,
}

/// 解析 patch：顶层键走结构化 SettingsPatch（deny_unknown_fields 原样保留），
/// 点号路径（features.<键> / roles.<字段>）单收进 dot_paths——白名单即
/// config::settings_schema 全表。表外路径报错并列合法路径（宁可拒不静默）。
fn parse_patch(raw: &Value) -> Result<SettingsPatch, String> {
    let mut obj = match raw {
        Value::Object(o) => o.clone(),
        Value::Null => return Ok(SettingsPatch::default()),
        _ => return Err("patch 需要 JSON object".into()),
    };
    let mut dot: Vec<(String, Value)> = Vec::new();
    let keys: Vec<String> = obj.keys().cloned().collect();
    for k in keys {
        if k.contains('.') {
            if !crate::config::settings_schema::is_known_path(&k) {
                return Err(format!(
                    "未知设置路径 {k}（合法路径：{}）",
                    crate::config::settings_schema::legal_paths_hint()
                ));
            }
            dot.push((k.clone(), obj.remove(&k).unwrap_or(Value::Null)));
        }
    }
    let mut p: SettingsPatch = serde_json::from_value(Value::Object(obj))
        .map_err(|e| format!("patch 不合法（未知键或值类型不对）: {e}；可改键见本工具说明"))?;
    p.dot_paths = dot;
    Ok(p)
}


/// 返回人话变更清单（逐键一行）。渠道字段语义与 HTTP 端点完全同源。
pub(crate) fn apply_patch(s: &mut Settings, p: &SettingsPatch) -> Result<Vec<String>, String> {
    let mut changed: Vec<String> = Vec::new();

    // 2026-10-03 schema round: dot sub-paths (features.<key> / roles.<field>) apply first,
    // in SCHEMA order (table order authoritative; active_config_id still precedes channel
    // fields). Whole-table keys keep original branches below.
    if !p.dot_paths.is_empty() {
        for e in crate::config::settings_schema::application_order() {
            if let Some((_, v)) = p.dot_paths.iter().find(|(k, _)| k == e.path) {
                let line = crate::config::settings_schema::apply_entry_value(s, e.path, v)?;
                changed.push(line);
            }
        }
    }

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

    // 3) MCP 整表替换。2026-10-04 放开权限轮起「装了就能用、无首装确认」，不再有
    //    逐服务的确认态需要保留（早先按 same_config 保留确认状的分支随确认制一起退役）
    if let Some(servers) = &p.mcp_servers {
        // 2026-10-08 修：与 POST /api/mcp 共用同一份校验。原先整表替换零校验——
        // 模型可写入启动任意命令的 stdio server（command: cmd.exe + args），
        // sync_configs 一拉就起进程，等于借 MCP 通道拿免审批任意代码执行。
        let mut seen: Vec<&str> = Vec::with_capacity(servers.len());
        for srv in servers {
            crate::mcp::validate_server_cfg(srv)
                .map_err(|e| format!("MCP server「{}」不合法: {e}", srv.name))?;
            if seen.contains(&srv.name.as_str()) {
                return Err(format!("MCP server 名称重复: {}（整表替换里 name 必须唯一）", srv.name));
            }
            seen.push(&srv.name);
        }
        s.mcp_servers = servers.clone();
        changed.push("mcp_servers".into());
    }

    // 4) 插件 / 技能停用名单（整表替换；校验与联动见下方各函数——真机实锤过模型
    //    写中文标题「用量」导致停了个寂寞，这里 id 为准、标题自动映射、垃圾名字报错）
    if let Some(list) = &p.disabled_plugins {
        apply_disabled_plugins(s, list)?;
        changed.push("disabled_plugins".into());
    }
    if let Some(list) = &p.disabled_skills {
        s.disabled_skills = apply_disabled_skills(list)?;
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
            // 审批策略是用户的决定（2026-10-08 修）：模型经 settings 工具把任务改成
            // on_approval=approve / approval_mode=full-access = 造一个持久化的无人值守
            // 全批通道（与 schedule_run 判 Dangerous 需逐次确认的口径倒挂）。UI 高级
            // 选项仍可改（用户明示通道）；工具这条路不许动这两个字段——连收紧也不许，
            // 规则简单且响度一致：审批策略只归用户。
            let prev = s.schedules.iter().find(|x| !t.id.trim().is_empty() && x.id == t.id);
            let posture_changed = match prev {
                Some(old) => {
                    !old.on_approval.eq_ignore_ascii_case(&t.on_approval)
                        || !old.approval_mode.eq_ignore_ascii_case(&t.approval_mode)
                }
                None => {
                    t.on_approval.eq_ignore_ascii_case("approve") || t.approval_mode.eq_ignore_ascii_case("full-access")
                }
            };
            if posture_changed {
                return Err(format!(
                    "定时任务「{}」的审批策略（on_approval / approval_mode）只能由用户在设置→定时的高级选项里改：无人值守任务自动放行高危操作必须用户明示。请保留原值或留空。",
                    t.name
                ));
            }
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

/// 工具执行入口。
/// - action=get：脱敏快照（不受自改闸，只读）
/// - action=set：补丁合并（受闸）
/// - action=config：渠道配置新建/删除（受闸；删除高危必问）
/// - action=mcp：MCP server 重连（受闸）
/// - action=workspace：切换工作区（受闸 + 红线必问——改自己的文件边界）
/// - action=alias：设置/清除工作区别名（侧栏显示名；受闸，Normal）
/// - action=schedule_run：立即触发一条定时任务（受闸；真花钱，高危必问）
/// - action=memory：记忆库重建索引（受闸）
pub(crate) async fn run(args: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("").to_string();
    match action.as_str() {
        // schema: settable-path manifest (read-only, no selfmod gate; 2026-10-03 schema round)
        "schema" => Ok(crate::config::settings_schema::manifest_json().to_string()),
        // set_config: config.toml 键改写（2026-10-04 放开权限轮；toml_edit 保注释，
        // 改完需重启生效）。default_approval（审批档位红线）判级层已拦，这里不再重复。
        "set_config" => {
            super::selfmod_gate()?;
            let key = v.get("key").and_then(|x| x.as_str()).unwrap_or("").trim();
            if key.is_empty() {
                return Err("set_config 需要 key（形如 agent.max_steps / cost.spend_cap_usd）".into());
            }
            let value = v.get("value").cloned().unwrap_or(Value::Null);
            crate::config::set_config_key(key, &value)
        }
        "get" => {
            let handle = settings::shared_settings()
                .ok_or_else(|| "设置子系统尚未初始化（共享句柄缺失）".to_string())?;
            let s = handle.read().unwrap_or_else(|e| e.into_inner()).clone();
            Ok(crate::context::compress(&snapshot_json(&s), 8000))
        }
        // —— 以下全部受「中枢」插件闸（闸在分发层；助手函数保持纯净供测试直达） ——
        "set" => {
            super::selfmod_gate()?;
            let raw = v.get("patch").cloned().unwrap_or(Value::Null);
            let patch: SettingsPatch = parse_patch(&raw)?;
            let handle =
                settings::shared_settings().ok_or_else(|| "设置子系统尚未初始化（共享句柄缺失）".to_string())?;
            // 2026-10-08 修：与 HTTP 端点同一把写锁——agent 经工具改设置与用户经
            // 界面改设置并发时，两条读-改-写链路互不踩（后写者整份覆盖先写者的坑）。
            let write_lock = crate::server::shared_state()
                .map(|s| s.settings_write.clone())
                .ok_or_else(|| "设置子系统尚未初始化".to_string())?;
            let _w = write_lock.lock().unwrap_or_else(|e| e.into_inner());
            let mut s = handle.read().unwrap_or_else(|e| e.into_inner()).clone();
            let changed = apply_patch(&mut s, &patch)?;
            s.save().map_err(|e| format!("保存 settings.json 失败: {e}"))?;
            *handle.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
            crate::server::settings::sync_runtime(&s);
            // 让打开中的界面即时跟上：插件页/侧栏重拉清单（不然 agent 改完，用户看到的
            // 还是旧状态——真机实锤的坑）
            crate::server::ui_events::emit(json!({ "kind": "settings_changed", "keys": changed }));
            crate::observability::event(
                crate::observability::EventType::State,
                json!({ "kind": "settings.agent_updated", "keys": changed }),
            );
            Ok(format!("已保存并即时生效（{}）", changed.join("、")))
        }
        "config" => {
            super::selfmod_gate()?;
            config_op(&v).await
        }
        "mcp" => {
            super::selfmod_gate()?;
            mcp_refresh(&v).await
        }
        "workspace" => {
            super::selfmod_gate()?;
            switch_workspace(&v).await
        }
        // 工作区别名（侧栏「重命名项目」）：只改显示名，不动目录、不动文件边界
        "alias" => {
            super::selfmod_gate()?;
            set_workspace_alias(&v)
        }
        "schedule_run" => {
            super::selfmod_gate()?;
            schedule_run_now(&v).await
        }
        "memory" => {
            super::selfmod_gate()?;
            memory_op(&v).await
        }
        other => Err(format!(
            "未知 action: {other:?}（get / set / config / mcp / workspace / schedule_run / memory）"
        )),
    }
}

/// action=config：渠道配置新建（op=new，复制当前并激活）与删除（op=delete+id）。
async fn config_op(v: &Value) -> Result<String, String> {
    let handle =
        settings::shared_settings().ok_or_else(|| "设置子系统尚未初始化".to_string())?;
    let mut s = handle.read().unwrap_or_else(|e| e.into_inner()).clone();
    match v.get("op").and_then(|x| x.as_str()).unwrap_or("") {
        "new" => {
            // 不抢激活位：新配置不带 key（本工具拒绝碰 key），激活了等于把全服务
            // 打进「API key 未设置」——真机 E2E 实锤。建好放在旁边，key 请用户到设置页
            // 补，补完再 active_config_id 切换（切换是红线键，会弹卡）。
            let prev_active = s.active_config_id.clone();
            let id = s.new_config();
            if let Some(prev) = prev_active
                && !s.activate(&prev) {
                    s.ensure_active_config();
                }
            s.save().map_err(|e| format!("保存失败: {e}"))?;
            *handle.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
            crate::server::settings::sync_runtime(&s);
            crate::server::ui_events::emit(json!({ "kind": "settings_changed", "keys": ["configs"] }));
            Ok(format!("已新建渠道配置 id={id}（起点复制自原激活配置，当前激活没动——新配置没有密钥，要补 key 或改它的 base_url 请到设置页操作，或用 action=set 的 name/base_url 键）"))
        }
        "delete" => {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").trim();
            if id.is_empty() {
                return Err("缺少 id（action=get 看 configs 清单）".into());
            }
            if !s.delete_config(id) {
                return Err(format!("配置不存在: {id}"));
            }
            s.save().map_err(|e| format!("保存失败: {e}"))?;
            *handle.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
            crate::server::settings::sync_runtime(&s);
            crate::server::ui_events::emit(json!({ "kind": "settings_changed", "keys": ["configs"] }));
            Ok(format!("已删除渠道配置 {id}（它带的密钥一并清除）"))
        }
        other => Err(format!("未知 op: {other:?}（new / delete）")),
    }
}

/// action=mcp：MCP server 重连（只重连+重新发现工具，不改配置）。
async fn mcp_refresh(v: &Value) -> Result<String, String> {
    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if name.is_empty() {
        return Err("缺少 name（action=get 看 mcp_servers 清单）".into());
    }
    let known = settings::shared_settings()
        .map(|h| h.read().map(|s| s.mcp_servers.iter().any(|c| c.name == name)).unwrap_or(false))
        .unwrap_or(false);
    if !known {
        return Err(format!("未知 MCP server: {name}"));
    }
    let n = tokio::task::spawn_blocking(move || crate::mcp::ensure_online_by_name(&name))
        .await
        .map_err(|e| format!("重连任务失败: {e}"))?;
    match n {
        Ok(count) => Ok(format!("已重连并重新发现，找到 {count} 个工具")),
        Err(e) => Err(format!("重连失败: {e}")),
    }
}

/// action=alias：给工作区起侧栏显示名（不碰目录本身，所以与 action=workspace 的
/// 「改自己的文件边界」分级不同——这是可逆的展示层改动，Normal 按审批档位走）。
/// path 省略 = 当前工作区；name 留空 = 清除别名回落目录名。与侧栏「重命名项目」
/// 同一条落盘路（server::workspace::set_workspace_alias），改完界面即时刷新。
fn set_workspace_alias(v: &Value) -> Result<String, String> {
    let path = v
        .get("path")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| crate::tools::workspace_root().display().to_string());
    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let state = crate::server::shared_state().ok_or("工作区子系统尚未初始化")?;
    crate::server::workspace::set_workspace_alias(&state, &path, &name)?;
    if name.is_empty() {
        Ok(format!("已清除 {path} 的别名（侧栏恢复显示文件夹名）"))
    } else {
        Ok(format!(
            "已把 {path} 的侧栏显示名改成「{name}」（只改显示名，目录没动；开着的界面已即时刷新）"
        ))
    }
}

/// action=workspace：切换工作区（红线：改自己的文件边界 + 记忆层重开，任何档位必问）。
async fn switch_workspace(v: &Value) -> Result<String, String> {
    let path = v.get("path").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if path.is_empty() {
        return Err("缺少 path（目录绝对路径）".into());
    }
    let p = std::path::PathBuf::from(&path);
    if !p.is_dir() {
        return Err(format!("目录不存在: {path}"));
    }
    let state = crate::server::shared_state().ok_or("工作区子系统尚未初始化")?;
    let canon = crate::server::workspace::canon_workspace(p);
    crate::server::workspace::apply_workspace(&state, &canon)
        .map_err(|e| format!("切换失败: {e}"))?;
    Ok(format!(
        "已切换工作区到 {}（文件边界与记忆层已跟着切；旧工作区会话都还在侧栏原项目下）",
        canon.display()
    ))
}

/// action=schedule_run：立即触发一条定时任务（无头会话真跑，花真钱 → 高危必问）。
async fn schedule_run_now(v: &Value) -> Result<String, String> {
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if id.is_empty() {
        return Err("缺少 id（action=get 看 schedules 清单）".into());
    }
    let state = crate::server::shared_state().ok_or("定时任务子系统尚未初始化")?;
    crate::schedule::run_now(&state, &id).map(|_| format!("定时任务 {id} 已触发（无头会话在后台跑，结果进侧栏会话与通知）"))
}

/// action=memory：记忆库重建索引（强制忽略指纹，重新向量检索库）。
async fn memory_op(v: &Value) -> Result<String, String> {
    match v.get("op").and_then(|x| x.as_str()).unwrap_or("reindex") {
        "reindex" => {
            let state = crate::server::shared_state().ok_or("记忆库子系统尚未初始化")?;
            let lib = crate::server::memory::build_library(&state, true).await;
            // 键名与 build_library 的产出对齐（items；曾误读 points，永远报 0 个光点）
            let count = lib.get("items").and_then(|p| p.as_array()).map(|a| a.len()).unwrap_or(0);
            Ok(format!("记忆库索引已强制重建（{count} 个会话光点）"))
        }
        other => Err(format!("未知 op: {other:?}（reindex）")),
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

/// 插件停用名单（整表替换）的校验与联动：
/// - 名字以插件 id（目录名）为准；模型常拿中文标题来（实锤：「用量」），能对上标题的
///   自动映射成 id；对不上的直接报错并列出全部可用 id——宁可报错不静默落个废名单；
/// - 启用状态发生翻转的插件若 manifest 声明了 feature（插件开关 = 功能总开关，
///   2026-10-02 权限放开轮的联动语义），同步写 settings.features——与 UI 的
///   toggle_plugin 同一套，否则「插件停了功能还在跑」。
fn apply_disabled_plugins(s: &mut Settings, list: &[String]) -> Result<(), String> {
    let known = crate::plugin::ui::scan_all(&[]);
    let mut known = known;
    known.extend(crate::plugin::wasm::scan_plugins(&[]));

    let mut want: Vec<String> = Vec::new();
    for n in list {
        let n = n.trim();
        if n.is_empty() {
            continue;
        }
        // id 直认；标题映射；扫描不到任何插件（裸环境/测试）时放弃校验照收
        let id = if known.iter().any(|e| e.name == n) {
            n.to_string()
        } else {
            match known.iter().find(|e| e.title == n) {
                Some(e) => e.name.clone(),
                None if known.is_empty() => n.to_string(),
                None => {
                    let all = known.iter().map(|e| format!("{}({})", e.name, e.title)).collect::<Vec<_>>();
                    return Err(format!(
                        "未知插件: {n}（要用插件 id 不用中文标题。可用：{}）",
                        all.join("、")
                    ));
                }
            }
        };
        if !want.contains(&id) {
            want.push(id);
        }
    }

    // feature 联动：只对「这次发生翻转」的插件动 features（差集先拷自有，避免与
    // set_feature 的可变借用打架）
    let old_disabled = s.disabled_plugins.clone();
    let newly_off: Vec<String> =
        want.iter().filter(|n| !old_disabled.contains(n)).cloned().collect();
    let newly_on: Vec<String> =
        old_disabled.iter().filter(|n| !want.contains(n)).cloned().collect();
    for n in &newly_off {
        if let Some(e) = known.iter().find(|e| &e.name == n)
            && !e.feature.is_empty()
        {
            s.set_feature(&e.feature, false)?;
        }
    }
    for n in &newly_on {
        if let Some(e) = known.iter().find(|e| &e.name == n)
            && !e.feature.is_empty()
        {
            s.set_feature(&e.feature, true)?;
        }
    }

    s.disabled_plugins = want;
    Ok(())
}

/// 技能停用名单（整表替换）：技能名对不上就报错列可选（扫描不到技能的裸环境放弃校验）。
fn apply_disabled_skills(list: &[String]) -> Result<Vec<String>, String> {
    let known: Vec<String> = crate::skills::list().iter().map(|k| k.name.clone()).collect();
    let mut want: Vec<String> = Vec::new();
    for n in list {
        let n = n.trim();
        if n.is_empty() {
            continue;
        }
        if !known.is_empty() && !known.iter().any(|k| k == n) {
            return Err(format!("未知技能: {n}（可用：{}）", known.join("、")));
        }
        if !want.contains(&n.to_string()) {
            want.push(n.to_string());
        }
    }
    Ok(want)
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
        "mcp_servers": s.mcp_servers.iter().map(|c| json!({
            "name": c.name,
            "transport": c.transport,
            "command": c.command,
            "args": c.args,
            "url": c.url,
            "enabled": c.enabled,
            "has_oauth": c.oauth.is_some(),
        })).collect::<Vec<Value>>(),
        "disabled_plugins": s.disabled_plugins,
        "disabled_skills": s.disabled_skills,
        // 工作区别名现状（只读可见）：模型要「把项目名改成 X」时先看这里有没有旧别名、
        // 键长什么样（规范化绝对路径），别凭猜写 path 落个对不上的空别名
        "workspace": crate::tools::workspace_root().display().to_string(),
        "workspace_aliases": s.workspace_aliases,
        // 可选名单：停用名单是整表替换，模型得知道有哪些合法 id（id + 中文标题）
        "available_plugins": crate::plugin::ui::scan_all(&[])
            .iter()
            .chain(crate::plugin::wasm::scan_plugins(&[]).iter())
            .map(|e| json!({ "id": e.name, "title": e.title, "feature": e.feature }))
            .collect::<Vec<Value>>(),
        "available_skills": crate::skills::list().iter().map(|k| k.name.clone()).collect::<Vec<String>>(),
        "schedules": s.schedules,
        "red_line_keys": crate::config::settings_schema::red_line_paths(),
        "out_of_scope": "api_key/价格表/信任插件不可经本工具修改；工作区目录切换走 action=workspace、显示名走 action=alias；config.toml（花费上限/审批默认档/钩子）改文件且需重启，写前必经用户确认",
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
        };

        // 原样带回（比如只是想改 enabled）→ enabled 按整表语义保留
        let mut s = test_settings();
        s.mcp_servers.push(mk("node old.js"));
        let mut keep = mk("node old.js");
        keep.enabled = false;
        let p = SettingsPatch { mcp_servers: Some(vec![keep]), ..Default::default() };
        apply_patch(&mut s, &p).unwrap();
        assert!(!s.mcp_servers[0].enabled);
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
        // 技能名用环境里真实存在的（测试环境 app_root=仓库，skills/ 非空；校验对不上会报错）
        let skill = crate::skills::list()
            .first()
            .map(|k| k.name.clone())
            .unwrap_or_else(|| "any".into());
        let p = patch(json!({"disabled_skills": [skill], "disabled_plugins": [" css-ui "]}));
        apply_patch(&mut s, &p).unwrap();
        assert_eq!(s.disabled_skills, vec![skill]);
        assert_eq!(s.disabled_plugins, vec!["css-ui".to_string()], "名单条目去首尾空白");
        // 未知技能名直接报错（宁可报错不静默落废名单）
        if !crate::skills::list().is_empty() {
            let p = patch(json!({"disabled_skills": ["不存在的技能xyz"]}));
            assert!(apply_patch(&mut s, &p).is_err());
        }
    }

    #[test]
    fn schema_dot_paths_apply_and_unknown_rejected() {
        // 合法点号 + 顶层键混合：点号走 SCHEMA 表序，顶层键走原分支，各按各自语义落地
        let mut s = test_settings();
        let p = parse_patch(&json!({"features.galaxy": false, "roles.summary": "cfg-b", "thinking": "low"}))
            .expect("合法点号/顶层混合 patch 应解析通过");
        let changed = apply_patch(&mut s, &p).expect("合法混合 patch 应可应用");
        assert!(
            changed.iter().any(|x| x.as_str() == "features.galaxy"),
            "changed 应含点号路径行 features.galaxy: {changed:?}"
        );
        assert!(
            changed.iter().any(|x| x.as_str() == "roles.summary"),
            "changed 应含点号路径行 roles.summary: {changed:?}"
        );
        assert!(
            changed.iter().any(|x| x.as_str() == "thinking"),
            "changed 应含顶层键行 thinking: {changed:?}"
        );
        assert!(!s.features.galaxy, "features.galaxy 点号直写应生效");
        assert_eq!(
            s.roles.as_ref().unwrap().summary.as_deref(),
            Some("cfg-b"),
            "roles.summary 点号直写只改指定字段"
        );
        assert_eq!(
            s.active().unwrap().thinking.as_deref(),
            Some("low"),
            "同补丁的顶层 thinking 应照旧落到激活配置"
        );

        // 表外点号路径：解析层就拒，错误串点名路径并列合法路径
        let err = parse_patch(&json!({"nope.key": 1})).expect_err("表外点号路径必须报错");
        assert!(err.contains("nope.key"), "错误串要点名路径: {err}");
        assert!(err.contains("合法路径"), "错误串要列合法路径: {err}");

        // 表外顶层键：结构化 deny_unknown_fields 老路径不受影响（解析层即拒）
        assert!(
            parse_patch(&json!({"cost": {}})).is_err(),
            "范围外顶层键 cost 仍在解析层被拒（deny_unknown_fields 原样保留）"
        );

        // 空 patch：解析 Ok（default），apply 报「没有任何要改的键」
        let p = parse_patch(&Value::Null).expect("null patch 解析应 Ok");
        let err = apply_patch(&mut test_settings(), &p).expect_err("空 patch 应用应报错");
        assert!(err.contains("patch 里没有任何要改的键"), "错误串: {err}");

        // 自改总闸可用点路径关（语义=自改动作全拒，不是红线，无弹卡）
        let mut s = test_settings();
        let p = parse_patch(&json!({"features.self_mod": false})).expect("自改总闸点号路径应合法");
        apply_patch(&mut s, &p).expect("features.self_mod=false 应可应用");
        assert!(!s.features.self_mod, "自改总闸可经点号路径 features.self_mod 关闭");
    }
}

#[cfg(test)]
mod approval_posture_tests {
    use super::*;

    fn cfg() -> crate::config::settings::Config {
        crate::config::settings::Config {
            id: "cfg-a".into(),
            name: "主力".into(),
            provider_id: "custom".into(),
            base_url: "https://relay.example.com/v1".into(),
            model: "m-a".into(),
            models: vec!["m-a".into()],
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
        s.configs.push(cfg());
        s.active_config_id = Some("cfg-a".into());
        s
    }

    fn patch(json: Value) -> SettingsPatch {
        serde_json::from_value(json).expect("patch 应可解析")
    }

    fn sched(id: &str, on_approval: &str, approval_mode: &str) -> Value {
        json!({
            "id": id,
            "name": "巡检",
            "cron": "0 9 * * *",
            "message": "跑一遍构建",
            "on_approval": on_approval,
            "approval_mode": approval_mode,
        })
    }

    /// 2026-10-08 修：mcp_servers 整表替换原先零校验——模型可写入启动任意命令的
    /// stdio server 借 MCP 通道拿任意代码执行。现在与 POST /api/mcp 共用校验。
    #[test]
    fn mcp_servers_table_replacement_is_validated() {
        let mut s = test_settings();
        // 先铺一条合法的，之后所有非法补丁都不得动它
        let p = patch(json!({"mcp_servers": [{"name": "ok-srv", "transport": "http", "url": "https://mcp.example.com/"}]}));
        apply_patch(&mut s, &p).unwrap();
        // 非法：stdio 缺 command
        let p = patch(json!({"mcp_servers": [{"name": "bad", "transport": "stdio"}]}));
        let e = apply_patch(&mut s, &p).unwrap_err();
        assert!(e.contains("不合法") && e.contains("command"), "{e}");
        // 非法：transport 乱值 / 名称非法 / 重名 / http 缺 url
        let p = patch(json!({"mcp_servers": [{"name": "x", "transport": "carrier-pigeon"}]}));
        assert!(apply_patch(&mut s, &p).unwrap_err().contains("未知 transport"));
        let p = patch(json!({"mcp_servers": [{"name": "a__b", "transport": "http", "url": "https://x.example.com/"}]}));
        assert!(apply_patch(&mut s, &p).unwrap_err().contains("名称非法"));
        let p = patch(json!({"mcp_servers": [
            {"name": "dup", "transport": "http", "url": "https://a.example.com/"},
            {"name": "dup", "transport": "http", "url": "https://b.example.com/"}
        ]}));
        assert!(apply_patch(&mut s, &p).unwrap_err().contains("名称重复"));
        let p = patch(json!({"mcp_servers": [{"name": "nourl", "transport": "http"}]}));
        assert!(apply_patch(&mut s, &p).unwrap_err().contains("url"));
        // 校验失败不得落盘（表保持原样）
        assert_eq!(s.mcp_servers.len(), 1);
        assert_eq!(s.mcp_servers[0].name, "ok-srv");
        // 合法 stdio：command 是进程名，格式层面合法（与 POST /api/mcp 同规，
        // 拦「启动任意命令」靠的是审批档位与用户授权，不是格式校验）
        let p = patch(json!({"mcp_servers": [{"name": "cli", "transport": "stdio", "command": "cmd.exe", "args": ["/C", "whoami"]}]}));
        apply_patch(&mut s, &p).unwrap();
        assert_eq!(s.mcp_servers[0].name, "cli");
    }

    /// 2026-10-08 修：模型经 settings 工具把定时任务改成 approve/full-access =
    /// 造持久化无人值守全批通道。工具路径一律拦截；用户明示的 UI 通道不受影响
    /// （已在库的任务原值带回时放行——get→改→set 往返不误伤）。
    #[test]
    fn schedule_approval_posture_is_user_only() {
        let mut s = test_settings();
        // 新建任务带 approve：拒
        let p = patch(json!({"schedules": [sched("", "approve", "auto")]}));
        assert!(apply_patch(&mut s, &p).unwrap_err().contains("只能由用户"));
        // 新建任务带 full-access：拒
        let p = patch(json!({"schedules": [sched("", "deny", "full-access")]}));
        assert!(apply_patch(&mut s, &p).unwrap_err().contains("只能由用户"));
        // 新建任务 deny/auto：过
        let p = patch(json!({"schedules": [sched("", "deny", "auto")]}));
        apply_patch(&mut s, &p).unwrap();
        assert_eq!(s.schedules.len(), 1);
        // 把已存任务（deny）改成 approve：拒
        let id = s.schedules[0].id.clone();
        let p = patch(json!({"schedules": [sched(&id, "approve", "auto")]}));
        assert!(apply_patch(&mut s, &p).unwrap_err().contains("只能由用户"));
        // 原值带回（deny→deny）改别的字段：过
        let p = patch(json!({"schedules": [sched(&id, "deny", "auto")]}));
        apply_patch(&mut s, &p).unwrap();
        // 用户已在 UI 设了 approve 的任务，模型原值带回：过（不误伤用户明示的配置）
        s.schedules[0].on_approval = "approve".into();
        let p = patch(json!({"schedules": [sched(&id, "approve", "auto")]}));
        apply_patch(&mut s, &p).unwrap();
        // 乱值在 validate 层就拒（两条写入路同一道闸）
        let p = patch(json!({"schedules": [sched("", "yes-please", "auto")]}));
        assert!(apply_patch(&mut s, &p).unwrap_err().contains("on_approval 只能是"));
    }
}
