//! 服务端 → 前端 UI 事件流（2026-10-03 agent 自改设置轮补全）。
//!
//! GET /api/ui/events：单向 SSE。两个用途：
//! 1. agent 的 ui 工具开/关侧栏面板（open_panel/close_panel）——侧栏标签是纯前端
//!    状态（rpTabs），后端没有别的路子驱动它；
//! 2. settings 工具改完设置推 settings_changed——打开中的插件页/侧栏即时重拉清单，
//!    别让「用户看到的」和「实际发生的」对不上（真机实锤过的坑：agent 停用用量插件
//!    落盘成功，插件页还显示开着）。
//! broadcast 总线：无人订阅时发送即丢弃；EventSource 断线自带重连。
//!
//! 反向还有一个 POST /api/ui/state（前端上报界面状态）：侧栏开合是纯前端态，agent
//! 看不见也猜不准（「打开」=展开还是按一下开关，中文本就两义，模型按切换语义猜错过
//! ——真机实锤）。页面把自己的边栏/面板状态报上来（内存态，仅最近一次页面有效），
//! ui 工具的 state 方法读它，open/close 也靠它识别 no-op，不再凭空报「已完成」。
//!
//! 2026-10-03 重构轮：上报体新增 `ops` 字段——前端 UI_OPS 注册表序列化出的
//! 「界面能干什么」自描述清单（kind/title/risk/args 数组）。用途二：① ui 工具的
//! fire_event 通用通道准入参考（白名单之外、清单里登记且非破坏类的 kind 放行）；
//! ② rules 按清单 risk 分级。**清单是广告不是闸**——/api/ui/state 是无鉴权本地端点，
//! 任何本地进程可投毒，所以破坏类（见 core::rules 的破坏 denylist）由后端硬编码
//! 永远把守，未登记 kind 默认高危。

use super::*;
use futures_util::stream::Stream;
use tokio::sync::broadcast;

static BUS: std::sync::OnceLock<broadcast::Sender<Value>> = std::sync::OnceLock::new();
/// 前端最近一次上报的界面状态（无页面在时报空 = agent 能察觉没人在看）
static UI_STATE: std::sync::OnceLock<std::sync::RwLock<Option<Value>>> = std::sync::OnceLock::new();

fn state_slot() -> &'static std::sync::RwLock<Option<Value>> {
    UI_STATE.get_or_init(|| std::sync::RwLock::new(None))
}

fn bus() -> &'static broadcast::Sender<Value> {
    BUS.get_or_init(|| broadcast::channel(64).0)
}

/// 推一条 UI 事件（无人订阅/缓冲满都静默——界面刷新类事件，丢了等下一次）。
pub(crate) fn emit(v: Value) {
    let _ = bus().send(v);
}

/// 测试专用：订一条事件流。无人订阅时发送即丢弃，不订阅就验不了「后端真推了」。
#[cfg(test)]
pub(crate) fn test_subscribe() -> broadcast::Receiver<Value> {
    bus().subscribe()
}

/// 前端上报界面状态（POST /api/ui/state；任何来源的状态变化都该报，包括顶栏手点）。
pub(crate) fn report_state(v: Value) {
    if let Ok(mut slot) = state_slot().write() {
        *slot = Some(v);
    }
}

/// agent 读最近一次界面状态上报；没有页面报过返回 None。
pub(crate) fn last_state() -> Option<Value> {
    state_slot().read().ok().and_then(|s| s.clone())
}

/// 前端自报的 ops 动作清单（kind/title/risk/args 数组）；没有页面报过返回 None。
/// **不可信**：本地无鉴权端点可投毒——只作广告与准入参考，破坏类另由 denylist 把守。
pub(crate) fn ops_manifest() -> Option<Vec<Value>> {
    last_state()?
        .get("ops")?
        .as_array()
        .map(|a| a.iter().filter(|x| x.is_object()).cloned().collect())
}

/// 某 kind 在清单里的 risk 档（safe/normal/destructive）；未登记或没清单返回 None。
pub(crate) fn ops_risk(kind: &str) -> Option<String> {
    let ops = ops_manifest()?;
    ops.iter()
        .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
        .and_then(|o| o.get("risk").and_then(Value::as_str))
        .map(str::to_string)
}

/// GET /api/ui/events —— SSE 事件流（本机来源校验由全局中间件兜着）。
pub(super) async fn ui_events_stream() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = bus().subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(v) => {
                    let data = v.to_string();
                    return Some((Ok(Event::default().data(data)), rx));
                }
                // 消费者跟不上了丢几条：界面刷新类事件，下一轮改动还会再推
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream)
}

/// POST /api/ui/state —— 前端上报边栏/面板等界面状态（本机来源校验由全局中间件兜着）。
pub(super) async fn ui_state_post(Json(v): Json<Value>) -> Json<Value> {
    report_state(v);
    Json(json!({ "ok": true }))
}
