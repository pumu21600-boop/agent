//! Anthropic Messages 协议适配器。
//!
//! 边界协议翻译：请求侧把 OpenAI 会话形状（system 消息 / role:"tool" / tool_calls）
//! 翻译成 Messages API 形状（system 顶层参数 / tool_use·tool_result 内容块 / max_tokens 必填 /
//! x-api-key 鉴权）；响应侧把 Anthropic SSE 事件流**合成回 OpenAI delta 形状分片**——
//! 下游核心循环、会话回填、思考标签剥离、usage 台账全部零改动复用。
//!
//! 已知边界（v1 确定性规避）：思考开启时官方要求带签名的 thinking 块回放（我们只有
//! reasoning_content 文本，签名不可伪造），因此**本次请求携带工具结果时不开思考**——
//! 工具循环轮思考关闭、最终回答轮正常开启，行为确定可解释，等真渠道验证后再放宽。

use std::collections::HashMap;

use serde_json::{Value, json};

use super::{CacheRetention, ChatParams, Compat};

/// Anthropic API 版本头（官方要求必带）。
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// 思考预算下限（官方要求 budget_tokens ≥ 1024 且 < max_tokens）。
const THINKING_BUDGET_MIN: u64 = 1024;
/// 思考预算上限：预算取 max_tokens 一半，封顶 32k，防止大 max_tokens 时思考吃满一半。
const THINKING_BUDGET_MAX: u64 = 32000;

/// 是否 Anthropic 端点：provider_id=anthropic 或 base_url 的**主机名**是官方域。
/// 2026-10-08 修：原先是整串 URL 子串匹配——`anthropic.com.example.com`（攻击性
/// 域名/误配的中继）或路径里恰好含该子串的地址都会被判成 Messages 协议，请求体
/// 整体重写成 Messages 形状发给 OpenAI 形状中继 → 400。改按主机段匹配。
pub fn is_anthropic_endpoint(provider_id: Option<&str>, base_url: Option<&str>) -> bool {
    if provider_id == Some("anthropic") {
        return true;
    }
    base_url.is_some_and(|b| {
        let b = b.trim().to_lowercase();
        // 剥 scheme 后取主机段（到第一个 / ? # 为止），再判「等于官方域或以 .官方域 结尾」
        let after_scheme = b.split_once("://").map(|(_, r)| r).unwrap_or(&b);
        let host = after_scheme
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("")
            .rsplit('@')
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("");
        host == "anthropic.com" || host.ends_with(".anthropic.com")
    })
}

/// Messages 端点 URL：已含 /messages 原样（兼容端点给全路径）；以 /v1 结尾补 /messages；其余补 /v1/messages。
pub fn messages_url(base_url: &str) -> String {
    let b = base_url.trim().trim_end_matches('/');
    if b.ends_with("/messages") {
        b.to_string()
    } else if b.ends_with("/v1") {
        format!("{b}/messages")
    } else {
        format!("{b}/v1/messages")
    }
}

/// 模型列表 URL（/v1/models，响应 data[].id 与 OpenAI 同形）。
pub fn models_url(base_url: &str) -> String {
    let b = base_url.trim().trim_end_matches('/');
    if b.ends_with("/models") {
        b.to_string()
    } else if b.ends_with("/v1") {
        format!("{b}/models")
    } else {
        format!("{b}/v1/models")
    }
}

/// 消息 content 取文本（string 原样；分块数组拼 text 块——多模态图像 v1 不回放）。
fn string_content(m: &Value) -> Option<String> {
    match m.get("content") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(a)) => {
            let mut buf = String::new();
            for b in a {
                if b.get("type").and_then(Value::as_str) == Some("text")
                    && let Some(t) = b.get("text").and_then(Value::as_str)
                {
                    buf.push_str(t);
                }
            }
            Some(buf)
        }
        _ => None,
    }
}

/// 本次请求是否携带工具结果（存在 role:"tool" 消息）。
/// 是则不开思考：回放的 assistant tool_use 轮没有可用的官方签名 thinking 块。
fn carries_tool_result(messages: &[Value]) -> bool {
    messages.iter().any(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
}

/// 思考预算：按档位取 max_tokens 比例（用户机制：Anthropic 档位 低/中/高），
/// 夹在 [1024, 32000]，且必须 < max_tokens；放不下就不开思考。
/// on/中=50%，低=20%，高=80%。
pub fn thinking_budget(max_tokens: u64, requested: &str) -> Option<u64> {
    let frac = match requested {
        "low" | "minimal" => 0.20,
        "high" | "max" | "xhigh" => 0.80,
        _ => 0.50, // on / medium
    };
    let budget = ((max_tokens as f64 * frac) as u64).clamp(THINKING_BUDGET_MIN, THINKING_BUDGET_MAX);
    (budget < max_tokens).then_some(budget)
}

/// 组装 Messages API 请求体（纯函数）：
/// system 提顶层、role:"tool" 组队成 user 的 tool_result 块、assistant tool_calls → tool_use 块、
/// 工具定义 function.parameters → input_schema、max_tokens 必填（缺省 8192）、温度域夹到 0..1。
/// OpenAI 多模态 content 数组 → Anthropic content 块数组（审计 04：Anthropic Messages
/// API 不认 `image_url` 块，原样透传必 400）。
/// - `data:` URL → `{type:image,source:{type:base64,media_type,data}}`；
/// - http(s) 图片链接 → `{type:image,source:{type:url}}`；
/// - text 块与未知块原样保留；非数组（纯字符串）不动。
fn convert_multimodal_content(content: &Value) -> Value {
    let Some(arr) = content.as_array() else {
        return content.clone();
    };
    let blocks: Vec<Value> = arr
        .iter()
        .map(|part| {
            if part.get("type").and_then(Value::as_str) == Some("image_url") {
                let url = part.pointer("/image_url/url").and_then(Value::as_str).unwrap_or("");
                image_block(url).unwrap_or_else(|| part.clone())
            } else {
                part.clone()
            }
        })
        .collect();
    json!(blocks)
}

/// 单个 image_url → Anthropic image 块；不是 data:image/* 也不是 http(s) 就 None（调用方原样保留）。
fn image_block(url: &str) -> Option<Value> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',')?;
        let mime = meta.split(';').next().unwrap_or("");
        if !mime.starts_with("image/") {
            return None;
        }
        return Some(json!({
            "type": "image",
            "source": { "type": "base64", "media_type": mime, "data": data },
        }));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(json!({
            "type": "image",
            "source": { "type": "url", "url": url },
        }));
    }
    None
}

