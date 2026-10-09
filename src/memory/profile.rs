//! profile.md 读写层（记忆系统方案 §3.5/§3.6/§6.2，施工单第 1 步）
//!
//! 自动记忆的载体：`<app_root>/.agent/profile.md`（按安装隔离，agent 只写它），两个分区——
//! - **偏好区**：用户明说的长期偏好（T1 一次就够），不带状态、带 hits；
//! - **教训区**：重复踩坑转正的教训（T2 要第二次佐证），带「观察中/已确认」状态。
//!
//! 铁律：**行级操作，绝不整段重写**。读文件 → 只动目标那一行 → 原子写回；
//! 用户改的其他行一概不动；按原文匹配，匹配不上就报错「这条不在了」，不硬盖；
//! 用户删掉的条目不偷偷加回来。这条是「用户可自由编辑面板」的前提（§6.1）。
//!
//! 条目形态（人看无感、工具解析得到）：
//! ```markdown
//! ## 偏好区
//! - 以后回答先给结论 <!-- src: 工作区 · 2026-10-09 · hits: 1 -->
//!
//! ## 教训区
//! - cargo test 失败先看前 50 行 <!-- src: agent-copy · 2026-10-08 · hits: 2 · 已确认 -->
//! ```

use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use super::sanitize;

/// 分区头（文件里就长这样；用户改头名 = 那区对工具不可见，内容原样保留不动）
pub const PREFS_HEADER: &str = "## 偏好区";
pub const LESSONS_HEADER: &str = "## 教训区";

/// 上限只用字符一把尺子（§6.2）：每条 ≤40 字已被提示词层约束住，行数不再是独立变量，
/// 双口径必然出现「哪个先到」的死阈值。硬顶 2500 ≈ 2.5k–3.3k token（对应社区共识 4k 上限）。
pub const HARD_CAP_CHARS: usize = 2500;
/// 预警线 2000 字符（80%），此时约 25–26 条
pub const WARN_CHARS: usize = 2000;
/// 理想区间 1200 字符以内（约 15 条）≈ 1.2k–1.6k token，宁可少而精
pub const IDEAL_CHARS: usize = 1200;

/// 观察中清除：首次记录超 30 天的「观察中」教训静默清掉（§3.6）。
/// 已确认的教训不清（能转正说明当时被佐证过，之后不再出现可能只是场景没复发）。
pub const OBSERVATION_MAX_AGE_DAYS: i64 = 30;

/// 教训区条目状态（偏好区不带状态：偏好一次就够，无需佐证）
pub const STATUS_OBSERVING: &str = "观察中";
pub const STATUS_CONFIRMED: &str = "已确认";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// 偏好区（T1：用户明说的长期偏好）
    Prefs,
    /// 教训区（T2：重复踩坑转正的教训）
    Lessons,
}

impl Section {
    pub fn header(self) -> &'static str {
        match self {
            Section::Prefs => PREFS_HEADER,
            Section::Lessons => LESSONS_HEADER,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Section::Prefs => "偏好区",
            Section::Lessons => "教训区",
        }
    }
    /// 仅教训区有条目状态
    pub fn has_status(self) -> bool {
        matches!(self, Section::Lessons)
    }
}

/// 一条条目：正文 + HTML 注释里的元数据（src 纯来源记录 / 日期=首次记录 / hits 参考值）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub text: String,
    pub src: String,
    /// 首次记录日期，YYYY-MM-DD
    pub date: String,
    /// 命中次数（参考值不是判据：模型数错不影响任何逻辑）
    pub hits: u32,
    /// 仅教训区：观察中 / 已确认
    pub status: Option<String>,
}

impl Entry {
    /// 新条目（hits 从 1 起；教训区首记默认「观察中」，第二次佐证才转正）
    pub fn new(text: &str, src: &str, date: &str, section: Section) -> Self {
        Self {
            text: text.to_string(),
            src: src.to_string(),
            date: date.to_string(),
            hits: 1,
            status: if section.has_status() {
                Some(STATUS_OBSERVING.to_string())
            } else {
                None
            },
        }
    }
}

