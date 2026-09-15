//! 记忆库 —— 会话级记忆索引 + 向量检索（星云视图数据源）
//!
//! 每条会话 = 一个光点：标题/滚动摘要/用户消息压成检索正文（digest）。
//! 优先用 EMBEDDING 角色渠道生成神经向量；未配置嵌入渠道时退化为本地
//! 字符 n-gram 哈希向量（零依赖、离线可用）。检索按「向量余弦 + 词元重合」
//! 混合排序。索引落在 `<log_dir>/memory/library.json`（派生缓存，可随时重建）。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session::{SessionFile, SessionManager};

pub const LIB_VERSION: u32 = 1;
/// 本地哈希向量方法名（也是 index.model 的取值之一；换算法必须改名以触发全量重建）
pub const LOCAL_MODEL: &str = "local-hash-v2";
/// 本地向量维度（512 条会话 ≈ 1MB，够用且省盘）
pub const LOCAL_DIM: usize = 384;
/// 单次同步最多新算多少条向量（其余留给下次，避免首屏卡死）
pub const MAX_EMBED_PER_SYNC: usize = 64;
/// 外部嵌入每次请求的批量
const EMBED_BATCH: usize = 16;
/// digest 正文字符上限
const DIGEST_MAX: usize = 1200;
const SNIPPET_MAX: usize = 180;

/// 一条会话在记忆库里的投影（不含消息体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub project: String,
    pub created_at: String,
    pub updated_at: String,
    pub count: usize,
    /// 对话上下文总字符数（全部消息文本）：记忆库光点亮度/大小按它分级
    #[serde(default)]
    pub chars: usize,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    /// 参与检索的正文（标题 + 摘要 + 首尾用户消息 + 末条回答，截断）
    pub digest: String,
    /// 卡片展示用短句
    pub snippet: String,
    /// 指纹（updated_at|count|title）：变了才重算 digest/向量
    pub fp: String,
    /// 归一化向量（空 = 待索引）
    #[serde(default)]
    pub vec: Vec<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Library {
    #[serde(default)]
    pub version: u32,
    /// 向量来源：外部嵌入模型名 或 LOCAL_MODEL
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

impl Default for Library {
    fn default() -> Self {
        Self { version: LIB_VERSION, model: String::new(), entries: Vec::new() }
    }
}

pub fn lib_path(log_dir: &Path) -> PathBuf {
    log_dir.join("memory").join("library.json")
}

pub fn load(log_dir: &Path) -> Library {
    match std::fs::read_to_string(lib_path(log_dir)) {
        Ok(t) => serde_json::from_str(&t).unwrap_or_default(),
        Err(_) => Library::default(),
    }
}

pub fn save(log_dir: &Path, lib: &Library) -> anyhow::Result<()> {
    let path = lib_path(log_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string(lib)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

// —— 会话 → 检索正文 ——

fn msg_text(m: &serde_json::Value) -> String {
    match m.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|p| p.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// 剥离形如 [Audio understanding...] 的前置投影块，只留用户真说的话。
fn strip_projection(s: &str) -> &str {
    let mut t = s.trim();
    while t.starts_with('[') {
        match t.find(']') {
            Some(i) => t = t[i + 1..].trim(),
            None => break,
        }
    }
    t
}

fn clip(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= n {
        return t.to_string();
    }
    t.chars().take(n).collect::<String>() + "…"
}

/// 会话上下文总字符数：所有消息文本的字符和（记忆库光点亮度的长度信号）。
pub fn chars_of(f: &SessionFile) -> usize {
    f.messages.iter().map(|m| msg_text(m).chars().count()).sum()
}

/// 会话 → 检索正文：标题 + 滚动摘要 + 首尾用户消息 + 末条回答。
pub fn digest_of(f: &SessionFile) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !f.title.trim().is_empty() {
        parts.push(f.title.clone());
    }
    if let Some(sm) = f.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(sm.to_string());
    }
    let users: Vec<&serde_json::Value> = f
        .messages
        .iter()
        .filter(|m| m.get("role").and_then(serde_json::Value::as_str) == Some("user"))
        .collect();
    for m in users.iter().take(4) {
        let raw = msg_text(m);
        let t = strip_projection(&raw);
        if !t.is_empty() {
            parts.push(clip(t, 240));
        }
    }
    if users.len() > 8 {
        for m in users.iter().skip(users.len() - 3) {
            let raw = msg_text(m);
            let t = strip_projection(&raw);
            if !t.is_empty() {
                parts.push(clip(t, 240));
            }
        }
    }
    if let Some(a) = f
        .messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(serde_json::Value::as_str) == Some("assistant"))
    {
        let t = msg_text(a);
        if !t.trim().is_empty() {
            parts.push(clip(&t, 320));
        }
    }
    clip(&parts.join("\n"), DIGEST_MAX)
}

/// 卡片短句：摘要优先，其次首条用户消息。
pub fn snippet_of(f: &SessionFile) -> String {
    if let Some(sm) = f.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return clip(sm, SNIPPET_MAX);
    }
    if let Some(m) = f
        .messages
        .iter()
        .find(|m| m.get("role").and_then(serde_json::Value::as_str) == Some("user"))
    {
        return clip(strip_projection(&msg_text(m)), SNIPPET_MAX);
    }
    String::new()
}

