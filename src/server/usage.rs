//! 用量台账报表（P19 自 server.rs 按域拆出；P21 施工单 030 新增成本台账端点）

use super::*;

/// GET /api/usage?group=provider|model&range=today|7d|30d|all —— 成本台账聚合报表。
/// 数字全部来自 `<log_dir>/usage/ledger.jsonl`（网关单点记录），费用按 prices.json 当时有效价。
pub(super) async fn get_usage(Query(q): Query<UsageQuery>) -> Json<Value> {
    // 缺省口径：按供应商、看今天
    let group = if q.group.trim().is_empty() { crate::usage::GROUP_PROVIDER.to_string() } else { q.group };
    let range = if q.range.trim().is_empty() { "today".to_string() } else { q.range };
    let report = crate::usage::report(&group, &range);
    Json(json!({
        "group": report.group,
        "range": report.range,
        "from": report.from,
        "rows": report.rows,
        "totals": report.totals,
        "spend_cap_usd": report.spend_cap_usd,
        "spend_total_usd": report.spend_total_usd,
        "priced": report.priced,
        "prices_file": crate::config::app_root().join("prices.json").display().to_string(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    /// provider（默认）| model
    #[serde(default)]
    pub group: String,
    /// today（默认）| 7d | 30d | all
    #[serde(default)]
    pub range: String,
}
