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

/// 组装 /chat/completions 请求体（纯函数，单测覆盖各端点样式）。
/// 硬约束：不含 api_key；思考参数按端点 compat 适配而非按 provider id 分支。
/// 生产路径走 build_body_with_compat（发送前先探测一次 compat 并复用）；本包装只给单测用。
#[cfg(test)]
pub fn build_body(
    model: &str,
    messages: &Value,
    stream: bool,
    params: &ChatParams,
    tools: Option<&Value>,
) -> Value {
    let compat = compat::resolve(
        params.provider_id.as_deref(),
        params.base_url.as_deref(),
        model,
        params.thinking_format,
    );
    build_body_with_compat(model, messages, stream, params, tools, &compat)
}

/// 带 compat 的组装（供网关在发送前先做历史净化时复用同一次探测）。
pub fn build_body_with_compat(
    model: &str,
    messages: &Value,
    stream: bool,
    params: &ChatParams,
    tools: Option<&Value>,
    compat: &Compat,
) -> Value {
    // Anthropic Messages 协议（P23）：请求体形状完全不同，交给专用适配器；
    // 响应侧同样由适配器合成为 OpenAI 分片，下游零感知
    if compat.protocol == Protocol::AnthropicMessages {
        return anthropic::build_body(model, messages, stream, params, tools);
    }
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
    });
    if stream && compat.supports_usage_in_streaming {
        // 流式末尾带回 usage（非标端点不收 stream_options，按 compat 跳过）
        body["stream_options"] = json!({ "include_usage": true });
    }
    if let Some(t) = tools {
        body["tools"] = t.clone();
    }
    if let Some(mt) = params.max_tokens {
        body[compat.max_tokens_field] = json!(mt);
    }
    if let Some(t) = params.temperature
        && compat.supports_temperature {
            body["temperature"] = json!(t);
        }
    if let Some(t) = params.top_p {
        body["top_p"] = json!(t);
    }

    // —— 思考参数：按 thinkingFormat 分支（对齐 OpenClaw buildOpenAICompletionsParams）——
    // 关键保底：默认档位（None 或空串）绝不向任何网关发送任何思考字段，坚决尊重渠道自身行为
    let requested = params
        .thinking
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match compat.thinking_format {
        // 极简模式：不向端点发送任何思考相关参数
        ThinkingFormat::None => {}
        // zai（智谱 GLM/豆包 Seed）：{type:enabled|disabled}
        ThinkingFormat::Zai => {
            if let Some(t) = requested {
                body["thinking"] = json!({ "type": if t == "off" { "disabled" } else { "enabled" } });
            }
        }
        // deepseek（V4 Pro/Flash/R1）：thinking 对象；V4 支持 reasoning_effort (low/high/max)
        ThinkingFormat::Deepseek => {
            if let Some(t) = requested {
                if t == "off" {
                    body["thinking"] = json!({ "type": "disabled" });
                    body["reasoning_effort"] = json!("none");
                } else {
                    body["thinking"] = json!({ "type": "enabled" });
                    let eff = match t {
                        "low" => Some("low"),
                        "medium" => Some("medium"),
                        "high" => Some("high"),
                        "max" | "xhigh" => Some("xhigh"),
                        _ => None,
                    };
                    if let Some(e) = eff {
                        body["reasoning_effort"] = json!(e);
                    }
                }
            }
        }
        // qwen（百炼/混元）：顶层布尔 enable_thinking
        ThinkingFormat::Qwen => {
            if let Some(t) = requested {
                body["enable_thinking"] = json!(t != "off");
            }
        }
        // 本地推理服务聊天模板：chat_template_kwargs.enable_thinking（vLLM/llama.cpp Qwen3 系）
        ThinkingFormat::QwenChatTemplate => {
            if let Some(t) = requested {
                let kwargs = body
                    .as_object_mut()
                    .unwrap()
                    .entry("chat_template_kwargs")
                    .or_insert_with(|| json!({}));
                kwargs["enable_thinking"] = json!(t != "off");
            }
        }
        // openai：reasoning_effort 档位，经 effort::resolve 按模型降级；无法表达"关"时不发
        ThinkingFormat::Openai => {
            let level = match requested {
                Some("on") => Some("medium".to_string()),
                Some(v) => effort::resolve_effort(model, v, None),
                None => None,
            };
            if let Some(level) = level {
                body["reasoning_effort"] = json!(level);
            }
        }
    }
    body
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

