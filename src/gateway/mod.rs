//! 网关 —— OpenAI 兼容 /chat/completions 单实现（施工单 001）+ 模型列表获取。
//!
//! base_url / model / api_key 全部由调用方（设置系统）动态传入，不再写死。
//! api_key 只出现在内存中的 Authorization 头，绝不写日志/文件。
//! P8：采样参数、本地限流（max_rpm）、embeddings 接口。
//! P9（OpenClaw 对齐）：思考参数按「端点探测（compat）+ reasoning_effort 档位解析（effort）」
//! 组装，词表与规则移植自 OpenClaw v2026.8.2 packages/ai；响应端支持
//! reasoning_content/reasoning/reasoning_text/reasoning_details 四字段 + 14 种思考标签剥离；
//! 发送前按端点契约净化历史（回放字段/developer 角色/空消息过滤）。
//! P21（施工单 030）：**计量收口在网关单点**——stream_chat / complete 是全部模型流量的
//! 唯一出入口，每次调用记一条用量台账（token 三路分计量 + 状态 + 延迟），
//! 并在发送前过「全局花费上限」硬闸。

use std::sync::{Arc, RwLock};
use std::time::Instant;

use anyhow::{Context, anyhow};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::core::cancel::CancelFlag;
use crate::observability::{self, EventType};

pub mod anthropic;
pub mod compat;
pub mod effort;
pub mod providers;
pub mod replay;
pub mod tags;

pub mod openai;
mod accounting;

pub use accounting::*;
pub use openai::*;

#[cfg(test)]
mod tests;
use compat::{Compat, Protocol, ThinkingFormat};
use providers::ProviderPreset;
use tags::TagSplit;

pub struct Gateway {
    /// 热可重建的 HTTP 客户端：reqwest 在构建时固化系统代理设置，
    /// 之后 Clash 开/关/换节点不会自动跟进。网络层失败时 rebuild_client()
    /// 重新读系统代理，服务不用重启就能自愈。
    client: RwLock<reqwest::Client>,
    /// 本地限流（P8）：Some=请求间最小间隔（60/max_rpm 秒，向上取整），None=不限。
    throttle: Option<Throttle>,
    /// P23 思考参数自适应降级登记：渠道 400 拒收思考字段后剥参重试成功，
    /// 这里记下「base_url|model」，后续请求直接不发思考字段（每轮渠道组装时出人话提示）。
    thinking_degraded: RwLock<std::collections::HashSet<String>>,
}

/// 请求参数（P9）：全部可选，None=不发字段走渠道默认。
/// 思考接线不再自带厂商字段——由 compat 按端点探测；用户设置只有开/关 + 档位。
#[derive(Debug, Clone)]
pub struct ChatParams {
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// 思考档位请求：Some("on")/Some("off")/Some("low"|"medium"|"high"|"xhigh"|"max"|"minimal")；
    /// None=渠道默认（不发思考参数）。on 在 effort 侧解析为 medium，布尔/对象侧一律开。
    pub thinking: Option<String>,
    /// 渠道配置的 thinking_format 覆盖（providers.json 预设或设置面板）；None=端点探测。
    pub thinking_format: Option<ThinkingFormat>,
    /// 端点探测线索：渠道预设 id 与实际 base_url（build_body 内部探测用）。
    pub provider_id: Option<String>,
    pub base_url: Option<String>,
    /// A3 能力标记「工具调用」闭环（施工单 012）：false 时核心循环不携带 tools 参数，
    /// 模型无法发起新工具调用；历史消息里的 tool_calls 回放不受影响。默认 true。
    pub tools_enabled: bool,
}

impl Default for ChatParams {
    fn default() -> Self {
        Self {
            max_tokens: None,
            temperature: None,
            top_p: None,
            thinking: None,
            thinking_format: None,
            provider_id: None,
            base_url: None,
            tools_enabled: true,
        }
    }
}

