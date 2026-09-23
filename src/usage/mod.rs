//! 用量台账（施工单 030，P21）—— 每一次模型调用的计量、计价、聚合与花费硬闸。
//!
//! 定位：**计量收口在网关单点**（`Gateway::stream_chat` / `complete` 是全部模型流量的唯一
//! 出入口），业务代码零埋点。台账落 `<log_dir>/usage/ledger.jsonl`（追加写 + flush，
//! 与 JSONL 日志同规：落盘即完整），超 32MB 归档一份 `.1`。
//!
//! 三条硬语义：
//! 1. **分计量**：prompt / completion / cache 三路分开记；渠道不回 usage 的调用 token 记 0，
//!    但请求与延迟照记——报表按「未计价请求数」区分「渠道没给用量」与「价格表没这条」。
//! 2. **价格表带生效时间**：费用按记录发生时间取当时有效的价格（见 prices.rs），
//!    调价不改历史账。
//! 3. **全局花费上限是硬闸**（红线 1）：累计花费达 `[cost] spend_cap_usd` 即拒绝新的模型
//!    请求（主流程与辅助调用同一道闸），调高上限即恢复，不锁死数据。
//!
//! P22 衔接：`parent_session` 字段 v1 恒为 None（子代理在 P22）；子会话填父任务 id 后
//! 报表按父任务聚合即可，无需数据迁移（docs/subagent-draft.md 第 4 节）。

use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::observability::{try_event, EventType};

pub mod prices;

/// 台账单文件上限（字节）：超过归档成 `ledger.jsonl.1`（只留一份，防无限增长）。
const LEDGER_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// 写满这么多条复查一次体积（避免每条都 stat）。
const ROTATE_CHECK_EVERY: u64 = 512;

/// 一次模型调用的台账记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    /// 记录时间（RFC3339 毫秒，UTC）
    pub ts: String,
    /// 归属会话
    pub session_id: String,
    /// P22 子代理花销记父任务：子会话填父任务 id；主会话为 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// 用途：main（主循环）| summary（滚动摘要）| title（标题）| media（媒体旁路）
    pub purpose: String,
    /// 渠道 host（报表按供应商分组的键）
    pub provider: String,
    /// 供应商预设 id（自定义中转时为空；价格表可按它命中）
    #[serde(default)]
    pub provider_id: String,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    /// ok（成功）| error（失败）| cancelled（取消）
    pub status: String,
    pub latency_ms: u64,
    /// 失败原因（截断；绝不含 key）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UsageRecord {
    /// 便捷构造（时间戳取现在；token 与状态由调用方填）。
    pub fn new(ctx_session: &str, purpose: &str, provider: &str, provider_id: &str, model: &str) -> Self {
        Self {
            ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            session_id: ctx_session.to_string(),
            parent_session: None,
            purpose: purpose.to_string(),
            provider: provider.to_string(),
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            status: "ok".to_string(),
            latency_ms: 0,
            error: None,
        }
    }
}

/// 台账文件句柄（进程内单例）。
struct Ledger {
    file: Mutex<File>,
    path: PathBuf,
    bytes: AtomicU64,
    writes: AtomicU64,
}

static LEDGER: OnceLock<Ledger> = OnceLock::new();
/// 累计花费（美元，f64 存 bits）：启动时扫台账装入，之后随记录增量更新。
static SPEND: AtomicU64 = AtomicU64::new(0);
/// 全局花费上限（美元；0 = 不限）。config.toml [cost] spend_cap_usd。
static CAP: RwLock<f64> = RwLock::new(0.0);
/// 价格表（启动时从 prices.json 装入；测试可替换）。
static PRICES: RwLock<prices::PriceTable> = RwLock::new(prices::PriceTable { prices: Vec::new() });