/// 计量上下文（P21 用量台账）：一次模型调用的归属信息。
/// provider / model 由 channel 推导（host + 预设 id），调用方只报「谁、干什么用」。
#[derive(Debug, Clone)]
pub struct UsageCtx {
    pub session_id: String,
    /// P22 子代理花销记父任务：子会话填父任务 id；主会话为 None
    pub parent_session: Option<String>,
    /// 用途：main（主循环）| summary（滚动摘要）| title（标题）| media（媒体旁路）
    pub purpose: &'static str,
}

impl UsageCtx {
    /// 主循环对话。
    pub fn main(session_id: &str) -> Self {
        Self { session_id: session_id.to_string(), parent_session: None, purpose: "main" }
    }

    /// 辅助调用（摘要/标题/媒体旁路）。
    pub fn aux(session_id: &str, purpose: &'static str) -> Self {
        Self { session_id: session_id.to_string(), parent_session: None, purpose }
    }
}

/// 渠道 host（台账按供应商分组的键；保留端口以区分同域不同端的中转）。
fn host_of(base_url: &str) -> String {
    let b = base_url.trim();
    let after_scheme = b.split_once("://").map(|(_, r)| r).unwrap_or(b);
    after_scheme.split('/').next().unwrap_or(after_scheme).to_string()
}

/// 一次调用的计量结果（记录前汇聚，避免 record_usage 参数过长）。
struct UsageOutcome {
    status: &'static str,
    tokens: (u64, u64, u64),
    error: Option<String>,
}

impl UsageOutcome {
    fn ok(tokens: (u64, u64, u64)) -> Self {
        Self { status: "ok", tokens, error: None }
    }
    fn err(status: &'static str, error: String) -> Self {
        Self { status, tokens: (0, 0, 0), error: Some(error) }
    }
}

/// 记一条台账（失败原因截断到 200 字；绝不包含 key——入参里就不放 key）。
fn record_usage(ctx: &UsageCtx, base_url: &str, provider_id: Option<&str>, model: &str, latency_ms: u64, outcome: UsageOutcome) {
    let mut rec = crate::usage::UsageRecord::new(
        &ctx.session_id,
        ctx.purpose,
        &host_of(base_url),
        provider_id.unwrap_or(""),
        model,
    );
    rec.parent_session = ctx.parent_session.clone();
    rec.prompt_tokens = outcome.tokens.0;
    rec.completion_tokens = outcome.tokens.1;
    rec.cached_tokens = outcome.tokens.2;
    rec.status = outcome.status.to_string();
    rec.latency_ms = latency_ms;
    rec.error = outcome.error.map(|e| e.chars().take(200).collect());
    crate::usage::record(rec);
}

