//! session 工具（2026-10-03 agent 自改大轮）：会话管理正门——新建/切换/改名/分叉/
//! 删除/置顶/归档/清单。
//!
//! 会话是「用户看着的界面实体」：切换、侧栏刷新、顶栏同步都在前端，所以 new/open/
//! rename/fork/delete/pin/archive 走 /api/ui/events 事件流，前端执行与用户手点完全
//! 相同的函数（同一扇门，界面永远不会和实际状态对不上）；list 走共享 AppState 直读。
//! 全部方法受「自改」插件闸（list 只读除外）；删除不可逆 → Dangerous 必问。

use serde_json::{Value, json};

fn need_id(v: &Value) -> Result<String, String> {
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if id.is_empty() {
        return Err("缺少 id（先用 method=list 查会话清单）".into());
    }
    Ok(id)
}

fn emit(v: Value) {
    crate::server::ui_events::emit(v);
}

pub(crate) async fn run(args: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let method = v.get("method").and_then(|x| x.as_str()).unwrap_or("");
    match method {
        // —— 只读，不受闸 ——
        "list" => {
            let state = crate::server::shared_state().ok_or("会话子系统尚未初始化")?;
            // 与 GET /api/sessions 同一份元数据结构（id/title/workspace/pinned/archived 等直接序列化）
            let sessions = state.session.list();
            Ok(json!({ "sessions": sessions, "note": "id 用于 open/rename/fork/delete/pin/archive；workspace 是所属工作区路径" }).to_string())
        }
        // —— 改动类，全部受自改闸 ——
        "new" => {
            super::selfmod_gate()?;
            emit(json!({ "kind": "new_session" }));
            Ok("已新建会话：界面已切到新会话（当前工作区），侧栏能看到它。注意你的回复仍落在原会话里，用户此刻看的是新会话。".into())
        }
        "open" => {
            super::selfmod_gate()?;
            let id = need_id(&v)?;
            emit(json!({ "kind": "open_session", "id": id }));
            Ok(format!("已请求界面切换到会话 {id}（侧栏没有这条时会没反应，可先 method=list 核对 id）。你的回复仍落在原会话。"))
        }
        "rename" => {
            super::selfmod_gate()?;
            let id = need_id(&v)?;
            let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
            if title.is_empty() {
                return Err("缺少 title".into());
            }
            emit(json!({ "kind": "rename_session", "id": id, "title": title }));
            Ok(format!("已请求把会话 {id} 改名为「{title}」"))
        }
        "fork" => {
            super::selfmod_gate()?;
            let id = need_id(&v)?;
            emit(json!({ "kind": "fork_session", "id": id }));
            Ok(format!("已请求分叉会话 {id}（界面会切到分叉出的新会话）"))
        }
        "delete" => {
            super::selfmod_gate()?;
            let id = need_id(&v)?;
            emit(json!({ "kind": "delete_session", "id": id }));
            Ok(format!("已请求删除会话 {id}（用户已在审批卡上确认过；若删的是当前会话，界面会自动开一条新会话）"))
        }
        "pin" | "archive" => {
            super::selfmod_gate()?;
            let id = need_id(&v)?;
            let on = v.get("on").and_then(|x| x.as_bool()).unwrap_or(true);
            let kind = if method == "pin" { "pin_session" } else { "archive_session" };
            emit(json!({ "kind": kind, "id": id, "on": on }));
            let action = if method == "pin" {
                if on { "置顶" } else { "取消置顶" }
            } else if on {
                "归档"
            } else {
                "取消归档"
            };
            Ok(format!("已请求{action}会话 {id}"))
        }
        other => Err(format!(
            "未知 method: {other:?}（list / new / open / rename / fork / delete / pin / archive）"
        )),
    }
}
