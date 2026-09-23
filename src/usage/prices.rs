//! 价格表（施工单 030，P21 成本台账）—— `prices.json`（app_root，与 providers.json 同规）。
//!
//! 形状：`{"prices": [{provider, model, effective_from, prompt, completion, cached}]}`，
//! 单价一律 **美元 / 1M tokens**；缓存命中 token 走 `cached` 单价（多数厂商缓存输入更便宜）。
//!
//! 匹配规则（窄而刚，两条）：
//!
//! 1. `provider` 命中记录里的渠道 host **或** 预设 id，`*` 通配；
//! 2. `model` 精确相等、`前缀*` 前缀匹配或 `*` 通配。
//!
//! 多条命中时取 **`effective_from ≤ 记录时间` 的最新一条**（价格表带生效时间的语义）；
//! 没有任何命中 = 该条记录未计价（报表按「未计价请求数」显式呈现，不静默当 0 元）。

use std::path::Path;

use serde::Deserialize;

use crate::observability::{try_event, EventType};

/// 一条价格（某一供应商/模型自 effective_from 起的单价）。
#[derive(Debug, Clone, Deserialize)]
pub struct PricePoint {
    /// 渠道 host 或预设 id；`*` = 任意供应商
    #[serde(default = "star")]
    pub provider: String,
    /// 模型名；支持 `前缀*` 与 `*`
    #[serde(default = "star")]
    pub model: String,
    /// 生效时间（ISO 日期或时间戳；与记录 ts 同口径按字符串比较）
    #[serde(default)]
    pub effective_from: String,
    /// 输入单价（美元 / 1M tokens，未命中缓存部分）
    pub prompt: f64,
    /// 输出单价（美元 / 1M tokens）
    #[serde(default)]
    pub completion: f64,
    /// 缓存命中输入单价（美元 / 1M tokens；缺省 0）
    #[serde(default)]
    pub cached: f64,
}

fn star() -> String {
    "*".to_string()
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PriceTable {
    #[serde(default)]
    pub prices: Vec<PricePoint>,
}

/// 计价目标：一条记录的「谁、什么时候、哪个模型」（价格匹配用）。
pub struct PriceWho<'a> {
    /// 渠道 host
    pub provider: &'a str,
    /// 供应商预设 id（可空）
    pub provider_id: &'a str,
    pub model: &'a str,
    /// 记录时间（RFC3339；按它取当时有效的价格）
    pub ts: &'a str,
}

impl PriceTable {
    /// 读价格表文件；不存在/读不动 = 空表（全部记录未计价，报表显式说明）。
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else { return Self::default() };
        match serde_json::from_str::<PriceTable>(&text) {
            Ok(t) => t,
            Err(e) => {
                try_event(
                    EventType::Error,
                    serde_json::json!({ "kind": "usage.prices_invalid", "path": path.display().to_string(), "error": e.to_string() }),
                );
                Self::default()
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.prices.is_empty()
    }

    /// 取该记录适用的价格点（生效时间最新的一条）；没有命中返回 None。
    pub fn lookup(&self, who: &PriceWho<'_>) -> Option<&PricePoint> {
        self.prices
            .iter()
            .filter(|p| match_provider(&p.provider, who.provider, who.provider_id) && match_model(&p.model, who.model))
            .filter(|p| p.effective_from.as_str() <= who.ts)
            .max_by(|a, b| a.effective_from.cmp(&b.effective_from))
    }

    /// 算费用（美元）；未命中价格表返回 None。
    /// 缓存 token 是 prompt_tokens 的子集（OpenAI/DeepSeek 语义）：未命中部分按 prompt 单价。
    pub fn cost(&self, tokens: (u64, u64, u64), who: &PriceWho<'_>) -> Option<f64> {
        let p = self.lookup(who)?;
        let (prompt, completion, cached) = tokens;
        let cached = cached.min(prompt);
        let billable_prompt = prompt - cached;
        let usd = (billable_prompt as f64 * p.prompt + cached as f64 * p.cached + completion as f64 * p.completion) / 1_000_000.0;
        Some(usd)
    }
}

/// provider 匹配：`*` 通配；否则相等命中 host 或预设 id 任一。
fn match_provider(pattern: &str, provider: &str, provider_id: &str) -> bool {
    pattern == "*" || pattern == provider || (!provider_id.is_empty() && pattern == provider_id)
}

/// model 匹配：`*` 通配；`前缀*` 前缀匹配；其余精确相等。
fn match_model(pattern: &str, model: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return model.starts_with(prefix);
    }
    pattern == model
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(json: &str) -> PriceTable {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn effective_from_picks_latest_applicable() {
        let t = table(r#"{"prices":[
            {"provider":"api.deepseek.com","model":"deepseek-chat","effective_from":"2026-01-01","prompt":0.27,"completion":1.10,"cached":0.027},
            {"provider":"api.deepseek.com","model":"deepseek-chat","effective_from":"2026-06-01","prompt":0.30,"completion":1.20,"cached":0.03}
        ]}"#);
        // 旧时间用旧价
        let old = t.lookup(&PriceWho { provider: "api.deepseek.com", provider_id: "deepseek", model: "deepseek-chat", ts: "2026-03-01T00:00:00.000Z" }).unwrap();
        assert_eq!(old.prompt, 0.27);
        // 新时间用新价（生效时间的意义）
        let new = t.lookup(&PriceWho { provider: "api.deepseek.com", provider_id: "deepseek", model: "deepseek-chat", ts: "2026-09-01T00:00:00.000Z" }).unwrap();
        assert_eq!(new.prompt, 0.30);
        // 早于所有生效时间 → 未命中
        assert!(t.lookup(&PriceWho { provider: "api.deepseek.com", provider_id: "deepseek", model: "deepseek-chat", ts: "2025-12-31T00:00:00.000Z" }).is_none());
    }

