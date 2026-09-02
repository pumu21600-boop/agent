//! 网关 —— OpenAI 兼容 /chat/completions 单实现（施工单 001）+ 模型列表获取。
//!
//! base_url / model / api_key 全部由调用方（设置系统）动态传入，不再写死。
//! api_key 只出现在内存中的 Authorization 头，绝不写日志/文件。

use std::sync::Arc;

use anyhow::{Context, anyhow};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::cancel::CancelFlag;
use crate::logging::{self, EventType};

pub struct Gateway {
    client: reqwest::Client,
}

/// 流式请求的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    Done,
    Cancelled,
}

impl Gateway {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("构建 HTTP 客户端失败");
        Self { client }
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
        logging::event(EventType::State, json!({ "kind": "gateway.request", "url": url, "body": json!(null) }));
        logging::debug(&format!("[gateway] GET {url}"));

        let response = self
            .client
            .get(&url)
            .bearer_auth(api_key)
            .send()
            .await
            .with_context(|| format!("请求渠道失败: {url}"))?;

        let status = response.status();
        logging::event(EventType::State, json!({ "kind": "gateway.response", "status": status.as_u16() }));
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            logging::event(
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
        logging::debug(&format!("[gateway] models: {ids:?}"));
        Ok(ids)
    }

    /// 单轮对话 → 流式输出。每个模型分片：
    /// 1) 写 model_chunk 到 JSONL；2) 回调 on_chunk 转发给前端。
    /// 开始前与每个分片前都检查取消标志。
    pub async fn stream_chat(
        self: &Arc<Self>,
        base_url: &str,
        model: &str,
        api_key: &str,
        message: &str,
        cancel: &CancelFlag,
        on_chunk: impl Fn(&Value),
    ) -> anyhow::Result<StreamOutcome> {
        if api_key.trim().is_empty() {
            let msg = "API key 未设置：请在设置面板填写并保存（或设环境变量 DEEPSEEK_API_KEY）";
            logging::event(EventType::Error, json!({ "message": msg }));
            return Err(anyhow!(msg));
        }

        // 每轮模型决策前先查取消标志
        if cancel.is_set() {
            logging::event(EventType::State, json!({ "kind": "cancel", "phase": "before_request" }));
            return Ok(StreamOutcome::Cancelled);
        }

        let url = Self::chat_url(base_url);
        let body = json!({
            "model": model,
            "messages": [{ "role": "user", "content": message }],
            "stream": true,
        });

        // 硬约束：debug 日志输出实际请求体（body 不含 key）
        logging::event(EventType::State, json!({ "kind": "gateway.request", "url": url, "body": body }));
        logging::debug(&format!("[gateway] POST {url}"));
        logging::debug(&format!("[gateway] request body: {body}"));

        let response = self
            .client
            .post(&url)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("请求渠道失败: {url}"))?;

        let status = response.status();
        logging::event(EventType::State, json!({ "kind": "gateway.response", "status": status.as_u16() }));
        logging::debug(&format!("[gateway] response status: {status}"));
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            logging::event(
                EventType::Error,
                json!({ "kind": "gateway.error", "status": status.as_u16(), "body": text }),
            );
            return Err(anyhow!("渠道返回 {status}: {text}"));
        }

        // —— SSE 解析：按行切，data: 前缀取 JSON，[DONE] 结束 ——
        let mut stream = response.bytes_stream();
        let mut buf = String::new();
        let mut chunk_count: u64 = 0;
        let mut done = false;
        while let Some(item) = stream.next().await {
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
                            logging::event(
                                EventType::Error,
                                json!({ "message": format!("解析 SSE 分片失败: {e}"), "raw": data }),
                            );
                            continue;
                        }
                    };
                    // 每个分片前查取消标志
                    if cancel.is_set() {
                        logging::event(
                            EventType::State,
                            json!({ "kind": "cancel", "phase": "streaming", "chunks": chunk_count }),
                        );
                        return Ok(StreamOutcome::Cancelled);
                    }
                    let delta = extract_delta(&chunk);
                    let mut payload = json!({ "raw": data, "content": delta.content });
                    if let Some(r) = &delta.reasoning {
                        payload["reasoning_content"] = json!(r);
                    }
                    if let Some(f) = &delta.finish {
                        payload["finish_reason"] = json!(f);
                    }
                    logging::event(EventType::ModelChunk, payload);
                    logging::debug(&format!("[gateway] sse: {data}"));
                    chunk_count += 1;
                    on_chunk(&chunk);
                }
            }
            if done {
                break;
            }
        }
        logging::event(
            EventType::State,
            json!({ "kind": "gateway.stream_end", "chunks": chunk_count, "done": done }),
        );
        Ok(StreamOutcome::Done)
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