/// 本次请求的缓存断点标记（对齐 pi-ai anthropic-messages）：
/// retention=none → 不发任何断点；short → `{type:ephemeral}`（官方默认 5 分钟）；
/// long → 追加 `ttl:"1h"`，但仅官方端点认（compat.supports_long_cache_retention），
/// 其余端点静默降级短保留——长保留档绝不会把请求打挂。
fn cache_control(params: &ChatParams, compat: &Compat) -> Option<Value> {
    match params.cache_retention {
        CacheRetention::None => None,
        CacheRetention::Short => Some(json!({ "type": "ephemeral" })),
        CacheRetention::Long if compat.supports_long_cache_retention => {
            Some(json!({ "type": "ephemeral", "ttl": "1h" }))
        }
        CacheRetention::Long => Some(json!({ "type": "ephemeral" })),
    }
}

/// 给内容块数组的最后一个块打缓存断点（仅限可缓存块型：text/image/tool_result）。
/// 返回是否打上——调用方据此决定是否还要给消息级兜底。
fn mark_last_block(blocks: &mut [Value], cc: &Value) -> bool {
    let Some(last) = blocks.last_mut() else { return false };
    let cacheable = matches!(
        last.get("type").and_then(Value::as_str),
        Some("text") | Some("image") | Some("tool_result")
    );
    if cacheable {
        last["cache_control"] = cc.clone();
    }
    cacheable
}