    #[test]
    fn provider_and_model_patterns() {
        let t = table(r#"{"prices":[
            {"provider":"*","model":"gpt-5*","prompt":1.25,"completion":10.0},
            {"provider":"openai","model":"*","prompt":2.5,"completion":10.0}
        ]}"#);
        // 前缀* + provider 通配
        let p = t.lookup(&PriceWho { provider: "api.openai.com", provider_id: "", model: "gpt-5-mini", ts: "2026-09-01T00:00:00.000Z" }).unwrap();
        assert_eq!(p.prompt, 1.25);
        // 精确 model + 预设 id 命中 provider
        let p = t.lookup(&PriceWho { provider: "custom.relay.example", provider_id: "openai", model: "gpt-4o-mini", ts: "2026-09-01T00:00:00.000Z" }).unwrap();
        assert_eq!(p.prompt, 2.5, "provider 可命中预设 id（自定义中转按 id 计价）");
        // 都不命中
        assert!(t.lookup(&PriceWho { provider: "api.deepseek.com", provider_id: "deepseek", model: "deepseek-chat", ts: "2026-09-01T00:00:00.000Z" }).is_none());
    }

    #[test]
    fn cost_splits_cached_tokens() {
        let t = table(r#"{"prices":[{"provider":"*","model":"*","prompt":1.0,"completion":2.0,"cached":0.1}]}"#);
        // 1000 prompt（其中 400 命中缓存）+ 500 completion，单价美元/1M：
        // (600*1.0 + 400*0.1 + 500*2.0)/1M = 1640/1M = 0.00164
        let c = t.cost((1000, 500, 400), &PriceWho { provider: "h", provider_id: "p", model: "m", ts: "2026-09-01T00:00:00.000Z" }).unwrap();
        assert!((c - 0.00164).abs() < 1e-12, "{c}");
        // 100 万 token 量级：1M prompt（全命中缓存）+ 0 completion = 0.1 美元
        let c = t.cost((1_000_000, 0, 1_000_000), &PriceWho { provider: "h", provider_id: "p", model: "m", ts: "2026-09-01T00:00:00.000Z" }).unwrap();
        assert!((c - 0.1).abs() < 1e-9, "{c}");
        // 缓存数异常大于 prompt（坏渠道数据）时夹紧到 prompt，不出现负价：
        // 100 prompt 全按 cached 单价 = 100*0.1/1M = 0.00001
        let c = t.cost((100, 0, 9999), &PriceWho { provider: "h", provider_id: "p", model: "m", ts: "2026-09-01T00:00:00.000Z" }).unwrap();
        assert!((c - 0.00001).abs() < 1e-12, "全部按 cached 单价: {c}");
        // 未命中价格表 → None（未计价）
        let empty = PriceTable::default();
        assert!(empty.cost((1, 1, 0), &PriceWho { provider: "h", provider_id: "p", model: "m", ts: "2026-09-01T00:00:00.000Z" }).is_none());
    }

    #[test]
    fn missing_file_is_empty_table() {
        let t = PriceTable::load(Path::new("definitely-not-exist-prices.json"));
        assert!(t.is_empty());
    }
}
