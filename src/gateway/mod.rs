//! 网关 —— OpenAI 兼容 /chat/completions 单实现（施工单 001）+ 模型列表获取。
//!
//! base_url / model / api_key 全部由调用方（设置系统）动态传入，不再写死。
//! api_key 只出现在内存中的 Authorization 头，绝不写日志/文件。

use std::sync::{Arc, RwLock};

use anyhow::{Context, anyhow};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::core::cancel::CancelFlag;
use crate::observability::{self, EventType};

pub mod providers;

pub struct Gateway {
    /// 热可重建的 HTTP 客户端：reqwest 在构建时固化系统代理设置，
    /// 之后 Clash 开/关/换节点不会自动跟进。网络层失败时 rebuild_client()
    /// 重新读系统代理，服务不用重启就能自愈。
    client: RwLock<reqwest::Client>,
}

fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("构建 HTTP 客户端失败")
}

/// 流式请求的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    Done,
    Cancelled,
}

/// 聚合后的模型回复（P2 核心循环用）：正文 / 思考 / 工具调用。
#[derive(Debug, Clone, Default)]
pub struct AssistantTurn {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    /// 渠道返回的用量（P5：缓存命中率展示用）
    pub prompt_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

pub enum ChatOutcome {
    Done(AssistantTurn),
    Cancelled,
}

impl Gateway {
    pub fn new() -> Self {
        Self { client: RwLock::new(build_client()) }
    }

    /// 重建 HTTP 客户端（重新读系统代理）。网络层失败时由 send() 路径调用；
    /// 集成测试（tests/gateway_retry.rs）也直接调它来驱动阶段切换。
    pub fn rebuild_client_for_test(&self) -> reqwest::Client {
        let fresh = build_client();
        if let Ok(mut slot) = self.client.write() {
            *slot = fresh.clone();
        }
        fresh
    }

    fn rebuild_client(&self) -> reqwest::Client {
        let fresh = self.rebuild_client_for_test();
        observability::event(
            EventType::State,
            json!({ "kind": "gateway.client_rebuild", "note": "网络层失败后重建 HTTP 客户端（重读系统代理）" }),
        );
        fresh
    }

    /// 发请求；两类自愈：
    /// 1) 网络层失败（连接/超时类）→ 重建客户端重试一次（Clash 开关热恢复）；
    /// 2) 渠道 429 → 指数退避重试最多 3 次（尊重 Retry-After 头，缺省 1s/2s/4s），
    ///    仍 429 才把错误抛给上层。用户不该看见瞬时限流。
    async fn send(
        &self,
        build: impl Fn(reqwest::Client) -> reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::Response> {
        let mut rebuilt = false;
        let mut attempt_429 = 0u32;
        loop {
            let client = self.client.read().ok().map(|c| c.clone()).unwrap_or_else(build_client);
            match build(client).send().await {
                Ok(r) => {
                    if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt_429 < 3 {
                        attempt_429 += 1;
                        let wait = r
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.trim_end_matches('s').parse::<u64>().ok())
                            .map(|s| s.min(30))
                            .unwrap_or(2u64.pow(attempt_429 - 1));
                        observability::event(
                            EventType::State,
                            json!({ "kind": "gateway.retry_429", "attempt": attempt_429, "wait_ms": wait * 1000 }),
                        );
                        observability::debug(&format!("[gateway] 429 限流，{wait}s 后第 {attempt_429} 次重试"));
                        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                        continue;
                    }
                    return Ok(r);
                }
                Err(e) if (e.is_connect() || e.is_timeout() || e.is_request()) && !rebuilt => {
                    rebuilt = true;
                    let fresh = self.rebuild_client();
                    match build(fresh).send().await {
                        Ok(r) => return Ok(r),
                        Err(e) => return Err(anyhow!(e.to_string())),
                    }
                }
                Err(e) => return Err(anyhow!(e.to_string())),
            }
        }
    }