impl Ledger {
    fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("ledger.jsonl");
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self { file: Mutex::new(file), path, bytes: AtomicU64::new(bytes), writes: AtomicU64::new(0) })
    }

    /// 追加一条（单行 JSON + flush；落盘即完整）。
    fn append(&self, line: &str) {
        let mut full = line.to_string();
        full.push('\n');
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(full.as_bytes());
            let _ = f.flush();
            let n = self.bytes.fetch_add(full.len() as u64, Ordering::Relaxed) + full.len() as u64;
            let w = self.writes.fetch_add(1, Ordering::Relaxed) + 1;
            if n > LEDGER_MAX_BYTES && w.is_multiple_of(ROTATE_CHECK_EVERY.min(64)) {
                self.rotate(&mut f);
            }
        }
    }

    /// 超限归档：当前文件 → `ledger.jsonl.1`（只留一份），重开新文件。
    fn rotate(&self, f: &mut File) {
        let backup = PathBuf::from(format!("{}.1", self.path.display()));
        if std::fs::rename(&self.path, &backup).is_ok()
            && let Ok(nf) = OpenOptions::new().create(true).append(true).open(&self.path)
        {
            *f = nf;
            self.bytes.store(0, Ordering::Relaxed);
            try_event(
                EventType::State,
                json!({ "kind": "usage.ledger_rotated", "limit_bytes": LEDGER_MAX_BYTES }),
            );
        }
    }

    /// 读全部记录（坏行跳过；报表口径）。
    fn read_all(&self) -> Vec<UsageRecord> {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return Vec::new() };
        text.lines().filter_map(|l| serde_json::from_str::<UsageRecord>(l).ok()).collect()
    }
}

/// 初始化台账（main.rs 启动时调一次；目录不存在则建）。
pub fn init(dir: &Path) {
    let ledger = match Ledger::open(dir) {
        Ok(l) => l,
        Err(e) => {
            try_event(
                EventType::Error,
                json!({ "kind": "usage.ledger_init_failed", "dir": dir.display().to_string(), "error": e.to_string() }),
            );
            return;
        }
    };
    // 启动时扫一遍：装入累计花费（硬闸口径 = 台账全量）
    let records = ledger.read_all();
    let mut total = 0.0f64;
    let mut unpriced = 0u64;
    for r in &records {
        match cost_of(r) {
            Some(c) => total += c,
            None => unpriced += 1,
        }
    }
    SPEND.store(total.to_bits(), Ordering::Relaxed);
    let _ = LEDGER.set(ledger);
    try_event(
        EventType::State,
        json!({ "kind": "usage.ledger_ready", "records": records.len(), "spend_usd": total, "unpriced": unpriced }),
    );
}

/// 装入价格表（main.rs 从 app_root/prices.json 调；测试可直接替换）。
pub fn install_prices(table: prices::PriceTable) {
    if let Ok(mut t) = PRICES.write() {
        *t = table;
    }
}

/// 从文件装价格表（不存在 = 空表）。
pub fn install_prices_file(path: &Path) {
    install_prices(prices::PriceTable::load(path));
}

/// 设置全局花费上限（美元；0 = 不限）。
pub fn set_cap(usd: f64) {
    if let Ok(mut c) = CAP.write() {
        *c = usd.max(0.0);
    }
}

pub fn cap_value() -> f64 {
    CAP.read().map(|c| *c).unwrap_or(0.0)
}

/// 累计花费（美元，台账全量口径）。
pub fn spend_total() -> f64 {
    f64::from_bits(SPEND.load(Ordering::Relaxed))
}

/// 硬闸：超限返回人话原因（未超 / 未启用返回 None）。
pub fn cap_block() -> Option<String> {
    let cap = cap_value();
    if cap <= 0.0 {
        return None;
    }
    let total = spend_total();
    if total >= cap {
        return Some(format!(
            "已触发全局花费上限：累计花费 ${total:.4} 已达到上限 ${cap:.2}（config.toml [cost] spend_cap_usd）。调高上限后才能继续调用模型。"
        ));
    }
    None
}

/// 记一条台账（未初始化 = 静默跳过，测试/早期启动不 panic）。
pub fn record(rec: UsageRecord) {
    let Some(ledger) = LEDGER.get() else { return };
    // 费用按记录时间取当时有效的价格；未命中价格表 = 未计价（累计值不加）
    if let Some(c) = cost_of(&rec) {
        let cur = f64::from_bits(SPEND.load(Ordering::Relaxed));
        SPEND.store((cur + c).to_bits(), Ordering::Relaxed);
    }
    let line = match serde_json::to_string(&rec) {
        Ok(s) => s,
        Err(_) => return,
    };
    ledger.append(&line);
}

