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

/// 界面专用气泡（通知/错误）：随快照落盘给界面回放，发送前由 gateway replay 剥离
/// （`apply_outgoing_transforms`），**模型永远看不到**——对齐 DSH「只给模型可见内容计价」，
/// 计价 0，否则估算/预算/圆环都会被没发出去的内容顶高。
fn is_display_only(m: &Value) -> bool {
    m.get("display_only").and_then(Value::as_bool) == Some(true)
}

/// 单条消息估算（role 开销 + content + tool_calls；`count_reasoning` 时含 reasoning_content）。
/// 思考是否计入**跟随回放判定**（should_preserve_replay）：DeepSeek/zai 契约与白名单
/// 模型每轮把思考重发进 prompt，本地预算必须看见；剥离渠道思考不进上下文、不计入
/// （用户拍板 2026-10-01：切勿对所有模型一律计入，否则剥离渠道会被过度裁剪）。
fn estimate_message(m: &Value, count_reasoning: bool) -> u64 {
    if is_display_only(m) {
        return 0;
    }
    let mut total: u64 = 4;
    if let Some(c) = m.get("content") {
        total += value_tokens(c);
    }
    if count_reasoning
        && let Some(c) = m.get("reasoning_content").and_then(Value::as_str)
    {
        total += estimate_tokens(c);
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
        // 2026-10-08 修：键名也要计——tool_calls 的 {"id"/"type"/"function"/"name"}
        // 这些结构键原先全漏，一次 write_file 调用少算 15-20 tokens，随步数线性
        // 累积，把裁剪时机系统性推晚（模块注释自己实锤过同类偏差：估算 1866 vs 实际 4750）。
        Value::Object(o) => o.iter().map(|(k, v)| estimate_tokens(k) + value_tokens(v)).sum(),
        Value::Null => 0,
        other => estimate_tokens(&other.to_string()),
    }
}

pub fn estimate_messages(messages: &[Value], count_reasoning: bool) -> u64 {
    messages.iter().map(|m| estimate_message(m, count_reasoning)).sum()
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
    /// 工具定义（随每次请求发送的 tools JSON Schema，渠道计入 prompt tokens；
    /// 不算它圆环会比真实用量少一大截）。由调用方（核心循环）估算后填入。
    pub tools_schema: u64,
}

impl Breakdown {
    pub fn total(&self) -> u64 {
        self.messages + self.tools + self.system + self.other + self.tools_schema
    }
}

pub fn estimate_breakdown(messages: &[Value], count_reasoning: bool) -> Breakdown {
    let mut b = Breakdown::default();
    for m in messages {
        if is_display_only(m) {
            continue;
        }
        match m.get("role").and_then(Value::as_str).unwrap_or("") {
            "system" => b.system += estimate_message(m, false),
            "tool" => b.tools += estimate_message(m, false),
            "user" => b.messages += estimate_message(m, false),
            "assistant" => {
                if let Some(c) = m.get("content") {
                    b.messages += value_tokens(c);
                }
                if count_reasoning
                    && let Some(c) = m.get("reasoning_content").and_then(Value::as_str)
                {
                    b.messages += estimate_tokens(c);
                }
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    b.other += 8 * calls.len() as u64 + calls.iter().map(value_tokens).sum::<u64>();
                }
                b.other += 4;
            }
            _ => b.other += estimate_message(m, false),
        }
    }
    b
}

