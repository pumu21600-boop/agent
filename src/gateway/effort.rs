//! reasoning_effort 档位解析 —— 移植自 OpenClaw `packages/ai/src/providers/openai-reasoning-effort.ts`（v2026.8.2）。
//!
//! 不同 GPT 家族接受的 effort 枚举不同（gpt-5 无 "none"，gpt-5.1+ 有，gpt-5.2+ 有 "xhigh"，
//! gpt-5.6 还有 "max" 且拒收 temperature），请求前必须按模型 id 解析成该模型支持的最近档位。
//! 规则与上游保持一致；本产品没有模型目录（catalog），supported 列表全部由模型 id 推导。

/// 上游规范档位集（含 off 输入别名；"none" 是 API 侧的"关"）。
const CANONICAL: [&str; 8] = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "off"];

const GPT_5_EFFORTS: [&str; 4] = ["minimal", "low", "medium", "high"];
const GPT_51_EFFORTS: [&str; 4] = ["none", "low", "medium", "high"];
const GPT_52_EFFORTS: [&str; 5] = ["none", "low", "medium", "high", "xhigh"];
const GPT_56_EFFORTS: [&str; 6] = ["none", "low", "medium", "high", "xhigh", "max"];
const GPT_CODEX_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];
const GPT_PRO_EFFORTS: [&str; 3] = ["medium", "high", "xhigh"];
const GPT_5_PRO_EFFORTS: [&str; 1] = ["high"];
const GPT_51_CODEX_MAX_EFFORTS: [&str; 4] = ["none", "medium", "high", "xhigh"];
const GPT_51_CODEX_MINI_EFFORTS: [&str; 1] = ["medium"];
const GENERIC_EFFORTS: [&str; 3] = ["low", "medium", "high"];

/// 模型 id 规范化：小写 + 去日期后缀（gpt-5.1-2025-06-09 → gpt-5.1）。
fn normalize_model_id(id: &str) -> String {
    let lower = id.trim().to_lowercase();
    // 尾 11 字节可能落在多字节字符中间（非 ASCII 模型名：中文/emoji/İ 这类
    // 大小写映射还会改变字节长度），必须先判字符边界再切，否则 panic——
    // 调用方在 tokio 任务里，panic 被运行时吞掉 = 这一轮无 error 无 done 静默死掉。
    // 日期后缀形如 -dddd-dd-dd 全是 ASCII，非边界时本就不可能匹配，跳过即等价。
    if lower.len() >= 11 && lower.is_char_boundary(lower.len() - 11) {
        let tail = &lower[lower.len() - 11..];
        let d = tail.as_bytes();
        // 形如 -\d{4}-\d{2}-\d{2}
        if d[0] == b'-' && d[5] == b'-' && d[8] == b'-' {
            let ok = d[1..5].iter().all(u8::is_ascii_digit)
                && d[6..8].iter().all(u8::is_ascii_digit)
                && d[9..11].iter().all(u8::is_ascii_digit);
            if ok {
                return lower[..lower.len() - 11].to_string();
            }
        }
    }
    lower
}

fn model_matches(id: &str, re: fn(&str) -> bool) -> bool {
    re(&normalize_model_id(id))
}

// 以下三个家族判定（5.4-mini / 5.5 / temperature 支持性）是 OpenClaw 逐行移植保留项，
// 当前未接线（档位解析只用到 5.6 与 codex 分支）；保留以便跟上游对齐时不产生差异。
#[allow(dead_code)]
fn is_gpt_5_4_mini(id: &str) -> bool {
    id.strip_prefix("gpt-5.4-mini").map(|r| r.is_empty() || r.starts_with('-')).unwrap_or(false)
}
#[allow(dead_code)]
fn is_gpt_5_5(id: &str) -> bool {
    id.strip_prefix("gpt-5.5").map(|r| r.is_empty() || r.starts_with('-')).unwrap_or(false)
}
fn is_gpt_5_6(id: &str) -> bool {
    id.strip_prefix("gpt-5.6").map(|r| r.is_empty() || r.starts_with('-')).unwrap_or(false)
}
fn is_gpt_5_x_codex(id: &str) -> bool {
    // /^gpt-5(\.\d+)?-codex(?:-|$)/
    let rest = match id.strip_prefix("gpt-5") {
        Some(r) => r,
        None => return false,
    };
    let rest = match rest.strip_prefix('.') {
        Some(minor) => {
            let digits = minor.split('-').next().unwrap_or("");
            if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
            &minor[digits.len()..]
        }
        None => rest,
    };
    rest.strip_prefix("-codex").map(|r| r.is_empty() || r.starts_with('-')).unwrap_or(false)
}
fn is_gpt_5_family(id: &str) -> bool {
    id == "gpt-5"
}
fn is_gpt_5_1_family(id: &str) -> bool {
    id == "gpt-5.1"
}
/// /^gpt-5\.[2-9](?:\.\d+)?(?:-|$)/
fn is_gpt_5_2_plus(id: &str) -> bool {
    versioned_head(id, |head| head.split('.').next().unwrap_or(""))
}
/// /^gpt-5\.[2-9](?:\.\d+)?-pro(?:-|$)/
fn is_gpt_5_2_plus_pro(id: &str) -> bool {
    versioned_head(id, |head| match head.split_once('-') {
        Some((minor, tail)) if tail == "pro" || tail.starts_with("pro-") => minor,
        _ => "",
    })
}