/// 工作区路径 → 项目显示名（末级目录名；空 = 未分组）。
pub fn project_name(ws: &str) -> String {
    let t = ws.trim().trim_end_matches(['/', '\\']);
    if t.is_empty() {
        return "未分组".to_string();
    }
    t.rsplit(['/', '\\']).next().unwrap_or(t).to_string()
}

// —— 词元 / 本地向量 / 相似度 ——

fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x9FFF | 0xF900..=0xFAFF | 0x3040..=0x30FF | 0xAC00..=0xD7AF)
}

fn fnv1a(s: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 中英混排切词：ASCII 词（小写，≥2 字符）+ CJK 相邻双字。
/// 多字 CJK 串只取双字——单字（的/是/我…）在中文语料里几乎人人在用，
/// 放进来只会制造噪声；孤立的单字词（如「猫」）仍保留。
pub fn tokens(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut ascii = String::new();
    let mut run: Vec<char> = Vec::new();

    fn flush_cjk(run: &mut Vec<char>, out: &mut Vec<String>) {
        match run.len() {
            0 => {}
            1 => out.push(run[0].to_string()),
            _ => {
                for w in run.windows(2) {
                    out.push(w.iter().collect());
                }
            }
        }
        run.clear();
    }
    fn flush_ascii(ascii: &mut String, out: &mut Vec<String>) {
        if ascii.chars().count() >= 2 {
            out.push(std::mem::take(ascii));
        } else {
            ascii.clear();
        }
    }

    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            flush_cjk(&mut run, &mut out);
            ascii.push(ch.to_ascii_lowercase());
            continue;
        }
        flush_ascii(&mut ascii, &mut out);
        if is_cjk(ch) {
            run.push(ch);
        } else {
            flush_cjk(&mut run, &mut out);
        }
    }
    flush_ascii(&mut ascii, &mut out);
    flush_cjk(&mut run, &mut out);
    out
}

fn l2_normalize(v: &mut [f32]) {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 1e-6 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

/// 本地哈希向量：双字/词元带符号哈希 + L2 归一（零依赖、可离线）。
pub fn local_vec(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; LOCAL_DIM];
    for t in tokens(text) {
        let h = fnv1a(&t);
        let idx = (h % LOCAL_DIM as u64) as usize;
        let sign = if (h >> 63) == 0 { 1.0 } else { -1.0 };
        v[idx] += sign;
    }
    l2_normalize(&mut v);
    v
}

/// 本地向量的经验标定：无关文本的余弦约 0.06 上下（哈希碰撞底噪），
/// 真正有重叠的多在 0.2 以上；线性压到 0..1。
fn local_calibrate(cos: f32) -> f32 {
    ((cos - 0.06) / 0.30).clamp(0.0, 1.0)
}