/// 占用口径（对齐 DSH token-meter 的 `projectedTokens`）：**实报锚点 + 自锚点以来的
/// 带符号增量**。`anchor = (渠道实报 prompt_tokens, 那次请求发出时的本地估价)`，
/// `est_now` = 当前上下文的本地估价。
///
/// 三个要点（都是本地自己算增量算不出来的）：
/// 1. 工具结果/追发消息等**在请求之后**落进上下文的部分自动计入（本地估价涨了多少就补多少）；
/// 2. 裁剪/折叠让上下文变小时增量是**负的**，圆环当场回落，不需要清空实报值；
/// 3. 渠道实报与本地估价不是一个尺子（实报是真实分词），所以只拿它当锚，增量用本地尺子量。
///
/// 没用过实报（首请求前）或渠道不回 usage 时 anchor 为 None → 纯估算兜底。
pub fn projected_tokens(anchor: Option<(u64, u64)>, est_now: u64) -> u64 {
    match anchor {
        Some((real, est_at_request)) => {
            let delta = est_now as i128 - est_at_request as i128;
            (real as i128 + delta).clamp(0, u64::MAX as i128) as u64
        }
        None => est_now,
    }
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
    /// P7：被折叠掉的原始消息（供滚动摘要；不折叠时为空）
    pub folded: Vec<Value>,
}