/// gpt-5.<minor>[.<patch>][-...] 的 minor 段校验：2-9 开头，其余段全数字。
fn versioned_head(id: &str, pick: impl Fn(&str) -> &str) -> bool {
    let rest = match id.strip_prefix("gpt-5.") {
        Some(r) => r,
        None => return false,
    };
    let head = rest.split('-').next().unwrap_or("");
    let minor = pick(head);
    if !matches!(minor, "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9") {
        return false;
    }
    head.split('.')
        .skip(1)
        .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// GPT-5.6 家族拒收 temperature（400），其余接受。上游 supportsOpenAITemperature。
/// （移植保留项，当前未接线——请求体组装按端点 compat 决定，不按模型家族判）
#[allow(dead_code)]
pub fn supports_temperature(model_id: &str) -> bool {
    !model_matches(model_id, is_gpt_5_6)
}

pub fn is_gpt_5_6_model(model_id: &str) -> bool {
    model_matches(model_id, is_gpt_5_6)
}
#[allow(dead_code)]
pub fn is_gpt_5_4_mini_model(model_id: &str) -> bool {
    model_matches(model_id, is_gpt_5_4_mini)
}
#[allow(dead_code)]
pub fn is_gpt_5_5_model(model_id: &str) -> bool {
    model_matches(model_id, is_gpt_5_5)
}

/// 该模型接受的 effort 枚举（上游 resolveOpenAISupportedReasoningEfforts，无目录时按 id 推导）。
pub fn supported_efforts(model_id: &str) -> &'static [&'static str] {
    let id = normalize_model_id(model_id);
    if is_gpt_5_6(&id) {
        return &GPT_56_EFFORTS;
    }
    if id == "gpt-5.1-codex-mini" {
        return &GPT_51_CODEX_MINI_EFFORTS;
    }
    if id == "gpt-5.1-codex-max" {
        return &GPT_51_CODEX_MAX_EFFORTS;
    }
    if is_gpt_5_x_codex(&id) {
        return &GPT_CODEX_EFFORTS;
    }
    if id == "gpt-5-pro" {
        return &GPT_5_PRO_EFFORTS;
    }
    if is_gpt_5_2_plus_pro(&id) {
        return &GPT_PRO_EFFORTS;
    }
    if is_gpt_5_2_plus(&id) {
        return &GPT_52_EFFORTS;
    }
    if is_gpt_5_1_family(&id) {
        return &GPT_51_EFFORTS;
    }
    if is_gpt_5_family(&id) {
        return &GPT_5_EFFORTS;
    }
    &GENERIC_EFFORTS
}

/// 归一化：仅规范名折叠大小写；渠道原生值保持原样（上游 normalizeOpenAIReasoningEffort）。
pub fn normalize_effort(effort: &str) -> String {
    let t = effort.trim();
    let folded = t.to_lowercase();
    if CANONICAL.contains(&folded.as_str()) {
        folded
    } else {
        t.to_string()
    }
}

fn is_disabled(effort: &str) -> bool {
    effort == "none" || effort == "off"
}