pub fn build_body(
    model: &str,
    messages: &Value,
    stream: bool,
    params: &ChatParams,
    tools: Option<&Value>,
    compat: &Compat,
) -> Value {
    let arr = messages.as_array().cloned().unwrap_or_default();

    // system / developer 消息提为顶层 system 参数（按序拼接）
    let mut system_parts: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for m in &arr {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "system" | "developer" => {
                if let Some(t) = string_content(m)
                    && !t.trim().is_empty()
                {
                    system_parts.push(t);
                }
            }
            "tool" => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.get("tool_call_id").and_then(Value::as_str).unwrap_or(""),
                    "content": string_content(m).unwrap_or_default(),
                });
                // 连续 tool 消息合并进同一条 user 消息（官方要求 tool_result 挂在 user 上）
                let appendable = matches!(&out.last(), Some(last)
                    if last.get("role").and_then(Value::as_str) == Some("user")
                        && last.get("content").and_then(Value::as_array).map(|a|
                            a.iter().all(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))).unwrap_or(false));
                if appendable
                    && let Some(last) = out.last_mut()
                    && let Some(a) = last.pointer_mut("/content").and_then(Value::as_array_mut)
                {
                    a.push(block);
                    continue;
                }
                out.push(json!({ "role": "user", "content": [block] }));
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(t) = string_content(m)
                    && !t.is_empty()
                {
                    blocks.push(json!({ "type": "text", "text": t }));
                }
                if let Some(tcs) = m.get("tool_calls").and_then(Value::as_array) {
                    for tc in tcs {
                        let args = tc.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}");
                        let input: Value = serde_json::from_str(args).unwrap_or(json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": tc.get("id").and_then(Value::as_str).unwrap_or(""),
                            "name": tc.pointer("/function/name").and_then(Value::as_str).unwrap_or(""),
                            "input": if input.is_object() { input } else { json!({}) },
                        }));
                    }
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "" }));
                }
                out.push(json!({ "role": "assistant", "content": blocks }));
            }
            // user：string 原样；数组过一遍多模态转换（审计 04）
            _ => {
                let content = match m.get("content") {
                    Some(Value::String(s)) => json!(s),
                    Some(v) if !v.is_null() => convert_multimodal_content(v),
                    _ => json!(""),
                };
                out.push(json!({ "role": "user", "content": content }));
            }
        }
    }

    // 提示缓存：本请求的三个断点标记（system / 工具尾 / 末消息），None=用户关缓存
    let cache_cc = cache_control(params, compat);

    // 提示缓存断点三：最后一条 user 消息的最后一个内容块（对话增量）。
    // 下一轮请求的前半段与本次逐字节相同 → 整段前缀从缓存读，只为新内容付费。
    // 字符串内容先包成 text 块数组（Anthropic 的 cache_control 只认块级挂载）。
    // 注意必须在 body 组装前改 out——json! 对 out 是克隆快照，事后再改不进门。
    if let Some(cc) = &cache_cc
        && let Some(last) = out.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("user")
    {
        match last.get_mut("content") {
            Some(Value::Array(blocks)) => {
                mark_last_block(blocks, cc);
            }
            Some(Value::String(s)) if !s.is_empty() => {
                let text = std::mem::take(s);
                last["content"] = json!([{ "type": "text", "text": text, "cache_control": cc }]);
            }
            _ => {}
        }
    }

    let mut body = json!({
        "model": model,
        "max_tokens": params.max_tokens.unwrap_or(8192),
        "messages": out,
        "stream": stream,
    });
    // 提示缓存断点一：system（最稳的区域——整段会话不变）。
    // 单字符串要升级成块数组才能挂 cache_control（对齐 pi-ai/ZCode 形状）。
    if !system_parts.is_empty() {
        let mut blocks: Vec<Value> = system_parts
            .into_iter()
            .map(|t| json!({ "type": "text", "text": t }))
            .collect();
        if let Some(cc) = &cache_cc {
            mark_last_block(&mut blocks, cc);
        }
        body["system"] = json!(blocks);
    }
    if let Some(t) = tools {
        let mut defs: Vec<Value> = t
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|d| {
                        let schema = d
                            .pointer("/function/parameters")
                            .cloned()
                            .unwrap_or(json!({ "type": "object", "properties": {} }));
                        json!({
                            "name": d.pointer("/function/name").and_then(Value::as_str).unwrap_or(""),
                            "description": d.pointer("/function/description").and_then(Value::as_str).unwrap_or(""),
                            "input_schema": if schema.get("type").is_some() { schema } else { json!({ "type": "object", "properties": {} }) },
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        // 提示缓存断点二：工具定义最后一个（system→tools→messages 的前缀序，
        // 打在工具尾=「稳定区」全部进缓存；对话增量每次只付新增部分）。
        if let Some(cc) = &cache_cc
            && let Some(last) = defs.last_mut()
        {
            last["cache_control"] = cc.clone();
        }
        body["tools"] = json!(defs);
    }
    // 思考：开（非 off）且本请求不带工具结果才发 thinking 对象；默认档不发。
    // 带工具结果不开思考：官方要求带签名 thinking 块回放，签名不可伪造——v1 确定性规避。
    // 档位→预算比例见 thinking_budget（低/中/高=20%/50%/80%）。
    let requested = params.thinking.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(t) = requested
        && t != "off"
        && !carries_tool_result(&arr)
        && let Some(budget) = thinking_budget(params.max_tokens.unwrap_or(8192), t)
    {
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    }
    // Anthropic 温度域 0..1，且不与 top_p 同发（官方禁止同时改）；**思考开启时
    // 官方还要求 temperature 不设或恰为 1**（2026-10-08 修：旧版无条件照发，
    // 同发直接 400；降级重试只剥 thinking 便成功 → 误判「端点拒收思考」并
    // mark_thinking_degraded 永久登记，根因（温度设置）反而一直藏着）。
    if let Some(t) = params.temperature
        && (body.get("thinking").is_none() || (t - 1.0).abs() <= f64::EPSILON)
    {
        body["temperature"] = json!(t.clamp(0.0, 1.0));
    }
    body
}

/// Anthropic SSE 事件翻译结果。
pub enum Translated {
    /// 合成出的 OpenAI 形状分片（走下游既有处理）
    Chunk(Value),
    /// message_stop：流正常结束
    Done,
    /// ping / signature_delta 等无需处理的事件
    Ignore,
}

/// Anthropic SSE → OpenAI delta 合成器。
/// usage 在翻译器内累积（message_start 进 prompt/cache，message_delta 进 completion），
/// 每个合成 chunk 都带全量已知 usage，下游按最后一次覆盖取值。
#[derive(Default)]
pub struct SseTranslator {
    /// Anthropic content_block index → (OpenAI tool_call index, id, name)。
    /// 块序号含 thinking/text 块（不稠密），工具调用必须独立重新编号——
    /// 直接透传块序号会在 OpenAI 聚合器里留下空槽（真机踩过：空 name 触发误审批）。
    tool_blocks: HashMap<usize, (usize, String, String)>,
    acc_prompt: Option<u64>,
    acc_cached: Option<u64>,
    /// cache_creation_input_tokens（写缓存也算输入，2026-10-08 修：原先整个丢掉）
    acc_cache_creation: Option<u64>,
    acc_completion: Option<u64>,
}

impl SseTranslator {
    fn chunk(&self, delta: Value, finish: Value) -> Value {
        let mut chunk = json!({ "choices": [{ "delta": delta, "finish_reason": finish }] });
        if self.acc_prompt.is_some() || self.acc_completion.is_some() || self.acc_cached.is_some() {
            // Anthropic 的 input_tokens **不含** cache_read/cache_creation，而 OpenAI 形状的
            // prompt_tokens 是含缓存的总输入——下游锚点（report_api_usage 的 real = prompt
            // + completion）按 OpenAI 口径消费，这里必须补齐，否则缓存命中越多锚点低估越狠
            // （上下文圆环/压缩阈值系统性偏低）。cached_tokens 仍只报 cache_read。
            let input = self.acc_prompt.unwrap_or(0);
            let cache_read = self.acc_cached.unwrap_or(0);
            let cache_creation = self.acc_cache_creation.unwrap_or(0);
            chunk["usage"] = json!({
                "prompt_tokens": input + cache_read + cache_creation,
                "completion_tokens": self.acc_completion,
                "prompt_tokens_details": { "cached_tokens": cache_read },
            });
        }
        chunk
    }

    /// 翻译一个 data: JSON（Anthropic data 自带 type，event: 行直接忽略）。
    /// Err = 渠道流内报错（error 事件），上层应以人话终止本轮。
    pub fn translate(&mut self, data: &Value) -> Result<Translated, String> {
        let typ = data.get("type").and_then(Value::as_str).unwrap_or("");
        match typ {
            "ping" => Ok(Translated::Ignore),
            "error" => {
                let msg = data.pointer("/error/message").and_then(Value::as_str).unwrap_or("未知错误");
                Err(msg.to_string())
            }
            "message_start" => {
                let u = data.pointer("/message/usage").cloned().unwrap_or(json!({}));
                self.acc_prompt = u.get("input_tokens").and_then(Value::as_u64);
                self.acc_cached = u.get("cache_read_input_tokens").and_then(Value::as_u64);
                self.acc_cache_creation = u.get("cache_creation_input_tokens").and_then(Value::as_u64);
                Ok(Translated::Chunk(self.chunk(json!({}), json!(null))))
            }
            "content_block_start" => {
                let cb = data.get("content_block").cloned().unwrap_or(json!({}));
                if cb.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let block_idx = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let id = cb.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let name = cb.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let call_idx = self.tool_blocks.len();
                    self.tool_blocks.insert(block_idx, (call_idx, id.clone(), name.clone()));
                    return Ok(Translated::Chunk(self.chunk(
                        json!({ "tool_calls": [{ "index": call_idx, "id": id, "type": "function", "function": { "name": name, "arguments": "" } }] }),
                        json!(null),
                    )));
                }
                Ok(Translated::Ignore)
            }
            "content_block_delta" => {
                let block_idx = data.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = data.get("delta").cloned().unwrap_or(json!({}));
                match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => {
                        let t = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        if t.is_empty() {
                            return Ok(Translated::Ignore);
                        }
                        Ok(Translated::Chunk(self.chunk(json!({ "content": t }), json!(null))))
                    }
                    "thinking_delta" => {
                        let t = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        if t.is_empty() {
                            return Ok(Translated::Ignore);
                        }
                        Ok(Translated::Chunk(self.chunk(json!({ "reasoning_content": t }), json!(null))))
                    }
                    "input_json_delta" => {
                        let args = delta.get("partial_json").and_then(Value::as_str).unwrap_or("");
                        if args.is_empty() {
                            return Ok(Translated::Ignore);
                        }
                        // 块序号 → 工具调用序号。start 行丢失（该行 JSON 解析失败被吞/
                        // 中继不发）时原先兜底分配空 id/name 的新槽——发出去就是空 name 的
                        // 工具调用，下游「未知工具: 」+ 一张空名字的审批卡（正是空槽想避免
                        // 的那种误审批）。Anthropic 的 SSE 严格有序（start 必先于 delta），
                        // 先见到 delta 而没见过 start = start 永不会来，这段参数只能丢弃
                        // （整轮无工具调用时上游的解析失败守卫会报错，不会静默空回答）。
                        // 2026-10-08 修。
                        let Some((ci, _, name)) = self.tool_blocks.get(&block_idx) else {
                            return Ok(Translated::Ignore);
                        };
                        if name.is_empty() {
                            return Ok(Translated::Ignore);
                        }
                        let call_idx = *ci;
                        Ok(Translated::Chunk(self.chunk(
                            json!({ "tool_calls": [{ "index": call_idx, "function": { "arguments": args } }] }),
                            json!(null),
                        )))
                    }
                    // signature_delta 等：思考签名不参与正文/思考通道
                    _ => Ok(Translated::Ignore),
                }
            }
            "message_delta" => {
                let mut finish = json!(null);
                if let Some(s) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    let f = match s {
                        "tool_use" => "tool_calls",
                        "max_tokens" => "length",
                        _ => "stop",
                    };
                    finish = json!(f);
                }
                if let Some(ot) = data.pointer("/usage/output_tokens").and_then(Value::as_u64) {
                    self.acc_completion = Some(ot);
                }
                Ok(Translated::Chunk(self.chunk(json!({}), finish)))
            }
            "message_stop" => Ok(Translated::Done),
            _ => Ok(Translated::Ignore),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params() -> ChatParams {
        ChatParams { provider_id: Some("anthropic".into()), ..Default::default() }
    }

    fn compat() -> super::super::compat::Compat {
        super::super::compat::resolve(Some("anthropic"), Some("https://api.anthropic.com"), "claude-sonnet-4-5", None)
    }

    #[test]
    fn multimodal_image_url_converted_for_anthropic() {
        // 审计 04：Anthropic Messages API 不认 image_url，必须转成原生 image 块
        let msgs = json!([
            {"role":"user","content":[
                {"type":"text","text":"这是什么"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,aGVsbG8="}},
                {"type":"image_url","image_url":{"url":"https://example.com/cat.jpg"}},
            ]},
        ]);
        let body = build_body("claude-x", &msgs, false, &params(), None, &compat());
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0], json!({"type":"text","text":"这是什么"}), "text 块原样保留");
        assert_eq!(
            content[1],
            json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}),
            "data URL → base64 source"
        );
        assert_eq!(
            content[2]["source"],
            json!({"type":"url","url":"https://example.com/cat.jpg"}),
            "http 图片链接 → url source（末块另带缓存断点，见 cache_breakpoint_tests）"
        );
        // 非 image 的 data URL 与空 url 不硬转（原样保留，让渠道报明确的错）
        let weird = json!([{"type":"image_url","image_url":{"url":"data:application/pdf;base64,AAAA"}}]);
        let body = build_body("claude-x", &json!([{"role":"user","content":weird}]), false, &params(), None, &compat());
        assert_eq!(body["messages"][0]["content"][0]["type"], "image_url", "非图片 data URL 原样保留");
        // 纯文本消息：末条 user 的字符串内容会被包成带缓存断点的 text 块（见 cache_breakpoint_tests）
        let body = build_body("claude-x", &json!([{"role":"user","content":"hi"}]), false, &params(), None, &compat());
        assert_eq!(body["messages"][0]["content"][0]["text"], json!("hi"));
    }

    #[test]
    fn url_shapes() {
        assert_eq!(messages_url("https://api.anthropic.com"), "https://api.anthropic.com/v1/messages");
        assert_eq!(messages_url("https://api.anthropic.com/"), "https://api.anthropic.com/v1/messages");
        assert_eq!(messages_url("https://api.anthropic.com/v1"), "https://api.anthropic.com/v1/messages");
        assert_eq!(messages_url("https://relay.example.com/anthropic/v1/messages"), "https://relay.example.com/anthropic/v1/messages");
        assert_eq!(models_url("https://api.anthropic.com"), "https://api.anthropic.com/v1/models");
        assert_eq!(models_url("https://api.anthropic.com/v1"), "https://api.anthropic.com/v1/models");
    }

    #[test]
    fn endpoint_detection() {
        assert!(is_anthropic_endpoint(Some("anthropic"), None));
        assert!(is_anthropic_endpoint(Some("custom"), Some("https://api.anthropic.com/v1")));
        assert!(!is_anthropic_endpoint(Some("zhipu"), Some("https://open.bigmodel.cn/api/paas/v4")));
        assert!(!is_anthropic_endpoint(None, None));
    }

    #[test]
    fn body_system_and_basic_messages() {
        let msgs = json!([
            {"role":"system","content":"你是助手"},
            {"role":"developer","content":"规则B"},
            {"role":"user","content":"hi"},
            {"role":"assistant","content":"hello"},
            {"role":"user","content":"more"}
        ]);
        let b = build_body("claude-sonnet", &msgs, true, &params(), None, &compat());
        assert_eq!(b["model"], json!("claude-sonnet"));
        assert_eq!(b["max_tokens"], json!(8192), "max_tokens 必填，缺省 8192");
        assert_eq!(b["stream"], json!(true));
        // system 提顶层为块数组（多段 system/developer 各一块，末块带缓存断点）
        assert_eq!(b["system"][0], json!({"type":"text","text":"你是助手"}));
        assert_eq!(b["system"][1]["text"], json!("规则B"));
        assert_eq!(b["system"][1]["cache_control"], json!({"type":"ephemeral"}));
        assert!(b.get("temperature").is_none() && b.get("top_p").is_none());
        assert_eq!(b["messages"].as_array().unwrap().len(), 3, "system 消息不进 messages");
        assert_eq!(b["messages"][0]["role"], json!("user"));
        assert_eq!(b["messages"][1]["content"][0], json!({"type":"text","text":"hello"}), "assistant 统一内容块数组");
    }

    #[test]
    fn body_tool_round_trip() {
        let msgs = json!([
            {"role":"system","content":"sys"},
            {"role":"user","content":"读文件"},
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"tc_1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}},
                {"id":"tc_2","type":"function","function":{"name":"shell","arguments":"不是json"}}
            ]},
            {"role":"tool","tool_call_id":"tc_1","content":"文件内容"},
            {"role":"tool","tool_call_id":"tc_2","content":"输出"}
        ]);
        let tools = json!([
            {"type":"function","function":{"name":"read_file","description":"读","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}
        ]);
        let p = params();
        let b = build_body("claude-sonnet", &msgs, true, &p, Some(&tools), &compat());
        // assistant → tool_use 块；坏 JSON 参数兜底空对象
        let asst = &b["messages"][1];
        assert_eq!(asst["role"], json!("assistant"));
        assert_eq!(asst["content"][0]["type"], json!("tool_use"));
        assert_eq!(asst["content"][0]["input"], json!({"path":"a.txt"}));
        assert_eq!(asst["content"][1]["input"], json!({}));
        // 连续 tool 消息合并为一条 user 的 tool_result 块
        let tr = &b["messages"][2];
        assert_eq!(tr["role"], json!("user"));
        let blocks = tr["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], json!("tool_result"));
        assert_eq!(blocks[0]["tool_use_id"], json!("tc_1"));
        assert_eq!(blocks[0]["content"], json!("文件内容"));
        // 工具定义翻译
        assert_eq!(b["tools"][0]["name"], json!("read_file"));
        assert_eq!(b["tools"][0]["input_schema"]["type"], json!("object"));
        // 带工具结果的请求不开思考（签名不可回放）
        assert!(b.get("thinking").is_none());
    }

    #[test]
    fn body_thinking_budget() {
        let msgs = json!([{"role":"user","content":"hi"}]);
        let mut p = params();
        p.thinking = Some("on".into());
        p.max_tokens = Some(8192);
        let b = build_body("claude-sonnet", &msgs, true, &p, None, &compat());
        assert_eq!(b["thinking"], json!({"type":"enabled","budget_tokens":4096}), "on=预算取 max_tokens 一半");
        // 档位比例：低=20%、高=80%
        p.thinking = Some("low".into());
        let b = build_body("claude-sonnet", &msgs, true, &p, None, &compat());
        assert_eq!(b["thinking"]["budget_tokens"], json!(1638), "低=20%");
        p.thinking = Some("high".into());
        let b = build_body("claude-sonnet", &msgs, true, &p, None, &compat());
        assert_eq!(b["thinking"]["budget_tokens"], json!(6553), "高=80%");
        // max_tokens 太小放不下预算：不开思考
        p.thinking = Some("on".into());
        p.max_tokens = Some(1024);
        let b = build_body("claude-sonnet", &msgs, true, &p, None, &compat());
        assert!(b.get("thinking").is_none(), "budget 必须严格小于 max_tokens，放不下就不发");
        // off 不发
        p.max_tokens = Some(8192);
        p.thinking = Some("off".into());
        let b = build_body("claude-sonnet", &msgs, true, &p, None, &compat());
        assert!(b.get("thinking").is_none());
        // 默认档不发
        p.thinking = None;
        let b = build_body("claude-sonnet", &msgs, true, &p, None, &compat());
        assert!(b.get("thinking").is_none());
        // 温度夹取 0..1
        p.temperature = Some(1.7);
        let b = build_body("claude-sonnet", &msgs, true, &p, None, &compat());
        assert_eq!(b["temperature"], json!(1.0));
    }

    #[test]
    fn sse_translation_full_tool_round() {
        let mut tr = SseTranslator::default();
        let feed = |tr: &mut SseTranslator, v: Value| match tr.translate(&v).unwrap() {
            Translated::Chunk(c) => Some(c),
            Translated::Done => None,
            Translated::Ignore => None,
        };
        // message_start：input usage（含缓存读）
        let c = feed(&mut tr, json!({"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":64}}})).unwrap();
        // 2026-10-08 修：prompt_tokens 补齐缓存字段（100+64）——Anthropic 的
        // input_tokens 不含 cache_read/cache_creation，而 OpenAI 形状的 prompt_tokens
        // 是含缓存的总输入，下游锚点按后者消费
        assert_eq!(c["usage"]["prompt_tokens"], json!(164));
        assert_eq!(c["usage"]["prompt_tokens_details"]["cached_tokens"], json!(64));
        // 思考增量
        let c = feed(&mut tr, json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"我想"}})).unwrap();
        assert_eq!(c["choices"][0]["delta"]["reasoning_content"], json!("我想"));
        // 正文增量
        let c = feed(&mut tr, json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"答案"}})).unwrap();
        assert_eq!(c["choices"][0]["delta"]["content"], json!("答案"));
        // 工具调用：块开始（登记 id/name）+ 参数增量——块序号 2（前面有思考/正文块），
        // OpenAI 工具序号必须重编号为 0（稀疏块序号直接透传会留空槽，真机踩过）
        let c = feed(&mut tr, json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"tc_9","name":"read_file"}})).unwrap();
        assert_eq!(c["choices"][0]["delta"]["tool_calls"][0]["index"], json!(0), "工具调用独立重编号");
        assert_eq!(c["choices"][0]["delta"]["tool_calls"][0]["id"], json!("tc_9"));
        assert_eq!(c["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], json!("read_file"));
        let c = feed(&mut tr, json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}})).unwrap();
        assert_eq!(c["choices"][0]["delta"]["tool_calls"][0]["index"], json!(0), "参数增量跟随重编号");
        assert_eq!(c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], json!("{\"pa"));
        // signature_delta 忽略
        assert!(matches!(tr.translate(&json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"x"}})), Ok(Translated::Ignore)));
        // message_delta：stop_reason 映射 + output usage
        let c = feed(&mut tr, json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":55}})).unwrap();
        assert_eq!(c["choices"][0]["finish_reason"], json!("tool_calls"));
        assert_eq!(c["usage"]["completion_tokens"], json!(55));
        assert_eq!(c["usage"]["prompt_tokens"], json!(164), "usage 累积：末尾分片带全量（100+64 缓存读）");
        // message_stop
        assert!(matches!(tr.translate(&json!({"type":"message_stop"})), Ok(Translated::Done)));
        // 错误事件 → Err 人话
        assert!(tr.translate(&json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}})).is_err());
        // ping 忽略
        assert!(matches!(tr.translate(&json!({"type":"ping"})), Ok(Translated::Ignore)));
    }

    #[test]
    fn sse_stop_reason_mapping() {
        let mut tr = SseTranslator::default();
        for (raw, want) in [("end_turn", "stop"), ("stop_sequence", "stop"), ("max_tokens", "length"), ("tool_use", "tool_calls")] {
            let c = match tr.translate(&json!({"type":"message_delta","delta":{"stop_reason":raw}})).unwrap() {
                Translated::Chunk(c) => c,
                _ => panic!("应为 Chunk"),
            };
            assert_eq!(c["choices"][0]["finish_reason"], json!(want), "{raw} → {want}");
        }
    }
}

