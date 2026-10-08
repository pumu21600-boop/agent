use super::*;

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
pub(crate) fn host_of(base_url: &str) -> String {
    let b = base_url.trim();
    let after_scheme = b.split_once("://").map(|(_, r)| r).unwrap_or(b);
    after_scheme.split('/').next().unwrap_or(after_scheme).to_string()
}

/// 一次调用的计量结果（记录前汇聚，避免 record_usage 参数过长）。
pub(crate) struct UsageOutcome {
    pub(crate) status: &'static str,
    pub(crate) tokens: (u64, u64, u64),
    pub(crate) error: Option<String>,
}

impl UsageOutcome {
    pub(crate) fn ok(tokens: (u64, u64, u64)) -> Self {
        Self { status: "ok", tokens, error: None }
    }
    pub(crate) fn err(status: &'static str, error: String) -> Self {
        Self { status, tokens: (0, 0, 0), error: Some(error) }
    }
}

/// 记一条台账（失败原因截断到 200 字；绝不包含 key——入参里就不放 key）。
pub(crate) fn record_usage(ctx: &UsageCtx, base_url: &str, provider_id: Option<&str>, model: &str, latency_ms: u64, outcome: UsageOutcome) {
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
/// Anthropic 口径注意（2026-10-08 修）：它的 input_tokens **不含** cache_read/cache_creation，
/// 而 OpenAI 的 prompt_tokens 是含缓存的总输入——下游锚点按 OpenAI 口径消费，Anthropic
/// 响应必须把两个 cache 字段补齐，否则缓存命中越多锚点低估越狠（上下文圆环系统性偏低）。
pub(crate) fn usage_triple(data: &Value) -> (u64, u64, u64) {
    let u = match data.get("usage") {
        Some(u) => u,
        None => return (0, 0, 0),
    };
    let cache_read = u
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| u.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .or_else(|| u.get("cache_read_input_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let p = u
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        // OpenAI 形状的 prompt_tokens 已含缓存，直接取；Anthropic 形状才需要补
        .or_else(|| {
            u.get("input_tokens")
                .and_then(Value::as_u64)
                .map(|input| {
                    input
                        + cache_read
                        + u.get("cache_creation_input_tokens").and_then(Value::as_u64).unwrap_or(0)
                })
        })
        .unwrap_or(0);
    let c = u
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .or_else(|| u.get("output_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    (p, c, cache_read)
}