/// 把请求档位解析成该模型支持的最近档位（上游 resolveOpenAIReasoningEffortForModel）。
/// `fallback_map`：渠道配置的覆盖表（键为规范名，值为渠道原生标签，如 off→"none"）。
/// 返回 None = 该模型无法表达"思考"档位（连关都关不掉时调用方按未开启处理）。
pub fn resolve_effort(model_id: &str, requested: &str, fallback_map: Option<&std::collections::HashMap<String, String>>) -> Option<String> {
    let req = normalize_effort(requested);
    let mapped = fallback_map.and_then(|m| {
        m.get(&req)
            .cloned()
            .or_else(|| {
                if CANONICAL.contains(&req.as_str()) {
                    m.iter()
                        .find(|(k, _)| normalize_effort(k) == req)
                        .map(|(_, v)| v.clone())
                } else {
                    None
                }
            })
    });
    let normalized = mapped.map(|m| m.trim().to_string()).unwrap_or(req.clone());
    let supported = supported_efforts(model_id);
    if supported.contains(&normalized.as_str()) {
        return Some(normalized);
    }
    if req == "off" && supported.contains(&"none") {
        return Some("none".into());
    }
    if is_disabled(&req) || is_disabled(&normalized) {
        return None;
    }
    if req == "minimal" && supported.contains(&"low") {
        return Some("low".into());
    }
    if (req == "minimal" || req == "low") && supported.contains(&"medium") {
        return Some("medium".into());
    }
    if req == "xhigh" && supported.contains(&"high") {
        return Some("high".into());
    }
    if req == "max" && supported.contains(&"xhigh") {
        return Some("xhigh".into());
    }
    supported
        .iter()
        .find(|e| !is_disabled(&normalize_effort(e)))
        .map(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_and_strips_date_suffix() {
        assert_eq!(normalize_model_id("GPT-5.1-2025-06-09"), "gpt-5.1");
        assert_eq!(normalize_model_id("gpt-5.2"), "gpt-5.2");
        assert_eq!(normalize_effort("HIGH"), "high");
        assert_eq!(normalize_effort(" custom-tag "), "custom-tag");
    }

    /// 2026-10-08 修：非 ASCII 模型名尾 11 字节落在多字节字符中间，
    /// 原先直接 panic（字节下标不是字符边界）。日期后缀全是 ASCII，
    /// 非边界时跳过剥离即可，行为不变。
    #[test]
    fn non_ascii_model_id_does_not_panic() {
        // "gpt-5-" + 4 个汉字 = 6 + 12 = 18 字节；18-11=7 正落在「中」中间
        assert_eq!(normalize_model_id("gpt-5-中文测试"), "gpt-5-中文测试");
        // İ 小写映射 2 字节 → 3 字节，长度变化同样不能炸
        assert_eq!(normalize_model_id("gpt-5.1-İ-İ-İ"), "gpt-5.1-i̇-i̇-i̇");
        // 带日期后缀的非 ASCII 名照旧剥后缀
        assert_eq!(normalize_model_id("gpt-5.2-中文-2025-06-09"), "gpt-5.2-中文");
        // 档位解析链路同样不能炸
        assert_eq!(resolve_effort("gpt-5-中文测试", "high", None).as_deref(), Some("high"));
    }

    #[test]
    fn supported_lists_by_family() {
        // gpt-5 原代：无 none
        assert_eq!(supported_efforts("gpt-5"), GPT_5_EFFORTS.to_vec().as_slice());
        // gpt-5.1+ 有 none
        assert!(supported_efforts("gpt-5.1").contains(&"none"));
        // gpt-5.2+ 有 xhigh
        assert!(supported_efforts("gpt-5.2").contains(&"xhigh"));
        // gpt-5.6 有 max
        assert!(supported_efforts("gpt-5.6-mini").contains(&"max"));
        // 未知模型走通用三档
        assert_eq!(supported_efforts("some-vendor-model"), GENERIC_EFFORTS.to_vec().as_slice());
    }

    #[test]
    fn resolves_with_downgrade() {
        assert_eq!(resolve_effort("gpt-5", "high", None).as_deref(), Some("high"));
        // xhigh 在 gpt-5 上降为 high
        assert_eq!(resolve_effort("gpt-5", "xhigh", None).as_deref(), Some("high"));
        // off：gpt-5 无 none → None（不发参数）；gpt-5.1+ → "none"
        assert_eq!(resolve_effort("gpt-5", "off", None), None);
        assert_eq!(resolve_effort("gpt-5.1", "off", None).as_deref(), Some("none"));
        // fallback_map 命中但通用三档不含 none：关不掉 → None（上游同款，宁缺勿错）
        let mut m = std::collections::HashMap::new();
        m.insert("off".to_string(), "none".to_string());
        assert_eq!(resolve_effort("custom-model", "off", Some(&m)), None);
        // max → xhigh 降一级（gpt-5.2 有 xhigh）
        assert_eq!(resolve_effort("gpt-5.2", "max", None).as_deref(), Some("xhigh"));
        assert_eq!(resolve_effort("gpt-5.6", "max", None).as_deref(), Some("max"));
    }

    #[test]
    fn temperature_guard_for_gpt56() {
        assert!(!supports_temperature("gpt-5.6"));
        assert!(!supports_temperature("gpt-5.6-2026-01-01"));
        assert!(supports_temperature("gpt-5.5"));
        assert!(supports_temperature("claude-sonnet-4-5"));
    }
}
