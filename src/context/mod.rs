//! 上下文工程 v1 —— 施工单 006（P5）
//!
//! 1. token 启发式估算：CJK ≈1 token/字，ASCII ≈1 token/4 字符，其余 ≈1/2——无 tokenizer 依赖。
//! 2. 工具结果头尾压缩：头部 60% + 尾部 40%，中间插省略标记（替代旧的单向硬截断）。
//! 3. 历史裁剪：每轮调模型前估算；超预算先归档旧工具结果（保留最近 N 个），
//!    仍超则按「轮」（user 开头的一组）从旧到新整组折叠成占位——保持 tool_calls/tool 配对完整。
//!    system、最后一条 user、最近 N 个工具结果永不裁。

use serde_json::{json, Value};

/// 头尾压缩时插入的省略标记（工具结果 truncated 判定依据）。
pub const OMITTED_MARK: &str = "……[中间省略";

/// 启发式 token 估算。
pub fn estimate_tokens(text: &str) -> u64 {
    let mut cjk: u64 = 0;
    let mut ascii: u64 = 0;
    let mut other: u64 = 0;
    for ch in text.chars() {
        if ('\u{4E00}'..='\u{9FFF}').contains(&ch)
            || ('\u{3000}'..='\u{303F}').contains(&ch)
            || ('\u{FF00}'..='\u{FFEF}').contains(&ch)
        {
            cjk += 1;
        } else if ch.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(2) + ascii.div_ceil(4)
}

/// 单条消息估算（role 开销 + content + tool_calls）。
fn estimate_message(m: &Value) -> u64 {
    let mut total: u64 = 4;
    if let Some(c) = m.get("content") {
        total += value_tokens(c);
    }
    if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            total += 8 + value_tokens(call);
        }
    }
    total
}

fn value_tokens(v: &Value) -> u64 {
    match v {
        Value::String(s) => estimate_tokens(s),
        Value::Array(a) => a.iter().map(value_tokens).sum(),
        Value::Object(o) => o.values().map(value_tokens).sum(),
        Value::Null => 0,
        other => estimate_tokens(&other.to_string()),
    }
}

pub fn estimate_messages(messages: &[Value]) -> u64 {
    messages.iter().map(estimate_message).sum()
}

/// 分类占比（前端「上下文容量」卡片用）：各分类之和 == estimate_messages。
#[derive(Debug, Clone, Copy, Default)]
pub struct Breakdown {
    /// user + assistant 正文
    pub messages: u64,
    /// 工具结果（role=tool）
    pub tools: u64,
    /// 系统提示词（role=system）
    pub system: u64,
    /// tool_calls 结构与每条消息的固定开销
    pub other: u64,
}

impl Breakdown {
    pub fn total(&self) -> u64 {
        self.messages + self.tools + self.system + self.other
    }
}

pub fn estimate_breakdown(messages: &[Value]) -> Breakdown {
    let mut b = Breakdown::default();
    for m in messages {
        match m.get("role").and_then(Value::as_str).unwrap_or("") {
            "system" => b.system += estimate_message(m),
            "tool" => b.tools += estimate_message(m),
            "user" => b.messages += estimate_message(m),
            "assistant" => {
                if let Some(c) = m.get("content") {
                    b.messages += value_tokens(c);
                }
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    b.other += 8 * calls.len() as u64 + calls.iter().map(value_tokens).sum::<u64>();
                }
                b.other += 4;
            }
            _ => b.other += estimate_message(m),
        }
    }
    b
}

/// 头尾压缩：短文本原样返回；超限保留头 60% + 尾 40%。
pub fn compress(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let head = max_chars * 6 / 10;
    let tail = max_chars - head;
    let head_s: String = text.chars().take(head).collect();
    let tail_s: String = text.chars().skip(total - tail).collect();
    let omitted = total - head - tail;
    format!("{head_s}\n{OMITTED_MARK} {omitted} 字符，完整内容见日志]……\n{tail_s}")
}

/// 裁剪配置直接复用 config.toml [context] 节。
pub use crate::config::ContextConfig as CompactConfig;

#[derive(Debug, Clone, Default)]
pub struct CompactStats {
    pub compacted: bool,
    pub archived_tools: usize,
    pub folded_messages: usize,
    pub tokens_before: u64,
    pub tokens_after: u64,
}