impl ChatParams {
    /// 从供应商预设取覆盖字段（用户设置字段由调用方另行填入）。
    pub fn from_preset(p: &ProviderPreset) -> Self {
        Self {
            thinking_format: p.thinking_format,
            provider_id: Some(p.id.clone()),
            ..Default::default()
        }
    }
}

/// 本地限流器（P8）：请求间保证最小间隔，排队等待而非拒绝。
struct Throttle {
    min_interval: std::time::Duration,
    last: tokio::sync::Mutex<Option<Instant>>,
}

impl Throttle {
    fn new(max_rpm: u32) -> Self {
        // ceil(60_000 / rpm) ms；rpm=0 不该走到这（Gateway 里已判空）
        let ms = (60_000f64 / max_rpm.max(1) as f64).ceil() as u64;
        Self { min_interval: std::time::Duration::from_millis(ms.max(1)), last: tokio::sync::Mutex::new(None) }
    }

    async fn gate(&self) {
        let wait = {
            let mut last = self.last.lock().await;
            let w = match *last {
                Some(t) => self
                    .min_interval
                    .checked_sub(t.elapsed())
                    .unwrap_or(std::time::Duration::ZERO),
                None => std::time::Duration::ZERO,
            };
            if w.is_zero() {
                *last = Some(Instant::now());
            }
            w
        };
        if !wait.is_zero() {
            observability::event(
                EventType::State,
                json!({ "kind": "gateway.throttle", "wait_ms": wait.as_millis() as u64 }),
            );
            tokio::time::sleep(wait).await;
            // 睡完补记时间戳，保证下一发的间隔基准
            *self.last.lock().await = Some(Instant::now());
        }
    }
}

fn build_client() -> reqwest::Client {
    // 构建失败（TLS 后端异常等）不该 panic 拖垮整个服务：退回默认配置的客户端，
    // 超时/连接超时用默认值，比「网关起不来」好得多。
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|e| {
            crate::observability::try_event(
                crate::observability::EventType::State,
                serde_json::json!({ "kind": "gateway.client_fallback", "error": e.to_string() }),
            );
            reqwest::Client::new()
        })
}