    fn chat_url(base_url: &str) -> String {
        let b = base_url.trim().trim_end_matches('/');
        if b.ends_with("/chat/completions") {
            b.to_string()
        } else {
            format!("{b}/chat/completions")
        }
    }

    fn models_url(base_url: &str) -> String {
        let b = base_url.trim().trim_end_matches('/');
        if b.ends_with("/models") {
            b.to_string()
        } else {
            format!("{b}/models")
        }
    }

    /// GET {base}/models → 模型 id 列表（OpenAI 兼容）。
    pub async fn list_models(&self, base_url: &str, api_key: &str) -> anyhow::Result<Vec<String>> {
        let url = Self::models_url(base_url);
        observability::event(EventType::State, json!({ "kind": "gateway.request", "url": url, "body": json!(null) }));
        observability::debug(&format!("[gateway] GET {url}"));

        let response = self
            .send(|c| c.get(&url).bearer_auth(api_key))
            .await
            .with_context(|| format!("请求渠道失败: {url}（网络层不通：检查 Clash/系统代理或本机网络）"))?;

        let status = response.status();
        observability::event(EventType::State, json!({ "kind": "gateway.response", "status": status.as_u16() }));
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            observability::event(
                EventType::Error,
                json!({ "kind": "gateway.error", "status": status.as_u16(), "body": text }),
            );
            return Err(anyhow!("渠道返回 {status}: {text}"));
        }

        let data: Value = response.json().await.context("解析模型列表失败")?;
        let mut ids: Vec<String> = data
            .pointer("/data")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.pointer("/id").and_then(Value::as_str).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids.dedup();
        observability::debug(&format!("[gateway] models: {ids:?}"));
        Ok(ids)
    }

    /// 非流式单轮补全（P7 滚动摘要用）：返回 choices[0].message.content。
    /// 不查取消标志（摘要失败由调用方降级），超时走客户端全局 600s。
    pub async fn complete(
        &self,
        base_url: &str,
        model: &str,
        api_key: &str,
        messages: &Value,
        max_tokens: u64,
    ) -> anyhow::Result<String> {
        if api_key.trim().is_empty() {
            return Err(anyhow!("API key 未设置，无法生成摘要"));
        }
        let url = Self::chat_url(base_url);
        let body = json!({
            "model": model,
            "messages": messages,
            "stream": false,
            "max_tokens": max_tokens,
        });
        observability::event(EventType::State, json!({ "kind": "gateway.request", "url": url, "body": body, "purpose": "summary" }));
        let response = self
            .send(|c| c.post(&url).bearer_auth(api_key).json(&body))
            .await
            .with_context(|| format!("请求渠道失败: {url}"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("渠道返回 {status}: {text}"));
        }
        let data: Value = response.json().await.context("解析摘要响应失败")?;
        let content = data
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Ok(content)
    }