/// 外部嵌入向量归一化（各家是否归一不保证，统一归一到单位球再算余弦）。
pub fn norm_vec(raw: &[f64]) -> Vec<f32> {
    let mut v: Vec<f32> = raw.iter().map(|x| *x as f32).collect();
    l2_normalize(&mut v);
    v
}

/// 余弦相似度（两侧都已归一化时即点积）。
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f32>().clamp(-1.0, 1.0)
}

// —— 检索 ——

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: String,
    /// 混合总分 0..1
    pub score: f32,
    /// 向量余弦（无向量时 0）
    pub vector: f32,
    /// 词元重合（含标题加权与短语加成）
    pub lexical: f32,
    /// 命中上下文（digest 里 query 附近的窗口）
    pub snippet: String,
}

/// 语料级 idf（df = 含该词元的文档数）。
fn idf_map(entries: &[Entry]) -> HashMap<String, f32> {
    let n = entries.len().max(1) as f32;
    let mut df: HashMap<String, usize> = HashMap::new();
    for e in entries {
        let mut seen: HashSet<String> = HashSet::new();
        for t in tokens(&format!("{} {}", e.title, e.digest)) {
            if seen.insert(t.clone()) {
                *df.entry(t).or_insert(0) += 1;
            }
        }
    }
    df.into_iter().map(|(t, c)| (t, (1.0 + n / (1.0 + c as f32)).ln())).collect()
}

fn lexical_score(qt: &[String], doc_toks: &[String], title: &str, idf: &HashMap<String, f32>) -> f32 {
    if qt.is_empty() {
        return 0.0;
    }
    let doc: HashSet<&String> = doc_toks.iter().collect();
    let title: HashSet<String> = tokens(title).into_iter().collect();
    let (mut got, mut total) = (0f32, 0f32);
    for t in qt {
        let w = idf.get(t).copied().unwrap_or(1.0);
        total += w;
        if doc.contains(t) {
            got += w;
            if title.contains(t) {
                got += w * 0.5; // 标题命中加权
            }
        }
    }
    if total <= 0.0 {
        return 0.0;
    }
    (got / total).min(1.0)
}

/// digest 内查询词附近的窗口（找不到时退回首段）。
fn best_window(digest: &str, q: &str) -> String {
    let d: Vec<char> = digest.chars().collect();
    if d.is_empty() {
        return String::new();
    }
    let ql = q.trim().to_lowercase();
    let dl = digest.to_lowercase();
    let byte_pos = if ql.chars().count() >= 2 { dl.find(&ql) } else { None };
    let start = match byte_pos {
        Some(b) => {
            let idx = dl[..b].chars().count();
            idx.saturating_sub(30)
        }
        None => 0,
    };
    let end = (start + SNIPPET_MAX).min(d.len());
    let mut s: String = d[start..end].iter().collect();
    if start > 0 {
        s = format!("…{s}");
    }
    if end < d.len() {
        s.push('…');
    }
    s
}

/// 混合检索：向量余弦 + 词元重合。`qvec` 为空（无向量语料/失败）时纯词法。
/// `min_score` 以下的丢掉——搜到什么才呈现什么，宁缺毋滥。
pub fn search(lib: &Library, qvec: &[f32], query: &str, top_k: usize, min_score: f32) -> Vec<Hit> {
    let local = lib.model.is_empty() || lib.model == LOCAL_MODEL;
    // 本地哈希向量对短查询只是噪声（实测无关文本余弦也常在 0.1+），本地模式只认词元分。
    rank(lib, qvec, query, top_k, min_score, if local { 0.0 } else { 0.62 })
}