/// 历史裁剪。messages[0] 视为 system 永不裁。
/// 策略：① 归档旧工具结果（保留最近 N 个）② 仍超则按轮折叠旧消息。
pub fn compact_messages(messages: &[Value], cfg: &CompactConfig) -> (Vec<Value>, CompactStats) {
    let mut stats = CompactStats { tokens_before: estimate_messages(messages), ..Default::default() };
    if messages.is_empty() || stats.tokens_before <= cfg.budget_tokens {
        stats.tokens_after = stats.tokens_before;
        return (messages.to_vec(), stats);
    }
    let mut out: Vec<Value> = messages.to_vec();

    // ① 归档旧工具结果：只动 content 超过 300 字符的，避免动小占位
    let tool_idx: Vec<usize> = out
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(Value::as_str) == Some("tool"))
        .map(|(i, _)| i)
        .collect();
    let keep_from = tool_idx.len().saturating_sub(cfg.recent_tool_results_kept);
    for &i in &tool_idx[..keep_from] {
        let content = out[i].get("content").and_then(Value::as_str).unwrap_or("");
        if content.chars().count() > 300 {
            let name = out[i].get("name").and_then(Value::as_str).unwrap_or("tool");
            out[i]["content"] = json!(format!(
                "[工具结果已归档：{name}，原约 {} tokens，完整内容见日志]",
                estimate_tokens(content)
            ));
            stats.archived_tools += 1;
        }
    }

    // ② 仍超 → 按「轮」折叠：轮 = user 开头到下一个 user 之前的相邻消息组，
    //    整组折叠保持 assistant(tool_calls)+tool 配对完整；最后一组（含最新 user）永不折。
    let mut after = estimate_messages(&out);
    if after > cfg.budget_tokens {
        let mut groups: Vec<(usize, usize)> = Vec::new(); // [start, end)
        let mut start = 1usize; // 0 是 system
        for i in 2..out.len() {
            if out[i].get("role").and_then(Value::as_str) == Some("user") {
                groups.push((start, i));
                start = i;
            }
        }
        groups.push((start, out.len()));

        // 保护：最后一组；含最近 N 个工具结果起始位置的组（该组及之后全保）
        let protected_start = tool_idx
            .len()
            .checked_sub(cfg.recent_tool_results_kept)
            .and_then(|k| tool_idx.get(k))
            .copied();
        let foldable: Vec<(usize, usize)> = groups
            .iter()
            .take(groups.len().saturating_sub(1)) // 最后一组不折
            .copied()
            .filter(|(_, gend)| protected_start.map_or(true, |ps| ps >= *gend))
            .collect();

        let mut folded_ranges: Vec<(usize, usize)> = Vec::new();
        for (gstart, gend) in foldable {
            if after <= cfg.budget_tokens {
                break;
            }
            for i in gstart..gend {
                after -= estimate_message(&out[i]);
                stats.folded_messages += 1;
            }
            folded_ranges.push((gstart, gend));
        }
        if !folded_ranges.is_empty() {
            let folded_count: usize = folded_ranges.iter().map(|(s, e)| e - s).sum();
            let kept: Vec<Value> = out
                .iter()
                .enumerate()
                .filter(|(i, _)| !folded_ranges.iter().any(|(s, e)| *i >= *s && *i < *e))
                .map(|(_, m)| m.clone())
                .collect();
            let mut new_out = vec![out[0].clone()];
            new_out.push(json!({ "role": "user", "content": format!("[早期对话已省略 {folded_count} 条消息]") }));
            new_out.extend(kept);
            out = new_out;
        }
    }

    stats.tokens_after = estimate_messages(&out);
    stats.compacted = stats.archived_tools > 0 || stats.folded_messages > 0;
    (out, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimation_cjk_vs_ascii() {
        // 纯 ASCII：约 1 token / 4 字符
        let ascii = "a".repeat(400);
        assert_eq!(estimate_tokens(&ascii), 100);
        // 纯 CJK：1 token / 字
        let cjk = "中文测试".repeat(25); // 100 字
        assert_eq!(estimate_tokens(&cjk), 100);
        // 混合：CJK 全算 + ASCII/4
        let mixed = format!("{cjk}{ascii}");
        assert_eq!(estimate_tokens(&mixed), 200);
    }

    #[test]
    fn compress_keeps_head_and_tail() {
        let short = "hello";
        assert_eq!(compress(short, 100), "hello");
        let long: String = std::iter::repeat_n('x', 10_000).collect();
        let out = compress(&long, 800);
        assert!(out.contains(OMITTED_MARK));
        assert!(out.starts_with("xxx"));
        assert!(out.ends_with("xxx"));
        assert!(out.chars().count() < 10_000);
    }

    fn user_msg(s: &str) -> Value {
        json!({ "role": "user", "content": s })
    }
    fn tool_msg(s: &str) -> Value {
        json!({ "role": "tool", "tool_call_id": "t", "name": "shell", "content": s })
    }
    fn asst_msg() -> Value {
        json!({ "role": "assistant", "content": "好的" })
    }

    #[test]
    fn compact_archives_old_tool_results_keeps_recent() {
        let long_result = "r".repeat(5000);
        let mut messages = vec![json!({ "role": "system", "content": "sys" }), user_msg("开始")];
        for _ in 0..10 {
            messages.push(asst_msg());
            messages.push(tool_msg(&long_result));
        }
        messages.push(user_msg("最后问题"));
        let cfg = CompactConfig { budget_tokens: 100_000, recent_tool_results_kept: 6 };
        // 预算足够 → 不动
        let (out, stats) = compact_messages(&messages, &cfg);
        assert!(!stats.compacted);
        assert_eq!(out.len(), messages.len());

        // 预算紧张 → 旧工具结果归档，最近 6 个完整
        let cfg = CompactConfig { budget_tokens: 3000, recent_tool_results_kept: 6 };
        let (out, stats) = compact_messages(&messages, &cfg);
        assert!(stats.compacted && stats.archived_tools >= 4);
        let tools: Vec<&Value> =
            out.iter().filter(|m| m.get("role").and_then(Value::as_str) == Some("tool")).collect();
        let archived = tools
            .iter()
            .filter(|m| {
                m.get("content").and_then(Value::as_str).unwrap_or("").starts_with("[工具结果已归档")
            })
            .count();
        assert_eq!(archived, tools.len() - 6, "最近 6 个必须完整保留");
        // system 与最后 user 不动
        assert_eq!(out[0]["content"], "sys");
        assert_eq!(out.last().unwrap()["content"], "最后问题");
    }

    #[test]
    fn compact_folds_whole_rounds_when_still_over() {
        let long_result = "r".repeat(6000);
        let mut messages = vec![json!({ "role": "system", "content": "sys" })];
        for i in 0..8 {
            messages.push(user_msg(&format!("问题{i}")));
            messages.push(asst_msg());
            messages.push(tool_msg(&long_result));
        }
        messages.push(user_msg("最后问题"));
        // 预算极小：归档完工具结果也压不下来 → 折叠整轮
        let cfg = CompactConfig { budget_tokens: 2000, recent_tool_results_kept: 2 };
        let (out, stats) = compact_messages(&messages, &cfg);
        assert!(stats.compacted && stats.folded_messages > 0);
        assert_eq!(out[0]["content"], "sys");
        assert!(out
            .iter()
            .any(|m| m.get("content").and_then(Value::as_str).unwrap_or("").contains("早期对话已省略")));
        assert_eq!(out.last().unwrap()["content"], "最后问题");
        // 折到无可折为止：保护区（最近 2 个工具结果）本身可能超预算，此时不强求达标
        assert!(stats.tokens_after < stats.tokens_before);
    }

    #[test]
    fn compact_keeps_tool_pairs_intact() {
        // 折叠后不允许出现孤儿 tool 消息（tool 必须紧跟含 tool_calls 的 assistant 之后）
        let long_result = "r".repeat(4000);
        let mut messages = vec![json!({ "role": "system", "content": "sys" })];
        for i in 0..6 {
            messages.push(user_msg(&format!("q{i}")));
            messages.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{ "id": format!("c{i}"), "type": "function",
                    "function": { "name": "shell", "arguments": "{}" } }]
            }));
            messages.push(tool_msg(&long_result));
        }
        messages.push(user_msg("最后问题"));
        let cfg = CompactConfig { budget_tokens: 1500, recent_tool_results_kept: 1 };
        let (out, _) = compact_messages(&messages, &cfg);
        for (i, m) in out.iter().enumerate() {
            if m.get("role").and_then(Value::as_str) == Some("tool") {
                let prev = &out[i - 1];
                assert_eq!(prev.get("role").and_then(Value::as_str), Some("assistant"));
                assert!(prev.get("tool_calls").is_some(), "折叠破坏了 tool_calls/tool 配对");
            }
        }
    }
}
