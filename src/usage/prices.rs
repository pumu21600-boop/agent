//! 模型价格表（施工单 030，P21）—— 存 **settings.json**（美元 / 1M tokens 规范存储），
//! 由「用量」插件的价格设置页维护；台账与花费硬闸都按这个口径算。
//!
//! 两条硬规则：
//! 1. **键是「供应商 host + 模型 id」两元组**——同模型 id 不同供应商是两行，靠 host 区分，
//!    绝不只看模型名（第三方中转同 id 不同价是常态，用户明确要求不能搞混）；
//! 2. **费用在记账时算定、冻进台账记录**（`cost_usd`）——之后改价只影响新请求，
//!    历史账一分不动（换来价格界面的简洁：一行一条，没有生效时间/版本历史）。
//!    要按新价改历史账，用价格页的「重算」显式触发（usage::recompute）。
//!
//! 匹配优先级（窄而刚，四级）：host+model 精确 > `*`+model > host+`*` > `*`+`*`。

use crate::config::settings::ModelPrice;

/// 取这条记录适用的价格（精确优先于通配）。
pub fn lookup<'a>(prices: &'a [ModelPrice], provider: &str, model: &str) -> Option<&'a ModelPrice> {
    let hit = |p: &str, m: &str| prices.iter().find(|e| e.provider == p && e.model == m);
    hit(provider, model)
        .or_else(|| hit("*", model))
        .or_else(|| hit(provider, "*"))
        .or_else(|| hit("*", "*"))
}

/// 算费用（美元）；没有命中任何价格行返回 None = 未计价（不静默当 0 元）。
/// 缓存 token 是 prompt_tokens 的子集（OpenAI/DeepSeek 语义）：未命中部分按 prompt 单价。
pub fn cost(prices: &[ModelPrice], tokens: (u64, u64, u64), provider: &str, model: &str) -> Option<f64> {
    let p = lookup(prices, provider, model)?;
    let (prompt, completion, cached) = tokens;
    let cached = cached.min(prompt);
    let billable_prompt = prompt - cached;
    Some((billable_prompt as f64 * p.prompt + cached as f64 * p.cached + completion as f64 * p.completion) / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::ModelPrice;

    fn row(provider: &str, model: &str, prompt: f64, cached: f64, completion: f64) -> ModelPrice {
        ModelPrice { provider: provider.into(), model: model.into(), prompt, cached, completion }
    }

    #[test]
    fn same_model_id_on_two_providers_never_mix() {
        // 两家渠道都有 step-5-preview，单价不同——按 host 区分，绝不能搞混
        let prices = vec![
            row("api.stepfun.com", "step-5-preview", 1.0, 0.1, 2.0),
            row("relay.example.com", "step-5-preview", 0.5, 0.05, 1.0),
        ];
        let a = cost(&prices, (1_000_000, 0, 0), "api.stepfun.com", "step-5-preview").unwrap();
        let b = cost(&prices, (1_000_000, 0, 0), "relay.example.com", "step-5-preview").unwrap();
        assert!((a - 1.0).abs() < 1e-9, "StepFun 应 1.0，实际 {a}");
        assert!((b - 0.5).abs() < 1e-9, "中转应 0.5，实际 {b}");
    }

    #[test]
    fn wildcard_fallback_priority() {
        let prices = vec![
            row("*", "m1", 9.0, 0.0, 0.0),     // 任意供应商 + 指定模型
            row("h.com", "*", 5.0, 0.0, 0.0),  // 指定供应商 + 任意模型
            row("*", "*", 1.0, 0.0, 0.0),      // 全局兜底
            row("h.com", "m1", 2.0, 0.0, 0.0), // 精确（应赢）
        ];
        let exact = cost(&prices, (1_000_000, 0, 0), "h.com", "m1").unwrap();
        assert!((exact - 2.0).abs() < 1e-9, "精确匹配应赢，实际 {exact}");
        let by_model = cost(&prices, (1_000_000, 0, 0), "other.com", "m1").unwrap();
        assert!((by_model - 9.0).abs() < 1e-9, "* + 指定模型应次之，实际 {by_model}");
        let by_provider = cost(&prices, (1_000_000, 0, 0), "h.com", "mX").unwrap();
        assert!((by_provider - 5.0).abs() < 1e-9, "指定供应商 + * 应再次，实际 {by_provider}");
        let global = cost(&prices, (1_000_000, 0, 0), "zz.com", "mZ").unwrap();
        assert!((global - 1.0).abs() < 1e-9, "全局兜底，实际 {global}");
    }

    #[test]
    fn cost_splits_cached_and_clamps() {
        let prices = vec![row("*", "*", 1.0, 0.1, 2.0)];
        // 1000 prompt（400 缓存命中）+ 500 completion = (600*1 + 400*0.1 + 500*2)/1M
        let c = cost(&prices, (1000, 500, 400), "h", "m").unwrap();
        assert!((c - 0.00164).abs() < 1e-12, "{c}");
        // 坏数据：缓存数大于 prompt → 夹紧，不出负价
        let c = cost(&prices, (100, 0, 9999), "h", "m").unwrap();
        assert!((c - 0.00001).abs() < 1e-12, "{c}");
    }

    #[test]
    fn no_match_is_unpriced_not_zero() {
        assert!(cost(&[], (1, 1, 0), "h", "m").is_none(), "空价格表 = 未计价");
        assert!(cost(&[row("a.com", "m", 1.0, 0.0, 0.0)], (1, 0, 0), "b.com", "m").is_none());
    }
}