/// 「相似会话」：拿这条会话自己的向量当查询（长文本 vs 长文本，哈希向量才有效）。
/// 返回按相似度排名的其它会话；最高分归一到 1.0 便于展示。
pub fn similar(lib: &Library, id: &str, top_k: usize, min_score: f32) -> Vec<Hit> {
    let Some(src) = lib.entries.iter().find(|e| e.id == id) else {
        return Vec::new();
    };
    let qtext = clip(&format!("{} {}", src.title, src.snippet), 200);
    let qvec: &[f32] = if src.vec.is_empty() { &[] } else { &src.vec };
    let mut hits = rank(lib, qvec, &qtext, top_k + 1, min_score, 0.7);
    hits.retain(|h| h.id != id);
    hits.truncate(top_k);
    if let Some(top) = hits.first().map(|h| h.score).filter(|s| *s > 1e-6) {
        for h in hits.iter_mut() {
            h.score = (h.score / top).clamp(0.0, 1.0);
        }
    }
    hits
}

/// 排序内核：`vector_weight` = 向量分权重（0=纯词法；本地哈希向量对短查询无意义）。
fn rank(lib: &Library, qvec: &[f32], query: &str, top_k: usize, min_score: f32, vector_weight: f32) -> Vec<Hit> {
    let qt = tokens(query);
    if qt.is_empty() {
        return Vec::new();
    }
    let idf = idf_map(&lib.entries);
    let ql = query.trim().to_lowercase();
    let local = lib.model.is_empty() || lib.model == LOCAL_MODEL;
    let w = vector_weight.clamp(0.0, 1.0);
    let mut hits: Vec<Hit> = Vec::new();
    for e in &lib.entries {
        let doc_toks = tokens(&format!("{} {}", e.title, e.digest));
        let lex = lexical_score(&qt, &doc_toks, &e.title, &idf);
        let vec = if qvec.is_empty() || e.vec.len() != qvec.len() {
            0.0
        } else {
            let c = cosine(qvec, &e.vec).max(0.0);
            if local { local_calibrate(c) } else { c }
        };
        let mut score = if qvec.is_empty() || w <= 0.0 {
            lex
        } else {
            vec * w + lex * (1.0 - w)
        };
        // 标题/正文包含整句：确定性加成（中文短查询的词元法偏弱）
        if ql.chars().count() >= 2
            && (e.title.to_lowercase().contains(&ql) || e.digest.to_lowercase().contains(&ql))
        {
            score = (score + 0.25).min(1.0);
        }
        if score >= min_score {
            hits.push(Hit {
                id: e.id.clone(),
                score,
                vector: vec,
                lexical: lex,
                snippet: best_window(&e.digest, query),
            });
        }
    }
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(top_k.max(1));
    hits
}

// —— 索引同步（会话目录 → library.json）——

/// 外部嵌入渠道（EMBEDDING 角色解析产物）。
pub struct Embedder<'a> {
    pub gw: &'a crate::gateway::Gateway,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub total: usize,
    /// 本次新算向量的条数
    pub indexed: usize,
    /// 仍未索引的条数（下次同步继续）
    pub pending: usize,
    /// 向量来源：外部模型名 或 local-hash-v1
    pub model: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 需要重算正文（digest/snippet/chars）的条件：无旧条目、指纹变了、force，
/// 或旧索引缺 chars（chars 之前落的库没有该字段，count>0 却 chars=0 视为待回填）。
fn needs_text(m: &crate::session::SessionMeta, prev: Option<&Entry>, fp: &str, force: bool) -> bool {
    match prev {
        None => true,
        Some(p) => force || p.fp != fp || (p.chars == 0 && m.count > 0),
    }
}

fn build_entry(m: &crate::session::SessionMeta, f: Option<&SessionFile>, prev: Option<&Entry>, force: bool) -> Option<Entry> {
    let fp = format!("{}|{}|{}", m.updated_at, m.count, m.title);
    let reuse_text = if !needs_text(m, prev, &fp, force) { prev } else { None };
    let (digest, snippet, chars) = match reuse_text {
        // chars 只随消息变化，而消息变化必然改动 updated_at → 指纹，随指纹一起复用即保新鲜
        Some(p) => (p.digest.clone(), p.snippet.clone(), p.chars),
        None => {
            let file = f?;
            (digest_of(file), snippet_of(file), chars_of(file))
        }
    };
    // 向量只认指纹：正文重算（chars 回填）不代表内容变了，向量照旧复用
    let vec = prev.filter(|p| p.fp == fp && !force).map(|p| p.vec.clone()).unwrap_or_default();
    Some(Entry {
        id: m.id.clone(),
        title: m.title.clone(),
        workspace: m.workspace.clone(),
        project: project_name(&m.workspace),
        created_at: m.created_at.clone(),
        updated_at: m.updated_at.clone(),
        count: m.count,
        chars,
        pinned: m.pinned,
        archived: m.archived,
        digest,
        snippet,
        fp,
        vec,
    })
}

