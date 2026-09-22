//! CDP（Chrome DevTools Protocol）客户端 —— P20 浏览器内核；P21 增补两件事：
//! ① **事件广播通道**——actor 把每条 CDP 事件复制一份进 broadcast（screencast 帧、
//!    JS dialog 事件由标签页级订阅者消费；等待器机制不变）；② **隔离世界求值**
//!    （`Page.createIsolatedWorld` + 带 contextId 的 `Runtime.evaluate`，快照/定位/求值
//!    都在隔离世界跑，页面覆写全局对象或 getter 也改不了结果——移植自 ZCode）。
//!
//! 架构：单 actor 任务独占 WS 流（select 命令通道 + WS 消息），请求按 id 配对经
//! oneshot 回传；事件等待器与命令走同一条 FIFO 通道，「先注册等待器再发命令」
//! 在 actor 处理顺序上天然成立，无竞态。调用超时在客户端侧计时，超时后等待器
//! 残留由 actor 在事件到达时「发入已关闭通道」自然丢弃。

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// 单次 CDP 调用的默认预算。
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// 事件广播容量：screencast 帧消费慢时丢弃中间帧（最新帧胜出），不阻塞 actor。
const EVENT_CHANNEL_CAP: usize = 256;

enum ToActor {
    Call { method: String, params: Value, reply: oneshot::Sender<Result<Value, String>> },
    WaitEvent { method: String, reply: oneshot::Sender<Result<Value, String>> },
}

/// 页面级 CDP 会话句柄。
pub struct Cdp {
    tx: mpsc::UnboundedSender<ToActor>,
    /// CDP 事件广播（每条事件复制一份；无订阅者时发送失败被忽略）。
    events: broadcast::Sender<(String, Value)>,
    /// 隔离世界 executionContextId 缓存（world 名 → contextId；导航销毁后重建）。
    worlds: Arc<Mutex<HashMap<String, i64>>>,
    /// 连接是否还活着（actor 退出即假——浏览器进程死了/壳重启后旧连接全断）。
    /// 端口会复生（壳重启后 9223 又起来了），光探端口分不出会话里的 tab 是不是僵尸。
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl Cdp {
    /// 连接页面级 CDP 端点并起 actor。
    pub async fn connect(ws_url: &str) -> Result<Self, String> {
        let (ws, _resp) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| format!("CDP WebSocket 连接失败: {e}"))?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<ToActor>();
        let (ev_tx, _rx) = broadcast::channel::<(String, Value)>(EVENT_CHANNEL_CAP);
        let ev_in_actor = ev_tx.clone();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let alive_in_actor = alive.clone();

        tokio::spawn(async move {
            let mut next_id: u64 = 1;
            let mut pending: std::collections::HashMap<u64, oneshot::Sender<Result<Value, String>>> =
                std::collections::HashMap::new();
            // 事件等待器：注册顺序即触发顺序（同页面同方法极少并发等待）
            let mut waiters: Vec<(String, oneshot::Sender<Result<Value, String>>)> = Vec::new();

            let die = |pending: &mut std::collections::HashMap<
                u64,
                oneshot::Sender<Result<Value, String>>,
            >,
                      waiters: &mut Vec<(String, oneshot::Sender<Result<Value, String>>)>| {
                alive_in_actor.store(false, std::sync::atomic::Ordering::Relaxed);
                for (_, slot) in pending.drain() {
                    let _ = slot.send(Err("CDP 连接已断开（浏览器退出或标签页崩溃）".to_string()));
                }
                for (_, slot) in waiters.drain(..) {
                    let _ = slot.send(Err("CDP 连接已断开".to_string()));
                }
            };

            loop {
                tokio::select! {
                    cmd = rx.recv() => {
                        let Some(cmd) = cmd else { break };
                        match cmd {
                            ToActor::Call { method, params, reply } => {
                                let id = next_id;
                                next_id += 1;
                                let msg = json!({ "id": id, "method": method, "params": params });
                                if let Err(e) = sink.send(Message::Text(msg.to_string())).await {
                                    let _ = reply.send(Err(format!("CDP 发送失败: {e}")));
                                    die(&mut pending, &mut waiters);
                                    break;
                                }
                                pending.insert(id, reply);
                            }
                            ToActor::WaitEvent { method, reply } => {
                                waiters.push((method, reply));
                            }
                        }
                    }
                    msg = stream.next() => {
                        let msg = match msg {
                            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => {
                                die(&mut pending, &mut waiters);
                                break;
                            }
                            Some(Ok(m)) => m,
                        };
                        let Message::Text(txt) = msg else { continue };
                        let Ok(v) = serde_json::from_str::<Value>(&txt) else { continue };
                        if let Some(id) = v.get("id").and_then(Value::as_u64) {
                            if let Some(slot) = pending.remove(&id) {
                                let res = match v.get("error") {
                                    Some(err) => Err(format!(
                                        "CDP {}: {}",
                                        err.get("message").and_then(Value::as_str).unwrap_or("错误"),
                                        err.get("data").and_then(Value::as_str).unwrap_or("")
                                    )),
                                    None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                                };
                                let _ = slot.send(res);
                            }
                        } else if let Some(method) = v.get("method").and_then(Value::as_str) {
                            // 广播给订阅者（screencast 帧 / dialog 事件）；无订阅者时丢弃
                            let _ = ev_in_actor.send((
                                method.to_string(),
                                v.get("params").cloned().unwrap_or(Value::Null),
                            ));
                            if let Some(pos) = waiters.iter().position(|(m, _)| method == m || method.starts_with(m.as_str()))
                            {
                                let (_, slot) = waiters.remove(pos);
                                let _ = slot.send(Ok(v.get("params").cloned().unwrap_or(Value::Null)));
                            }
                        }
                    }
                }
            }
        });

        Ok(Self { tx, events: ev_tx, worlds: Arc::new(Mutex::new(HashMap::new())), alive })
    }

