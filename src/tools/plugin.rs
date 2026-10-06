//! 插件信任工具（2026-10-04 放开权限轮）：第三方插件的「装 / 审 / 卸」正门。
//!
//! 流程（用户拍板）：先审查后安装——审查有 blocker 自动停下、带回完整报告，agent 把
//! 问题列给用户问是否继续；用户点头后带 force=true 重装（force 判 Dangerous 必弹卡，
//! 审批卡就是用户的选择权）。全程复用宿主现成管道（draft 提交 → promote 落位 →
//! review 静态审查 → pin 信任），与 HTTP 端点 /api/plugins/* 同一批内核函数，
//! 不分叉第二套逻辑——审查规则（外联/密钥/动态执行 blocker）见 plugin/review.rs。

use serde_json::{json, Value};

use crate::plugin::{draft, review, trust};

/// 工具入口。`args` 是模型给的参数 JSON 串。
pub(crate) async fn run(args: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("");
    match action {
        // install 受闸（装插件=改插件面，自改纪律管着）；review 只读不受闸
        "install" => {
            super::selfmod_gate()?;
            install(&v).await
        }
        "review" => review_one(&v),
        "unpin" => {
            super::selfmod_gate()?;
            unpin(&v).await
        }
        // 删除同样受闸（删插件=改插件面，且不可恢复）；内核与 HTTP 端点同一份
        "remove" => {
            super::selfmod_gate()?;
            remove_one(&v).await
        }
        _ => Err("未知 action（install/review/unpin/remove）".into()),
    }
}

/// action=install：{ name, manifest, files, force? }。
/// draft 提交（名字/文件数/单文件大小/契约校验齐全）→ promote 落位 → 静态审查：
/// 通过 → pin 信任并即时生效；blocker 且未 force → **不信任**（插件落位但休眠不加载），
/// 返回完整报告让 agent 转述给用户；force=true（判级层已弹卡）→ 记 user_override 强装。
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
    draft::promote(name)?;

    let key = format!("plugins:{name}");
    let report = review::review_key(&key)?;
    let blocked = report.verdict == review::Verdict::Blocked;
    let force = v.get("force").and_then(|x| x.as_bool()).unwrap_or(false);

    if blocked && !force {
        let reasons = review::block_reasons(&report);
        trust::audit_pinned(&key, "", "review_blocked");
        return Ok(json!({
            "installed": false,
            "blocked": reasons,
            "report": report,
            "hint": "插件文件已就位但未信任（不会加载执行）。把 blocked 里的逐条问题讲给用户听，用户同意继续就带 force=true 重试（会弹审批卡让用户亲自确认）",
        })
        .to_string());
    }

    pin(&key, report.digest.clone().unwrap_or_default(), if force { "agent_user_override" } else { "agent" })?;
    let reason = json!({ "installed": true, "key": key, "forced": blocked && force, "report": report });
    Ok(reason.to_string())
}

/// action=review：{ key }（形如 plugins:xxx）。只审不信，返回完整报告。
fn review_one(v: &Value) -> Result<String, String> {
    let key = v.get("key").and_then(|x| x.as_str()).unwrap_or("").trim();
    if key.is_empty() || !key.contains(':') {
        return Err("review 需要 key（形如 plugins:名字）".into());
    }
    let report = review::review_key(key)?;
    Ok(json!({ "key": key, "verdict": format!("{:?}", report.verdict), "report": report }).to_string())
}

/// action=unpin：{ key }。撤回信任（插件停载但不删文件，可逆）。
async fn unpin(v: &Value) -> Result<String, String> {
    let key = v.get("key").and_then(|x| x.as_str()).unwrap_or("").trim();
    if key.is_empty() || !key.contains(':') {
        return Err("unpin 需要 key（形如 plugins:名字）".into());
    }
    let handle = crate::config::settings::shared_settings()
        .ok_or_else(|| "设置子系统尚未初始化（共享句柄缺失）".to_string())?;
    let mut s = handle.read().unwrap_or_else(|e| e.into_inner()).clone();
    let removed = s.trusted_plugins.remove(key).is_some();
    if !removed {
        return Err(format!("{key} 本来就不在信任表里"));
    }
    s.save().map_err(|e| format!("保存 settings.json 失败: {e}"))?;
    *handle.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    trust::install(&s.trusted_plugins);
    crate::plugin::wasm::rescan();
    crate::server::ui_events::emit(json!({ "kind": "settings_changed", "keys": ["trusted_plugins"] }));
    Ok(format!("已取消信任 {key}（插件停载，文件保留，可重新 install 恢复）"))
}

/// action=remove：{ name }。删除 user 来源插件（删目录，不可恢复）。内置拒删并说人话
/// （内核与 HTTP 端点同一份：plugin::ui::remove）。unpin 是「停载不删文件」的可逆路，
/// remove 是连文件一起删的不可逆路——用户说「删掉/卸掉」且指向具体插件时才用它。
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

/// pin 落信任表并即时生效（与 HTTP trust_plugin 的 pin 分支同语义）。
fn pin(key: &str, digest: String, by: &str) -> Result<(), String> {
    let handle = crate::config::settings::shared_settings()
        .ok_or_else(|| "设置子系统尚未初始化（共享句柄缺失）".to_string())?;
    let mut s = handle.read().unwrap_or_else(|e| e.into_inner()).clone();
    s.trusted_plugins.insert(key.to_string(), digest.clone());
    s.save().map_err(|e| format!("保存 settings.json 失败: {e}"))?;
    *handle.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    trust::install(&s.trusted_plugins);
    crate::plugin::wasm::rescan();
    trust::audit_pinned(key, &digest, by);
    crate::server::ui_events::emit(json!({ "kind": "settings_changed", "keys": ["trusted_plugins"] }));
    Ok(())
}