/// 同步会话目录 → 记忆库索引，增量补齐向量。
/// `force` = 忽略指纹全部重算；外部渠道失败时整体回落本地向量（保证可检索）。
pub async fn sync(
    sessions: &SessionManager,
    log_dir: &Path,
    embedder: Option<Embedder<'_>>,
    force: bool,
) -> SyncReport {
    let label = embedder.as_ref().map(|e| e.model.clone()).unwrap_or_else(|| LOCAL_MODEL.to_string());
    let mut lib = load(log_dir);
    if lib.model != label || force {
        for e in lib.entries.iter_mut() {
            e.vec.clear();
        }
    }
    lib.model = label.clone();
    lib.version = LIB_VERSION;

    let metas = sessions.list();
    let mut old: HashMap<String, Entry> = std::mem::take(&mut lib.entries).into_iter().map(|e| (e.id.clone(), e)).collect();
    let mut entries: Vec<Entry> = Vec::with_capacity(metas.len());
    for m in &metas {
        let prev = old.remove(&m.id);
        let fp = format!("{}|{}|{}", m.updated_at, m.count, m.title);
        let need_text = needs_text(m, prev.as_ref(), &fp, force);
        let file = if need_text { sessions.get(&m.id) } else { None };
        if let Some(e) = build_entry(m, file.as_ref(), prev.as_ref(), force) {
            entries.push(e);
        }
    }
    let total = entries.len();

    let mut report = SyncReport {
        total,
        indexed: 0,
        pending: 0,
        model: lib.model.clone(),
        mode: if embedder.is_some() { "external".into() } else { "local".into() },
        error: None,
    };

    let todo: Vec<usize> = entries.iter().enumerate().filter(|(_, e)| e.vec.is_empty()).map(|(i, _)| i).collect();
    if !todo.is_empty() {
        match embedder.as_ref() {
            // 外部嵌入：分批补齐，失败只记错误不翻口径（待索引条目下轮再试；
            // 这段时间检索退化为纯词法，见 search 对空 qvec 的处理）
            Some(emb) => {
                let batch_idx: Vec<usize> = todo.iter().copied().take(MAX_EMBED_PER_SYNC).collect();
                for chunk in batch_idx.chunks(EMBED_BATCH) {
                    let inputs: Vec<String> = chunk.iter().map(|&i| entries[i].digest.clone()).collect();
                    match emb.gw.embed(&emb.base_url, &emb.model, &emb.api_key, &inputs).await {
                        Ok(vs) if vs.len() == chunk.len() && vs.iter().all(|v| !v.is_empty()) => {
                            for (k, &i) in chunk.iter().enumerate() {
                                entries[i].vec = norm_vec(&vs[k]);
                            }
                            report.indexed += chunk.len();
                        }
                        Ok(_) => {
                            report.error = Some("嵌入渠道返回的向量条数不符；本次未索引，下次同步重试".into());
                            break;
                        }
                        Err(e) => {
                            report.error = Some(format!("嵌入渠道调用失败（{e}）；本次未索引，下次同步重试"));
                            break;
                        }
                    }
                }
            }
            // 未配置嵌入席位：本地哈希向量（零依赖、离线可用）
            None => {
                for e in entries.iter_mut().filter(|e| e.vec.is_empty()) {
                    e.vec = local_vec(&e.digest);
                    report.indexed += 1;
                }
            }
        }
    }
    report.pending = entries.iter().filter(|e| e.vec.is_empty()).count();

    lib.entries = entries;
    if let Err(e) = save(log_dir, &lib) {
        report.error = Some(match report.error.take() {
            Some(prev) => format!("{prev}；索引落盘失败: {e}"),
            None => format!("索引落盘失败: {e}"),
        });
    }
    report
}

