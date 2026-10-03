//! 服务端 → 前端 UI 事件流（2026-10-03 agent 自改设置轮补全）。
//!
//! GET /api/ui/events：单向 SSE。两个用途：
//! 1. agent 的 ui 工具开/关侧栏面板（open_panel/close_panel）——侧栏标签是纯前端
//!    状态（rpTabs），后端没有别的路子驱动它；
//! 2. settings 工具改完设置推 settings_changed——打开中的插件页/侧栏即时重拉清单，
//!    别让「用户看到的」和「实际发生的」对不上（真机实锤过的坑：agent 停用用量插件
//!    落盘成功，插件页还显示开着）。
//! broadcast 总线：无人订阅时发送即丢弃；EventSource 断线自带重连。

use super::*;
use futures_util::stream::Stream;
use tokio::sync::broadcast;

static BUS: std::sync::OnceLock<broadcast::Sender<Value>> = std::sync::OnceLock::new();

fn bus() -> &'static broadcast::Sender<Value> {
    BUS.get_or_init(|| broadcast::channel(64).0)
}

/// 推一条 UI 事件（无人订阅/缓冲满都静默——界面刷新类事件，丢了等下一次）。
pub(crate) fn emit(v: Value) {
    let _ = bus().send(v);
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