/// 聚合后的模型回复（P2 核心循环用）：正文 / 思考 / 工具调用。
#[derive(Debug, Clone, Default)]
pub struct AssistantTurn {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    /// 渠道返回的用量（P5：缓存命中率展示用；P21：台账分计量）
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    /// 结束原因（P7.6："length"=输出被 max_tokens 截断，核心循环据此提示用户）
    pub finish_reason: Option<String>,
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
        Self { client: RwLock::new(build_client()), throttle: None, thinking_degraded: RwLock::new(std::collections::HashSet::new()) }
    }

    /// 该渠道（base_url|model）是否已被登记为「端点拒收思考参数」。
    pub fn thinking_is_degraded(&self, base_url: &str, model: &str) -> bool {
        self.thinking_degraded
            .read()
            .map(|s| s.contains(&Self::degrade_key(base_url, model)))
            .unwrap_or(false)
    }

    fn mark_thinking_degraded(&self, base_url: &str, model: &str) {
        if let Ok(mut s) = self.thinking_degraded.write() {
            s.insert(Self::degrade_key(base_url, model));
        }
    }

    fn degrade_key(base_url: &str, model: &str) -> String {
        format!("{}|{}", base_url.trim(), model.trim())
    }

    /// 请求体是否携带思考字段（两协议：thinking 对象 / enable_thinking / reasoning_effort / 聊天模板）。
    fn body_has_thinking_params(body: &Value) -> bool {
        ["thinking", "enable_thinking", "reasoning_effort", "chat_template_kwargs"]
            .iter()
            .any(|k| body.get(*k).is_some())
    }

    /// 打开本地限流（P8）：max_rpm>0 时生效。启动时由 main 配置一次（Arc 包装前）。
    pub fn set_max_rpm(&mut self, max_rpm: u32) {
        if max_rpm > 0 && self.throttle.is_none() {
            self.throttle = Some(Throttle::new(max_rpm));
        }
    }

    /// 发请求前过本地限流（max_rpm=0 时直通）。
    async fn gate(&self) {
        if let Some(t) = &self.throttle {
            t.gate().await;
        }
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
        self.gate().await;
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
                Err(e) if e.is_connect() || e.is_timeout() || e.is_request() => {
                    // 网络类失败：热重建客户端再试一次（成败都收口，不回到循环头）
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

    /// 按协议取对话端点 URL：OpenAI /chat/completions；Anthropic /v1/messages（P23）。
    fn request_url(base_url: &str, protocol: Protocol) -> String {
        match protocol {
            Protocol::OpenAiCompletions => Self::chat_url(base_url),
            Protocol::AnthropicMessages => anthropic::messages_url(base_url),
        }
    }

    /// 按协议挂鉴权：OpenAI Authorization Bearer；Anthropic x-api-key + anthropic-version（P23）。
    fn auth(rb: reqwest::RequestBuilder, api_key: &str, protocol: Protocol) -> reqwest::RequestBuilder {
        match protocol {
            Protocol::OpenAiCompletions => rb.bearer_auth(api_key),
            Protocol::AnthropicMessages => rb
                .header("x-api-key", api_key)
                .header("anthropic-version", anthropic::ANTHROPIC_VERSION),
        }
    }

    /// GET {base}/models → 模型 id 列表（OpenAI 兼容）。
    pub async fn list_models(&self, base_url: &str, api_key: &str) -> anyhow::Result<Vec<String>> {
        // Anthropic /v1/models 的 data[].id 与 OpenAI 同形，仅 URL 与鉴权头不同（P23）
        let protocol = if anthropic::is_anthropic_endpoint(None, Some(base_url)) {
            Protocol::AnthropicMessages
        } else {
            Protocol::OpenAiCompletions
        };
        let url = match protocol {
            Protocol::AnthropicMessages => anthropic::models_url(base_url),
            Protocol::OpenAiCompletions => Self::models_url(base_url),
        };
        observability::event(EventType::State, json!({ "kind": "gateway.request", "url": url, "body": json!(null) }));
        observability::debug(&format!("[gateway] GET {url}"));

        let response = self
            .send(|c| Self::auth(c.get(&url), api_key, protocol))
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

    /// 非流式单轮补全（P7 滚动摘要 / P8 标题生成 / 媒体旁路用）：返回 choices[0].message.content。
    /// 不查取消标志（摘要失败由调用方降级），超时走客户端全局 600s。
    /// P21：与流式同一道计量（台账 + 花费硬闸），ctx.purpose 区分用途。
    pub async fn complete(
        &self,
        ctx: &UsageCtx,
        base_url: &str,
        model: &str,
        api_key: &str,
        messages: &Value,
        params: &ChatParams,
    ) -> anyhow::Result<String> {
        let started = Instant::now();
        let compat = compat::resolve(
            params.provider_id.as_deref(),
            params.base_url.as_deref().or(Some(base_url.trim())),
            model,
            params.thinking_format,
        );
        let url = Self::request_url(base_url, compat.protocol);
        // P23 降级登记：该端点已确认拒收思考字段 → 本请求直接不发（人话提示由每轮渠道组装时出）
        let degraded = self.thinking_is_degraded(base_url, model);
        let eff_params = if degraded {
            let mut p = params.clone();
            p.thinking = None;
            p
        } else {
            params.clone()
        };
        let mut outgoing = messages.clone();
        replay::apply_outgoing_transforms(&mut outgoing, model, &compat);
        let body = build_body_with_compat(model, &outgoing, false, &eff_params, None, &compat);
        observability::event(EventType::State, json!({ "kind": "gateway.request", "url": url, "body": body, "purpose": "aux" }));
        // 红线1（P21）：花费上限硬闸——辅助调用与主流程同一道闸
        if let Some(msg) = crate::usage::cap_block() {
            observability::event(
                EventType::State,
                json!({ "kind": "cost.cap_blocked", "session": ctx.session_id, "purpose": ctx.purpose, "model": model, "spend_usd": crate::usage::spend_total(), "cap_usd": crate::usage::cap_value() }),
            );
            return Err(anyhow!(msg));
        }
        let mut response = match self
            .send(|c| Self::auth(c.post(&url), api_key, compat.protocol).json(&body))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                record_usage(ctx, base_url, params.provider_id.as_deref(), model, started.elapsed().as_millis() as u64, UsageOutcome::err("error", format!("{e:#}")));
                return Err(anyhow!("请求渠道失败: {url}"));
            }
        };
        // P23 思考参数自适应降级：400 且请求带思考字段 → 剥参重试一次；
        // 成功则登记该渠道（后续请求不再发思考字段），失败则抛出重试的真实错误
        if response.status().as_u16() == 400 && !degraded && Self::body_has_thinking_params(&body) {
            let text = response.text().await.unwrap_or_default();
            observability::event(
                EventType::State,
                json!({ "kind": "gateway.thinking_degrade_retry", "status": 400, "url": url, "model": model, "error_body": text.chars().take(300).collect::<String>() }),
            );
            let retry_params = {
                let mut p = params.clone();
                p.thinking = None;
                p
            };
            let retry_body = build_body_with_compat(model, &outgoing, false, &retry_params, None, &compat);
            response = match self
                .send(|c| Self::auth(c.post(&url), api_key, compat.protocol).json(&retry_body))
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    record_usage(ctx, base_url, params.provider_id.as_deref(), model, started.elapsed().as_millis() as u64, UsageOutcome::err("error", format!("{e:#}")));
                    return Err(anyhow!("请求渠道失败: {url}"));
                }
            };
            if response.status().is_success() {
                self.mark_thinking_degraded(base_url, model);
                observability::event(
                    EventType::State,
                    json!({ "kind": "gateway.thinking_degraded", "url": url, "model": model, "note": "端点拒收思考参数，剥参重试成功；该渠道后续请求不再发送思考字段" }),
                );
            }
        }
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            record_usage(ctx, base_url, params.provider_id.as_deref(), model, started.elapsed().as_millis() as u64, UsageOutcome::err("error", format!("渠道返回 {status}")));
            return Err(anyhow!("渠道返回 {status}: {text}"));
        }
        let data: Value = response.json().await.context("解析补全响应失败")?;
        if let Some(err) = data.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or_default();
            record_usage(ctx, base_url, params.provider_id.as_deref(), model, started.elapsed().as_millis() as u64, UsageOutcome::err("error", format!("渠道业务错误: {msg}")));
            return Err(anyhow!("渠道业务错误: {msg} (raw: {err})"));
        }
        // 台账：usage 三路分计量（渠道不回 usage 时记 0，请求与延迟照记）
        let (p, c, k) = usage_triple(&data);
        record_usage(ctx, base_url, params.provider_id.as_deref(), model, started.elapsed().as_millis() as u64, UsageOutcome::ok((p, c, k)));
        // 取正文：Anthropic content[] 拼 text 块（思考走独立块不混正文）；OpenAI choices[0].message.content
        let raw = match compat.protocol {
            Protocol::AnthropicMessages => data
                .get("content")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect::<String>()
                })
                .unwrap_or_default(),
            Protocol::OpenAiCompletions => data
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        };
        if compat.protocol == Protocol::AnthropicMessages {
            return Ok(raw);
        }
        // 反代 <think> 当正文吐：剥离标签，摘要只要正文
        let mut ts = TagSplit::default();
        let (_, mut clean) = ts.feed(&raw);
        let (_, tail) = ts.finish();
        clean.push_str(&tail);
        Ok(clean)
    }

    /// 向量嵌入（P8 EMBEDDING 角色预留，OpenAI 兼容 /embeddings）。
    /// 记忆 v1 不消费；语义检索需求真实出现后由上层接线。
    pub async fn embed(
        &self,
        base_url: &str,
        model: &str,
        api_key: &str,
        input: &[String],
    ) -> anyhow::Result<Vec<Vec<f64>>> {
        if api_key.trim().is_empty() {
            return Err(anyhow!("API key 未设置，无法调用 embeddings"));
        }
        let b = base_url.trim().trim_end_matches('/');
        let url = format!("{b}/embeddings");
        let body = json!({ "model": model, "input": input });
        observability::event(EventType::State, json!({ "kind": "gateway.request", "url": url, "purpose": "embedding", "inputs": input.len() }));
        let response = self
            .send(|c| c.post(&url).bearer_auth(api_key).json(&body))
            .await
            .with_context(|| format!("请求渠道失败: {url}"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow!("渠道返回 {status}: {text}"));
        }
        let data: Value = response.json().await.context("解析 embeddings 响应失败")?;
        let vecs: Vec<Vec<f64>> = data
            .pointer("/data")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|d| {
                        d.get("embedding")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(Value::as_f64).collect())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default();
        if vecs.is_empty() {
            return Err(anyhow!("embeddings 响应不含向量"));
        }
        Ok(vecs)
    }
}

