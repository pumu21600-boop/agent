//! 历史回放净化 —— 移植自 OpenClaw `packages/ai/src/transports/openai-completions-replay.ts`（v2026.8.2）。
//!
//! OpenAI Chat Completions 的 assistant 输入消息没有 reasoning 字段：Stock OpenAI 会在
//! 下一轮请求里拒收；OpenRouter/DeepSeek 式厂商反而要求带回（跨轮思考连续性）。
//! 因此按端点归属决定保留还是剥离，并附带两条消息级 transform：
//! developer 角色改写（OpenAI 新系）与空消息过滤（ModelStudio/Qwen 拒收空 content）。

use serde_json::Value;

use super::compat::{Compat, ThinkingFormat};

/// assistant 消息上可能出现的思考回放字段（上游 COMPLETIONS_REASONING_REPLAY_FIELDS）。
pub const REPLAY_FIELDS: [&str; 4] = ["reasoning_details", "reasoning_content", "reasoning", "reasoning_text"];

/// DeepSeek 式 reasoning_content 回放契约的白名单模型（上游 REASONING_CONTENT_REPLAY_MODEL_IDS）。
const REPLAY_MODEL_IDS: [&str; 15] = [
    "deepseek-v4-flash",
    "deepseek-v4-pro",
    "kimi-for-coding",
    "kimi-k2.5",
    "kimi-k2.6",
    "kimi-k2.7-code",
    "kimi-k2.7-code-highspeed",
    "kimi-k3",
    "kimi-k2-thinking",
    "kimi-k2-thinking-turbo",
    "mimo-v2-pro",
    "mimo-v2-omni",
    "mimo-v2.5",
    "mimo-v2.5-pro",
    "mimo-v2.6-pro",
];

/// 渠道在模型 id 后追加的档位后缀（free/paid/trial），剥掉再比对白名单。
const TIER_SUFFIXES: [&str; 3] = ["-free", "-paid", "-trial"];

fn replay_model_id_candidates(model_id: &str) -> Vec<String> {
    let normalized = model_id.trim().to_lowercase();
    if normalized.is_empty() {
        return vec![];
    }
    let mut candidates: Vec<String> = Vec::new();
    let final_part = normalized.rsplit('/').next().unwrap_or(&normalized).to_string();
    candidates.push(final_part.clone());
    let mut colon: Vec<String> = final_part
        .split(':')
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect();
    if colon.len() > 1 {
        candidates.push(colon.remove(0));
        candidates.push(colon.pop().unwrap_or_default());
    }
    let base = candidates.clone();
    for c in base {
        for suffix in TIER_SUFFIXES {
            if c.len() > suffix.len() && c.ends_with(suffix) {
                candidates.push(c[..c.len() - suffix.len()].to_string());
            }
        }
    }
    candidates
}

/// assistant 消息是否应保留 reasoning_content 回放（上游 shouldPreserveReasoningContentReplay）。
fn should_preserve_replay(model_id: &str, compat: &Compat) -> bool {
    if compat.requires_reasoning_content_replay || compat.thinking_format == ThinkingFormat::Deepseek {
        return true;
    }
    replay_model_id_candidates(model_id)
        .iter()
        .any(|c| REPLAY_MODEL_IDS.contains(&c.as_str()))
}

/// 发送前净化 messages（原位修改）：
/// 1) assistant 消息的思考回放字段——保留（DeepSeek/zai 契约或白名单模型）或剥离（其余）；
/// 2) OpenRouter 形态 reasoning_details → reasoning 字符串合并后再按契约去留；
/// 3) developer 角色改写（上游 supportsDeveloperRole：OpenAI 新系）；
/// 4) 空消息过滤（上游 requiresNonEmptyUserOrAssistantMessage：ModelStudio/Qwen）。
pub fn apply_outgoing_transforms(messages: &mut Value, model_id: &str, compat: &Compat) {
    let arr = match messages.as_array_mut() {
        Some(a) => a,
        None => return,
    };
    let preserve = should_preserve_replay(model_id, compat);

    for msg in arr.iter_mut() {
        let is_assistant = msg.get("role").and_then(Value::as_str) == Some("assistant");
        if is_assistant {
            if preserve {
                // reasoning_details(string) → reasoning 合并，其余非法形态删除（上游 sanitize 同款）
                if let Some(obj) = msg.as_object_mut() {
                    if let Some(details) = obj.get("reasoning_details") {
                        match details {
                            Value::String(s) => {
                                if !s.is_empty()
                                    && (!obj.get("reasoning").map(Value::is_string).unwrap_or(false))
                                {
                                    obj.insert("reasoning".into(), Value::String(s.clone()));
                                }
                                obj.remove("reasoning_details");
                            }
                            Value::Array(_) => {} // 合法形态：保留
                            _ => {
                                obj.remove("reasoning_details");
                            }
                        }
                    }
                }
            } else if let Some(obj) = msg.as_object_mut() {
                for field in REPLAY_FIELDS {
                    obj.remove(field);
                }
            }
        }
        // developer 改写：OpenAI 新系把 system 称为 developer
        if compat.thinking_format == ThinkingFormat::Openai && !compat_requires_system(compat) {
            // 仅在明确 supports 时改写；本产品保守起见只在 OpenAI 官方格式做
            if is_assistant {
                // no-op
            }
        }
    }

    // developer 改写（放循环外，避免借用冲突）
    if compat.thinking_format == ThinkingFormat::Openai && developer_role_for_model(model_id) {
        for msg in arr.iter_mut() {
            if msg.get("role").and_then(Value::as_str) == Some("system") {
                if let Some(obj) = msg.as_object_mut() {
                    obj.insert("role".into(), Value::String("developer".into()));
                }
            }
        }
    }

    // 空消息过滤：ModelStudio/Qwen 拒收空 content 的 user/assistant（无工具调用时）
    if compat.thinking_format == ThinkingFormat::Qwen {
        arr.retain(|msg| {
            let role = msg.get("role").and_then(Value::as_str);
            if !matches!(role, Some("user") | Some("assistant")) {
                return true;
            }
            let empty_content = msg
                .get("content")
                .map(|c| c.as_str().map(str::trim).unwrap_or("").is_empty() || c.is_null())
                .unwrap_or(true);
            let has_tool_calls = msg
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            let has_tool_call_id = msg.get("tool_call_id").is_some();
            !(empty_content && !has_tool_calls && !has_tool_call_id)
        });
    }
}

