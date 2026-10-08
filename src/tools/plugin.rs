//! 插件安装工具（2026-10-04 放开权限轮）：模型自产插件进 draft 隔离区并一键转正。
//!
//! 流程：draft 提交（名字/文件数/单文件大小/契约校验齐全，权限一律清空）→ promote
//! 落位到 `plugins/<name>/` 即加载。全程复用宿主现成管道，与 HTTP 端点
//! `/api/plugins/*` 同一批内核函数，不分叉第二套逻辑。draft 隔离区本身即边界：
//! 权限清空 + WASM 纯计算沙箱（零宿主函数）+ 面板 iframe 不透明源。
//!
//! 2026-10-08：准入审核（静态审查）与 pin 验签按用户拍板移除——生态起来后再重建。
//! 安装路径回到「写文件 → 转正落位 → 即时生效」，没有前置门。

use serde_json::{json, Value};

use crate::plugin::draft;

/// 工具入口。`args` 是模型给的参数 JSON 串。
pub(crate) async fn run(args: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("");
    match action {
        // install 受闸（装插件=改插件面，自改纪律管着）
        "install" => {
            super::selfmod_gate()?;
            install(&v).await
        }
        // 删除同样受闸（删插件=改插件面，且不可恢复）；内核与 HTTP 端点同一份
        "remove" => {
            super::selfmod_gate()?;
            remove_one(&v).await
        }
        _ => Err("未知 action（install/remove）".into()),
    }
}

/// action=install：{ name, manifest, files }。
/// draft 提交（规范化：权限清空）→ promote 落位 → 即时生效；落位后推
/// settings_changed 让界面清单刷新（后台实体要即时进界面）。
async fn install(v: &Value) -> Result<String, String> {
    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").trim();
    if name.is_empty() {
        return Err("install 需要 name（插件名，仅字母数字与 - _ .）".into());
    }
    let manifest = v.get("manifest").cloned().unwrap_or(json!({}));
    let mut files = std::collections::BTreeMap::new();
    if let Some(obj) = v.get("files").and_then(Value::as_object) {
        for (k, val) in obj {
            match val.as_str() {
                Some(s) => files.insert(k.clone(), s.to_string()),
                None => return Err(format!("files[{k}] 必须是字符串内容")),
            };
        }
    }
    draft::submit(name, &manifest, &files)?;
    let written = draft::promote(name)?;
    // 落位即加载：推事件让前端重拉插件清单（与 toggle 端点同一条总线）
    crate::server::ui_events::emit(json!({ "kind": "settings_changed", "keys": ["disabled_plugins"] }));
    Ok(json!({ "installed": true, "name": name, "written": written }).to_string())
}

/// action=remove：{ name }。删除 user 来源插件（删目录，不可恢复）。内置拒删并说人话
/// （内核与 HTTP 端点同一份：plugin::ui::remove）。
async fn remove_one(v: &Value) -> Result<String, String> {
    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").trim();
    if name.is_empty() {
        return Err("remove 需要 name（插件名，仅字母数字与 - _ .）".into());
    }
    let dir = crate::plugin::ui::remove(name).map_err(|(_, msg)| msg)?;
    // 目录没了 → 工具注册表重扫（它可能同时是个工具插件）；事件内核里已落
    crate::plugin::wasm::rescan();
    crate::server::ui_events::emit(json!({ "kind": "settings_changed", "keys": ["disabled_plugins"] }));
    Ok(json!({ "removed": name, "dir": dir }).to_string())
}