    /// 订阅全部 CDP 事件（(方法名, params) 流）。慢消费者会拿到 Lagged 并丢中间帧——
    /// screencast 场景下最新帧胜出，可接受；dialog 等低频事件不受影响。
    pub fn subscribe_events(&self) -> broadcast::Receiver<(String, Value)> {
        self.events.subscribe()
    }

    /// 调用一个 CDP 方法，返回 result（默认 10s 预算）。
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.call_budgeted(method, params, CALL_TIMEOUT).await
    }

    /// 带预算的调用。
    pub async fn call_budgeted(
        &self,
        method: &str,
        params: Value,
        budget: std::time::Duration,
    ) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ToActor::Call { method: method.into(), params, reply: tx })
            .map_err(|_| "CDP actor 已退出".to_string())?;
        match tokio::time::timeout(budget, rx).await {
            Err(_) => Err(format!("CDP 调用超时: {method}")),
            Ok(Err(_)) => Err("CDP actor 已退出".to_string()),
            Ok(Ok(res)) => res,
        }
    }

    /// 发命令并等指定事件到达（等待器先同步注册、命令后入队——同一条 FIFO 通道，
    /// actor 处理顺序即注册顺序，无竞态）。事件超时视为失败（导航没完成）。
    pub async fn call_and_wait_event(
        &self,
        method: &str,
        params: Value,
        event: &str,
        budget: std::time::Duration,
    ) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ToActor::WaitEvent { method: event.into(), reply: tx })
            .map_err(|_| "CDP actor 已退出".to_string())?;
        let result = self.call_budgeted(method, params, budget).await;
        let event_res = match tokio::time::timeout(budget + std::time::Duration::from_secs(2), rx).await {
            Err(_) => Err(format!("等待事件超时: {event}")),
            Ok(Err(_)) => Err("CDP actor 已退出".to_string()),
            Ok(Ok(res)) => res,
        };
        result?;
        event_res
    }

    /// 在页面的**隔离世界**里执行表达式（移植自 ZCode：worldName 隔离 + contextId 求值）。
    /// contextId 按 world 名缓存；页面导航会销毁执行上下文——导航事件会主动清缓存
    /// （见 spawn_tab_events），这里再兜一层：调用报错也清缓存重试一次。
    pub async fn evaluate_in_isolated_world(&self, world: &str, expression: &str) -> Result<Value, String> {
        let mut last_err = String::new();
        for attempt in 0..2 {
            let context_id = match self.ensure_world_context(world).await {
                Ok(id) => id,
                Err(e) => {
                    if attempt == 0 {
                        continue; // 缓存 context 失效时已清缓存，重试走重建
                    }
                    return Err(e);
                }
            };
            let res = match self
                .call(
                    "Runtime.evaluate",
                    json!({
                        "expression": expression,
                        "contextId": context_id,
                        "awaitPromise": true,
                        "returnByValue": true,
                    }),
                )
                .await
            {
                Ok(res) => res,
                Err(e) => {
                    // 页面刚导航过（用户点链接等不走 navigate_tab 的路径）：context 已销毁，
                    // 清缓存重试一次
                    last_err = e;
                    self.invalidate_worlds();
                    if attempt == 0 {
                        continue;
                    }
                    return Err(last_err);
                }
            };
            if res.get("exceptionDetails").is_some() {
                let exc = res.get("exceptionDetails").cloned().unwrap_or(Value::Null);
                let desc = exc
                    .get("exception")
                    .and_then(|e| e.get("description"))
                    .and_then(Value::as_str)
                    .or_else(|| exc.get("text").and_then(Value::as_str))
                    .unwrap_or("未知异常");
                return Err(format!("页面脚本执行异常: {}", desc.chars().take(300).collect::<String>()));
            }
            return res
                .get("result")
                .and_then(|r| r.get("value"))
                .cloned()
                .ok_or_else(|| "隔离世界求值结果缺 value".to_string());
        }
        Err(if last_err.is_empty() { "隔离世界求值失败".to_string() } else { last_err })
    }

    /// 拿（或重建）隔离世界的 executionContextId。
    async fn ensure_world_context(&self, world: &str) -> Result<i64, String> {
        if let Some(id) = self.worlds.lock().ok().and_then(|m| m.get(world).copied()) {
            return Ok(id);
        }
        let tree = self.call("Page.getFrameTree", json!({})).await?;
        let frame_id = tree
            .get("frameTree")
            .and_then(|t| t.get("frame"))
            .and_then(|f| f.get("id"))
            .and_then(Value::as_str)
            .ok_or("拿不到主 frame id（隔离世界需要 frameId）")?
            .to_string();
        let world_res = self
            .call(
                "Page.createIsolatedWorld",
                json!({ "frameId": frame_id, "worldName": world, "grantUniveralAccess": false }),
            )
            .await?;
        let context_id = world_res
            .get("executionContextId")
            .and_then(Value::as_i64)
            .ok_or("Page.createIsolatedWorld 未返回 executionContextId")?;
        if let Ok(mut m) = self.worlds.lock() {
            m.insert(world.to_string(), context_id);
        }
        Ok(context_id)
    }

    /// 页面已导航/上下文销毁时调用：清掉全部隔离世界 context 缓存（下次求值自动重建）。
    pub fn invalidate_worlds(&self) {
        if let Ok(mut m) = self.worlds.lock() {
            m.clear();
        }
    }

    /// 连接是否还活着（actor 退出即假）。
    pub fn is_alive(&self) -> bool {
        self.alive.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Clone for Cdp {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone(), events: self.events.clone(), worlds: self.worlds.clone(), alive: self.alive.clone() }
    }
}