/// developer 角色：GPT-5/o 系（OpenAI 新对话模型）。上游按模型目录；本产品按 id 推导。
fn developer_role_for_model(model_id: &str) -> bool {
    let id = model_id.trim().to_lowercase();
    let norm = id.rsplit('/').next().unwrap_or(&id).to_string();
    norm.starts_with("gpt-5") || norm.starts_with("o1") || norm.starts_with("o3") || norm.starts_with("o4")
}

fn compat_requires_system(_compat: &Compat) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn openai_compat() -> Compat {
        Compat {
            thinking_format: ThinkingFormat::Openai,
            max_tokens_field: "max_tokens",
            supports_usage_in_streaming: true,
            supports_temperature: true,
            requires_reasoning_content_replay: false,
        }
    }

    #[test]
    fn strips_reasoning_for_stock_endpoints() {
        let mut msgs = json!([
            {"role":"user","content":"hi"},
            {"role":"assistant","content":"hello","reasoning_content":"hidden chain"},
            {"role":"user","content":"more"}
        ]);
        apply_outgoing_transforms(&mut msgs, "mystery-model", &openai_compat());
        let a = msgs.as_array().unwrap();
        assert!(a[1].get("reasoning_content").is_none(), "非契约端点应剥离 reasoning_content");
        assert_eq!(a[1]["content"], json!("hello"));
    }

    #[test]
    fn preserves_for_deepseek_contract_and_whitelist() {
        let mut compat = openai_compat();
        compat.thinking_format = ThinkingFormat::Deepseek;
        compat.requires_reasoning_content_replay = true;
        let mut msgs = json!([
            {"role":"assistant","content":"hi","reasoning_content":"chain"}
        ]);
        apply_outgoing_transforms(&mut msgs, "whatever", &compat);
        assert_eq!(msgs[0]["reasoning_content"], json!("chain"));
        // 白名单模型（kimi 家族）在 openai 格式下也保留
        let mut msgs = json!([{"role":"assistant","content":"hi","reasoning_content":"chain"}]);
        apply_outgoing_transforms(&mut msgs, "kimi-k2-thinking", &openai_compat());
        assert_eq!(msgs[0]["reasoning_content"], json!("chain"));
        // 档位后缀剥离后仍命中白名单
        let mut msgs = json!([{"role":"assistant","content":"hi","reasoning_content":"chain"}]);
        apply_outgoing_transforms(&mut msgs, "deepseek-v4-flash-free", &openai_compat());
        assert_eq!(msgs[0]["reasoning_content"], json!("chain"));
    }

    #[test]
    fn developer_role_and_empty_filter() {
        // GPT-5 新系：system → developer
        let mut msgs = json!([
            {"role":"system","content":"be nice"},
            {"role":"user","content":"hi"}
        ]);
        apply_outgoing_transforms(&mut msgs, "gpt-5", &openai_compat());
        assert_eq!(msgs[0]["role"], json!("developer"));
        // 旧模型不动
        let mut msgs = json!([{"role":"system","content":"x"}]);
        apply_outgoing_transforms(&mut msgs, "gpt-4o-mini", &openai_compat());
        assert_eq!(msgs[0]["role"], json!("system"));

        // qwen：空 user/assistant 消息剔除
        let mut compat = openai_compat();
        compat.thinking_format = ThinkingFormat::Qwen;
        let mut msgs = json!([
            {"role":"system","content":"sys"},
            {"role":"user","content":""},
            {"role":"assistant","content":null},
            {"role":"user","content":"real"},
            {"role":"assistant","content":"","tool_calls":[{"id":"c1"}]}
        ]);
        apply_outgoing_transforms(&mut msgs, "qwen-plus", &compat);
        let roles: Vec<&str> = msgs.as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, vec!["system", "user", "assistant"], "空消息剔除且带 tool_calls 的保留");
    }
}