/// 从一行解析条目；不是条目行（无 "- " 前缀或正文为空）返回 None。
/// 元数据注释缺失/残缺不报错——按缺省值兜底（hits=1、无状态），文件照样能用。
fn parse_entry_line(line: &str) -> Option<Entry> {
    let body = line.trim().strip_prefix("- ")?.trim();
    if body.is_empty() {
        return None;
    }
    let (text, comment) = match body.find("<!--") {
        Some(i) => (body[..i].trim(), Some(body[i..].trim())),
        None => (body, None),
    };
    if text.is_empty() {
        return None;
    }
    let mut e = Entry {
        text: text.to_string(),
        src: String::new(),
        date: String::new(),
        hits: 1,
        status: None,
    };
    if let Some(c) = comment {
        let inner = c.trim_start_matches("<!--").trim_end_matches("-->").trim();
        for (k, part) in inner.split(" · ").enumerate() {
            let part = part.trim();
            if let Some(v) = part.strip_prefix("src:") {
                e.src = v.trim().to_string();
            } else if let Some(v) = part.strip_prefix("hits:") {
                e.hits = v.trim().parse().unwrap_or(1);
            } else if k == 1 {
                // 第二段固定是日期（src: X · 日期 · hits: N [· 状态]）
                e.date = part.to_string();
            } else if part == STATUS_OBSERVING || part == STATUS_CONFIRMED {
                e.status = Some(part.to_string());
            }
        }
    }
    Some(e)
}

fn render_entry(e: &Entry) -> String {
    let mut meta = format!("src: {} · {} · hits: {}", e.src, e.date, e.hits);
    if let Some(s) = &e.status {
        meta.push_str(" · ");
        meta.push_str(s);
    }
    format!("- {} <!-- {} -->", e.text, meta)
}

/// 分区块结束行号（头行 start 之后、下一个 "## " 头之前；没有下一个头 = 到文末）
fn block_end<S: AsRef<str>>(lines: &[S], start: usize) -> usize {
    lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, l)| l.as_ref().trim_start().starts_with("## "))
        .map(|(i, _)| i)
        .unwrap_or(lines.len())
}

/// profile.md 读写层。每次现开现读现写（文件小），不持有跨调用副本。
pub struct Profile {
    path: PathBuf,
    /// 文件原文（读入时统一成 \n 行尾；写出时保证结尾换行）
    raw: String,
}

impl Profile {
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("profile.md");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        Self { path, raw }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 全部分区条目（保序；面板与注入用）。未知分区/分区外的行一概不解释、原样保留。
    pub fn entries(&self) -> Vec<(Section, Entry)> {
        let mut out = Vec::new();
        let mut cur: Option<Section> = None;
        for line in self.raw.lines() {
            let t = line.trim();
            if t == PREFS_HEADER {
                cur = Some(Section::Prefs);
                continue;
            }
            if t == LESSONS_HEADER {
                cur = Some(Section::Lessons);
                continue;
            }
            if t.starts_with("## ") {
                cur = None;
                continue;
            }
            if let Some(s) = cur {
                if let Some(e) = parse_entry_line(line) {
                    out.push((s, e));
                }
            }
        }
        out
    }

    /// 字符一把尺子：save() 会写出的内容字符数（注入什么就数什么；用户手写的
    /// ~/.agent/AGENTS.md 不计入不限制，那是用户自己的地盘）。
    /// 内存里的 raw 可能缺结尾换行（lines()+join 会吃掉），补上再数，与落盘一致。
    pub fn chars(&self) -> usize {
        let extra = (!self.raw.is_empty() && !self.raw.ends_with('\n')) as usize;
        self.raw.chars().count() + extra
    }