/// 分组口径。
pub const GROUP_PROVIDER: &str = "provider";
pub const GROUP_MODEL: &str = "model";

/// 一个分组 / 总计的聚合结果。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Agg {
    pub key: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub requests: u64,
    pub errors: u64,
    pub cancelled: u64,
    pub cost_usd: f64,
    /// 价格表没命中、无法计价的请求数（不等于 0 花费的请求）
    pub unpriced_requests: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub sessions: usize,
}

/// 报表（按 group 分组 + 一行总计）。
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub group: String,
    pub range: String,
    /// 时间下界（ISO；all = None）
    pub from: Option<String>,
    pub rows: Vec<Agg>,
    pub totals: Agg,
    pub spend_cap_usd: f64,
    pub spend_total_usd: f64,
    /// 价格表是否可用（false = 全部记录未计价，界面要显式说明）
    pub priced: bool,
}

/// 累计中的一组（分位与去重会话要留原始数据，输出前再算）。
#[derive(Default)]
struct Acc {
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    requests: u64,
    errors: u64,
    cancelled: u64,
    cost_usd: f64,
    unpriced: u64,
    latencies: Vec<u64>,
    sessions: HashSet<String>,
}

impl Acc {
    fn add(&mut self, r: &UsageRecord, cost: Option<f64>) {
        self.prompt_tokens += r.prompt_tokens;
        self.completion_tokens += r.completion_tokens;
        self.cached_tokens += r.cached_tokens;
        self.requests += 1;
        match r.status.as_str() {
            "error" => self.errors += 1,
            "cancelled" => self.cancelled += 1,
            _ => {}
        }
        match cost {
            Some(c) => self.cost_usd += c,
            None => self.unpriced += 1,
        }
        self.latencies.push(r.latency_ms);
        if !r.session_id.is_empty() {
            self.sessions.insert(r.session_id.clone());
        }
    }

    fn finish(&self, key: &str) -> Agg {
        let mut lat = self.latencies.clone();
        lat.sort_unstable();
        Agg {
            key: key.to_string(),
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            cached_tokens: self.cached_tokens,
            requests: self.requests,
            errors: self.errors,
            cancelled: self.cancelled,
            cost_usd: self.cost_usd,
            unpriced_requests: self.unpriced,
            p50_ms: percentile(&lat, 0.5),
            p95_ms: percentile(&lat, 0.95),
            sessions: self.sessions.len(),
        }
    }
}

/// 分位（已排序数组；空数组返回 0）。
fn percentile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// 时间范围 → 下界（UTC；all = None）。
fn range_from(range: &str) -> Option<DateTime<Utc>> {
    let now = Utc::now();
    match range {
        "today" => now.date_naive().and_hms_opt(0, 0, 0)?.and_utc().into(),
        "7d" => now.checked_sub_signed(Duration::days(7)),
        "30d" => now.checked_sub_signed(Duration::days(30)),
        _ => None, // all
    }
}

/// 聚合报表。group = provider | model；range = today | 7d | 30d | all。
pub fn report(group: &str, range: &str) -> Report {
    let group = if group == GROUP_MODEL { GROUP_MODEL } else { GROUP_PROVIDER };
    let from = range_from(range);
    let records = LEDGER.get().map(|l| l.read_all()).unwrap_or_default();
    let mut groups: HashMap<String, Acc> = HashMap::new();
    let mut totals = Acc::default();
    let mut unpriced_total = 0u64;
    for r in &records {
        if let Some(f) = from {
            let Ok(ts) = DateTime::parse_from_rfc3339(&r.ts) else { continue };
            if ts.with_timezone(&Utc) < f {
                continue;
            }
        }
        let cost = cost_of(r);
        if cost.is_none() {
            unpriced_total += 1;
        }
        let key = if group == GROUP_MODEL {
            if r.model.is_empty() { "（未知模型）".to_string() } else { r.model.clone() }
        } else if r.provider.is_empty() {
            "（未知渠道）".to_string()
        } else {
            r.provider.clone()
        };
        totals.add(r, cost);
        groups.entry(key).or_default().add(r, cost);
    }
    let mut rows: Vec<Agg> = groups.iter().map(|(k, a)| a.finish(k)).collect();
    // 花费降序（看谁在花钱），同额按 token 降序
    rows.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.prompt_tokens.cmp(&a.prompt_tokens))
    });
    Report {
        group: group.to_string(),
        range: range.to_string(),
        from: from.map(|f| f.to_rfc3339_opts(SecondsFormat::Secs, true)),
        rows,
        totals: totals.finish(""),
        spend_cap_usd: cap_value(),
        spend_total_usd: spend_total(),
        priced: prices_available() && unpriced_total == 0,
    }
}