/// 一轮模型调用的目标渠道（P8 降级链/角色映射的基本单位）。
#[derive(Debug, Clone, Default)]
pub struct Channel {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub params: ChatParams,
}

impl Channel {
    pub fn new(base_url: &str, model: &str, api_key: &str, params: ChatParams) -> Self {
        Self {
            base_url: base_url.to_string(),
            model: model.to_string(),
            api_key: api_key.to_string(),
            params,
        }
    }
}

impl Gateway {
    /// 一轮模型调用 → 流式输出并聚合结果。
    ///
    /// 每个分片：1) 写 model_chunk 到 JSONL；2) 回调 on_chunk 转发给前端。
    /// 开始前与每个分片前都查取消标志（核心循环同款检查点）。
    /// P21：ctx 带归属信息（会话/用途/父任务），本方法内完成台账记录与花费硬闸。
    pub async fn stream_chat(
        self: &Arc<Self>,
        ctx: &UsageCtx,
        channel: &Channel,
        messages: &Value,
        tools: Option<&Value>,
        cancel: &CancelFlag,
        on_chunk: impl Fn(&Value),
    ) -> anyhow::Result<ChatOutcome> {
        let base_url = &channel.base_url;
        let model = &channel.model;
        let started = Instant::now();
        let provider_id = channel.params.provider_id.clone();
        // 红线1（P21）：花费上限硬闸——降级链每次尝试都过同一道闸
        if let Some(msg) = crate::usage::cap_block() {
            observability::event(
                EventType::State,
                json!({ "kind": "cost.cap_blocked", "session": ctx.session_id, "purpose": ctx.purpose, "model": model, "spend_usd": crate::usage::spend_total(), "cap_usd": crate::usage::cap_value() }),
            );
            return Err(anyhow!(msg));
        }
        if channel.api_key.trim().is_empty() {
            let msg = "API key 未设置：请在设置面板填写并保存（或设环境变量 DEEPSEEK_API_KEY）";
            observability::event(EventType::Error, json!({ "message": msg }));
            return Err(anyhow!(msg));
        }

        // 每轮模型决策前先查取消标志
        if cancel.is_set() {
            observability::event(EventType::State, json!({ "kind": "cancel", "phase": "before_request" }));
            return Ok(ChatOutcome::Cancelled);
        }

        let compat = compat::resolve(
            channel.params.provider_id.as_deref(),
            Some(base_url.trim()),
            &channel.model,
            channel.params.thinking_format,
        );
        let url = Self::request_url(base_url, compat.protocol);
        // P23 降级登记：该端点已确认拒收思考字段 → 本请求直接不发（人话提示由每轮渠道组装时出）
        let degraded = self.thinking_is_degraded(base_url, model);
        let eff_params = if degraded {
            let mut p = channel.params.clone();
            p.thinking = None;
            p
        } else {
            channel.params.clone()
        };
        let mut outgoing = messages.clone();
        replay::apply_outgoing_transforms(&mut outgoing, &channel.model, &compat);
        // P27.3 思考预算扩容：思考开启时上游把思考+正文算进同一个 max_tokens，用户配的
        // 「最大输出长度」会被思考吃掉导致正文莫名截断——发送值加思考余量（端点硬上限
        // 拒收时下方回退原值重试）。思考关闭/未配 max_tokens 时等价原值。
        let mut send_params = eff_params.clone();
        let inflated = match (eff_params.max_tokens, effective_max_tokens(&eff_params)) {
            (Some(orig), Some(eff)) if eff != orig => {
                send_params.max_tokens = Some(eff);
                observability::event(
                    EventType::State,
                    json!({ "kind": "gateway.max_tokens_inflated", "url": url, "model": model, "configured": orig, "sent": eff }),
                );
                true
            }
            _ => false,
        };
        let body = build_body_with_compat(model, &outgoing, true, &send_params, tools, &compat);

        // 硬约束：debug 日志输出实际请求体（body 不含 key）
        observability::event(
            EventType::State,
            json!({ "kind": "gateway.request", "url": url, "body": body, "reasoning": {
                "protocol": compat.protocol.as_str(),
                "thinking_format": compat.thinking_format.as_str(),
                "thinking": channel.params.thinking,
            } }),
        );
        observability::debug(&format!("[gateway] POST {url}"));
        observability::debug(&format!("[gateway] request body: {body}"));

        let mut response = match self
            .send(|c| Self::auth(c.post(&url), &channel.api_key, compat.protocol).json(&body))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                record_usage(ctx, base_url, provider_id.as_deref(), model, started.elapsed().as_millis() as u64, UsageOutcome::err("error", format!("{e:#}")));
                return Err(anyhow!("请求渠道失败: {url}（网络层不通：检查 Clash/系统代理或本机网络）"));
            }
        };

        // P23 思考参数自适应降级：400 且请求带思考字段 → 剥参重试一次；
        // 成功则登记该渠道（后续请求不再发思考字段），失败则抛出重试的真实错误
        let mut degrade_retried = false;
        if response.status().as_u16() == 400 && !degraded && Self::body_has_thinking_params(&body) {
            degrade_retried = true;
            let text = response.text().await.unwrap_or_default();
            observability::event(
                EventType::State,
                json!({ "kind": "gateway.thinking_degrade_retry", "status": 400, "url": url, "model": model, "error_body": text.chars().take(300).collect::<String>() }),
            );
            let retry_params = {
                let mut p = channel.params.clone();
                p.thinking = None;
                p
            };
            let retry_body = build_body_with_compat(model, &outgoing, true, &retry_params, tools, &compat);
            response = match self
                .send(|c| Self::auth(c.post(&url), &channel.api_key, compat.protocol).json(&retry_body))
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    record_usage(ctx, base_url, provider_id.as_deref(), model, started.elapsed().as_millis() as u64, UsageOutcome::err("error", format!("{e:#}")));
                    return Err(anyhow!("请求渠道失败: {url}（网络层不通：检查 Clash/系统代理或本机网络）"));
                }
            };
            if response.status().is_success() {
                self.mark_thinking_degraded(base_url, model);
                observability::event(
                    EventType::State,
                    json!({ "kind": "gateway.thinking_degraded", "url": url, "model": model, "note": "端点拒收思考参数，剥参重试成功；该渠道后续请求不再发送思考字段" }),
                );
            }
        }

        // P27.3 扩容回退：扩容后的 max_tokens 超出该模型硬上限被 400 拒收 → 用配置原值
        // 重试一次（思考参数不动；剥参重试已跑过则不重复——它本来就用的原值）
        if response.status().as_u16() == 400 && inflated && !degrade_retried {
            let text = response.text().await.unwrap_or_default();
            observability::event(
                EventType::State,
                json!({ "kind": "gateway.max_tokens_inflate_retry", "status": 400, "url": url, "model": model, "error_body": text.chars().take(300).collect::<String>() }),
            );
            let retry_body = build_body_with_compat(model, &outgoing, true, &eff_params, tools, &compat);
            response = match self
                .send(|c| Self::auth(c.post(&url), &channel.api_key, compat.protocol).json(&retry_body))
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    record_usage(ctx, base_url, provider_id.as_deref(), model, started.elapsed().as_millis() as u64, UsageOutcome::err("error", format!("{e:#}")));
                    return Err(anyhow!("请求渠道失败: {url}（网络层不通：检查 Clash/系统代理或本机网络）"));
                }
            };
            if response.status().is_success() {
                observability::event(
                    EventType::State,
                    json!({ "kind": "gateway.max_tokens_inflate_fallback", "url": url, "model": model, "note": "扩容值超出模型硬上限，已回退配置原值" }),
                );
            }
        }

        let status = response.status();
        observability::event(EventType::State, json!({ "kind": "gateway.response", "status": status.as_u16() }));
        observability::debug(&format!("[gateway] response status: {status}"));
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            observability::event(
                EventType::Error,
                json!({ "kind": "gateway.error", "status": status.as_u16(), "body": text }),
            );
            record_usage(ctx, base_url, provider_id.as_deref(), model, started.elapsed().as_millis() as u64, UsageOutcome::err("error", format!("渠道返回 {status}")));
            return Err(anyhow!("渠道返回 {status}: {text}"));
        }

        // —— SSE 解析：按行切，data: 前缀取 JSON；聚合 content/reasoning/tool_calls ——
        // 分片间 120s 无数据即报错（渠道/代理断流时不再干等总超时 600s）
        const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
        let mut stream = response.bytes_stream();
        let mut buf = String::new();
        let mut done = false;
        let mut agg = StreamAgg::new();
        // Anthropic 协议（P23）：事件流经 SseTranslator 合成回 OpenAI delta 分片再进同一聚合器
        let mut translator = anthropic::SseTranslator::default();
        let is_anthropic = compat.protocol == Protocol::AnthropicMessages;
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
                // event: 行直接忽略——Anthropic data 自带 type，OpenAI 只认 data:
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                if !is_anthropic && data == "[DONE]" {
                    done = true;
                    break;
                }
                let mut chunk: Value = match serde_json::from_str(data) {
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
                        json!({ "kind": "cancel", "phase": "streaming", "chunks": agg.chunk_count }),
                    );
                    record_usage(ctx, base_url, provider_id.as_deref(), model, started.elapsed().as_millis() as u64, UsageOutcome::err("cancelled", "用户取消".into()));
                    return Ok(ChatOutcome::Cancelled);
                }
                if is_anthropic {
                    match translator.translate(&chunk) {
                        Ok(anthropic::Translated::Chunk(mut synthetic)) => {
                            agg.feed(&mut synthetic, data);
                            on_chunk(&synthetic);
                        }
                        Ok(anthropic::Translated::Done) => {
                            done = true;
                            break;
                        }
                        Ok(anthropic::Translated::Ignore) => {}
                        Err(msg) => {
                            record_usage(ctx, base_url, provider_id.as_deref(), model, started.elapsed().as_millis() as u64, UsageOutcome::err("error", format!("渠道流式错误: {msg}")));
                            anyhow::bail!("渠道流式错误: {msg}");
                        }
                    }
                } else {
                    agg.feed(&mut chunk, data);
                    on_chunk(&chunk);
                }
            }
            if done {
                break;
            }
        }
        agg.finish();
        let turn = agg.turn;
        // P21 台账：成功调用记 usage 三路（渠道不回 usage 时 token 记 0，请求与延迟照记）
        record_usage(
            ctx,
            base_url,
            provider_id.as_deref(),
            model,
            started.elapsed().as_millis() as u64,
            UsageOutcome::ok((
                turn.prompt_tokens.unwrap_or(0),
                turn.completion_tokens.unwrap_or(0),
                turn.cached_tokens.unwrap_or(0),
            )),
        );
        observability::event(
            EventType::State,
            json!({ "kind": "gateway.stream_end", "chunks": agg.chunk_count, "done": done, "tool_calls": turn.tool_calls.len() }),
        );
        Ok(ChatOutcome::Done(turn))
    }
}