/// 历史裁剪。messages[0] 视为 system 永不裁。
/// 策略：① 归档旧工具结果（保留最近 N 个）② 仍超则按轮折叠旧消息。
/// `tools_schema_tokens`：随每次请求发送的工具定义（渠道计入 prompt），不与消息
/// 一起裁但占预算——不算它，裁剪时机被系统性推迟（实锤：估算 1866 vs 实际 4750）。
/// `count_reasoning`：回放型渠道（DeepSeek/zai 契约、白名单模型）思考每轮重发，
/// 预算要看见；剥离渠道传 false。
pub fn compact_messages(
    messages: &[Value],
    cfg: &CompactConfig,
    tools_schema_tokens: u64,
    count_reasoning: bool,
) -> (Vec<Value>, CompactStats) {
    let mut stats = CompactStats {
        tokens_before: estimate_messages(messages, count_reasoning) + tools_schema_tokens,
        ..Default::default()
    };
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
    let mut after = estimate_messages(&out, count_reasoning) + tools_schema_tokens;
    if after > cfg.budget_tokens {
        let mut groups: Vec<(usize, usize)> = Vec::new(); // [start, end)
        let mut start = 1usize; // 0 是 system
        for (i, m) in out.iter().enumerate().skip(2) {
            if m.get("role").and_then(Value::as_str) == Some("user") {
                groups.push((start, i));
                start = i;
            }
        }
        groups.push((start, out.len()));

        // 保护：最后一组；含最近 N 个工具结果起始位置的组（该组及之后全保）
        // 2026-10-08 修：工具结果总数 ≤ 保留数时 checked_sub 下溢成 None，而
        // is_none_or(None) 返回 true——「最近 N 个永不裁」反转为「全组可裁」，
        // 最新的工具结果被整组折叠（且不像归档那样留占位符，模型侧无声丢失）。
        // 总数不超保留数时第一个工具结果就是保护起点；一个工具结果都没有时
        // 才是真正的「无保护」（None）。
        let protected_start = if tool_idx.len() <= cfg.recent_tool_results_kept {
            tool_idx.first().copied()
        } else {
            tool_idx.get(tool_idx.len() - cfg.recent_tool_results_kept).copied()
        };
        let foldable: Vec<(usize, usize)> = groups
            .iter()
            .take(groups.len().saturating_sub(1)) // 最后一组不折
            .copied()
            .filter(|(_, gend)| protected_start.is_none_or(|ps| ps >= *gend))
            .collect();

        let mut folded_ranges: Vec<(usize, usize)> = Vec::new();
        for (gstart, gend) in foldable {
            if after <= cfg.budget_tokens {
                break;
            }
            for m in &out[gstart..gend] {
                after -= estimate_message(m, count_reasoning);
                stats.folded_messages += 1;
            }
            folded_ranges.push((gstart, gend));
        }
        if !folded_ranges.is_empty() {
            let folded_count: usize = folded_ranges.iter().map(|(s, e)| e - s).sum();
            // P7：把被折叠的原始消息带出来，供滚动摘要压缩
            stats.folded = folded_ranges
                .iter()
                .flat_map(|(s, e)| out[*s..*e].to_vec())
                .collect();
            let kept: Vec<Value> = out
                .iter()
                .enumerate()
                // 0 号（system）由下面单独前置，绝不能再进 kept——否则折叠后 system 出现两次：
                // 请求里多发一份（实测 system ≈1386 tokens）、tokens_after/审计/圆环都算两遍，
                // 而且这份重复会随 snapshot_msgs（只 skip(1)）落进会话文件当历史重发。
                .filter(|(i, _)| {
                    *i != 0 && !folded_ranges.iter().any(|(s, e)| *i >= *s && *i < *e)
                })
                .map(|(_, m)| m.clone())
                .collect();
            let mut new_out = vec![out[0].clone()];
            new_out.push(json!({ "role": "user", "content": format!("[早期对话已省略 {folded_count} 条消息]") }));
            new_out.extend(kept);
            out = new_out;
        }
    }

    stats.tokens_after = estimate_messages(&out, count_reasoning) + tools_schema_tokens;
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
        let (out, stats) = compact_messages(&messages, &cfg, 0, false);
        assert!(!stats.compacted);
        assert_eq!(out.len(), messages.len());

        // 预算紧张 → 旧工具结果归档，最近 6 个完整
        let cfg = CompactConfig { budget_tokens: 3000, recent_tool_results_kept: 6 };
        let (out, stats) = compact_messages(&messages, &cfg, 0, false);
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
    fn tools_schema_counts_into_compaction_budget() {
        // 工具定义随每次请求发送、占预算但不随消息裁：不算它裁剪会被系统性推迟
        // （4 轮 × 5000 字符结果 ≈ 5.1k tokens 估算，预算 6k：不带 tools 不裁，带上 1.5k 就过线）
        let long_result = "r".repeat(5000);
        let mut messages = vec![json!({ "role": "system", "content": "sys" }), user_msg("开始")];
        for _ in 0..4 {
            messages.push(asst_msg());
            messages.push(tool_msg(&long_result));
        }
        messages.push(user_msg("最后问题"));
        let cfg = CompactConfig { budget_tokens: 6_000, recent_tool_results_kept: 2 };
        let (_, stats) = compact_messages(&messages, &cfg, 0, false);
        assert!(!stats.compacted, "消息在预算内不该裁");
        let (_, stats2) = compact_messages(&messages, &cfg, 1_500, false);
        assert!(stats2.compacted && stats2.archived_tools >= 2, "tools 计入后应触发归档");
        assert!(stats2.tokens_after <= stats2.tokens_before);
    }

    #[test]
    fn reasoning_counts_only_when_replayed() {
        // 思考计入与否跟随回放判定：回放渠道（DeepSeek/zai 契约、白名单）思考每轮
        // 重发进 prompt，预算要看见；剥离渠道不计入，否则过度裁剪
        let msgs = vec![json!({
            "role": "assistant",
            "content": "好的",
            "reasoning_content": "思".repeat(100),
        })];
        let stripped = estimate_messages(&msgs, false);
        let replayed = estimate_messages(&msgs, true);
        assert_eq!(stripped, 4 + 2, "剥离渠道：只算 role 开销 + 正文");
        assert_eq!(replayed, stripped + 100, "回放渠道：思考按 CJK 1 token/字计入");
        assert_eq!(estimate_breakdown(&msgs, true).messages, 2 + 100, "breakdown 同口径");
        assert_eq!(estimate_breakdown(&msgs, false).messages, 2);
    }

    #[test]
    fn compact_folds_whole_rounds_when_still_over() {        let long_result = "r".repeat(6000);
        let mut messages = vec![json!({ "role": "system", "content": "sys" })];
        for i in 0..8 {
            messages.push(user_msg(&format!("问题{i}")));
            messages.push(asst_msg());
            messages.push(tool_msg(&long_result));
        }
        messages.push(user_msg("最后问题"));
        // 预算极小：归档完工具结果也压不下来 → 折叠整轮
        let cfg = CompactConfig { budget_tokens: 2000, recent_tool_results_kept: 2 };
        let (out, stats) = compact_messages(&messages, &cfg, 0, false);
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
    fn compact_returns_folded_messages_for_summary() {
        // P7：折叠发生时，被折叠的原始消息要带出来供滚动摘要
        let long_result = "r".repeat(6000);
        let mut messages = vec![json!({ "role": "system", "content": "sys" })];
        for i in 0..8 {
            messages.push(user_msg(&format!("问题{i}")));
            messages.push(asst_msg());
            messages.push(tool_msg(&long_result));
        }
        messages.push(user_msg("最后问题"));
        let cfg = CompactConfig { budget_tokens: 2000, recent_tool_results_kept: 2 };
        let (out, stats) = compact_messages(&messages, &cfg, 0, false);
        assert!(stats.folded_messages > 0);
        assert!(!stats.folded.is_empty(), "折叠的原始消息必须带出");
        // 带出的内容含被折叠轮次的 user 消息原文
        assert!(stats.folded.iter().any(|m| m.get("content").and_then(Value::as_str) == Some("问题0")));
        // 折叠后 out 里不再有这些原文（占位符替代）
        assert!(!out.iter().any(|m| m.get("content").and_then(Value::as_str) == Some("问题0")));
    }

    #[test]
    fn fold_keeps_exactly_one_system_message() {
        // 实锤回归（折叠后 system 被复制一份）：kept 曾把 0 号消息也收进去，而它又单独
        // 前置了一次 → 输出 [system, 占位, system, …]。后果：请求多发一份 system、
        // tokens_after/圆环重复计入、重复那份还会随快照落进会话文件。
        let long_result = "r".repeat(6000);
        let mut messages = vec![json!({ "role": "system", "content": "SYS" })];
        for i in 0..8 {
            messages.push(user_msg(&format!("问题{i}")));
            messages.push(asst_msg());
            messages.push(tool_msg(&long_result));
        }
        messages.push(user_msg("最后问题"));
        let cfg = CompactConfig { budget_tokens: 2000, recent_tool_results_kept: 2 };
        let (out, stats) = compact_messages(&messages, &cfg, 0, false);
        assert!(stats.folded_messages > 0, "该用例必须真的折叠");
        let systems = out
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("system"))
            .count();
        assert_eq!(systems, 1, "折叠后只允许有一条 system（0 号那条）: {out:?}");
        assert_eq!(out[0]["content"], "SYS");
        // 折叠确实让估价变小（重复的那份会把 tokens_after 顶高，等于白折）
        assert!(stats.tokens_after < stats.tokens_before);
    }

    #[test]
    fn display_only_bubbles_are_not_priced() {
        // 通知/错误气泡只给界面看（发送前被 gateway replay 剥离），不该进估算/预算
        let msgs = vec![
            json!({ "role": "system", "content": "sys" }),
            user_msg("问题"),
            json!({ "role": "system", "content": format!("[通知] {}", "啰嗦".repeat(500)), "display_only": true }),
        ];
        let b = estimate_breakdown(&msgs, false);
        assert_eq!(b.system, 4 + 1, "只算真正的 system 提示词");
        assert_eq!(estimate_messages(&msgs, false), 4 + 1 + 4 + 2, "display_only 计价 0");
        // 预算判定也不该被它顶高
        let cfg = CompactConfig { budget_tokens: 100, recent_tool_results_kept: 1 };
        let (_, stats) = compact_messages(&msgs, &cfg, 0, false);
        assert!(!stats.compacted, "气泡不该把上下文顶过预算");
    }

    #[test]
    fn projected_follows_anchor_with_signed_delta() {
        // 锚点 + 带符号增量：涨了补上（工具结果/追发消息），缩了回落（裁剪），没锚点用估算
        let anchor = Some((10_000u64, 9_000u64));
        assert_eq!(projected_tokens(anchor, 9_000), 10_000, "上下文没变 → 就是实报值");
        assert_eq!(projected_tokens(anchor, 12_000), 13_000, "新增 3000 估价的工具结果 → 补上");
        assert_eq!(projected_tokens(anchor, 4_000), 5_000, "裁剪掉 5000 → 负增量，当场回落");
        assert_eq!(projected_tokens(anchor, 0), 1_000, "极端缩水不出现负数");
        assert_eq!(projected_tokens(None, 7_777), 7_777, "首请求前/渠道不回 usage → 纯估算");
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
        let (out, _) = compact_messages(&messages, &cfg, 0, false);
        for (i, m) in out.iter().enumerate() {
            if m.get("role").and_then(Value::as_str) == Some("tool") {
                let prev = &out[i - 1];
                assert_eq!(prev.get("role").and_then(Value::as_str), Some("assistant"));
                assert!(prev.get("tool_calls").is_some(), "折叠破坏了 tool_calls/tool 配对");
            }
        }
    }
}

