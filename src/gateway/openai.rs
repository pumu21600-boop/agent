use super::*;

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
    build_body_with_compat(model, messages, stream, params, tools, &compat, "")
}

/// OpenAI prompt_cache_key 长度上限（官方规范：64 字符）。
/// 会话 id 超长时截断——同会话内键稳定即可，不需要全局唯一。
pub const PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

/// 会话 id → prompt_cache_key（对齐 pi-ai clampOpenAIPromptCacheKey）。
pub fn clamp_prompt_cache_key(session_id: &str) -> String {
    session_id.trim().chars().take(PROMPT_CACHE_KEY_MAX_LENGTH).collect()
}

/// 思考预算扩容：思考开启时上游把 reasoning+正文一起算进同一个 max_tokens
/// （StepFun 官方文档：max_tokens 默认 INF、推理深度只有 reasoning_effort 三档、
/// 没有单独思考预算参数；GLM/DeepSeek/Qwen 同构——共用一个输出上限）。
/// 用户配的「最大输出长度」本意是正文额度，被思考吃掉半截正文就莫名截断
/// （2026-09-29 用户实锤：step-5-preview + high 档思考，SVG 写到一半被切）。
/// 发送值 = 配置值 + max(配置值, 8192)，给思考留出与正文同量级的余量；
/// 端点硬上限拒收（400）时由 stream_chat 回退配置原值重试，不影响可用性。
/// 思考关闭 / 未配 max_tokens → 原样返回（此时上限本就只约束正文，语义正确）。
pub fn effective_max_tokens(params: &ChatParams) -> Option<u64> {
    let mt = params.max_tokens?;
    let thinking_on = params
        .thinking
        .as_deref()
        .map(str::trim)
        .is_some_and(|t| !t.is_empty() && t != "off");
    if !thinking_on {
        return Some(mt);
    }
    Some(mt + mt.max(8192))
}