/// 算一条记录的费用（美元；未命中价格表 = None = 未计价）。
/// 价格按**记录发生时间**取（effective_from 语义），调价不改历史账。
pub fn cost_of(rec: &UsageRecord) -> Option<f64> {
    let t = PRICES.read().ok()?;
    let who = prices::PriceWho {
        provider: &rec.provider,
        provider_id: &rec.provider_id,
        model: &rec.model,
        ts: &rec.ts,
    };
    t.cost((rec.prompt_tokens, rec.completion_tokens, rec.cached_tokens), &who)
}

/// 价格表是否可用（空表 = 全部记录未计价，界面要显式说明）。
pub fn prices_available() -> bool {
    PRICES.read().map(|t| !t.is_empty()).unwrap_or(false)
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 台账是进程级单例、价格表/上限是全局态：本文件测试全部串行（一把锁），
    /// 且每个用例先确保台账已初始化（未初始化时 record 静默丢弃）。
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn ledger() -> &'static Ledger {
        LEDGER.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("agent-usage-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Ledger::open(&dir).unwrap()
        })
    }

    /// 全通配价格表：prompt 1.0 / completion 2.0 / cached 0.1 美元每 1M tokens。
    fn prices_star() {
        install_prices(prices::PriceTable {
            prices: vec![prices::PricePoint {
                provider: "*".into(),
                model: "*".into(),
                effective_from: "2020-01-01".into(),
                prompt: 1.0,
                completion: 2.0,
                cached: 0.1,
            }],
        });
    }

    fn rec(provider: &str, model: &str, prompt: u64, completion: u64, cached: u64, status: &str, latency: u64) -> UsageRecord {
        let mut r = UsageRecord::new("sess-test", "main", provider, "", model);
        r.prompt_tokens = prompt;
        r.completion_tokens = completion;
        r.cached_tokens = cached;
        r.status = status.to_string();
        r.latency_ms = latency;
        r
    }

    #[test]
    fn record_and_report_aggregate_by_provider() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ledger();
        prices_star();
        let before = spend_total();
        // a1：1000 prompt（400 缓存命中）+ 500 completion
        //   = (600*1.0 + 400*0.1 + 500*2.0)/1M = 0.00164
        record(rec("api.a.com", "m1", 1000, 500, 400, "ok", 100));
        // a2：2000 prompt 全未命中，失败调用也记账（失败率口径）
        //   = 2000*1.0/1M = 0.002
        record(rec("api.a.com", "m1", 2000, 0, 0, "error", 300));
        // a3：渠道不回 usage → token 0，请求与延迟照记，计 0 元
        record(rec("api.a.com", "m1", 0, 0, 0, "ok", 200));
        // b1：取消调用也记账（不计入失败率）
        //   = (500*1.0 + 100*2.0)/1M = 0.0007
        record(rec("api.b.com", "m2", 500, 100, 0, "cancelled", 50));
        let after = spend_total();
        let delta = after - before;
        assert!((delta - 0.00434).abs() < 1e-9, "累计应加 0.00434 美元，实际 {delta}");

        let rep = report(GROUP_PROVIDER, "all");
        let a = rep.rows.iter().find(|r| r.key == "api.a.com").expect("应有 api.a.com 行");
        assert_eq!(a.requests, 3);
        assert_eq!(a.errors, 1, "失败数只数 error（cancelled 单列）");
        assert_eq!(a.prompt_tokens, 3000);
        assert_eq!(a.completion_tokens, 500);
        assert_eq!(a.cached_tokens, 400, "缓存 token 分路计量");
        assert_eq!(a.sessions, 1);
        assert_eq!(a.p50_ms, 200, "三条延迟 [100,200,300] 的 p50");
        assert_eq!(a.p95_ms, 300);
        let b = rep.rows.iter().find(|r| r.key == "api.b.com").expect("应有 api.b.com 行");
        assert_eq!(b.cancelled, 1);
        // 台账是进程级共享的（其它用例也往里写），总计按本次两组求和核对
        let mine: u64 = rep
            .rows
            .iter()
            .filter(|r| r.key == "api.a.com" || r.key == "api.b.com")
            .map(|r| r.requests)
            .sum();
        assert_eq!(mine, 4, "本用例记了 4 条");
        assert!(rep.totals.requests >= mine, "总计应覆盖全部记录: {} vs {mine}", rep.totals.requests);
        // 行按花费降序：api.a.com（0.00364）应在 api.b.com（0.0007）前
        let ia = rep.rows.iter().position(|r| r.key == "api.a.com").unwrap();
        let ib = rep.rows.iter().position(|r| r.key == "api.b.com").unwrap();
        assert!(ia < ib, "应按花费降序");
    }

    #[test]
    fn report_groups_by_model_and_counts_unpriced() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ledger();
        // 空价格表 → 全部未计价，但请求/延迟照记（不静默当 0 元）
        install_prices(prices::PriceTable::default());
        record(rec("api.c.com", "mm-x", 10, 5, 0, "ok", 10));
        let rep = report(GROUP_MODEL, "all");
        let row = rep.rows.iter().find(|r| r.key == "mm-x").expect("应按模型分组出行");
        assert_eq!(row.requests, 1);
        assert_eq!(row.unpriced_requests, 1, "价格表没命中 = 未计价");
        assert!(row.cost_usd.abs() < 1e-12, "未计价不计费");
        assert!(!rep.priced, "有未计价记录时 priced=false");
        // 按 model 分组时 provider 键不出现
        assert!(!rep.rows.iter().any(|r| r.key == "api.c.com"));
    }

    #[test]
    fn cap_is_a_hard_gate() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ledger();
        prices_star();
        set_cap(0.0);
        assert!(cap_block().is_none(), "0 = 不限");
        // 造一笔确定的花费：1M prompt × 1.0/M = 1.0 美元
        let before = spend_total();
        record(rec("api.cap.com", "m", 1_000_000, 0, 0, "ok", 1));
        let spent = spend_total() - before;
        assert!((spent - 1.0).abs() < 1e-9, "1M prompt 应花 1.0 美元，实际 {spent}");
        // 上限设在「已花 - 0.5」→ 必超 → 拦
        set_cap(before + spent - 0.5);
        let msg = cap_block().expect("累计已达上限 → 应拦");
        assert!(msg.contains("spend_cap_usd"), "人话要指路配置项: {msg}");
        assert!(msg.contains("上限"), "人话要说清是上限: {msg}");
        // 调高上限 → 立即恢复（不锁死数据）
        set_cap(1_000_000.0);
        assert!(cap_block().is_none());
        set_cap(0.0);
    }

    #[test]
    fn range_filter_today_excludes_old_records() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ledger();
        prices_star();
        let mut old = rec("api.old.com", "m", 1, 1, 0, "ok", 1);
        old.ts = "2020-01-01T00:00:00.000Z".to_string();
        record(old);
        let rep = report(GROUP_PROVIDER, "today");
        assert!(!rep.rows.iter().any(|r| r.key == "api.old.com"), "今天范围不该包含 2020 年的记录");
        assert!(rep.from.is_some(), "today 应带下界");
        let all = report(GROUP_PROVIDER, "all");
        assert!(all.rows.iter().any(|r| r.key == "api.old.com"), "all 范围应包含");
        assert!(all.from.is_none());
    }

    #[test]
    fn instant_latency_is_measured() {
        use std::time::Instant;
        let t = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(t.elapsed().as_millis() >= 5);
    }
}