    /// 注入用文本（§4：空分区不注入，不占 token）：只含有条目的分区，条目按标准
    /// 形态重渲染。分区外的用户自注不进注入——注入契约只覆盖两个分区（面板里看得到
    /// 全文）。无任何条目时返回空串。
    pub fn injected_text(&self) -> String {
        let entries = self.entries();
        if entries.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        for section in [Section::Prefs, Section::Lessons] {
            let items: Vec<&Entry> = entries
                .iter()
                .filter(|(s, _)| *s == section)
                .map(|(_, e)| e)
                .collect();
            if items.is_empty() {
                continue;
            }
            out.push_str(section.header());
            out.push('\n');
            for e in items {
                out.push_str(&render_entry(e));
                out.push('\n');
            }
        }
        out
    }

    /// 定位目标行：分区内正文与 match_text 完全相同的条目行号。
    /// 匹配不上（含分区不存在、条目被删/改）一律报错——不硬盖、不偷偷加回来。
    fn locate(&self, section: Section, match_text: &str) -> Result<usize, String> {
        let want = match_text.trim();
        let lines: Vec<&str> = self.raw.lines().collect();
        let Some(start) = lines.iter().position(|l| l.trim() == section.header()) else {
            return Err(format!(
                "这条不在了，可能你删了或改了：{want}（{}还没有任何条目）",
                section.label()
            ));
        };
        let end = block_end(&lines, start);
        for (offset, line) in lines[start + 1..end].iter().enumerate() {
            if let Some(e) = parse_entry_line(line) {
                if e.text == want {
                    return Ok(start + 1 + offset);
                }
            }
        }
        Err(format!("这条不在了，可能你删了或改了：{want}"))
    }

    /// 加一条（写入前过 sanitize）。分区不存在就建分区头；已有分区则插到该区末尾
    /// （最后一个条目之后、块尾空行之前——用户加的空行分隔不动）。
    pub fn add(&mut self, section: Section, entry: Entry) -> Result<(), String> {
        let text = sanitize(entry.text.trim());
        if text.is_empty() {
            return Err("条目不能为空".into());
        }
        let e = Entry { text, ..entry };
        let line = render_entry(&e);
        let lines: Vec<String> = self.raw.lines().map(str::to_string).collect();
        match lines.iter().position(|l| l.trim() == section.header()) {
            Some(start) => {
                let end = block_end(&lines, start);
                // 插入点避开块尾空行（空行是用户的分区间隔，不属于条目区）
                let mut at = end;
                while at > start + 1 && lines[at - 1].trim().is_empty() {
                    at -= 1;
                }
                let mut out: Vec<String> = lines[..at].to_vec();
                out.push(line);
                out.extend_from_slice(&lines[at..]);
                self.raw = out.join("\n");
            }
            None => {
                // 分区不存在：文末追加，与已有内容之间空一行
                if !self.raw.is_empty() {
                    if !self.raw.ends_with('\n') {
                        self.raw.push('\n');
                    }
                    self.raw.push('\n');
                }
                self.raw.push_str(section.header());
                self.raw.push('\n');
                self.raw.push_str(&line);
            }
        }
        self.save()
    }

    /// 换一条：按原文匹配，只改正文，元数据（src/日期/hits/状态）原样保留。
    /// 匹配不上报错，绝不整段重写。
    pub fn replace(&mut self, section: Section, match_text: &str, new_text: &str) -> Result<(), String> {
        let i = self.locate(section, match_text)?;
        let text = sanitize(new_text.trim());
        if text.is_empty() {
            return Err("新内容不能为空".into());
        }
        let mut lines: Vec<String> = self.raw.lines().map(str::to_string).collect();
        let mut e = parse_entry_line(&lines[i]).ok_or("这条不在了，可能你删了或改了")?;
        e.text = text;
        lines[i] = render_entry(&e);
        self.raw = lines.join("\n");
        self.save()
    }