    /// 一轮模型调用 → 流式输出并聚合结果。每个分片：
    /// 1) 写 model_chunk 到 JSONL；2) 回调 on_chunk 转发给前端。
    /// 开始前与每个分片前都查取消标志（核心循环同款检查点）。
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_chat(
        self: &Arc<Self>,
        base_url: &str,
        model: &str,
        api_key: &str,
        messages: &Value,
        tools: Option<&Value>,
        max_tokens: Option<u64>,
        cancel: &CancelFlag,
        on_chunk: impl Fn(&Value),
    ) -> anyhow::Result<ChatOutcome> {
        if api_key.trim().is_empty() {
            let msg = "API key 未设置：请在设置面板填写并保存（或设环境变量 DEEPSEEK_API_KEY）";
            observability::event(EventType::Error, json!({ "message": msg }));
            return Err(anyhow!(msg));
        }

        // 每轮模型决策前先查取消标志
        if cancel.is_set() {
            observability::event(EventType::State, json!({ "kind": "cancel", "phase": "before_request" }));
            return Ok(ChatOutcome::Cancelled);
        }

        let url = Self::chat_url(base_url);
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            // 流式末尾带回 usage（OpenAI 兼容渠道普遍支持；不支持的渠道会忽略）
            "stream_options": { "include_usage": true },
        });
        if let Some(t) = tools {
            body["tools"] = t.clone();
        }
        if let Some(mt) = max_tokens {
            body["max_tokens"] = json!(mt);
        }

        // 硬约束：debug 日志输出实际请求体（body 不含 key）
        observability::event(EventType::State, json!({ "kind": "gateway.request", "url": url, "body": body }));
        observability::debug(&format!("[gateway] POST {url}"));
        observability::debug(&format!("[gateway] request body: {body}"));

        let response = self
            .send(|c| c.post(&url).bearer_auth(api_key).json(&body))
            .await
            .with_context(|| format!("请求渠道失败: {url}（网络层不通：检查 Clash/系统代理或本机网络）"))?;

        let status = response.status();
        observability::event(EventType::State, json!({ "kind": "gateway.response", "status": status.as_u16() }));
        observability::debug(&format!("[gateway] response status: {status}"));
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            observability::event(
                EventType::Error,
                json!({ "kind": "gateway.error", "status": status.as_u16(), "body": text }),
            );
            return Err(anyhow!("渠道返回 {status}: {text}"));
        }

        // —— SSE 解析：按行切，data: 前缀取 JSON，[DONE] 结束；聚合 content/reasoning/tool_calls ——
        // 分片间 120s 无数据即报错（渠道/代理断流时不再干等总超时 600s）
        const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
        let mut stream = response.bytes_stream();
        let mut buf = String::new();
        let mut chunk_count: u64 = 0;
        let mut done = false;
        let mut turn = AssistantTurn::default();
        let mut calls: Vec<ToolCall> = Vec::new();
        loop {
            let item = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                Ok(item) => item,
                Err(_) => anyhow::bail!("模型流超时：120 秒无新数据（渠道或代理可能断流，可重试）"),
            };
            let Some(item) = item else { break };
            let bytes = item.context("读流失败")?;
            buf.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].trim_end_matches('\r').to_string();
                buf.drain(..=pos);
                if line.is_empty() {
                    continue;
                }
                if let Some(data) = line.strip_prefix("data:") {
                    let data = data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    if data == "[DONE]" {
                        done = true;
                        break;
                    }
                    let chunk: Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(e) => {
                            observability::event(
                                EventType::Error,
                                json!({ "message": format!("解析 SSE 分片失败: {e}"), "raw": data }),
                            );
                            continue;
                        }
                    };
                    // 每个分片前查取消标志
                    if cancel.is_set() {
                        observability::event(
                            EventType::State,
                            json!({ "kind": "cancel", "phase": "streaming", "chunks": chunk_count }),
                        );
                        return Ok(ChatOutcome::Cancelled);
                    }
                    // 末尾分片的 usage：OpenAI 风格 prompt_tokens_details.cached_tokens
                    // 或 DeepSeek 风格 prompt_cache_hit_tokens（缓存命中率展示用）
                    if let Some(u) = chunk.get("usage") {
                        if let Some(p) = u.get("prompt_tokens").and_then(Value::as_u64) {
                            turn.prompt_tokens = Some(p);
                            turn.cached_tokens = u
                                .pointer("/prompt_tokens_details/cached_tokens")
                                .and_then(Value::as_u64)
                                .or_else(|| u.get("prompt_cache_hit_tokens").and_then(Value::as_u64));
                        }
                    }
                    let delta = extract_delta(&chunk);
                    let mut payload = json!({ "raw": data, "content": delta.content });
                    if let Some(r) = &delta.reasoning {
                        payload["reasoning_content"] = json!(r);
                    }
                    if let Some(f) = &delta.finish {
                        payload["finish_reason"] = json!(f);
                    }
                    let tc_arr = chunk
                        .pointer("/choices/0/delta/tool_calls")
                        .and_then(Value::as_array)
                        .cloned();
                    if let Some(arr) = &tc_arr {
                        payload["tool_calls"] = json!(arr);
                        accumulate_tool_calls(arr, &mut calls);
                    }
                    turn.content.push_str(&delta.content);
                    if let Some(r) = &delta.reasoning {
                        turn.reasoning.push_str(r);
                    }
                    observability::event(EventType::ModelChunk, payload);
                    observability::debug(&format!("[gateway] sse: {data}"));
                    chunk_count += 1;
                    on_chunk(&chunk);
                }
            }
            if done {
                break;
            }
        }
        turn.tool_calls = calls;
        observability::event(
            EventType::State,
            json!({ "kind": "gateway.stream_end", "chunks": chunk_count, "done": done, "tool_calls": turn.tool_calls.len() }),
        );
        Ok(ChatOutcome::Done(turn))
    }
}

