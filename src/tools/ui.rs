//! ui 工具（2026-10-03 agent 自改设置轮）：操作本应用自己的界面——右侧边栏的面板标签
//! 与会话动作。
//!
//! 侧栏标签/当前会话是前端状态（rpTabs / currentSessionId），后端唯一能驱动它们的路是
//! /api/ui/events 事件流：open_panel/close_panel/new_session 推事件，前端执行与用户
//! 手点完全相同的处理（用户看着的界面即时变化）。
//! 面板白名单与前端 RP_EXT_PANELS 对齐（trace 时间线 / board 项目文件 / usage 用量），
//! 白名单外不伺候（2026-10-03 用户拍板：侧栏就六个标签，其他插件不进侧栏）。
//! 面板对应插件被停用时拒绝并指路 settings 工具（先启用再打开）。只读 Safe，
//! 不落任何持久状态。

use serde_json::{Value, json};

/// 侧栏面板白名单（与前端 RP_EXT_PANELS 同源；两边改动要同步）。
const PANELS: &[(&str, &str)] = &[("trace", "时间线"), ("board", "项目文件"), ("usage", "用量")];

pub(crate) async fn run(args: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let method = v.get("method").and_then(|x| x.as_str()).unwrap_or("");
    match method {
        "new_session" => {
            // 与侧栏「新会话」按钮同一条路：事件 → 前端 startNewSession()（建会话 +
            // 刷新侧栏 + 切过去）。agent 自己的回复仍落在当前会话——结果里说清这点，
            // 防模型误以为用户还在看着这条对话。
            crate::server::ui_events::emit(json!({ "kind": "new_session" }));
            Ok("已新建会话：界面已切到新会话（当前工作区），侧栏能看到它。注意你的这条回复仍落在原会话里，用户此刻看的是新会话。".to_string())
        }
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
                "note": "name 是插件 id（settings 工具的 disabled_plugins 用它，别写中文标题）"
            })
            .to_string())
        }
        "open_panel" | "close_panel" => {
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
        other => Err(format!(
            "未知 method: {other:?}（open_panel / close_panel / list_panels / new_session）"
        )),
    }
}

/// 当前停用的插件名单（共享设置句柄；未初始化按空表=全启用处理）。
fn current_disabled() -> Vec<String> {
    crate::config::settings::shared_settings()
        .and_then(|h| h.read().ok().map(|s| s.disabled_plugins.clone()))
        .unwrap_or_default()
}
