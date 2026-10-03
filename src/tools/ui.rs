//! ui 工具（2026-10-03 agent 自改设置轮；同日大轮扩展）：操作本应用自己的界面。
//!
//! 侧栏标签/当前会话/弹层是前端状态（rpTabs / galaxyOpen），后端唯一能驱动它们的路是
//! /api/ui/events 事件流：open_panel/close_panel/open_file/open_memory 推事件，前端执行
//! 与用户手点完全相同的函数（用户看着的界面即时变化）。
//! 面板白名单与前端 RP_EXT_PANELS 对齐（trace 时间线 / board 项目文件 / usage 用量）。
//! 除 list_panels（只读）外全部受「自改」插件闸；会话动作在 session 工具里。

use serde_json::{Value, json};

/// 侧栏面板白名单（与前端 RP_EXT_PANELS 同源；两边改动要同步）。
const PANELS: &[(&str, &str)] = &[("trace", "时间线"), ("board", "项目文件"), ("usage", "用量")];

pub(crate) async fn run(args: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let method = v.get("method").and_then(|x| x.as_str()).unwrap_or("");
    match method {
        // —— 只读，不受闸 ——
        "list_panels" => {
            let disabled = current_disabled();
            let list: Vec<Value> = PANELS
                .iter()
                .map(|(name, title)| {
                    json!({ "name": name, "title": title, "enabled": !disabled.iter().any(|d| d == name) })
                })
                .collect();
            Ok(json!({
                "panels": list,
                "note": "name 是插件 id（settings 工具的 disabled_plugins 用它，别写中文标题）；会话管理在 session 工具"
            })
            .to_string())
        }
        // —— 改动类，全部受自改闸 ——
        "open_panel" | "close_panel" => {
            super::selfmod_gate()?;
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").trim();
            let (_, title) = PANELS.iter().find(|(n, _)| *n == name).ok_or_else(|| {
                format!(
                    "未知面板: {name}（可选：{}；先 list_panels 看启用态）",
                    PANELS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(" / ")
                )
            })?;
            if method == "open_panel" && current_disabled().iter().any(|d| d == name) {
                return Err(format!(
                    "面板「{title}」的插件当前是停用状态，打不开；先用 settings 工具把它从 disabled_plugins 移除（id={name}），再 open_panel"
                ));
            }
            crate::server::ui_events::emit(json!({ "kind": method, "name": name }));
            if method == "open_panel" {
                Ok(format!("已在右侧边栏打开「{title}」面板"))
            } else {
                Ok(format!("已关闭「{title}」面板（如果它开着）"))
            }
        }
        "open_file" => {
            super::selfmod_gate()?;
            let path = v.get("path").and_then(|x| x.as_str()).unwrap_or("").trim();
            if path.is_empty() {
                return Err("缺少 path（工作区相对或绝对路径）".into());
            }
            crate::server::ui_events::emit(json!({ "kind": "open_file", "path": path }));
            Ok(format!("已在侧边栏打开文件预览标签：{path}"))
        }
        "open_memory" => {
            super::selfmod_gate()?;
            let galaxy_on = crate::config::settings::shared_settings()
                .map(|h| h.read().map(|s| s.features.galaxy).unwrap_or(false))
                .unwrap_or(false);
            if !galaxy_on {
                return Err("星云入口当前是关闭状态（features.galaxy=false）；先用 settings 工具把 features.galaxy 打开，再 open_memory".into());
            }
            crate::server::ui_events::emit(json!({ "kind": "open_memory" }));
            Ok("已打开记忆库（星云视图）".into())
        }
        other => Err(format!(
            "未知 method: {other:?}（open_panel / close_panel / list_panels / open_file / open_memory；会话管理用 session 工具）"
        )),
    }
}

/// 当前停用的插件名单（共享设置句柄；未初始化按空表=全启用处理）。
fn current_disabled() -> Vec<String> {
    crate::config::settings::shared_settings()
        .and_then(|h| h.read().ok().map(|s| s.disabled_plugins.clone()))
        .unwrap_or_default()
}
