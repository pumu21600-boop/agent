use super::*;

pub(crate) fn is_summary_msg(m: &Value) -> bool {
    m.get("content").and_then(Value::as_str).map(|c| c.starts_with(SUMMARY_PREFIX)).unwrap_or(false)
}

/// 快照用：去掉 system 头与摘要消息（摘要由会话文件 summary 字段单独持久化）。
pub(crate) fn snapshot_msgs(msgs: &[Value]) -> Vec<Value> {
    msgs.iter().skip(1).filter(|m| !is_summary_msg(m)).cloned().collect()
}

/// 滚动摘要：折叠内容 + 旧摘要 → 模型压成 ≤300 字要点。失败返回 None（调用方降级）。
/// 角色映射：优先走 [roles] summary 专用渠道，失败回落主渠道再降级占位符。
/// 辅助调用记用量台账（purpose=summary），归属会话由 sid 带入。
pub(crate) async fn summarize_folded(
    gateway: &Arc<Gateway>,
    primary: &Channel,
    summary_role: Option<&Channel>,
    prior: &Option<String>,
    folded: &[Value],
    sid: &str,
) -> Option<String> {
    let mut text = String::new();
    if let Some(p) = prior {
        text.push_str(&format!("已有摘要：{p}\n\n"));
    }
    text.push_str("新增对话：\n");
    for m in folded {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("?");
        let body: String = match m.get("content") {
            Some(Value::String(s)) => s.chars().take(500).collect(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string().chars().take(500).collect(),
        };
        let calls = m
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.pointer("/function/name").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        text.push_str(&format!("{role}: {body}{}\n", if calls.is_empty() { String::new() } else { format!(" [调用工具: {calls}]") }));
        if text.chars().count() > 8000 {
            break;
        }
    }
    let req = json!([
        {"role": "system", "content": "你是对话历史压缩器。把给定对话压成不超过300字中文要点，必须保留：任务目标、关键决定、改过哪些文件、未完成事项。只输出要点正文，不要开场白。"},
        {"role": "user", "content": text},
    ]);
    // 辅助调用固定小输出，压 max_tokens=400；其余参数跟渠道走
    let params = ChatParams { max_tokens: Some(400), ..primary.params.clone() };
    let mut out = None;
    // 角色渠道优先（参数用角色的；无角色渠道则直接主渠道）
    if let Some(role) = summary_role {
        let rp = ChatParams { max_tokens: Some(400), ..role.params.clone() };
        let ch = Channel { params: rp, ..role.clone() };
        let ctx = UsageCtx::aux(sid, "summary");
        out = gateway.complete(&ctx, &ch.base_url, &ch.model, &ch.api_key, &req, &ch.params).await.ok();
    }
    // 回落主渠道
    if out.is_none() {
        let ch = Channel { params, ..primary.clone() };
        let ctx = UsageCtx::aux(sid, "summary");
        out = gateway.complete(&ctx, &ch.base_url, &ch.model, &ch.api_key, &req, &ch.params).await.ok();
    }
    let out = out?;
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

/// TITLE 角色：用最终回答生成 ≤20 字会话标题。失败返回 None（保留机械截断标题）。
/// 辅助调用记用量台账（purpose=title）。
pub(crate) async fn generate_title(gateway: &Arc<Gateway>, ch: &Channel, answer: &str, sid: &str) -> Option<String> {
    let text: String = answer.chars().take(1200).collect();
    let req = json!([
        {"role": "system", "content": "根据对话内容生成一个不超过20字的中文会话标题。只输出标题本身，不要引号、句号或任何解释。"},
        {"role": "user", "content": text},
    ]);
    let params = ChatParams { max_tokens: Some(50), ..ch.params.clone() };
    let ctx = UsageCtx::aux(sid, "title");
    let out = gateway.complete(&ctx, &ch.base_url, &ch.model, &ch.api_key, &req, &params).await.ok()?;
    let mut t = out.trim().trim_matches(|c| c == '"' || c == '“' || c == '”' || c == '「' || c == '」').to_string();
    // 防跑偏：截到 30 字符以内（标题超长说明模型没听话，硬截断）
    t = t.chars().take(30).collect();
    (!t.is_empty()).then_some(t)
}