/// 按 index 合并流式 tool_calls 分片（OpenAI 兼容增量格式）。
fn accumulate_tool_calls(arr: &[Value], calls: &mut Vec<ToolCall>) {
    for entry in arr {
        let index = entry.get("index").and_then(Value::as_i64).unwrap_or(0) as usize;
        while calls.len() <= index {
            calls.push(ToolCall { id: String::new(), name: String::new(), arguments: String::new() });
        }
        let call = &mut calls[index];
        if let Some(id) = entry.get("id").and_then(Value::as_str) {
            call.id.push_str(id);
        }
        if let Some(name) = entry.pointer("/function/name").and_then(Value::as_str) {
            call.name.push_str(name);
        }
        if let Some(args) = entry.pointer("/function/arguments").and_then(Value::as_str) {
            call.arguments.push_str(args);
        }
    }
}

pub struct Delta {
    pub content: String,
    pub reasoning: Option<String>,
    pub finish: Option<String>,
}

/// 从分片里提取增量内容与思考内容（reasoning_content，DeepSeek 思考模式）。
pub fn extract_delta(chunk: &Value) -> Delta {
    let choice = chunk.pointer("/choices/0").cloned().unwrap_or(Value::Null);
    let delta = choice.pointer("/delta").cloned().unwrap_or(Value::Null);
    Delta {
        content: delta.pointer("/content").and_then(Value::as_str).unwrap_or("").to_string(),
        reasoning: delta.pointer("/reasoning_content").and_then(Value::as_str).map(String::from),
        finish: choice.pointer("/finish_reason").and_then(Value::as_str).map(String::from),
    }
}

#[cfg(test)]
mod tests {
    //! 客户端热重建机制验证：死代理 → 失败 → 环境恢复 + rebuild → 同一实例恢复连通。
    //! （bin crate 的 tests/ 目录拿不到内部符号，机制测试放这里）
    use super::*;

    fn init_logger() {
        static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| {
            let dir = std::env::temp_dir().join("agent-test-logs");
            let _ = crate::observability::init(&dir, "gw.jsonl", format!("gw-{}", uuid::Uuid::new_v4()));
        });
    }

    #[tokio::test]
    async fn client_rebuild_picks_up_new_proxy_config() {
        init_logger();
        let g = Gateway::new();

        // 阶段1：代理指向死端口 → 必须失败（is_connect 分支）
        unsafe {
            std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:1");
            std::env::set_var("HTTP_PROXY", "http://127.0.0.1:1");
        }
        g.rebuild_client_for_test();
        let r1 = g.list_models("https://api.deepseek.com/v1", "sk-dummy").await;
        assert!(r1.is_err(), "死代理下应失败，实际 {:?}", r1.map(|v| v.len()));

        // 阶段2：清掉死代理 → rebuild → 同一 Gateway 实例应拿到渠道业务响应
        // （401 = 网络层已通，渠道在拒绝假 key——这正是"网络层恢复"的证据）
        unsafe {
            std::env::remove_var("HTTPS_PROXY");
            std::env::remove_var("HTTP_PROXY");
        }
        g.rebuild_client_for_test();
        let r2 = g.list_models("https://api.deepseek.com/v1", "sk-dummy").await;
        let msg = r2.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            msg.contains("401") && !msg.contains("请求渠道失败"),
            "重建后应拿到渠道 401（网络层恢复），实际: {msg}"
        );
    }
}