#[cfg(test)]
mod protected_start_tests {
    use super::*;

    fn user_msg(s: &str) -> Value {
        json!({ "role": "user", "content": s })
    }
    fn tool_msg(s: &str) -> Value {
        json!({ "role": "tool", "tool_call_id": "t", "name": "shell", "content": s })
    }
    fn asst_msg() -> Value {
        json!({ "role": "assistant", "content": "好的" })
    }

    /// 2026-10-08 修：工具结果总数 ≤ 保留数时，旧代码的 checked_sub 下溢成 None，
    /// is_none_or(None)=true 把「最近 N 个永不裁」反转为「全组可裁」——最新工具
    /// 结果被整组折叠（无占位符，模型侧无声丢失）。
    #[test]
    fn few_tool_results_are_never_folded() {
        let long_result = "r".repeat(5000);
        let mut messages = vec![json!({ "role": "system", "content": "sys" }), user_msg("开始")];
        for _ in 0..3 {
            messages.push(asst_msg());
            messages.push(tool_msg(&long_result));
        }
        messages.push(user_msg("最后问题"));
        // 保留数 6 ≥ 实际 3 个结果 + 预算极小（本来会触发折叠）
        let cfg = CompactConfig { budget_tokens: 2000, recent_tool_results_kept: 6 };
        let (out, stats) = compact_messages(&messages, &cfg, 0, false);
        let tools: Vec<&Value> = out
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .collect();
        assert_eq!(tools.len(), 3, "3 个工具结果一个都不许被折叠掉: {stats:?}");
        for t in &tools {
            let c = t.get("content").and_then(Value::as_str).unwrap_or("");
            assert_eq!(c.len(), 5000, "最新工具结果必须完整保留（不归档不折叠）");
        }
        // 折叠没发生（没有可折的组：含工具结果的组全在保护区内）
        assert_eq!(stats.folded_messages, 0, "没有组该被折叠: {stats:?}");
        // 超过预算是预期（保护优先于预算），但消息一条不少
        assert!(stats.tokens_after > cfg.budget_tokens);
        assert_eq!(out.len(), messages.len(), "消息一条不动");
    }

    /// 对照：结果数 > 保留数时，旧行为不变——更早的轮可折，最近 N 个结果所在的组受保护。
    #[test]
    fn many_tool_results_keep_recent_rounds() {
        let long_result = "r".repeat(6000);
        let mut messages = vec![json!({ "role": "system", "content": "sys" })];
        for i in 0..8 {
            messages.push(user_msg(&format!("问题{i}")));
            messages.push(asst_msg());
            messages.push(tool_msg(&long_result));
        }
        messages.push(user_msg("最后问题"));
        let cfg = CompactConfig { budget_tokens: 2000, recent_tool_results_kept: 2 };
        let (out, stats) = compact_messages(&messages, &cfg, 0, false);
        assert!(stats.folded_messages > 0, "旧轮该折");
        let tools: Vec<&Value> = out
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
            .collect();
        assert_eq!(tools.len(), 2, "最近 2 个工具结果必须留在上下文里");
    }
}
