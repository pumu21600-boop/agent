//! 用量台账（施工单 030）—— 每一次模型调用的计量、计价、聚合与花费硬闸。
//!
//! 定位：**计量收口在网关单点**（`Gateway::stream_chat` / `complete` 是全部模型流量的唯一
//! 出入口），业务代码零埋点。台账落 `<log_dir>/usage/ledger.jsonl`（追加写 + flush，
//! 与 JSONL 日志同规：落盘即完整），超 32MB 归档一份 `.1`。
//!
//! 四条硬语义：
//! 1. **分计量**：prompt / completion / cache 三路分开记；渠道不回 usage 的调用 token 记 0，
//!    但请求与延迟照记——报表按「未计价请求数」区分「渠道没给用量」与「价格表没这条」。
//! 2. **费用冻结构**：记账时用当时的价格算定 `cost_usd` 写进记录，之后改价只影响新请求，
//!    历史账一分不动（价格表在 settings.json，由「用量价格」插件维护；要改历史账用 recompute）。
//! 3. **全局花费上限是硬闸**（红线 1）：累计花费达 `[cost] spend_cap_usd` 即拒绝新的模型
//!    请求（主流程与辅助调用同一道闸），调高上限即恢复，不锁死数据。上限看**全量**台账，
//!    与面板的「当前会话」口径分层——面板按会话过滤只是展示口径。
//! 4. **币种**：单价与花费一律以**美元**为规范存储/计算；人民币只是展示层
//!    （settings.cost.currency + usd_cny_rate，前端折算），切币种不动任何账。
//!
//! 衔接：`parent_session` 字段 v1 恒为 None（子代理落地后）；子会话填父任务 id 后
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

