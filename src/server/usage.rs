//! 用量台账报表与价格设置（自 server.rs 按域拆出；施工单 030 新增成本台账端点；
//! 插件化：报表按会话过滤供「用量价格」插件面板用，价格表经 settings.json 读写）

use super::*;

/// GET /api/usage?group=provider|model&range=today|7d|30d|all&session=<id>
/// —— 台账聚合报表。数字全部来自 `<log_dir>/usage/ledger.jsonl`（网关单点记录，费用已冻结）。
/// `session` 非空 = 只算该会话（「用量价格」插件的「当前对话」口径）；缺省 = 全量。
pub(super) async fn get_usage(State(state): State<AppState>, Query(q): Query<UsageQuery>) -> Json<Value> {
    // 缺省口径：按供应商、看全部（面板自己传 session + range）
    let group = if q.group.trim().is_empty() { crate::usage::GROUP_PROVIDER.to_string() } else { q.group };
    let range = if q.range.trim().is_empty() { "all".to_string() } else { q.range };
    let report = crate::usage::report(&group, &range, &q.session);
    // 币种与汇率是展示层设置（settings.cost）：金额一律美元存，界面按它折算
    let cost = state.settings.read().unwrap_or_else(|e| e.into_inner()).cost.clone();
    Json(json!({
        "group": report.group,
        "range": report.range,
        "session": report.session,
        "from": report.from,
        "rows": report.rows,
        "totals": report.totals,
        "spend_cap_usd": report.spend_cap_usd,
        "spend_total_usd": report.spend_total_usd,
        "priced": report.priced,
        "currency": cost.currency,
        "usd_cny_rate": cost.usd_cny_rate,
    }))
}

#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    /// provider（默认）| model
    #[serde(default)]
    pub group: String,
    /// today | 7d | 30d | all（默认 all）
    #[serde(default)]
    pub range: String,
    /// 会话过滤：传了只算该会话（面板的「当前对话」口径）；缺省 = 全量
    #[serde(default)]
    pub session: String,
}

/// GET /api/usage/prices —— 价格设置页要的一切：现有价格表、可选渠道、币种与汇率、累计/上限。
pub(super) async fn get_usage_prices(State(state): State<AppState>) -> Json<Value> {
    let s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    // 渠道清单：从现有配置带出（名称 + host + 模型），价格键就是 host + 模型 id。
    // models 带该渠道**配置的全部模型**（用户实锤：不只当前选中的那个——一个渠道里
    // 多个模型价目不同，价格页要能按渠道带出模型来选）
    let channels: Vec<Value> = s
        .configs
        .iter()
        .map(|c| {
            let mut models = c.models.clone();
            let active = c.effective_model();
            if !active.is_empty() && !models.iter().any(|m| m == &active) {
                models.insert(0, active.clone());
            }
            json!({
                "name": c.name,
                "host": host_of(&c.effective_base_url()),
                "model": active,
                "models": models,
            })
        })
        .collect();
    Json(json!({
        "prices": s.cost.prices,
        "channels": channels,
        "currency": s.cost.currency,
        "usd_cny_rate": s.cost.usd_cny_rate,
        "has_prices": crate::usage::prices_available(),
        "spend_total_usd": crate::usage::spend_total(),
        "spend_cap_usd": crate::usage::cap_value(),
    }))
}

/// PUT /api/usage/prices {prices, currency, usd_cny_rate} —— 存价格表与币种（落 settings.json，
/// 同步刷新内存价格表——之后**新**请求按新价计价，历史账不动）。
pub(super) async fn put_usage_prices(
    State(state): State<AppState>,
    Json(req): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let err = |m: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": m })));
    let parsed: Vec<crate::config::settings::ModelPrice> = match req.get("prices") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(p) => p,
            Err(e) => return err(format!("prices 不合法: {e}")),
        },
        None => Vec::new(),
    };
    let mut prices = parsed;
    // 清洗：host/model 去空白，单价负数当 0；空行丢弃（不静默留脏数据）
    prices.retain(|p| !p.provider.trim().is_empty() && !p.model.trim().is_empty());
    for p in &mut prices {
        p.provider = p.provider.trim().to_string();
        p.model = p.model.trim().to_string();
        p.prompt = p.prompt.max(0.0);
        p.cached = p.cached.max(0.0);
        p.completion = p.completion.max(0.0);
    }
    let currency = match req.get("currency").and_then(Value::as_str) {
        Some("CNY") => "CNY".to_string(),
        _ => "USD".to_string(),
    };
    let rate = req.get("usd_cny_rate").and_then(Value::as_f64).unwrap_or(7.2);
    let rate = if rate > 0.0 { rate } else { 7.2 };

    // 2026-10-08 修：settings 写互斥（读-改-写整条链路持锁，防并发写者互覆盖）
    let _w = state.settings_write.lock().unwrap_or_else(|e| e.into_inner());
    let mut s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    s.cost.prices = prices.clone();
    s.cost.currency = currency;
    s.cost.usd_cny_rate = rate;
    if let Err(e) = s.save() {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })));
    }
    crate::usage::install_prices(&s.cost.prices);
    *state.settings.write().unwrap_or_else(|e| e.into_inner()) = s.clone();
    // 2026-10-02：保存价格即全量重算历史——用户实锤「设了价，历史消耗还是 0/未计价」
    // （必须再手动点一次重算按钮，没人知道要点）。全表价格按新价覆盖历史记录的费用，
    // 个别 host+model 没配价的仍按未计价计。
    let recompute = crate::usage::recompute().ok();
    observability::event(
        EventType::State,
        json!({ "kind": "usage.prices_saved", "rows": prices.len(), "currency": s.cost.currency,
               "recomputed": recompute.as_ref().map(|(n, u)| json!({ "records": n, "unpriced": u })) }),
    );
    (StatusCode::OK, Json(json!({
        "ok": true,
        "prices": prices,
        "recomputed": recompute.as_ref().map(|(n, u)| json!({ "records": n, "unpriced": u })),
    })))
}

/// POST /api/usage/recompute —— 用当前价格表重算全部历史记录的费用（配错价后的显式补救）。
pub(super) async fn post_usage_recompute() -> (StatusCode, Json<Value>) {
    match crate::usage::recompute() {
        Ok((n, unpriced)) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "records": n, "unpriced": unpriced, "spend_total_usd": crate::usage::spend_total() })),
        ),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))),
    }
}

/// base_url → host（价格表的键；与网关记账同口径）。
fn host_of(base_url: &str) -> String {
    let b = base_url.trim();
    let after_scheme = b.split_once("://").map(|(_, r)| r).unwrap_or(b);
    after_scheme.split('/').next().unwrap_or(after_scheme).to_string()
}
