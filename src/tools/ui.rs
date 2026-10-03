//! ui 工具（2026-10-03 agent 自改设置轮；同日大轮扩展）：操作本应用自己的界面。
//!
//! 侧栏标签/当前会话/弹层是前端状态（rpTabs / galaxyOpen），后端唯一能驱动它们的路是
//! /api/ui/events 事件流：open_panel/close_panel/open_file/open_memory 推事件，前端执行
//! 与用户手点完全相同的函数（用户看着的界面即时变化）。
//! 面板白名单与前端 RP_EXT_PANELS 对齐（trace 时间线 / board 项目文件 / usage 用量）。
//! 除 list_panels（只读）外全部受「自改」插件闸；会话动作在 session 工具里。
//!
//! 2026-10-03 重构轮：前端把 UI_OPS 注册表序列化成 ops 清单随 /api/ui/state 上报，
//! fire_event 通用通道的准入改为「SAFE_EVENT_KINDS 白名单 ∪ 前端清单里登记且非破坏类」——
//! 新增界面能力后前端注册表加一行即可投递，不必再改这里。但清单是广告不是闸：
//! DESTRUCTIVE_EVENT_KINDS 硬编码 denlist 永远把守（投毒伪造也不行），破坏类必须走具名
//! 方法（rules 才能按参数分级、工具层才能完成确认）。

use serde_json::{Value, json};

/// 侧栏面板白名单（与前端 RP_EXT_PANELS 同源；两边改动要同步）。
const PANELS: &[(&str, &str)] = &[("trace", "时间线"), ("board", "项目文件"), ("usage", "用量")];

/// 通用事件通道（2026-10-03 用户拍板「别一个一个加」）：新界面能力以后只加前端一条
/// 分支 + 这里一行 kind，模型即可触达，不再为每个动作改 Rust 工具签名。
/// 只收「非破坏」类的 kind（SAFE_EVENT_KINDS 白名单）：破坏类（delete_session 等）
/// 刻意不在表内——那些必须走具名方法，rules 才能按参数判 Dangerous 弹卡，从 fire_event
/// 混过去就绕过了审批分级。
///
/// 2026-10-03 重构轮：准入扩为「白名单 ∪ 前端清单」。前端 UI_OPS 注册表序列化成 ops
/// 清单随 /api/ui/state 上报，新 kind 只要前端登记且标 safe/normal 即放行，不再需要
/// 一个个改本文件。破坏类仍由 DESTRUCTIVE_EVENT_KINDS 把守。
pub(crate) const SAFE_EVENT_KINDS: &[&str] = &[
    "sidebar",
    "terminal",
    "settings",
    "theme",
    "open_tab",
    "close_tab",
    "css_inject",
    "open_file",
    "open_memory",
    "open_panel",
    "close_panel",
    "new_session",
    "open_session",
    "rename_session",
    "fork_session",
    "pin_session",
    "archive_session",
];

/// 服务端硬编码的破坏类事件 denylist（2026-10-03 重构轮新增）：这些 kind **永远不收**
/// 进通用 fire_event，哪怕前端清单把它们标成 safe（清单是广告不是闸——/api/ui/state
/// 无鉴权，本地任何进程可投毒；真前端也犯不着为破坏类开口子）。走具名方法保分级。
pub(crate) const DESTRUCTIVE_EVENT_KINDS: &[&str] = &["delete_session", "undo_checkpoint"];