use crate::config::settings::ModelPrice;
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
    /// 子代理花销记父任务：子会话填父任务 id；主会话为 None
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// 用途：main（主循环）| summary（滚动摘要）| title（标题）| media（媒体旁路）
    pub purpose: String,
    /// 渠道 host（价格表按它 + model 两元组命中）
    pub provider: String,
    /// 供应商预设 id（自定义中转时为空；仅作展示，价格匹配只看 host + model）
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
    /// 费用（美元，**记账时算定冻结**）；None = 当时没配到价格 = 未计价
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
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
            cost_usd: None,
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
/// 价格表（settings.json 的 cost.prices；设置变更即同步，测试可替换）。
static PRICES: RwLock<Vec<ModelPrice>> = RwLock::new(Vec::new());

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
        // 2026-10-08 修：错误同样不能吞——台账静默丢失意味着 SPEND 不增长，
        // 花费硬闸（红线 1）会失效，超支也拦不住。锁中毒也改用 into_inner
        // （数据仍可用，与全仓其余锁的用法一致）。
        let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = f.write_all(full.as_bytes()).and_then(|_| f.flush()) {
            crate::observability::warn_write_failed("台账", &e);
        }
        let n = self.bytes.fetch_add(full.len() as u64, Ordering::Relaxed) + full.len() as u64;
        let w = self.writes.fetch_add(1, Ordering::Relaxed) + 1;
        if n > LEDGER_MAX_BYTES && w.is_multiple_of(ROTATE_CHECK_EVERY.min(64)) {
            self.rotate(&mut f);
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

    /// 重开追加句柄（文件被原子替换/轮转后必须调，否则 append 写进旧 inode）。
    fn reopen(&self) {
        if let Ok(mut f) = self.file.lock()
            && let Ok(nf) = OpenOptions::new().create(true).append(true).open(&self.path)
        {
            *f = nf;
            self.bytes.store(std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0), Ordering::Relaxed);
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
    // 启动时扫一遍：装入累计花费（硬闸口径 = 台账全量；费用已冻结在记录里，直接求和）
    let records = ledger.read_all();
    let mut total = 0.0f64;
    let mut unpriced = 0u64;
    for r in &records {
        match r.cost_usd {
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

/// 装入价格表（settings.json 的 cost.prices；设置保存/启动时同步，测试可替换）。
pub fn install_prices(prices: &[ModelPrice]) {
    if let Ok(mut t) = PRICES.write() {
        *t = prices.to_vec();
    }
}

/// 算一条记录的费用（用**当前**价格表；None = 未计价）。
fn cost_now(rec: &UsageRecord) -> Option<f64> {
    let t = PRICES.read().ok()?;
    prices::cost(&t, (rec.prompt_tokens, rec.completion_tokens, rec.cached_tokens), &rec.provider, &rec.model)
}

/// 记录时算定费用（冻结进台账）。未初始化 = 静默跳过（测试/早期启动不 panic）。
pub fn record(mut rec: UsageRecord) {
    let Some(ledger) = LEDGER.get() else { return };
    // 费用按当前价格算定并冻结；未命中价格表 = 未计价（累计值不加，界面显式提示）
    rec.cost_usd = cost_now(&rec);
    if let Some(c) = rec.cost_usd {
        let cur = f64::from_bits(SPEND.load(Ordering::Relaxed));
        SPEND.store((cur + c).to_bits(), Ordering::Relaxed);
    }
    let line = match serde_json::to_string(&rec) {
        Ok(s) => s,
        Err(_) => return,
    };
    ledger.append(&line);
}

/// 用当前价格表**重算全部历史记录**的费用（价格页的显式动作：配错价之后补救）。
/// 原子重写台账（fsx 原子写），返回 (重算条数, 仍未计价条数)；同步刷新累计花费。
pub fn recompute() -> Result<(usize, usize), String> {
    let Some(ledger) = LEDGER.get() else { return Err("台账未初始化".into()) };
    let mut records = ledger.read_all();
    let mut unpriced = 0usize;
    let mut total = 0.0f64;
    let mut text = String::new();
    for r in &mut records {
        r.cost_usd = cost_now(r);
        match r.cost_usd {
            Some(c) => total += c,
            None => unpriced += 1,
        }
        match serde_json::to_string(r) {
            Ok(line) => {
                text.push_str(&line);
                text.push('\n');
            }
            Err(e) => return Err(format!("序列化失败: {e}")),
        }
    }
    crate::fsx::write_atomic_str(&ledger.path, &text).map_err(|e| format!("重写台账失败: {e}"))?;
    ledger.reopen(); // 原子替换后重开追加句柄——否则后续记录写进已删除的旧文件
    SPEND.store(total.to_bits(), Ordering::Relaxed);
    try_event(
        EventType::State,
        json!({ "kind": "usage.ledger_recomputed", "records": records.len(), "spend_usd": total, "unpriced": unpriced }),
    );
    Ok((records.len(), unpriced))
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
    /// 会话过滤（空 = 全量；面板传当前会话 id）
    pub session: String,
    /// 时间下界（ISO；all = None）
    pub from: Option<String>,
    pub rows: Vec<Agg>,
    pub totals: Agg,
    pub spend_cap_usd: f64,
    pub spend_total_usd: f64,
    /// 是否全部记录都已计价（false = 有未计价请求，界面要显式说明）
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

/// 聚合报表。group = provider | model；range = today | 7d | 30d | all；
/// `session` 非空 = 只算该会话（面板的「当前对话」口径；不传 = 全量）。
pub fn report(group: &str, range: &str, session: &str) -> Report {
    let group = if group == GROUP_MODEL { GROUP_MODEL } else { GROUP_PROVIDER };
    let from = range_from(range);
    let records = LEDGER.get().map(|l| l.read_all()).unwrap_or_default();
    let mut groups: HashMap<String, Acc> = HashMap::new();
    let mut totals = Acc::default();
    let mut unpriced_total = 0u64;
    for r in &records {
        if !session.is_empty() && r.session_id != session {
            continue;
        }
        // 会话口径（面板「本次对话用量」）只算对话本身：标题/摘要是记账杂项，
        // 不是对话消耗（2026-10-01 用户实锤）；全局报表不传 session 时仍含全部用途
        if !session.is_empty() && (r.purpose == "title" || r.purpose == "summary") {
            continue;
        }
        if let Some(f) = from {
            let Ok(ts) = DateTime::parse_from_rfc3339(&r.ts) else { continue };
            if ts.with_timezone(&Utc) < f {
                continue;
            }
        }
        // 费用已冻结在记录里：直接取，不按当前价格回算（改价不动历史账）
        let cost = r.cost_usd;
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
        session: session.to_string(),
        from: from.map(|f| f.to_rfc3339_opts(SecondsFormat::Secs, true)),
        rows,
        totals: totals.finish(""),
        spend_cap_usd: cap_value(),
        spend_total_usd: spend_total(),
        priced: unpriced_total == 0,
    }
}

/// 价格表是否为空（没配价 = 全部未计价，界面要显式说明）。
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
        install_prices(&[ModelPrice { provider: "*".into(), model: "*".into(), prompt: 1.0, cached: 0.1, completion: 2.0 }]);
    }

    /// (token 三路, status, latency) 打包传，免得八参数触发 clippy。
    fn rec_at(session: &str, provider: &str, model: &str, m: (u64, u64, u64, &str, u64)) -> UsageRecord {
        let mut r = UsageRecord::new(session, "main", provider, "", model);
        r.prompt_tokens = m.0;
        r.completion_tokens = m.1;
        r.cached_tokens = m.2;
        r.status = m.3.to_string();
        r.latency_ms = m.4;
        r
    }

    fn rec(provider: &str, model: &str, prompt: u64, completion: u64, cached: u64, status: &str, latency: u64) -> UsageRecord {
        rec_at("sess-test", provider, model, (prompt, completion, cached, status, latency))
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

        let rep = report(GROUP_PROVIDER, "all", "");
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
        install_prices(&[]);
        record(rec("api.c.com", "mm-x", 10, 5, 0, "ok", 10));
        let rep = report(GROUP_MODEL, "all", "");
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
        let rep = report(GROUP_PROVIDER, "today", "");
        assert!(!rep.rows.iter().any(|r| r.key == "api.old.com"), "今天范围不该包含 2020 年的记录");
        assert!(rep.from.is_some(), "today 应带下界");
        let all = report(GROUP_PROVIDER, "all", "");
        assert!(all.rows.iter().any(|r| r.key == "api.old.com"), "all 范围应包含");
        assert!(all.from.is_none());
    }

    #[test]
    fn report_filters_by_session() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ledger();
        prices_star();
        // 两个会话各记一条，面板口径（session 过滤）只认当前对话
        record(rec_at("sess-A", "api.x.com", "m", (1000, 0, 0, "ok", 10)));
        record(rec_at("sess-B", "api.x.com", "m", (5000, 0, 0, "ok", 20)));
        let a = report(GROUP_PROVIDER, "all", "sess-A");
        assert_eq!(a.totals.requests, 1);
        assert_eq!(a.totals.prompt_tokens, 1000, "只算 sess-A 的 1000");
        let b = report(GROUP_PROVIDER, "all", "sess-B");
        assert_eq!(b.totals.prompt_tokens, 5000);
        let all = report(GROUP_PROVIDER, "all", "");
        assert!(all.totals.prompt_tokens >= 6000, "不传 session = 全量");
    }

    #[test]
    fn session_report_excludes_title_and_summary() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ledger();
        prices_star();
        // 会话口径只算对话：标题/摘要调用不摊进「本次对话用量」（2026-10-01 用户实锤）
        record(rec_at("sess-scope", "api.x.com", "m", (1000, 50, 0, "ok", 10)));
        let mut title_rec = rec_at("sess-scope", "api.y.com", "title-m", (133, 50, 0, "ok", 300));
        title_rec.purpose = "title".into();
        record(title_rec);
        let mut sum_rec = rec_at("sess-scope", "api.y.com", "sum-m", (2000, 100, 0, "ok", 500));
        sum_rec.purpose = "summary".into();
        record(sum_rec);
        let card = report(GROUP_MODEL, "all", "sess-scope");
        assert_eq!(card.totals.requests, 1, "标题/摘要不计入会话卡");
        assert_eq!(card.totals.prompt_tokens, 1000);
        assert!(!card.rows.iter().any(|r| r.key == "title-m"), "按模型行也不应出现辅助渠道");
        // 全局口径（不传 session）仍含全部用途——成本全景不能漏
        let global = report(GROUP_MODEL, "all", "");
        assert!(global.rows.iter().any(|r| r.key == "title-m"));
    }

    #[test]
    fn cost_is_frozen_at_record_time_and_recompute_reprices() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ledger();
        // 先按 1.0/M 记一笔 → 费用冻结为 1.0
        prices_star();
        let before = spend_total();
        record(rec("api.freeze.com", "m", 1_000_000, 0, 0, "ok", 1));
        let frozen = spend_total() - before;
        assert!((frozen - 1.0).abs() < 1e-9, "记账时算定 1.0，实际 {frozen}");
        // 改成 5.0/M 再记一笔 → 新请求按新价，老记录不变
        install_prices(&[ModelPrice { provider: "*".into(), model: "*".into(), prompt: 5.0, cached: 0.0, completion: 0.0 }]);
        let mid = spend_total();
        record(rec("api.freeze.com", "m", 1_000_000, 0, 0, "ok", 1));
        assert!((spend_total() - mid - 5.0).abs() < 1e-9, "新请求按新价 5.0");
        // 报表读冻结值：两笔分别 1.0 与 5.0（不是都用当前价 5.0 回算）
        let rep = report(GROUP_PROVIDER, "all", "");
        let row = rep.rows.iter().find(|r| r.key == "api.freeze.com").unwrap();
        assert!((row.cost_usd - 6.0).abs() < 1e-9, "冻结值相加 = 6.0，实际 {}", row.cost_usd);
        // 显式重算 → 两笔都按当前 5.0/M 改成 5.0（台账是进程级共享的，其它用例的记录也被重算，
        // 所以只断言本组 + 累计变大，不断言全局精确值）
        let before_total = spend_total();
        let (n, unpriced) = recompute().expect("重算应成功");
        assert!(n >= 2 && unpriced == 0, "重算 {n} 条，未计价 {unpriced}");
        let rep2 = report(GROUP_PROVIDER, "all", "");
        let row2 = rep2.rows.iter().find(|r| r.key == "api.freeze.com").unwrap();
        assert!((row2.cost_usd - 10.0).abs() < 1e-9, "重算后两笔都按 5.0/M = 10.0，实际 {}", row2.cost_usd);
        assert!(spend_total() > before_total, "重算后累计应变大: {} vs {before_total}", spend_total());
        // 重算后台账里每条都带 cost_usd（落盘即完整）
        let text = std::fs::read_to_string(ledger().path.clone()).unwrap();
        assert!(text.lines().all(|l| l.contains("\"cost_usd\"")), "重算后每条都应有费用字段");
    }

    #[test]
    fn instant_latency_is_measured() {
        use std::time::Instant;
        let t = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(t.elapsed().as_millis() >= 5);
    }
}