    /// 删一条：按原文匹配，只删这一行。匹配不上报错。
    pub fn remove(&mut self, section: Section, match_text: &str) -> Result<(), String> {
        let i = self.locate(section, match_text)?;
        let mut lines: Vec<String> = self.raw.lines().map(str::to_string).collect();
        lines.remove(i);
        self.raw = lines.join("\n");
        self.save()
    }

    /// 合并路径（写入前先搜、搜到就合并）：hits+1；教训区 confirm=true 时
    /// 「观察中」转「已确认」。只换那一行的元数据注释，正文一字不动。
    pub fn merge_hit(&mut self, section: Section, match_text: &str, confirm: bool) -> Result<(), String> {
        let i = self.locate(section, match_text)?;
        let mut lines: Vec<String> = self.raw.lines().map(str::to_string).collect();
        let mut e = parse_entry_line(&lines[i]).ok_or("这条不在了，可能你删了或改了")?;
        e.hits = e.hits.saturating_add(1);
        if section.has_status() && confirm && e.status.as_deref() == Some(STATUS_OBSERVING) {
            e.status = Some(STATUS_CONFIRMED.to_string());
        }
        lines[i] = render_entry(&e);
        self.raw = lines.join("\n");
        self.save()
    }

    /// 清「观察中且首次记录超 30 天」的条目（§3.6）。静默执行，返回被清掉的正文
    /// 供调用方写 events.log 留痕。日期解析不出的条目不清（宁可留着也不误杀）。
    pub fn prune_stale(&mut self, today: NaiveDate) -> Vec<String> {
        let lines: Vec<&str> = self.raw.lines().collect();
        let Some(start) = lines.iter().position(|l| l.trim() == LESSONS_HEADER) else {
            return Vec::new();
        };
        let end = block_end(&lines, start);
        let mut drop_idx = Vec::new();
        let mut removed = Vec::new();
        for (offset, line) in lines[start + 1..end].iter().enumerate() {
            let i = start + 1 + offset;
            let Some(e) = parse_entry_line(line) else { continue };
            if e.status.as_deref() != Some(STATUS_OBSERVING) {
                continue;
            }
            let Ok(d) = NaiveDate::parse_from_str(e.date.trim(), "%Y-%m-%d") else { continue };
            if (today - d).num_days() > OBSERVATION_MAX_AGE_DAYS {
                drop_idx.push(i);
                removed.push(e.text);
            }
        }
        if removed.is_empty() {
            return removed;
        }
        let kept: Vec<&str> = lines
            .iter()
            .enumerate()
            .filter(|(i, _)| !drop_idx.contains(i))
            .map(|(_, l)| *l)
            .collect();
        self.raw = kept.join("\n");
        let _ = self.save();
        removed
    }