/// 带 compat 的组装（供网关在发送前先做历史净化时复用同一次探测）。
/// `session_id`：本次调用归属的会话 id——OpenAI 官方的 prompt_cache_key 用它做
/// 会话级缓存路由（同会话的前缀命中同一份缓存条目，跨会话不互相挤掉）。
pub fn build_body_with_compat(
    model: &str,
    messages: &Value,
    stream: bool,
    params: &ChatParams,
    tools: Option<&Value>,
    compat: &Compat,
    session_id: &str,
) -> Value {
    // Anthropic Messages 协议：请求体形状完全不同，交给专用适配器；
    // 响应侧同样由适配器合成为 OpenAI 分片，下游零感知
    if compat.protocol == Protocol::AnthropicMessages {
        return anthropic::build_body(model, messages, stream, params, tools, compat);
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
    // —— 提示缓存（对齐 pi-ai openai-completions）——
    // prompt_cache_key：官方 OpenAI 的会话级缓存路由键，同键请求优先命中同一缓存条目。
    // 只对 api.openai.com 发（中转/国产端点普遍 400，compat 已把开关关掉）；
    // retention=none 时连键也不发（用户显式关缓存）。
    if compat.supports_prompt_cache_key
        && params.cache_retention != CacheRetention::None
        && !session_id.trim().is_empty()
    {
        body["prompt_cache_key"] = json!(clamp_prompt_cache_key(session_id));
    }
    // prompt_cache_retention="24h"：长保留档，仅官方端点认；其余端点 long 静默降级短保留
    if params.cache_retention == CacheRetention::Long && compat.supports_long_cache_retention {
        body["prompt_cache_retention"] = json!("24h");
    }

    // —— 思考参数：按 thinkingFormat 分支 ——
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

/// 流式分片聚合器（从 stream_chat 抽出）：一个 OpenAI 形状分片进，
/// usage/思考剥离/tool_calls 增量聚合、model_chunk 事件、干净分片出。
/// Anthropic 协议的合成 OpenAI 分片走同一入口，下游零分叉。
pub(crate) struct StreamAgg {
    pub(crate) turn: AssistantTurn,
    pub(crate) calls: Vec<ToolCall>,
    pub(crate) think_split: TagSplit,
    pub(crate) chunk_count: u64,
}

impl StreamAgg {
    pub(crate) fn new() -> Self {
        Self { turn: AssistantTurn::default(), calls: Vec::new(), think_split: TagSplit::default(), chunk_count: 0 }
    }

    /// 处理一个分片：raw/data 原文 + OpenAI 形状 chunk（可变，思考剥离会写回）。
    pub(crate) fn feed(&mut self, chunk: &mut Value, data: &str) {
        // 末尾分片的 usage：OpenAI 风格 prompt_tokens_details.cached_tokens
        // 或 DeepSeek 风格 prompt_cache_hit_tokens（缓存命中率展示 +  台账分计量）
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
            // 思考时长口径：首尾 reasoning 分片的到达时刻差（「思考 · 持续了 N 秒」）
            let now = std::time::Instant::now();
            self.turn.reasoning_first_at.get_or_insert(now);
            self.turn.reasoning_last_at = Some(now);
        }
        observability::event(EventType::ModelChunk, payload);
        observability::debug(&format!("[gateway] sse: {data}"));
        self.chunk_count += 1;
    }

    /// 流结束：hold 里若剩半截标签（如截断在 "<thi"），按当前通道归位；tool_calls 落位。
    pub(crate) fn finish(&mut self) {
        let (tail_think, tail_content) = self.think_split.finish();
        if !tail_think.is_empty() {
            self.turn.reasoning.push_str(&tail_think);
            let now = std::time::Instant::now();
            self.turn.reasoning_first_at.get_or_insert(now);
            self.turn.reasoning_last_at = Some(now);
        }
        self.turn.content.push_str(&tail_content);
        self.turn.tool_calls = std::mem::take(&mut self.calls);
    }
}

/// tool_call index 的 sane 上限：并行调用再多也到不了这个量级。越界或负值都是协议
/// 违反——照旧合并会把 Vec 撑爆（`as usize` 把 -1 变成 ~1.8e19，`while calls.len()
/// <= index` 无限 push 到 OOM；index: 999999999 同理）。2026-10-08 修。
const MAX_TOOL_CALL_INDEX: i64 = 128;

/// 按 index 合并流式 tool_calls 分片（OpenAI 兼容增量格式）。
pub(crate) fn accumulate_tool_calls(arr: &[Value], calls: &mut Vec<ToolCall>) {
    for entry in arr {
        let index = match entry.get("index").and_then(Value::as_i64) {
            None => 0, // 缺 index 按 0 合并（原行为）
            Some(i) if (0..=MAX_TOOL_CALL_INDEX).contains(&i) => i as usize,
            Some(i) => {
                observability::event(
                    EventType::Error,
                    json!({ "message": format!("tool_calls 分片 index 越界（{i}），忽略该分片"), "raw": entry }),
                );
                continue;
            }
        };
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
/// 思考字段取 reasoning_content / reasoning / reasoning_text / reasoning_details 四路，
/// 按序取第一个非空（含 OpenRouter reasoning_details 的 type=reasoning.text）。
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
pub(crate) fn extract_reasoning(delta: &Value) -> Option<String> {
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
    use super::*;

    fn init_logger() {
        static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| {
            let dir = std::env::temp_dir().join("agent-test-logs");
            let _ = crate::observability::init(&dir, "openai.jsonl", format!("openai-{}", uuid::Uuid::new_v4()));
        });
    }

    /// 2026-10-08 修：负 index（`as usize` → ~1.8e19）与超大 index 会把合并 Vec
    /// 无限撑大直到 OOM；越界分片应被忽略，且不影响正常分片合并。
    #[test]
    fn tool_call_index_out_of_range_is_ignored() {
        init_logger();
        let mut calls: Vec<ToolCall> = Vec::new();
        accumulate_tool_calls(
            &[
                json!({"index": 0, "id": "a", "function": {"name": "read_file", "arguments": "{}"}}),
                json!({"index": -1, "id": "bad"}),
                json!({"index": 999999999, "id": "bad2"}),
                json!({"index": 1, "id": "b", "function": {"name": "shell", "arguments": "{\"c\":1}"}}),
            ],
            &mut calls,
        );
        assert_eq!(calls.len(), 2, "越界分片不得占位: {calls:?}");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[1].name, "shell");
        assert_eq!(calls[1].arguments, "{\"c\":1}");
        // 缺 index 仍按 0 合并（原行为不变）
        let mut c2: Vec<ToolCall> = Vec::new();
        accumulate_tool_calls(&[json!({"id": "x", "function": {"name": "git"}})], &mut c2);
        assert_eq!(c2.len(), 1);
        assert_eq!(c2[0].id, "x");
    }
}