/// fire_event 的 kind 准入：白名单 ∪ 前端清单（非破坏类）。
/// - denylist（DESTRUCTIVE_EVENT_KINDS）一律拒，哪怕清单把 kind 标成 safe；
/// - 在 SAFE_EVENT_KINDS 里 → 放行（无页面报清单的无头环境也认这层）；
/// - 页面报过 ops 清单且该 kind 登记其中、risk 标 safe/normal → 放行（新界面能力
///   只加前端注册表一行就可用，不再逐 kind 改这里）；
/// - 清单没报/没这条/标 destructive → 拒，报错里带当前可用 kind 与指路。
fn event_kind_allowed(kind: &str) -> Result<(), String> {
    if DESTRUCTIVE_EVENT_KINDS.contains(&kind) {
        return Err(format!(
            "kind「{kind}」是破坏类操作，只能走具名方法（删会话用 session 工具、撤销检查点用 undo_checkpoint）——审批分级不为它从通用通道开口子"
        ));
    }
    if SAFE_EVENT_KINDS.contains(&kind) {
        return Ok(());
    }
    match crate::server::ui_events::ops_risk(kind).as_deref() {
        Some("safe") | Some("normal") => Ok(()),
        Some("destructive") => Err(format!(
            "kind「{kind}」在界面清单里标的是破坏类（risk=destructive）——必须走具名方法，不进通用通道"
        )),
        _ => Err(format!(
            "kind「{kind}」当前界面没登记（没有页面在报清单，或前端还没有这个 kind）；当前可用：{}",
            list_reported_or_static_kinds()
        )),
    }
}

/// 报错用：清单里登记的非内部 kind + 静态白名单补集（没人报时指路白名单）。
fn list_reported_or_static_kinds() -> String {
    let mut ks: Vec<String> = crate::server::ui_events::ops_manifest()
        .unwrap_or_default()
        .iter()
        .filter_map(|o| o.get("kind").and_then(Value::as_str).map(str::to_string))
        .filter(|k| !DESTRUCTIVE_EVENT_KINDS.contains(&k.as_str()))
        .collect();
    for k in SAFE_EVENT_KINDS.iter() {
        if !ks.contains(&k.to_string()) {
            ks.push((*k).to_string());
        }
    }
    if ks.is_empty() {
        ks.push("（无）".into());
    }
    ks.join(" / ")
}