/// 从非流式响应里取 usage 三路（prompt / completion / cached；渠道不回 usage 时全 0）。
/// 字段兼容：OpenAI 系 / DeepSeek 系 / Anthropic（input_tokens、output_tokens、cache_read_input_tokens）。
fn usage_triple(data: &Value) -> (u64, u64, u64) {
    let u = match data.get("usage") {
        Some(u) => u,
        None => return (0, 0, 0),
    };
    let p = u
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .or_else(|| u.get("input_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let c = u
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .or_else(|| u.get("output_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let k = u
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| u.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .or_else(|| u.get("cache_read_input_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    (p, c, k)
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
        let body = build_body_with_compat(model, &outgoing, true, &eff_params, tools, &compat);

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
        if response.status().as_u16() == 400 && !degraded && Self::body_has_thinking_params(&body) {
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

/// 流式分片聚合器（P23 从 stream_chat 抽出）：一个 OpenAI 形状分片进，
/// usage/思考剥离/tool_calls 增量聚合、model_chunk 事件、干净分片出。
/// Anthropic 协议的合成 OpenAI 分片走同一入口，下游零分叉。
struct StreamAgg {
    turn: AssistantTurn,
    calls: Vec<ToolCall>,
    think_split: TagSplit,
    chunk_count: u64,
}

impl StreamAgg {
    fn new() -> Self {
        Self { turn: AssistantTurn::default(), calls: Vec::new(), think_split: TagSplit::default(), chunk_count: 0 }
    }

    /// 处理一个分片：raw/data 原文 + OpenAI 形状 chunk（可变，思考剥离会写回）。
    fn feed(&mut self, chunk: &mut Value, data: &str) {
        // 末尾分片的 usage：OpenAI 风格 prompt_tokens_details.cached_tokens
        // 或 DeepSeek 风格 prompt_cache_hit_tokens（缓存命中率展示 + P21 台账分计量）
        if let Some(u) = chunk.get("usage")
            && let Some(p) = u.get("prompt_tokens").and_then(Value::as_u64)
        {
            self.turn.prompt_tokens = Some(p);
            self.turn.completion_tokens = u
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .or_else(|| u.get("output_tokens").and_then(Value::as_u64));
            self.turn.cached_tokens = u
                .pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .or_else(|| u.get("prompt_cache_hit_tokens").and_then(Value::as_u64));
        }
        let mut delta = extract_delta(chunk);
        // 反代思考标签剥离：标签内文字改道思考通道；
        // 结果写回 chunk，让 on_chunk 转发路径（前端 SSE）也拿到干净值
        if !delta.content.is_empty() {
            let (think, clean) = self.think_split.feed(&delta.content);
            if !think.is_empty() || clean.len() != delta.content.len() {
                if !think.is_empty() {
                    delta.reasoning = Some(match delta.reasoning.take() {
                        Some(r) => format!("{r}{think}"),
                        None => think,
                    });
                }
                delta.content = clean.clone();
                if let Some(d) = chunk.pointer_mut("/choices/0/delta") {
                    d["content"] = json!(clean);
                    if let Some(r) = &delta.reasoning {
                        d["reasoning_content"] = json!(r);
                    }
                }
            }
        }
        let mut payload = json!({ "raw": data, "content": delta.content });
        if let Some(r) = &delta.reasoning {
            payload["reasoning_content"] = json!(r);
        }
        if let Some(f) = &delta.finish {
            payload["finish_reason"] = json!(f);
            self.turn.finish_reason = Some(f.clone());
        }
        let tc_arr = chunk
            .pointer("/choices/0/delta/tool_calls")
            .and_then(Value::as_array)
            .cloned();
        if let Some(arr) = &tc_arr {
            payload["tool_calls"] = json!(arr);
            accumulate_tool_calls(arr, &mut self.calls);
        }
        self.turn.content.push_str(&delta.content);
        if let Some(r) = &delta.reasoning {
            self.turn.reasoning.push_str(r);
        }
        observability::event(EventType::ModelChunk, payload);
        observability::debug(&format!("[gateway] sse: {data}"));
        self.chunk_count += 1;
    }

    /// 流结束：hold 里若剩半截标签（如截断在 "<thi"），按当前通道归位；tool_calls 落位。
    fn finish(&mut self) {
        let (tail_think, tail_content) = self.think_split.finish();
        self.turn.reasoning.push_str(&tail_think);
        self.turn.content.push_str(&tail_content);
        self.turn.tool_calls = std::mem::take(&mut self.calls);
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

/// 从分片里提取增量内容与思考内容。
/// 思考字段全量对齐 OpenClaw OPENAI_COMPLETIONS_REASONING_FIELDS +
/// OpenRouter reasoning_details（type=reasoning.text）：四字段按序取第一个非空。
pub fn extract_delta(chunk: &Value) -> Delta {
    let choice = chunk.pointer("/choices/0").cloned().unwrap_or(Value::Null);
    let delta = choice.pointer("/delta").cloned().unwrap_or(Value::Null);
    Delta {
        content: delta.pointer("/content").and_then(Value::as_str).unwrap_or("").to_string(),
        reasoning: extract_reasoning(&delta),
        finish: choice.pointer("/finish_reason").and_then(Value::as_str).map(String::from),
    }
}

/// OpenAI Completions 系思考字段（上游 OPENAI_COMPLETIONS_REASONING_FIELDS 顺序）。
fn extract_reasoning(delta: &Value) -> Option<String> {
    for field in ["reasoning_content", "reasoning", "reasoning_text"] {
        if let Some(s) = delta.get(field).and_then(Value::as_str)
            && !s.is_empty() {
                return Some(s.to_string());
            }
    }
    // OpenRouter：delta.reasoning_details 数组里的 reasoning.text 块
    if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
        let mut buf = String::new();
        for d in details {
            if d.get("type").and_then(Value::as_str) == Some("reasoning.text")
                && let Some(t) = d.get("text").and_then(Value::as_str) {
                    buf.push_str(t);
                }
        }
        if !buf.is_empty() {
            return Some(buf);
        }
    }
    None
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

    #[test]
    fn build_body_vendor_styles() {
        // REGISTRY 是进程级 OnceLock：测试进程里手动加载一次（幂等）
        let _ = crate::gateway::providers::init(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("providers.json"));
        // P23 预设收敛后旧预设 id 已下架：契约知识保留在端点探测，手工构造探测线索验证
        fn legacy(id: &str) -> ChatParams {
            ChatParams { provider_id: Some(id.into()), ..Default::default() }
        }
        let msgs = json!([{"role":"user","content":"hi"}]);
        // 智谱（zai 格式，端点探测出）：on/off 对象形态；档位无意义，非 off 一律 enabled
        let mut p = ChatParams::from_preset(crate::gateway::providers::find("zhipu").unwrap());
        p.thinking = Some("on".into());
        p.max_tokens = Some(2048);
        let b = build_body("glm-4.6", &msgs, true, &p, None);
        assert_eq!(b["thinking"], json!({"type":"enabled"}));
        assert_eq!(b["max_tokens"], json!(2048));
        assert_eq!(b["stream_options"]["include_usage"], json!(true));
        p.thinking = Some("off".into());
        let b = build_body("glm-4.6", &msgs, true, &p, None);
        assert_eq!(b["thinking"], json!({"type":"disabled"}));
        p.thinking = Some("high".into());
        let b = build_body("glm-4.6", &msgs, true, &p, None);
        assert_eq!(b["thinking"], json!({"type":"enabled"}), "档位对对象格式无意义");
        // 豆包（旧预设 id，契约知识保留在端点探测）：provider_id 直判 Zai
        let mut p = legacy("doubao");
        p.thinking = Some("on".into());
        let b = build_body("doubao-seed-1.6", &msgs, true, &p, None);
        assert_eq!(b["thinking"], json!({"type":"enabled"}));
        // 混元（旧预设 id）：provider_id 直判 qwen 形态，顶层布尔
        let mut p = legacy("hunyuan");
        p.thinking = Some("off".into());
        let b = build_body("hunyuan-turbos-latest", &msgs, false, &p, None);
        assert_eq!(b["enable_thinking"], json!(false));
        assert!(b.get("thinking").is_none(), "qwen 格式不发 thinking 对象");
        p.thinking = Some("on".into());
        let b = build_body("hunyuan-turbos-latest", &msgs, false, &p, None);
        assert_eq!(b["enable_thinking"], json!(true));
        // DeepSeek（deepseek 格式，端点探测）：thinking 对象；官方 reasoner 不收 effort
        let mut p = ChatParams::from_preset(crate::gateway::providers::find("deepseek").unwrap());
        p.thinking = Some("on".into());
        let b = build_body("deepseek-reasoner", &msgs, true, &p, None);
        assert_eq!(b["thinking"], json!({"type":"enabled"}));
        assert!(b.get("reasoning_effort").is_none(), "deepseek 格式 supports_reasoning_effort=false");
        // OpenAI（openai 格式）：on→medium；low/medium/high 原样；off 不发（o 系无可靠关参数）
        let mut p = legacy("openai");
        p.thinking = Some("on".into());
        let b = build_body("gpt-5", &msgs, true, &p, None);
        assert_eq!(b["reasoning_effort"], json!("medium"));
        for lvl in ["low", "medium", "high"] {
            p.thinking = Some(lvl.into());
            let b = build_body("gpt-5", &msgs, true, &p, None);
            assert_eq!(b["reasoning_effort"], json!(lvl), "档位 {lvl} 应解析后发出");
        }
        p.thinking = Some("xhigh".into());
        let b = build_body("gpt-5", &msgs, true, &p, None);
        assert_eq!(b["reasoning_effort"], json!("high"), "xhigh 在 gpt-5 上降级为 high");
        p.thinking = Some("off".into());
        let b = build_body("gpt-5", &msgs, true, &p, None);
        assert!(b.get("reasoning_effort").is_none(), "off 无参数可发时不发");
        // GPT-5 新系 max_tokens 字段切换
        let p = ChatParams { max_tokens: Some(1024), ..legacy("openai") };
        let b = build_body("gpt-5", &msgs, true, &p, None);
        assert!(b.get("max_tokens").is_none() && b["max_completion_tokens"] == json!(1024));
        let b = build_body("gpt-4o-mini", &msgs, true, &p, None);
        assert_eq!(b["max_tokens"], json!(1024));
        // 本地（qwen-chat-template）：chat_template_kwargs.enable_thinking
        let mut p = legacy("ollama");
        p.thinking = Some("on".into());
        let b = build_body("qwen3:8b", &msgs, true, &p, None);
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], json!(true));
        p.thinking = Some("off".into());
        let b = build_body("qwen3:8b", &msgs, true, &p, None);
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], json!(false));
        // OpenAI 兼容（自定义端点预设；端点认不出 → openai 格式）
        let mut p = ChatParams::from_preset(crate::gateway::providers::find("openai_compat").unwrap());
        p.thinking = Some("high".into());
        let b = build_body("mystery-model", &msgs, true, &p, None);
        assert_eq!(b["reasoning_effort"], json!("high"));
        // Anthropic 协议（P23）：build_body 入口直接产出 Messages 形状（system 顶层/max_tokens 必填/无 stream_options）
        let mut p = legacy("anthropic");
        p.thinking = Some("on".into());
        p.max_tokens = Some(8192);
        let b = build_body("claude-sonnet-4-5", &msgs, true, &p, None);
        assert_eq!(b["max_tokens"], json!(8192));
        assert!(b.get("stream_options").is_none(), "Anthropic 不带 OpenAI stream_options");
        assert_eq!(b["thinking"]["type"], json!("enabled"), "思考开 → thinking 对象（预算见 anthropic 模块测试）");
        // 默认不发任何思考参数（无论 None、空串、全空格，全网关均不产生任何思考字段）
        for empty in [None, Some("".to_string()), Some("   ".to_string())] {
            let mut p = ChatParams::from_preset(crate::gateway::providers::find("deepseek").unwrap());
            p.thinking = empty.clone();
            let b = build_body("deepseek-v4-flash", &msgs, true, &p, None);
            assert!(b.get("thinking").is_none() && b.get("reasoning_effort").is_none(), "DeepSeek 默认不注入参数");

            let mut p = legacy("qwen");
            p.thinking = empty.clone();
            let b = build_body("qwen3.8-flash", &msgs, true, &p, None);
            assert!(b.get("enable_thinking").is_none() && b.get("thinking").is_none(), "Qwen 默认不注入参数");

            let mut p = ChatParams::from_preset(crate::gateway::providers::find("zhipu").unwrap());
            p.thinking = empty.clone();
            let b = build_body("glm-5.2", &msgs, true, &p, None);
            assert!(b.get("thinking").is_none(), "Zai 默认不注入参数");
        }
        // 采样参数：有值才发
        let p = ChatParams { temperature: Some(0.3), top_p: Some(0.9), ..Default::default() };
        let b = build_body("m", &msgs, true, &p, None);
        assert_eq!(b["temperature"], json!(0.3));
        assert_eq!(b["top_p"], json!(0.9));
        let b = build_body("m", &msgs, true, &ChatParams::default(), None);
        assert!(b.get("temperature").is_none() && b.get("top_p").is_none());
        // 非流式不带 stream_options
        let b = build_body("m", &msgs, false, &ChatParams::default(), None);
        assert!(b.get("stream_options").is_none());
    }

    // —— P21 用量台账：渠道标识与 usage 解析 ——

    #[test]
    fn host_of_strips_scheme_and_path_keeps_port() {
        assert_eq!(host_of("https://api.deepseek.com"), "api.deepseek.com");
        assert_eq!(host_of("https://api.deepseek.com/v1"), "api.deepseek.com");
        assert_eq!(host_of("http://127.0.0.1:9399/v1"), "127.0.0.1:9399");
        assert_eq!(host_of(" relay.example.com:8080 "), "relay.example.com:8080");
    }

    #[test]
    fn usage_triple_covers_openai_and_deepseek_styles() {
        // OpenAI 风格：prompt_tokens_details.cached_tokens
        let (p, c, k) = usage_triple(&json!({"usage": {"prompt_tokens": 100, "completion_tokens": 50, "prompt_tokens_details": {"cached_tokens": 64}}}));
        assert_eq!((p, c, k), (100, 50, 64));
        // DeepSeek 风格：prompt_cache_hit_tokens
        let (p, c, k) = usage_triple(&json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5, "prompt_cache_hit_tokens": 8}}));
        assert_eq!((p, c, k), (10, 5, 8));
        // 渠道不回 usage：全 0（请求与延迟照记，见台账记录）
        assert_eq!(usage_triple(&json!({"choices": []})), (0, 0, 0));
        // output_tokens 兼容（个别反代用这个字段名）
        let (p, c, k) = usage_triple(&json!({"usage": {"prompt_tokens": 1, "output_tokens": 2}}));
        assert_eq!((p, c, k), (1, 2, 0));
    }

    #[test]
    fn extract_delta_reasoning_fields() {
        // DeepSeek 式 reasoning_content
        let d = extract_delta(&json!({"choices":[{"delta":{"reasoning_content":"想"}}]}));
        assert_eq!(d.reasoning.as_deref(), Some("想"));
        // 兼容字段 reasoning / reasoning_text
        let d = extract_delta(&json!({"choices":[{"delta":{"reasoning":"R"}}]}));
        assert_eq!(d.reasoning.as_deref(), Some("R"));
        let d = extract_delta(&json!({"choices":[{"delta":{"reasoning_text":"T"}}]}));
        assert_eq!(d.reasoning.as_deref(), Some("T"));
        // OpenRouter reasoning_details
        let d = extract_delta(&json!({"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":"OR"}]}}]}));
        assert_eq!(d.reasoning.as_deref(), Some("OR"));
        // 空串不触发
        let d = extract_delta(&json!({"choices":[{"delta":{"reasoning_content":""}}]}));
        assert_eq!(d.reasoning, None);
    }

    #[tokio::test]
    async fn throttle_enforces_interval() {
        init_logger();
        let t = Throttle::new(60); // 60 rpm → 最小间隔 1s
        let start = Instant::now();
        t.gate().await; // 第一发直通
        t.gate().await; // 第二发必须等满 1s
        let elapsed = start.elapsed();
        assert!(elapsed >= std::time::Duration::from_millis(900), "应被节流 ≥0.9s，实际 {elapsed:?}");
        assert!(elapsed < std::time::Duration::from_millis(2500), "不应过度等待，实际 {elapsed:?}");
    }

    #[test]
    fn think_split_variants() {
        // 无标签：原样走正文
        let mut ts = TagSplit::default();
        assert_eq!(ts.feed("普通回答"), ("".into(), "普通回答".into()));
        // 标签整段：思考与正文分离
        let mut ts = TagSplit::default();
        assert_eq!(ts.feed("<think>先想想</think>答案"), ("先想想".into(), "答案".into()));
        // 跨分片断裂："<thi|nk>AB</thi|nk>CD"
        let mut ts = TagSplit::default();
        let (t1, c1) = ts.feed("<thi");
        assert_eq!((t1.as_str(), c1.as_str()), ("", ""));
        let (t2, c2) = ts.feed("nk>AB</thi");
        assert_eq!((t2.as_str(), c2.as_str()), ("AB", ""));
        let (t3, c3) = ts.feed("nk>CD");
        assert_eq!((t3.as_str(), c3.as_str()), ("", "CD"));
        let (t4, c4) = ts.finish();
        assert_eq!((t4.as_str(), c4.as_str()), ("", ""));
        // 未闭合 <think>（流在思考中被截断）：全部走思考
        let mut ts = TagSplit::default();
        let (t, c) = ts.feed("<think>还没想完");
        assert_eq!((t.as_str(), c.as_str()), ("还没想完", ""));
        // 正文里出现 "<thinking" 这类伪前缀：不误伤（'a' 即时归正文，伪标签流尾归位）
        let mut ts = TagSplit::default();
        let (t, c) = ts.feed("a<thinking");
        assert_eq!((t.as_str(), c.as_str()), ("", "a"));
        let (t2, c2) = ts.finish();
        assert_eq!((t2.as_str(), c2.as_str()), ("", "<thinking"));
        // 孤立 </think>（不在思考态）：当普通文本
        let mut ts = TagSplit::default();
        assert_eq!(ts.feed("x</think>y"), ("".into(), "xy".into()));
        // 截断在半截标签：finish 归位正文
        let mut ts = TagSplit::default();
        let (t, c) = ts.feed("尾<thi");
        assert_eq!((t.as_str(), c.as_str()), ("", "尾"));
        let (t2, c2) = ts.finish();
        assert_eq!((t2.as_str(), c2.as_str()), ("", "<thi"));
    }

    #[tokio::test]
    async fn client_rebuild_picks_up_new_proxy_config() {
        init_logger();
        let g = Gateway::new();

        // 本地 mock 渠道（曾直打 api.deepseek.com——2026-09-27 本机系统代理开关翻转后
        // 该测试稳定失败：reqwest 的代理解析跟着注册表走，外网假设不再成立）。
        // 回 401 即「网络层已通、渠道在拒假 key」；127.* 在 ProxyOverride 里天然绕代理。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let mock = std::thread::spawn(move || {
            use std::io::Write;
            let resp = b"HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
            while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        // 监听器是非阻塞的，Windows 上接受的 socket 会继承该标志——
                        // 不恢复阻塞模式 write_all 会撞 WouldBlock（被 let _ 吞掉）
                        let _ = s.set_nonblocking(false);
                        // 先把请求读完再回响应：不读就关，未读的入站数据会让 Windows
                        // 直接回 RST（os error 10054），客户端只会看到「连接被重置」
                        let mut buf = [0u8; 4096];
                        let _ = std::io::Read::read(&mut s, &mut buf);
                        let _ = s.write_all(resp);
                        let _ = s.flush();
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
                }
            }
        });
        let base = format!("http://127.0.0.1:{port}/v1");

        // 阶段1：代理指向死端口 → 必须失败（is_connect 分支）
        unsafe {
            std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:1");
            std::env::set_var("HTTP_PROXY", "http://127.0.0.1:1");
        }
        g.rebuild_client_for_test();
        let r1 = g.list_models(&base, "sk-dummy").await;
        assert!(r1.is_err(), "死代理下应失败，实际 {:?}", r1.map(|v| v.len()));

        // 阶段2：清掉死代理 → rebuild → 同一 Gateway 实例应拿到渠道业务响应
        // （401 = 网络层已通，渠道在拒绝假 key——这正是"网络层恢复"的证据）
        unsafe {
            std::env::remove_var("HTTPS_PROXY");
            std::env::remove_var("HTTP_PROXY");
        }
        g.rebuild_client_for_test();
        let r2 = g.list_models(&base, "sk-dummy").await;
        let msg = r2.err().map(|e| format!("{e:#}")).unwrap_or_default();
        assert!(
            msg.contains("401") && !msg.contains("请求渠道失败"),
            "重建后应拿到渠道 401（网络层恢复），实际: {msg}"
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        mock.join().unwrap();
    }
}