/// 无需外部渠道的同步（测试/离线用；生产走 build_library 的有界增量同步）。
#[cfg(test)]
pub async fn sync_local(sessions: &SessionManager, log_dir: &Path, force: bool) -> SyncReport {
    sync(sessions, log_dir, None, force).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("agent-galaxy-{tag}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn tokens_and_local_vectors() {
        let ts = tokens("修复 login.rs 的 Bug，登录失败");
        assert!(ts.contains(&"login.rs".to_string()), "{ts:?}");
        assert!(ts.contains(&"登录".to_string()) && ts.contains(&"失败".to_string()), "应有 CJK 双字: {ts:?}");
        assert!(!ts.contains(&"修".to_string()), "多字串里不取单字（噪声）: {ts:?}");
        assert!(ts.contains(&"的".to_string()), "孤立单字仍保留: {ts:?}");

        let a = local_vec("修复登录接口的 JWT 校验失败");
        let b = local_vec("登录接口 JWT 校验有问题，需要修复");
        let c = local_vec("紫色星云壁纸下载");
        let n: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-4, "应 L2 归一: {n}");
        let ab = cosine(&a, &b);
        let ac = cosine(&a, &c);
        assert!(ab > ac + 0.2, "相近文本余弦应明显更高: ab={ab} ac={ac}");
        assert_eq!(local_vec("同一段文字"), local_vec("同一段文字"), "本地向量须确定");
    }

    #[test]
    fn digest_prefers_summary_and_user_text() {
        let f = SessionFile {
            id: "s1".into(),
            title: "修登录".into(),
            created_at: "2026-09-01T00:00:00Z".into(),
            updated_at: "2026-09-01T00:00:00Z".into(),
            messages: vec![
                json!({"role":"user","content":"[Audio understanding]\n把登录改成 JWT"}),
                json!({"role":"assistant","content":"已改完签发与校验"}),
            ],
            summary: Some("任务目标：登录改 JWT".into()),
            workspace: "C:\\ws\\demo".into(),
            pinned: false,
            archived: false,
        };
        let d = digest_of(&f);
        assert!(d.contains("修登录") && d.contains("登录改 JWT"), "{d}");
        assert!(d.contains("把登录改成 JWT"), "投影块应剥离后保留原文: {d}");
        assert!(!d.contains("Audio understanding"), "投影块不该进 digest: {d}");
        let s = snippet_of(&f);
        assert!(s.contains("登录改 JWT"), "摘要优先作短句: {s}");
        assert_eq!(project_name("C:\\ws\\demo"), "demo");
        assert_eq!(project_name(""), "未分组");
    }

    #[tokio::test]
    async fn sync_then_search_ranks_and_filters() {
        let dir = tmp("sync");
        let sessions = SessionManager::new(&dir);
        let a = sessions.new_id();
        sessions
            .save(&a, &[json!({"role":"user","content":"把登录接口改成 JWT 并补校验"})], Some("任务目标：登录改 JWT"))
            .unwrap();
        let b = sessions.new_id();
        sessions.save(&b, &[json!({"role":"user","content":"调星云背景的配色，紫色多一点"})], None).unwrap();
        let c = sessions.new_id();
        sessions.save(&c, &[json!({"role":"user","content":"修一下 README 错别字"})], None).unwrap();

        let rep = sync_local(&sessions, &dir, false).await;
        assert_eq!(rep.total, 3);
        assert_eq!(rep.pending, 0, "本地模式全部可索引");
        assert_eq!(rep.mode, "local");
        assert!(dir.join("memory").join("library.json").exists());

        let lib = load(&dir);
        assert_eq!(lib.entries.len(), 3);
        assert!(lib.entries.iter().all(|e| !e.vec.is_empty()));

        // 检索：语义相关的一条排第一，噪声查询不返回
        let q = local_vec("登录 JWT 校验");
        let hits = search(&lib, &q, "登录 JWT 校验", 10, 0.15);
        assert!(!hits.is_empty(), "应有命中");
        assert_eq!(hits[0].id, a, "登录相关会话应排第一: {:?}", hits.iter().map(|h| (&h.id, h.score)).collect::<Vec<_>>());
        assert!(!hits.iter().any(|h| h.id == c), "无关会话不应出现: {:?}", hits.len());
        assert!(hits[0].snippet.contains("JWT") || hits[0].snippet.contains("登录"), "应给出命中窗口");

        // 无意义查询 → 空（宁缺毋滥）
        let q2 = local_vec("zzzqqq 完全不存在的词元组合");
        assert!(search(&lib, &q2, "zzzqqq 完全不存在的词元组合", 10, 0.15).is_empty());

        // 增量：重复同步不重算（indexed=0），改内容后更新
        let rep2 = sync_local(&sessions, &dir, false).await;
        assert_eq!(rep2.indexed, 0, "无变化不该重算向量");
        sessions
            .save(
                &b,
                &[json!({"role":"user","content":"调星云背景的配色"}), json!({"role":"assistant","content":"好"})],
                Some("任务目标：星云配色改紫色"),
            )
            .unwrap();
        let rep3 = sync_local(&sessions, &dir, false).await;
        assert_eq!(rep3.indexed, 1, "变更会话应重算");
        let lib = load(&dir);
        assert!(lib.entries.iter().find(|e| e.id == b).unwrap().digest.contains("紫色"));

        // 删除会话 → 索引里剔除
        sessions.delete(&c).unwrap();
        let rep4 = sync_local(&sessions, &dir, false).await;
        assert_eq!(rep4.total, 2);
        assert!(!load(&dir).entries.iter().any(|e| e.id == c));

        // force：全部重算
        let rep5 = sync_local(&sessions, &dir, true).await;
        assert_eq!(rep5.indexed, 2, "force 应全量重算");

        // 相似会话：同主题的 d 应排最前，且列表不含自己
        let d = sessions.new_id();
        sessions
            .save(&d, &[json!({"role":"user","content":"登录鉴权改 JWT：签发与校验都要补齐"})], Some("任务目标：JWT 登录鉴权"))
            .unwrap();
        sync_local(&sessions, &dir, false).await;
        let lib = load(&dir);
        let sim = similar(&lib, &a, 5, 0.2);
        assert!(sim.iter().all(|h| h.id != a), "相似列表不该含自己");
        assert!(!sim.is_empty(), "应能找到相似会话");
        assert_eq!(sim[0].id, d, "最相似的应是同主题会话: {:?}", sim.iter().map(|h| (&h.id, h.score)).collect::<Vec<_>>());
        assert!((sim[0].score - 1.0).abs() < 1e-3, "最高分归一化到 1");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 旧版本落的索引没有 chars 字段（serde 默认 0）：同步时不重算向量，但正文要回填 chars。
    #[tokio::test]
    async fn old_index_without_chars_gets_backfilled() {
        let dir = tmp("backfill");
        let sessions = SessionManager::new(&dir);
        let a = sessions.new_id();
        sessions
            .save(&a, &[json!({"role":"user","content":"修复登录接口"})], None)
            .unwrap();
        sync_local(&sessions, &dir, false).await;

        // 模拟旧索引：chars 归零（指纹与向量都保留）
        let mut lib = load(&dir);
        assert!(lib.entries[0].chars > 0);
        for e in lib.entries.iter_mut() {
            e.chars = 0;
        }
        save(&dir, &lib).unwrap();

        let rep = sync_local(&sessions, &dir, false).await;
        assert_eq!(rep.indexed, 0, "向量不该重算");
        let lib = load(&dir);
        assert!(lib.entries.iter().all(|e| e.chars > 0), "chars 应回填: {:?}", lib.entries.iter().map(|e| e.chars).collect::<Vec<_>>());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 外部嵌入渠道全链路（内嵌 mock /embeddings，不碰用户配置）：
    /// 索引走外部模型 → 存 32 维外部向量 → 查询向量同口径 → 失败时只记错误不崩。
    #[tokio::test]
    async fn external_embeddings_path_and_failure_grace() {
        use axum::{Json, Router, routing::post};
        // gateway.embed 会写 gateway.request 事件 → 先初始化日志器
        let _ = crate::observability::init(
            &std::env::temp_dir().join("agent-test-logs"),
            "library.jsonl",
            format!("lib-{}", uuid::Uuid::new_v4()),
        );
        let app = Router::new().route(
            "/embeddings",
            post(|Json(b): Json<serde_json::Value>| async move {
                let inputs: Vec<String> = b
                    .get("input")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let data: Vec<serde_json::Value> = inputs
                    .iter()
                    .map(|t| {
                        let mut v = vec![0f64; 32];
                        for (i, c) in t.chars().enumerate() {
                            v[(c as usize + i) % 32] += 1.0;
                        }
                        json!({ "embedding": v })
                    })
                    .collect();
                Json(json!({ "data": data }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tmp("ext");
        let sessions = SessionManager::new(&dir);
        let a = sessions.new_id();
        sessions
            .save(&a, &[json!({"role":"user","content":"把登录接口改成 JWT 并补校验"})], Some("任务目标：登录改 JWT"))
            .unwrap();
        let b = sessions.new_id();
        sessions.save(&b, &[json!({"role":"user","content":"调星云背景的配色，紫色多一点"})], None).unwrap();

        // 并发跑的 gateway 代理测试会把 HTTP(S)_PROXY 指到死端口（进程级 env），
        // 显式声明 localhost 不走代理，保证本测试的 mock 渠道可达。
        unsafe {
            std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
        }
        let gw = crate::gateway::Gateway::new();
        let emb = Embedder {
            gw: &gw,
            base_url: format!("http://{addr}"),
            model: "mock-embed".into(),
            api_key: "sk-test".into(),
        };
        let rep = sync(&sessions, &dir, Some(emb), false).await;
        assert_eq!(rep.mode, "external");
        assert_eq!(rep.model, "mock-embed");
        assert_eq!(rep.indexed, 2, "两条都该走外部嵌入: {:?}", rep.error);
        assert!(rep.error.is_none() && rep.pending == 0);

        let lib = load(&dir);
        assert_eq!(lib.model, "mock-embed");
        assert!(lib.entries.iter().all(|e| e.vec.len() == 32), "应存外部向量而非本地向量");

        // 查询向量走同一渠道（外部口径），混合检索命中登录那条
        let vs = gw
            .embed(&format!("http://{addr}"), "mock-embed", "sk-test", &["登录 JWT".to_string()])
            .await
            .unwrap();
        let hits = search(&lib, &norm_vec(&vs[0]), "登录 JWT", 5, 0.15);
        assert_eq!(hits.first().map(|h| h.id.as_str()), Some(a.as_str()), "外部向量检索应命中");

        // 渠道挂掉：只记错误 + 留 pending，不崩；检索退化纯词法仍可用
        let dir2 = tmp("extfail");
        let s2 = SessionManager::new(&dir2);
        let c = s2.new_id();
        s2.save(&c, &[json!({"role":"user","content":"沙箱探针的内存上限怎么测"})], None).unwrap();
        let dead = Embedder { gw: &gw, base_url: "http://127.0.0.1:1".into(), model: "mock-embed".into(), api_key: "k".into() };
        let r2 = sync(&s2, &dir2, Some(dead), false).await;
        assert!(r2.error.is_some(), "失败必须记错误给前端提示");
        assert_eq!(r2.pending, 1, "未索引条目留给下轮同步");
        let lib2 = load(&dir2);
        let hits2 = search(&lib2, &[], "沙箱探针 内存", 5, 0.1);
        assert!(!hits2.is_empty(), "纯词法兜底仍可检索");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dir2).ok();
    }
}
