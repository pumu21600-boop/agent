//! CDP（Chrome DevTools Protocol）客户端 —— P20 浏览器内核。
//!
//! 进程内直持 CDP 会话：本进程拉起 Edge/Chrome（headless）、直连**页面级** WebSocket
//! 端点，JSON-RPC 收发全部自持（不经过任何外部 driver）。协议面刻意收窄到 v1 需要的
//! 几个域：Page（导航/加载事件）、Runtime（求值）、Input（鼠标/键盘）。
//!
//! 架构：单 actor 任务独占 WS 流（select 命令通道 + WS 消息），请求按 id 配对经
//! oneshot 回传；事件等待器与命令走同一条 FIFO 通道，「先注册等待器再发命令」
//! 在 actor 处理顺序上天然成立，无竞态。调用超时在客户端侧计时，超时后等待器
//! 残留由 actor 在事件到达时「发入已关闭通道」自然丢弃。

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

/// 单次 CDP 调用的默认预算。
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

enum ToActor {
    Call { method: String, params: Value, reply: oneshot::Sender<Result<Value, String>> },
    WaitEvent { method: String, reply: oneshot::Sender<Result<Value, String>> },
}

/// 页面级 CDP 会话句柄。
pub struct Cdp {
    tx: mpsc::UnboundedSender<ToActor>,
}

impl Cdp {
    /// 连接页面级 CDP 端点并起 actor。
    pub async fn connect(ws_url: &str) -> Result<Self, String> {
        let (ws, _resp) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| format!("CDP WebSocket 连接失败: {e}"))?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<ToActor>();

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
                        } else if let Some(method) = v.get("method").and_then(Value::as_str)
                            && let Some(pos) = waiters.iter().position(|(m, _)| method == m || method.starts_with(m.as_str()))
                        {
                            let (_, slot) = waiters.remove(pos);
                            let _ = slot.send(Ok(v.get("params").cloned().unwrap_or(Value::Null)));
                        }
                    }
                }
            }
        });

        Ok(Self { tx })
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

    /// 同步注册一个事件等待器，稍后再 await 回传端（先注册后触发动作的时序由调用方保证）。
    pub fn register_wait(
        &self,
        event: &str,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<Value, String>>, String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ToActor::WaitEvent { method: event.into(), reply: tx })
            .map_err(|_| "CDP actor 已退出".to_string())?;
        Ok(rx)
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
}

impl Clone for Cdp {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone() }
    }
}