/// 提示缓存断点（对齐 pi-ai anthropic-messages：system / 工具尾 / 末消息三处）。
/// 命中率的地基是「下一轮请求的前半段与本轮逐字节相同」——断点把稳定区钉进缓存，
/// 每轮只为对话增量付费；保留档决定 TTL，none 档一个标记都不发。
#[cfg(test)]
mod cache_breakpoint_tests {
    use super::*;

    fn params() -> ChatParams {
        ChatParams { provider_id: Some("anthropic".into()), ..Default::default() }
    }

    fn compat() -> super::super::compat::Compat {
        super::super::compat::resolve(Some("anthropic"), Some("https://api.anthropic.com"), "claude-sonnet-4-5", None)
    }

    #[test]
    fn system_tools_last_message_all_get_breakpoints() {
        let msgs = json!([
            {"role":"system","content":"系统提示"},
            {"role":"user","content":"第一问"},
            {"role":"assistant","content":"第一答"},
            {"role":"user","content":"第二问"}
        ]);
        let tools = json!([
            {"type":"function","function":{"name":"read_file","description":"读","parameters":{"type":"object","properties":{}}}},
            {"type":"function","function":{"name":"shell","description":"跑","parameters":{"type":"object","properties":{}}}}
        ]);
        let b = build_body("claude-sonnet", &msgs, true, &params(), Some(&tools), &compat());
        // 断点一：system 升级为块数组，末块带断点（单字符串挂不了 cache_control）
        let sys = b["system"].as_array().unwrap();
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["type"], json!("text"));
        assert_eq!(sys[0]["cache_control"], json!({"type":"ephemeral"}), "short 档=裸 ephemeral（5 分钟）");
        // 断点二：只有最后一个工具带（Anthropic 上限 4 个，system+tools+末消息正好 3）
        assert!(b["tools"][0].get("cache_control").is_none(), "非末工具不打");
        assert_eq!(b["tools"][1]["cache_control"], json!({"type":"ephemeral"}));
        // 断点三：末条 user 的字符串内容包成 text 块并挂断点
        let last = b["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["role"], json!("user"));
        let blocks = last["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], json!("text"));
        assert_eq!(blocks[0]["text"], json!("第二问"));
        assert_eq!(blocks[0]["cache_control"], json!({"type":"ephemeral"}));
    }

    #[test]
    fn tool_result_and_array_content_last_blocks_marked() {
        // 末消息是工具结果（user + tool_result 块数组）→ 断点打在最后一个 tool_result 上
        let msgs = json!([
            {"role":"user","content":"干活"},
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"tc_1","type":"function","function":{"name":"read_file","arguments":"{}"}}
            ]},
            {"role":"tool","tool_call_id":"tc_1","content":"文件内容"}
        ]);
        let b = build_body("claude-sonnet", &msgs, true, &params(), None, &compat());
        let blocks = b["messages"].as_array().unwrap().last().unwrap()["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], json!("tool_result"));
        assert_eq!(blocks[0]["cache_control"], json!({"type":"ephemeral"}), "tool_result 块可缓存");
        // 内容数组（text+image）→ 末块（image）带断点
        let msgs = json!([{"role":"user","content":[
            {"type":"text","text":"看图"},
            {"type":"image_url","image_url":{"url":"https://example.com/cat.jpg"}}
        ]}]);
        let b = build_body("claude-sonnet", &msgs, true, &params(), None, &compat());
        let blocks = b["messages"].as_array().unwrap().last().unwrap()["content"].as_array().unwrap();
        assert_eq!(blocks[1]["type"], json!("image"));
        assert_eq!(blocks[1]["cache_control"], json!({"type":"ephemeral"}));
    }

    #[test]
    fn retention_none_sends_no_markers() {
        let msgs = json!([{"role":"system","content":"系统提示"},{"role":"user","content":"问"}]);
        let tools = json!([{"type":"function","function":{"name":"read_file","description":"读","parameters":{"type":"object","properties":{}}}}]);
        let mut p = params();
        p.cache_retention = CacheRetention::None;
        let b = build_body("claude-sonnet", &msgs, true, &p, Some(&tools), &compat());
        // system 仍是块数组（形状不变），但不带任何 cache_control
        assert!(b["system"][0].get("cache_control").is_none());
        assert!(b["tools"][0].get("cache_control").is_none());
        // 末消息保持纯字符串（不白包一层 text 块）
        assert_eq!(b["messages"].as_array().unwrap().last().unwrap()["content"], json!("问"));
    }

    #[test]
    fn long_retention_adds_ttl_only_on_official() {
        let msgs = json!([{"role":"system","content":"系统提示"},{"role":"user","content":"问"}]);
        let mut p = params();
        p.cache_retention = CacheRetention::Long;
        // 官方端点：ttl 1h
        let b = build_body("claude-sonnet", &msgs, true, &p, None, &compat());
        assert_eq!(b["system"][0]["cache_control"], json!({"type":"ephemeral","ttl":"1h"}));
        assert_eq!(
            b["messages"].as_array().unwrap().last().unwrap()["content"][0]["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
        // 非官方（中转说 Anthropic 协议但长保留没谱）：静默降级裸 ephemeral，绝不打挂请求
        let mut relay = compat();
        relay.supports_long_cache_retention = false;
        let b = build_body("claude-sonnet", &msgs, true, &p, None, &relay);
        assert_eq!(b["system"][0]["cache_control"], json!({"type":"ephemeral"}), "不支持的端点降级短保留");
    }

    #[test]
    fn no_user_tail_no_message_breakpoint() {
        // 空消息：不崩、消息侧无从打起（system 也无——没有 system 消息就不造 system 参数）
        let b = build_body("claude-sonnet", &json!([]), true, &params(), None, &compat());
        assert!(b["messages"].as_array().unwrap().is_empty());
        assert!(b.get("system").is_none());
        // 末条是 assistant：消息侧不打点，system 断点照打（稳定区不受尾部形态影响）
        let msgs = json!([
            {"role":"system","content":"系统提示"},
            {"role":"user","content":"问"},
            {"role":"assistant","content":"答"}
        ]);
        let b = build_body("claude-sonnet", &msgs, true, &params(), None, &compat());
        let last = b["messages"].as_array().unwrap().last().unwrap();
        assert!(last["content"][0].get("cache_control").is_none(), "assistant 尾部不打消息断点");
        assert_eq!(b["system"][0]["cache_control"], json!({"type":"ephemeral"}));
    }

    #[test]
    fn consecutive_requests_share_byte_identical_prefix() {
        // 命中率的本质：第 N+1 轮的前半段与第 N 轮逐字节相同。构造「同一会话追加一轮」
        // 的两次请求，除末条消息外一切一致——下一轮整个稳定区应从缓存读。
        let base = json!([
            {"role":"system","content":"系统提示"},
            {"role":"user","content":"第一问"},
            {"role":"assistant","content":"第一答"}
        ]);
        let mut turn2 = base.as_array().unwrap().clone();
        turn2.push(json!({"role":"user","content":"第二问"}));
        let b1 = build_body("claude-sonnet", &json!(base), true, &params(), None, &compat());
        let b2 = build_body("claude-sonnet", &json!(turn2), true, &params(), None, &compat());
        assert_eq!(b1["system"], b2["system"], "system 逐字节稳定");
        assert_eq!(b2["messages"][0], b1["messages"][0]);
        assert_eq!(b2["messages"][1], b1["messages"][1], "历史前半段不动");
        // 第 2 轮末条带断点——钉住的前缀比第 1 轮更长，增量只有新增的那条 user
        let last2 = b2["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last2["content"][0]["cache_control"], json!({"type":"ephemeral"}));
        // 第 1 轮末条是 assistant：不打消息断点（打了也无意义，前缀在它之前已经钉过）
        let last1 = b1["messages"].as_array().unwrap().last().unwrap();
        assert!(last1["content"][0].get("cache_control").is_none());
    }
}

#[cfg(test)]
mod b20261008_tests {
    use super::*;

    fn params() -> ChatParams {
        ChatParams { provider_id: Some("anthropic".into()), ..Default::default() }
    }

    fn compat() -> super::super::compat::Compat {
        super::super::compat::resolve(Some("anthropic"), Some("https://api.anthropic.com"), "claude-sonnet-4-5", None)
    }

    /// 2026-10-08 修：思考开启时 Anthropic 要求 temperature 不设或恰为 1——
    /// 旧版无条件照发，400 后被降级重试剥 thinking 成功 → 误判「端点拒收思考」
    /// 并 mark_thinking_degraded 永久登记（根因是温度设置，被错误归因）。
    #[test]
    fn temperature_dropped_when_thinking_enabled() {
        let msgs = json!([{"role":"user","content":"hi"}]);
        let mut p = params();
        p.thinking = Some("high".into());
        p.temperature = Some(0.5);
        p.max_tokens = Some(8192);
        let body = build_body("claude-x", &msgs, false, &p, None, &compat());
        assert!(body.get("thinking").is_some(), "思考应开启");
        assert!(body.get("temperature").is_none(), "思考开启时不得发 temperature: {}", body);
        // 温度恰为 1 时保留（官方允许）
        p.temperature = Some(1.0);
        let body = build_body("claude-x", &msgs, false, &p, None, &compat());
        assert_eq!(body["temperature"], json!(1.0), "temperature=1 可随思考同发");
        // 思考关时温度照发
        p.thinking = Some("off".into());
        p.temperature = Some(0.3);
        let body = build_body("claude-x", &msgs, false, &p, None, &compat());
        assert!(body.get("thinking").is_none(), "off 不发 thinking");
        assert_eq!(body["temperature"], json!(0.3), "思考关闭时 temperature 照发");
    }

    /// 2026-10-08 修：Anthropic 的 input_tokens 不含 cache_read/cache_creation，
    /// 而 OpenAI 形状的 prompt_tokens 是含缓存的总输入——下游锚点按后者消费，
    /// 不补齐则缓存命中越多锚点低估越狠（上下文圆环/压缩阈值系统性偏低）。
    #[test]
    fn usage_prompt_includes_cache_fields() {
        let mut tr = SseTranslator::default();
        let c = tr
            .translate(&json!({"type":"message_start","message":{"usage":{
                "input_tokens":100,"cache_read_input_tokens":64,"cache_creation_input_tokens":32
            }}}))
            .unwrap();
        let Translated::Chunk(v) = c else { panic!("应产出 chunk") };
        assert_eq!(v["usage"]["prompt_tokens"], json!(196), "100+64+32");
        assert_eq!(v["usage"]["prompt_tokens_details"]["cached_tokens"], json!(64), "cached 只报 cache_read");
    }

    /// 2026-10-08 修：端点判定原先是整串 URL 子串匹配——anthropic.com.example.com
    /// 这种域名/路径里含子串的中继都会被判成 Messages 协议，请求体整体重写后 400。
    #[test]
    fn endpoint_match_is_host_scoped() {
        assert!(is_anthropic_endpoint(Some("anthropic"), None));
        assert!(is_anthropic_endpoint(None, Some("https://api.anthropic.com/v1")));
        assert!(is_anthropic_endpoint(None, Some("https://api.anthropic.com")));
        assert!(!is_anthropic_endpoint(None, Some("https://anthropic.com.example.com/v1")));
        assert!(!is_anthropic_endpoint(None, Some("https://relay.example.com/anthropic.com/v1")));
        assert!(!is_anthropic_endpoint(None, Some("https://evil.com/?x=anthropic.com")));
        assert!(!is_anthropic_endpoint(None, Some("https://open.bigmodel.cn/api/paas/v4")));
    }

    /// 2026-10-08 修：start 行丢失（JSON 解析失败被吞）时 input_json_delta 的兜底
    /// 槽没有 id/name——发出去就是空 name 的工具调用（下游「未知工具」+ 空审批卡）。
    /// Anthropic SSE 严格有序，先见 delta 未见 start = start 永不会来，丢弃该段。
    #[test]
    fn orphan_input_json_delta_is_dropped() {
        let mut tr = SseTranslator::default();
        let out = tr
            .translate(&json!({"type":"content_block_delta","index":0,"delta":{
                "type":"input_json_delta","partial_json":"{\"a\":1}"
            }}))
            .unwrap();
        assert!(
            matches!(out, Translated::Ignore),
            "没见过 content_block_start 的 tool_use 参数必须丢弃"
        );
        // 正常路径不受影响：start 之后的 delta 照常产出
        let _ = tr
            .translate(&json!({"type":"content_block_start","index":0,"content_block":{
                "type":"tool_use","id":"call_1","name":"read_file"
            }}))
            .unwrap();
        let out = tr
            .translate(&json!({"type":"content_block_delta","index":0,"delta":{
                "type":"input_json_delta","partial_json":"{\"a\":1}"
            }}))
            .unwrap();
        let Translated::Chunk(v) = out else { panic!("应产出 chunk") };
        assert_eq!(v["choices"][0]["delta"]["tool_calls"][0]["index"], json!(0));
        assert_eq!(
            v["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            json!("{\"a\":1}")
        );
    }
}