    /// 原子写回（fsx 的临时文件 + rename；写盘期间被杀不留半截文件）
    fn save(&self) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("建 profile.md 目录失败: {e}"))?;
        }
        let mut text = self.raw.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        crate::fsx::write_atomic_str(&self.path, &text).map_err(|e| format!("写入 profile.md 失败: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-profile-{tag}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 9).unwrap()
    }

    #[test]
    fn add_creates_sections_and_appends_in_block() {
        let d = tmp("add");
        let mut p = Profile::open(&d);
        p.add(Section::Prefs, Entry::new("以后回答先给结论", "工作区", "2026-10-09", Section::Prefs)).unwrap();
        p.add(Section::Lessons, Entry::new("cargo test 失败先看前 50 行", "agent-copy", "2026-10-08", Section::Lessons)).unwrap();
        // 第二个偏好插在偏好区末尾，不跑到教训区去
        p.add(Section::Prefs, Entry::new("回复用中文", "工作区", "2026-10-09", Section::Prefs)).unwrap();
        let raw = std::fs::read_to_string(p.path()).unwrap();
        assert_eq!(
            raw,
            "## 偏好区\n- 以后回答先给结论 <!-- src: 工作区 · 2026-10-09 · hits: 1 -->\n- 回复用中文 <!-- src: 工作区 · 2026-10-09 · hits: 1 -->\n\n## 教训区\n- cargo test 失败先看前 50 行 <!-- src: agent-copy · 2026-10-08 · hits: 1 · 观察中 -->\n"
        );
        let es = p.entries();
        assert_eq!(es.len(), 3);
        assert_eq!(es[0].0, Section::Prefs);
        assert_eq!(es[2].0, Section::Lessons);
        assert_eq!(es[2].1.status.as_deref(), Some("观察中"), "教训区首记默认观察中");
        assert_eq!(es[0].1.status, None, "偏好区不带状态");
        std::fs::remove_dir_all(&d).ok();
    }

    /// 核心安全属性：用户手改的其他行（含分区外的自注、空行、格式）一字不动
    #[test]
    fn line_level_ops_never_touch_user_lines() {
        let d = tmp("userlines");
        std::fs::write(
            d.join("profile.md"),
            "# 我的自动记忆\n\n## 偏好区\n- 旧偏好甲 <!-- src: 工作区 · 2026-10-01 · hits: 3 -->\n- 旧偏好乙 <!-- src: 工作区 · 2026-10-02 · hits: 1 -->\n\n## 教训区\n- 坑一 <!-- src: agent-copy · 2026-10-03 · hits: 2 · 已确认 -->\n\n<!-- 用户自己加的备注：这几条是我手挑的 -->\n",
        )
        .unwrap();
        let mut p = Profile::open(&d);
        // 改一行：只动这一行
        p.replace(Section::Prefs, "旧偏好甲", "新偏好甲").unwrap();
        // 合并一行：只换元数据注释
        p.merge_hit(Section::Prefs, "旧偏好乙", false).unwrap();
        // 加一行：插在偏好区末尾（用户加的空行分隔不动）
        p.add(Section::Prefs, Entry::new("偏好丙", "工作区", "2026-10-09", Section::Prefs)).unwrap();
        let raw = std::fs::read_to_string(p.path()).unwrap();
        assert_eq!(
            raw,
            "# 我的自动记忆\n\n## 偏好区\n- 新偏好甲 <!-- src: 工作区 · 2026-10-01 · hits: 3 -->\n- 旧偏好乙 <!-- src: 工作区 · 2026-10-02 · hits: 2 -->\n- 偏好丙 <!-- src: 工作区 · 2026-10-09 · hits: 1 -->\n\n## 教训区\n- 坑一 <!-- src: agent-copy · 2026-10-03 · hits: 2 · 已确认 -->\n\n<!-- 用户自己加的备注：这几条是我手挑的 -->\n"
        );
        // 删一行：只删这一行
        p.remove(Section::Prefs, "偏好丙").unwrap();
        let raw2 = std::fs::read_to_string(p.path()).unwrap();
        assert!(!raw2.contains("偏好丙"));
        assert!(raw2.contains("<!-- 用户自己加的备注"), "用户备注必须保留");
        assert!(raw2.contains("- 新偏好甲"), "改过的那行必须保留");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn match_failure_errors_and_never_readds() {
        let d = tmp("nomatch");
        let mut p = Profile::open(&d);
        p.add(Section::Prefs, Entry::new("偏好甲", "ws", "2026-10-09", Section::Prefs)).unwrap();
        // 匹配不上：报错且文件不变（不硬盖、不偷偷加回来）
        let before = std::fs::read_to_string(p.path()).unwrap();
        assert!(p.replace(Section::Prefs, "不存在的条目", "x").unwrap_err().contains("这条不在了"));
        assert!(p.remove(Section::Prefs, "偏好甲改过一个字").unwrap_err().contains("这条不在了"));
        assert!(p.merge_hit(Section::Lessons, "偏好甲", true).unwrap_err().contains("这条不在了"));
        assert_eq!(std::fs::read_to_string(p.path()).unwrap(), before, "报错不得改动文件");
        // 用户删掉的条目不偷偷加回来
        p.remove(Section::Prefs, "偏好甲").unwrap();
        assert!(p.merge_hit(Section::Prefs, "偏好甲", false).is_err());
        assert_eq!(p.entries().len(), 0);
        // 空文本拒绝
        assert!(p.add(Section::Prefs, Entry::new("  ", "ws", "2026-10-09", Section::Prefs)).is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn merge_hit_confirms_observation_only_in_lessons() {
        let d = tmp("merge");
        let mut p = Profile::open(&d);
        p.add(Section::Lessons, Entry::new("坑一", "ws", "2026-10-09", Section::Lessons)).unwrap();
        p.add(Section::Prefs, Entry::new("偏好甲", "ws", "2026-10-09", Section::Prefs)).unwrap();
        // 第二次佐证：观察中 → 已确认，hits+1
        p.merge_hit(Section::Lessons, "坑一", true).unwrap();
        let es = p.entries();
        let lesson = es.iter().find(|(s, _)| *s == Section::Lessons).unwrap();
        assert_eq!(lesson.1.hits, 2);
        assert_eq!(lesson.1.status.as_deref(), Some("已确认"));
        // 偏好区 confirm 不起状态（偏好区本来就不带状态）
        p.merge_hit(Section::Prefs, "偏好甲", true).unwrap();
        let prefs = p.entries().into_iter().find(|(s, _)| *s == Section::Prefs).unwrap();
        assert_eq!(prefs.1.hits, 2);
        assert_eq!(prefs.1.status, None);
        // 已确认的再合并不退回观察中
        p.merge_hit(Section::Lessons, "坑一", true).unwrap();
        let lesson2 = p.entries().into_iter().find(|(s, _)| *s == Section::Lessons).unwrap();
        assert_eq!(lesson2.1.hits, 3);
        assert_eq!(lesson2.1.status.as_deref(), Some("已确认"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn prune_stale_clears_only_old_observations() {
        let d = tmp("prune");
        std::fs::write(
            d.join("profile.md"),
            "## 教训区\n- 老观察条目 <!-- src: ws · 2026-08-01 · hits: 1 · 观察中 -->\n- 新观察条目 <!-- src: ws · 2026-10-08 · hits: 1 · 观察中 -->\n- 老已确认条目 <!-- src: ws · 2026-07-01 · hits: 2 · 已确认 -->\n- 日期坏了 <!-- src: ws · 不是日期 · hits: 1 · 观察中 -->\n\n## 偏好区\n- 老偏好 <!-- src: ws · 2026-01-01 · hits: 1 -->\n",
        )
        .unwrap();
        let mut p = Profile::open(&d);
        let removed = p.prune_stale(today());
        assert_eq!(removed, vec!["老观察条目".to_string()], "只清超 30 天的观察中");
        let raw = std::fs::read_to_string(p.path()).unwrap();
        assert!(!raw.contains("老观察条目"));
        assert!(raw.contains("新观察条目"), "30 天内的观察中不清");
        assert!(raw.contains("老已确认条目"), "已确认的教训不清（归宿是上限预警的压缩清单）");
        assert!(raw.contains("日期坏了"), "日期解析不出不清（宁可留着不误杀）");
        assert!(raw.contains("老偏好"), "偏好区永不被自动清");
        // 幂等：再扫一遍无事
        assert!(p.prune_stale(today()).is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn chars_counts_whole_file_and_sanitize_runs_on_write() {
        let d = tmp("chars");
        let mut p = Profile::open(&d);
        assert_eq!(p.chars(), 0, "空文件 0 字符");
        p.add(Section::Prefs, Entry::new("密钥是 sk-abc123456789 别记", "ws", "2026-10-09", Section::Prefs)).unwrap();
        let raw = std::fs::read_to_string(p.path()).unwrap();
        assert!(raw.contains("sk-***"), "写入前过 sanitize: {raw}");
        assert!(!raw.contains("abc123456789"));
        assert_eq!(p.chars(), raw.chars().count());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn crlf_and_missing_file_are_tolerated() {
        let d = tmp("crlf");
        std::fs::write(
            d.join("profile.md"),
            "## 偏好区\r\n- 偏好甲 <!-- src: ws · 2026-10-09 · hits: 1 -->\r\n",
        )
        .unwrap();
        let mut p = Profile::open(&d);
        assert_eq!(p.entries().len(), 1, "CRLF 文件照常解析");
        p.merge_hit(Section::Prefs, "偏好甲", false).unwrap();
        let raw = std::fs::read_to_string(p.path()).unwrap();
        assert!(!raw.contains("\r\n"), "写出统一 \\n 行尾");
        assert!(raw.contains("hits: 2"));
        // 文件不存在：现开为空，首次写入才落盘
        let d2 = tmp("missing");
        let p2 = Profile::open(&d2);
        assert_eq!(p2.entries().len(), 0);
        assert!(!d2.join("profile.md").exists(), "只读不开火");
        std::fs::remove_dir_all(&d).ok();
        std::fs::remove_dir_all(&d2).ok();
    }

    #[test]
    fn injected_text_skips_empty_sections_and_outside_notes() {
        let d = tmp("inject");
        std::fs::write(
            d.join("profile.md"),
            "# 用户自己加的标题备注\n\n## 偏好区\n- 偏好甲 <!-- src: ws · 2026-10-09 · hits: 2 -->\n\n## 教训区\n\n## 别的区\n- 不该进注入 <!-- src: ws · 2026-10-09 · hits: 1 -->\n",
        )
        .unwrap();
        let p = Profile::open(&d);
        let t = p.injected_text();
        assert!(t.starts_with("## 偏好区"), "{t}");
        assert!(t.contains("- 偏好甲 <!-- src: ws · 2026-10-09 · hits: 2 -->"), "{t}");
        assert!(!t.contains("教训区"), "空分区不注入: {t}");
        assert!(!t.contains("用户自己加的标题备注"), "分区外自注不进注入: {t}");
        assert!(!t.contains("不该进注入"), "未知分区不进注入: {t}");
        // 一个条目都没有：整块空
        let d2 = tmp("inject-empty");
        std::fs::write(d2.join("profile.md"), "## 偏好区\n\n## 教训区\n").unwrap();
        assert_eq!(Profile::open(&d2).injected_text(), "");
        // 文件不存在：同样是空
        let d3 = tmp("inject-missing");
        assert_eq!(Profile::open(&d3).injected_text(), "");
        std::fs::remove_dir_all(&d).ok();
        std::fs::remove_dir_all(&d2).ok();
        std::fs::remove_dir_all(&d3).ok();
    }

    #[test]
    fn unknown_sections_and_stray_lines_survive() {
        let d = tmp("unknown");
        std::fs::write(
            d.join("profile.md"),
            "## 自定义区\n- 用户自己的一行 <!-- 随便写 -->\n\n普通文字段落，不是条目\n\n## 偏好区\n- 偏好甲 <!-- src: ws · 2026-10-09 · hits: 1 -->\n",
        )
        .unwrap();
        let mut p = Profile::open(&d);
        assert_eq!(p.entries().len(), 1, "只解释已知分区");
        p.add(Section::Prefs, Entry::new("偏好乙", "ws", "2026-10-09", Section::Prefs)).unwrap();
        let raw = std::fs::read_to_string(p.path()).unwrap();
        assert!(raw.contains("## 自定义区\n- 用户自己的一行 <!-- 随便写 -->"), "未知分区原样保留");
        assert!(raw.contains("普通文字段落，不是条目"), "分区外文字原样保留");
        assert!(raw.contains("- 偏好乙"));
        std::fs::remove_dir_all(&d).ok();
    }
}