pub(crate) async fn run(args: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let method = v.get("method").and_then(|x| x.as_str()).unwrap_or("");
    match method {
        // —— 只读，不受闸（界面上有什么/什么开着，看 ≠ 改） ——
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
        // 界面状态（侧栏/终端/主题/设置弹窗）：读前端最近一次上报；没有页面报过 = 没人开着界面
        "state" => match crate::server::ui_events::last_state() {
            Some(s) => Ok(s.to_string()),
            None => Err("界面状态未知：没有活跃页面在报（页面关着/壳没开时没法看界面）；界面动作对无人看的界面照样会播，但你看不到结果。".into()),
        },
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
        // 左右两侧边栏：side=left 会话列表侧栏，right 产物/页面/面板侧栏（含终端互斥切换）
        "open_sidebar" | "close_sidebar" => {
            super::selfmod_gate()?;
            let side = match v.get("side").and_then(|x| x.as_str()).unwrap_or("") {
                "left" => "left",
                "right" => "right",
                other => return Err(format!("未知 side: {other:?}（left / right）")),
            };
            let open = method == "open_sidebar";
            // no-op 识别：界面最近一次上报的状态已经是目标态就别再报「已完成」骗用户——
            // 真机上 agent 在目标态下照样报「已收起」是最扎眼的假完成
            let already = ui_state_side(side)
                .map(|cur| cur == open)
                .unwrap_or(false);
            crate::server::ui_events::emit(json!({ "kind": "sidebar", "side": side, "open": open }));
            let which = if side == "left" { "左侧边栏" } else { "右侧边栏" };
            let mut out = if open { format!("已打开{which}") } else { format!("已收起{which}") };
            if already {
                out.push_str("（界面本来就是目标态，这次没有实际变化——如实告知用户）");
            }
            Ok(out)
        }
        // 终端面板（与右侧面板互斥）：open=弹终端并收起右侧面板，close=收起终端
        "open_terminal" | "close_terminal" => {
            super::selfmod_gate()?;
            let open = method == "open_terminal";
            let already = ui_state_bool("terminal_open")
                .map(|cur| cur == open)
                .unwrap_or(false);
            crate::server::ui_events::emit(json!({ "kind": "terminal", "open": open }));
            let mut out = if open { "已打开终端面板".to_string() } else { "已收起终端面板".to_string() };
            if already {
                out.push_str("（界面本来就是目标态，这次没有实际变化——如实告知用户）");
            }
            Ok(out)
        }
        // 设置弹窗：open 可带 tab 直接跳到某页签（appearance/model/roles/memory/skills/mcp/ext/schedule）
        "open_settings" => {
            super::selfmod_gate()?;
            let tab = v.get("tab").and_then(|x| x.as_str()).unwrap_or("");
            let tabs = [
                "appearance", "model", "roles", "memory", "skills", "mcp", "ext", "schedule",
            ];
            let tab = match tab {
                "" => "",
                t if tabs.contains(&t) => t,
                other => return Err(format!("未知设置页签: {other:?}（可选：{}）", tabs.join(" / "))),
            };
            crate::server::ui_events::emit(json!({ "kind": "settings", "open": true, "tab": tab }));
            if tab.is_empty() {
                Ok("已打开设置弹窗".into())
            } else {
                Ok(format!("已打开设置弹窗并跳到「{tab}」页签"))
            }
        }
        "close_settings" => {
            super::selfmod_gate()?;
            crate::server::ui_events::emit(json!({ "kind": "settings", "open": false }));
            Ok("已关闭设置弹窗".into())
        }
        // 主题：dark/light/system（跟随系统）
        "set_theme" => {
            super::selfmod_gate()?;
            let mode = match v.get("mode").and_then(|x| x.as_str()).unwrap_or("") {
                "dark" | "light" | "system" => v.get("mode").and_then(|x| x.as_str()).unwrap_or(""),
                other => return Err(format!("未知主题: {other:?}（dark / light / system）")),
            };
            crate::server::ui_events::emit(json!({ "kind": "theme", "mode": mode }));
            Ok(format!("已切换主题：{mode}"))
        }
        // 内置标签页（产物/审查/网页）开在右侧边栏
        "open_tab" => {
            super::selfmod_gate()?;
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").trim();
            let (title, key) = match name {
                "artifacts" => ("产物", "artifacts"),
                "review" => ("审查", "review"),
                "web" => ("网页", "web"),
                other => return Err(format!("未知标签: {other:?}（artifacts / review / web）")),
            };
            crate::server::ui_events::emit(json!({ "kind": "open_tab", "key": key }));
            Ok(format!("已在右侧边栏打开「{title}」标签"))
        }
        // 关标签：key 形如 artifacts/review/web、ext:usage、file:<路径>（先 state 看 tabs_open/active_tab）
        "close_tab" => {
            super::selfmod_gate()?;
            let key = v.get("key").and_then(|x| x.as_str()).unwrap_or("").trim();
            if key.is_empty() {
                return Err("缺少 key（artifacts / review / web / ext:usage / file:<路径>）".into());
            }
            crate::server::ui_events::emit(json!({ "kind": "close_tab", "key": key }));
            Ok(format!("已请求关闭标签 {key}（本来就没开会没反应，如实告知用户）"))
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
        // CSS 注入：和用户在设置页「CSS 注入框」粘贴完全同一条路（同转义/同落盘，
        // 不依赖 css-ui 插件开没开——注入框只是编辑器，应用端在宿主）。append=true 追加
        // 在原有规则后；默认覆盖。要保留原有规则先 method=state 读 custom_css。
        "inject_css" => {
            super::selfmod_gate()?;
            let css = v.get("css").and_then(|x| x.as_str()).unwrap_or("");
            let append = v.get("append").and_then(|x| x.as_bool()).unwrap_or(false);
            let clear = v.get("clear").and_then(|x| x.as_bool()).unwrap_or(false);
            // 空 css 一律拒（真机实锤：模型只传 method 不传 css，工具不能把「什么都没说」
            // 当覆盖成功照单全收——那是又一种假完成）。要清空走显式 clear=true。
            if !clear && css.trim().is_empty() {
                return Err("缺少 css（要注入的 CSS 内容；要清空手写规则请显式传 clear=true）".into());
            }
            if clear && append {
                return Err("clear=true 时不能同时 append=true".into());
            }
            let css = if clear { "" } else { css };
            crate::server::ui_events::emit(json!({ "kind": "css_inject", "css": css, "append": append }));
            if clear {
                Ok("已清空用户手写 CSS（微调规则不受影响）".into())
            } else if append {
                Ok("已在用户 CSS 末尾追加这段规则并即时生效（重开也在）".into())
            } else {
                Ok("已按注入框同款路径写入这段 CSS 并即时生效（覆盖原有手写规则；重开也在）".into())
            }
        }
        // 通用事件通道：event 传整条事件 JSON（kind 必填，其余字段任意）。准入 =
        // SAFE_EVENT_KINDS 白名单 ∪ 前端 UI_OPS 清单（非破坏类），见 event_kind_allowed——
        // 前端新增界面事件并登记进注册表即可投递，不用再逐 kind 改这里。
        "fire_event" => {
            super::selfmod_gate()?;
            // event 收对象，也容错收 stringify 过的 JSON（模型常见手滑，别让它被自己的
            // 错误参数打得判 Dangerous 再被无头拒掉——那等于把路堵死还给人错信号）
            let ev = match v.get("event") {
                Some(Value::Object(_)) => v.get("event").cloned().unwrap_or(Value::Null),
                Some(Value::String(s)) => serde_json::from_str::<Value>(s)
                    .map_err(|e| format!("event 是字符串但不是合法 JSON: {e}"))?,
                _ => return Err("缺少 event（整条事件 JSON，如 {\"kind\":\"theme\",\"mode\":\"dark\"}）".into()),
            };
            let kind = ev
                .get("kind")
                .and_then(|x| x.as_str())
                .ok_or("event.kind 必填（如 sidebar/terminal/settings/theme/open_tab/close_tab/css_inject/open_file/open_memory/open_panel/close_panel/new_session/open_session/rename_session/fork_session/pin_session/archive_session）")?
                .to_string();
            event_kind_allowed(&kind)?;
            crate::server::ui_events::emit(ev);
            Ok(format!("已向界面投递事件 {kind}"))
        }
        // 撤销某次任务的改动：前端走与改动条「撤销」按钮同一条路（自带二次确认），
        // 高风险 → rules 判 Dangerous 必问（不被 fire_event 白名单收）。
        "undo_checkpoint" => {
            super::selfmod_gate()?;
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").trim();
            if id.is_empty() {
                return Err("缺少 id（检查点 id；/api/checkpoints 可列）".into());
            }
            crate::server::ui_events::emit(json!({ "kind": "undo_checkpoint", "id": id }));
            Ok(format!("已请求撤销检查点 {id} 的改动（界面会弹确认，用户点了才真做）"))
        }
        other => Err(format!(
            "未知 method: {other:?}（open_panel / close_panel / list_panels / open_file / open_memory / open_sidebar / close_sidebar / open_terminal / close_terminal / open_settings / close_settings / set_theme / open_tab / close_tab / state / inject_css；会话管理用 session 工具）"
        )),
    }
}

/// 当前停用的插件名单（共享设置句柄；未初始化按空表=全启用处理）。
fn current_disabled() -> Vec<String> {
    crate::config::settings::shared_settings()
        .and_then(|h| h.read().ok().map(|s| s.disabled_plugins.clone()))
        .unwrap_or_default()
}

/// 从界面最近一次上报里读某侧边栏的开合态（right/left → bool 是否开着）。
fn ui_state_side(side: &str) -> Option<bool> {
    let st = crate::server::ui_events::last_state()?;
    st.get("sidebars")
        .and_then(|s| s.get(side))
        .and_then(|b| b.as_bool())
}

/// 从界面最近一次上报里读一个布尔字段（terminal_open / settings_open 等）。
fn ui_state_bool(field: &str) -> Option<bool> {
    let st = crate::server::ui_events::last_state()?;
    st.get(field).and_then(|b| b.as_bool())
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // fire_event 的 kind 准入五分支。**单一写者**：本函数是整个 cargo test 进程里唯一
    // 往 ui_events 全局态注入假 ops 清单的地方（进程启动时没人报过 state = None），
    // 因此函数内必须顺序执行——先断言「无清单」分支，再注入清单断言登记分支；
    // rules 侧的分级映射与清单解析断言也折叠在这里，不另起写者把前提冲掉。
    // （cargo test 默认多线程并行：任何第二个写者都会让「无清单」断言随机翻车。）
    #[test]
    fn fire_event_kind_admission_five_branches() {
        // ① 无页面在报（进程态 = None）：破坏 denlist 一律拒，且指明具名方法在哪
        for k in DESTRUCTIVE_EVENT_KINDS {
            let e = event_kind_allowed(k).unwrap_err();
            assert!(e.contains("具名方法"), "{k} 拒因须指路具名方法: {e}");
        }
        // ② 无清单时静态白名单照样放行（无头环境行为不变）
        assert!(event_kind_allowed("theme").is_ok());
        assert!(event_kind_allowed("pin_session").is_ok());
        // ③ 白名单外又没人登记：拒，报错回带当前可用 kind（此处回落静态白名单）
        let e = event_kind_allowed("galaxy_toggle").unwrap_err();
        assert!(e.contains("没登记"), "拒因要说清: {e}");
        assert!(e.contains("theme"), "无清单时报错须回落白名单指路: {e}");

        // —— 注入假清单：模拟前端 UI_OPS 注册表自报（含把破坏类标 safe 的恶意条目，
        //    自证「清单是广告不是闸」——投毒伪造也绕不过 denlist）——
        crate::server::ui_events::report_state(json!({
            "ops": [
                { "kind": "galaxy_toggle",  "title": "星云入口",  "risk": "safe",   "args": [] },
                { "kind": "roles_sheet",    "title": "功能桥接",  "risk": "normal", "args": [] },
                { "kind": "delete_session", "title": "删会话",    "risk": "safe",   "args": [] },
                { "kind": "undo_checkpoint","title": "撤销",      "risk": "normal", "args": [] },
                "garbage_string_entry_not_an_object", // 非对象条目：应被 ops_manifest 的 is_object 滤掉
            ]
        }));
        // 清单解析：只收对象条目；能按 kind 取到 risk 档，未登记返回 None
        let man = crate::server::ui_events::ops_manifest().expect("注入后应读到清单");
        assert_eq!(man.len(), 4, "非对象条目应被滤掉: {man:?}");
        assert_eq!(crate::server::ui_events::ops_risk("galaxy_toggle").as_deref(), Some("safe"));
        assert_eq!(crate::server::ui_events::ops_risk("roles_sheet").as_deref(), Some("normal"));
        assert_eq!(crate::server::ui_events::ops_risk("no_such_kind"), None);

        // ④ 清单登记 safe/normal：放行（新界面能力 = 前端注册表加一行即用，不改后端）
        assert!(event_kind_allowed("galaxy_toggle").is_ok());
        assert!(event_kind_allowed("roles_sheet").is_ok());
        // ⑤ 清单把破坏类标 safe/normal 也拒——denlist 是闸，清单只是广告
        assert!(event_kind_allowed("delete_session").unwrap_err().contains("具名方法"));
        assert!(event_kind_allowed("undo_checkpoint").unwrap_err().contains("具名方法"));
        // 清单里没有、白名单也没有：拒，且报错列出清单登记的可投 kind
        let e = event_kind_allowed("no_such_kind").unwrap_err();
        assert!(e.contains("没登记"), "{e}");
        assert!(e.contains("galaxy_toggle") && e.contains("roles_sheet"), "报错应列出清单登记的可用 kind: {e}");

        // rules 侧同源映射（事实源在 rules.rs::evaluate_risk，与准入同一批常量/同一份清单）：
        let ws = std::path::Path::new("C:/test/ws");
        let risk = |kind: &str| {
            crate::core::rules::evaluate_risk("ui", &json!({"method": "fire_event", "event": {"kind": kind}}), ws).0
        };
        use crate::core::rules::RiskLevel::*;
        assert_eq!(risk("galaxy_toggle"), Safe,   "清单 safe → Safe");
        assert_eq!(risk("roles_sheet"),   Normal, "清单 normal → Normal");
        assert_eq!(risk("delete_session"), Dangerous, "清单标 safe 也改不了 denlist 判级");
        assert_eq!(risk("undo_checkpoint"), Dangerous);
        assert_eq!(risk("no_such_kind"), Dangerous, "未登记 → 高危兜底");
        // 白名单 kind 判 Safe 不受清单注入影响（分支先于清单查询）
        assert_eq!(risk("theme"), Safe);
    }
}
