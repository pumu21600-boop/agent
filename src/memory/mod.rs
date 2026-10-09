//! 记忆系统 v1 —— 总架构 §9 / 施工单 009
//!
//! 双全局根（2026-10-09 回滚 v0.32.32 搬根后定案，各管各的文件）：
//! - 用户约定根 `~/.agent/`（按 Windows 用户，永久生效）：只有用户手写的全局约定
//!   AGENTS.md，agent 绝不写它；
//! - 安装状态根 `<app_root>/.agent/`（按安装，一份安装一份）：index.json 项目登记
//!   + profile.md 自动记忆（读写层见施工单第 1 步）。
//! 项目层 `<workspace>/.agent/memory/`（与影子仓库同根）：
//! - events.log：任务/工具/交接/检查点的事件流水，跟 git 仓库走沉淀"经验"；
//!   默认脱敏（sk-/Bearer/api_key 打码），`persist_sensitive=true` 才存原文。
//! - AGENTS.md：全局 + 项目双层读取注入 system prompt；项目缺失时生成模板（开箱即用）。
//!
//! 语义记忆/向量检索在 memory/library.rs（EMBEDDING 角色渠道 + 本地哈希兜底）。

pub mod library;

use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::MemoryConfig;

/// 安装状态根：应用自己目录下的 `.agent`（按安装算，一份安装一份）。
///
/// 管 index.json（项目登记）与 profile.md（自动记忆）。v0.32.32 曾把全局约定
/// AGENTS.md 也放这里，导致用户改 `~/.agent/AGENTS.md` 没有任何效果（线上活
/// bug），2026-10-09 回滚：AGENTS.md 归 user_global_dir()，本根只管 agent
/// 自己的账。
pub fn install_state_dir() -> PathBuf {
    crate::config::app_root().join(".agent")
}

/// 用户约定根：`~/.agent`（按 Windows 用户算，永久生效，不随安装走）。
///
/// 只管用户手写的全局约定 AGENTS.md——"我这个人在所有项目里通用的设定"本来就
/// 该按用户算。agent 绝不写这个目录里的 AGENTS.md。
pub fn user_global_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".agent")
}

/// 把旧全局根（`~/.agent`，搬根前的位置）里的 index.json 并到安装状态根。
/// 只搬「新根没有」的——新根已有的，那是这份安装自己的账，绝不覆盖。
/// 旧根永不删：什么时候清理由用户决定。返回本次搬过来的文件名。
///
/// AGENTS.md 不搬：它的语义正主就是旧根（用户手写），回滚后无需迁移。
pub fn migrate_legacy_global(new_dir: &Path) -> Vec<&'static str> {
    migrate_legacy_global_at(new_dir, &user_global_dir())
}

fn migrate_legacy_global_at(new_dir: &Path, old_dir: &Path) -> Vec<&'static str> {
    if old_dir == new_dir {
        return Vec::new();
    }
    let mut moved = Vec::new();
    for name in ["index.json"] {
        let src = old_dir.join(name);
        let dst = new_dir.join(name);
        if dst.exists() || !src.is_file() {
            continue;
        }
        if std::fs::create_dir_all(new_dir).is_err() {
            continue;
        }
        if std::fs::copy(&src, &dst).is_ok() {
            moved.push(name);
        }
    }
    moved
}

/// 回滚全局约定路径（v0.32.32 活 bug 的收尾，§5）：AGENTS.md 的语义正主回到
/// `~/.agent`，安装状态根里那份 v0.32.32 遗留副本失效——内容不一致（含一方为空、
/// 一方非空的不对称情形）时备份为 `AGENTS.md.bak` 不删，一致（含两份都空）时直接
/// 移除冗余副本。返回告知语（无冲突 None，由启动方转告用户）。
///
/// 告知条件不能写成"两份都非空且不同"：那恰好排除了最危险的方向——`~/.agent` 空
/// 而安装根有内容时，回滚后后者失效进 .bak 却不告知，用户以为约定还在生效，正是
/// 本方案要消灭的"静默"。
pub fn rollback_global_agents_at(install_dir: &Path, user_dir: &Path) -> Option<String> {
    let install_agents = install_dir.join("AGENTS.md");
    if !install_agents.is_file() {
        return None;
    }
    // 同址（开发形态）时别把用户唯一那份删了
    if install_dir == user_dir {
        return None;
    }
    let user_agents = user_dir.join("AGENTS.md");
    // 行尾归一后比较：git checkout / 手动编辑带来的 CRLF 差异不算内容不一致
    let norm = |p: &Path| -> String {
        std::fs::read_to_string(p)
            .map(|t| t.replace("\r\n", "\n").trim().to_string())
            .unwrap_or_default()
    };
    let (a, b) = (norm(&install_agents), norm(&user_agents));
    if a == b {
        // 一致（含两份都空）：安装根那份是冗余副本，内容在用户根原样保留
        let _ = std::fs::remove_file(&install_agents);
        return None;
    }
    // 不一致：以 ~/.agent 为准。已存在旧 .bak 就不覆盖——第一份备份是最早的证据
    let bak = install_dir.join("AGENTS.md.bak");
    if !bak.exists() {
        let _ = std::fs::copy(&install_agents, &bak);
    }
    let _ = std::fs::remove_file(&install_agents);
    Some(format!(
        "发现两份全局约定内容不一致，已以 {} 为准，另一份备份在 {}，请确认",
        user_agents.display(),
        bak.display()
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub key: String,
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub remote: Option<String>,
    pub last_used: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
}

/// 空白模板：未改动的模板视为"留空"，不注入任何内容。
pub const AGENTS_TEMPLATE: &str = "# AGENTS.md\n\n<!-- 项目设定（用户自填）：你希望 agent 每次开工都知道的事——构建命令、代码约定、禁区。留空即不注入任何内容。 -->\n";

/// 读一份 AGENTS.md；缺失、空白或仍是未改动模板时返回空串（不注入）。
fn agents_text(label: &str, path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else { return String::new() };
    // 行尾归一（git checkout 可能把模板转成 CRLF，否则与常量 trim 不相等）
    let t = text.replace("\r\n", "\n").trim().to_string();
    if t.is_empty() || t == AGENTS_TEMPLATE.trim() {
        return String::new();
    }
    format!("\n\n【{label} AGENTS.md】\n{t}")
}

/// 两个全局根的落点（open() 生产路径现算，open_at() 测试隔离传入）。
#[derive(Debug, Clone)]
pub struct GlobalDirs {
    /// 安装状态根：index.json（+ profile.md）
    pub install: PathBuf,
    /// 用户约定根：AGENTS.md（用户手写）
    pub user: PathBuf,
}

/// 打开的记忆层（持有路径与开关，核心循环/server 共享一个实例）。
pub struct Memory {
    pub enabled: bool,
    persist_sensitive: bool,
    dirs: GlobalDirs,
    project_dir: PathBuf,
    ws: PathBuf,
    pub key: String,
    pub name: String,
    pub remote: Option<String>,
    /// 启动回滚发现的全局约定冲突告知（None = 无冲突）；面板与日志转告用户
    agents_conflict: Option<String>,
}

impl Memory {
    /// 计算项目身份、建目录、登记 index.json（enabled 时）。
    pub fn open(ws: &Path, cfg: &MemoryConfig) -> Self {
        let dirs = GlobalDirs {
            install: install_state_dir(),
            user: user_global_dir(),
        };
        // 搬根迁移：只在记忆开着时做——关着时不落任何文件（含迁移）
        if cfg.enabled {
            migrate_legacy_global(&dirs.install);
        }
        // 全局约定回滚不论记忆开关：AGENTS.md 注入本就不看 enabled，路径修错时
        // 用户改 ~/.agent 永远不生效，这个告知不能跟着开关一起哑
        let agents_conflict = rollback_global_agents_at(&dirs.install, &dirs.user);
        if let Some(ref notice) = agents_conflict {
            crate::observability::debug(&format!("[memory] {notice}"));
            eprintln!("[memory] {notice}");
        }
        let mut mem = Self::open_at(ws, cfg, dirs);
        mem.agents_conflict = agents_conflict;
        mem
    }

    /// 指定两个全局根的落点（测试隔离用；生产走 open()）。
    pub fn open_at(ws: &Path, cfg: &MemoryConfig, dirs: GlobalDirs) -> Self {
        let remote = crate::recovery::remote_url(ws);
        let root = crate::recovery::root_path(ws).unwrap_or_else(|| ws.to_path_buf());
        let key = match &remote {
            Some(r) => r.trim().to_lowercase(),
            None => root.to_string_lossy().replace('\\', "/").to_lowercase(),
        };
        let name = ws
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "project".to_string());
        let mem = Self {
            enabled: cfg.enabled,
            persist_sensitive: cfg.persist_sensitive,
            dirs,
            project_dir: ws.join(".agent").join("memory"),
            ws: ws.to_path_buf(),
            key,
            name,
            remote,
            agents_conflict: None,
        };
        if mem.enabled {
            if let Err(e) = std::fs::create_dir_all(&mem.project_dir) {
                crate::observability::debug(&format!("[memory] 建项目记忆目录失败: {e}"));
            }
            mem.register_index(ws);
        }
        mem
    }

    fn index_path(&self) -> PathBuf {
        self.dirs.install.join("index.json")
    }

    /// 登记/更新本项目在 index.json 的条目（按 key 去重，last_used 刷新）。
    fn register_index(&self, ws: &Path) {
        let mut idx = self.load_index();
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let entry = ProjectEntry {
            key: self.key.clone(),
            name: self.name.clone(),
            path: ws.to_string_lossy().to_string(),
            remote: self.remote.clone(),
            last_used: now,
        };
        match idx.projects.iter_mut().find(|p| p.key == self.key) {
            Some(old) => *old = entry,
            None => idx.projects.push(entry),
        }
        // 防膨胀：只留最近 50 个项目
        if idx.projects.len() > 50 {
            idx.projects.sort_by(|a, b| b.last_used.cmp(&a.last_used));
            idx.projects.truncate(50);
        }
        if let Err(e) = std::fs::create_dir_all(&self.dirs.install) {
            crate::observability::debug(&format!("[memory] 建全局目录失败: {e}"));
            return;
        }
        let text = serde_json::to_string_pretty(&idx).unwrap_or_default();
        // 2026-10-08 修：原子写。裸 fs::write 是「截断→写」，进程在写盘期间被杀
        // （断电/强杀）会留下空/半截文件；而 load_index 对解析失败
        // unwrap_or_default() 返回空表，下一次 register_index 就把只有当前项目的
        // 空表写回——此前登记的全部项目静默抹掉。同文件的 save_agents 早就用
        // fsx::write_atomic_str，这里补上。
        if let Err(e) = crate::fsx::write_atomic_str(&self.index_path(), &text) {
            crate::observability::debug(&format!("[memory] 写 index.json 失败: {e}"));
        }
    }

    pub fn load_index(&self) -> Index {
        match std::fs::read_to_string(self.index_path()) {
            Ok(t) => serde_json::from_str(&t).unwrap_or_default(),
            Err(_) => Index::default(),
        }
    }

    /// 追加一条事件到项目层 events.log（脱敏后落盘）。
    pub fn record(&self, kind: &str, payload: Value) {
        if !self.enabled {
            return;
        }
        let mut rec = json!({
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            "session": crate::observability::get().session_id(),
            "kind": kind,
        });
        if let (Some(obj), Value::Object(po)) = (rec.as_object_mut(), &payload) {
            for (k, v) in po {
                obj.insert(k.clone(), v.clone());
            }
        }
        let mut line = serde_json::to_string(&rec).unwrap_or_default();
        if !self.persist_sensitive {
            line = sanitize(&line);
        }
        line.push('\n');
        let path = self.project_dir.join("events.log");
        use std::io::Write;
        match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut f) => {
                let _ = f.write_all(line.as_bytes());
            }
            Err(e) => crate::observability::debug(&format!("[memory] events.log 写入失败: {e}")),
        }
    }

    /// 双层 AGENTS.md：全局在前、项目在后（项目优先级更高，靠后覆盖语义）。
    /// 全局层读用户约定根（~/.agent/AGENTS.md，2026-10-09 回滚后不再读安装根）。
    pub fn agents_md(&self) -> String {
        let mut out = String::new();
        out.push_str(&agents_text("全局约定", &self.dirs.user.join("AGENTS.md")));
        out.push_str(&agents_text("项目约定", &self.ws.join("AGENTS.md")));
        out
    }

    // —— 设置面板「记忆」分区（只读状态 + AGENTS.md 编辑）——
    /// 安装状态根（index.json / profile.md 所在，按安装隔离）。
    pub fn install_state_dir(&self) -> &Path {
        &self.dirs.install
    }
    /// 用户约定根（~/.agent，全局约定 AGENTS.md 所在，按用户）。
    pub fn user_global_dir(&self) -> &Path {
        &self.dirs.user
    }
    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }
    /// 全局约定 AGENTS.md 路径（用户根 ~/.agent/AGENTS.md；v0.32.32 曾错指安装根，
    /// 2026-10-09 回滚）。
    pub fn global_agents_path(&self) -> PathBuf {
        self.dirs.user.join("AGENTS.md")
    }
    /// 启动回滚的冲突告知语（无冲突 None）。
    pub fn agents_conflict(&self) -> Option<&str> {
        self.agents_conflict.as_deref()
    }
    /// 项目层 AGENTS.md 路径（工作区根 AGENTS.md）。
    pub fn project_agents_path(&self) -> PathBuf {
        self.ws.join("AGENTS.md")
    }
    /// 项目层 events.log 行数（无文件为 0）。
    pub fn events_lines(&self) -> usize {
        std::fs::read_to_string(self.project_dir.join("events.log"))
            .map(|c| c.lines().count())
            .unwrap_or(0)
    }
    /// AGENTS.md 原文（编辑器用；缺省返回模板，模板不注入只展示）。
    /// scope: "global"（~/.agent/AGENTS.md）| "project"（工作区根 AGENTS.md）
    pub fn agents_raw(&self, scope: &str) -> String {
        let path = match scope {
            "global" => self.dirs.user.join("AGENTS.md"),
            _ => self.ws.join("AGENTS.md"),
        };
        std::fs::read_to_string(path).unwrap_or_else(|_| {
            if scope == "global" {
                String::new()
            } else {
                AGENTS_TEMPLATE.to_string()
            }
        })
    }
    /// 保存用户编辑的 AGENTS.md（设置面板写入；下次任务注入自动生效）。
    /// global 写用户约定根（~/.agent）——那是用户的地盘，agent 只在这里落用户编辑。
    pub fn save_agents(&self, scope: &str, content: &str) -> anyhow::Result<PathBuf> {
        let path = match scope {
            "global" => {
                std::fs::create_dir_all(&self.dirs.user)?;
                self.dirs.user.join("AGENTS.md")
            }
            _ => self.ws.join("AGENTS.md"),
        };
        crate::fsx::write_atomic_str(&path, content)?;
        Ok(path)
    }

    /// 项目根无 AGENTS.md 时生成空白模板（开箱即用；内容由用户自填，留空=不注入）。
    pub fn ensure_agents_template(&self, ws: &Path) {
        if !self.enabled {
            return;
        }
        let path = ws.join("AGENTS.md");
        if path.exists() {
            return;
        }
        if let Err(e) = crate::fsx::write_atomic_str(&path, AGENTS_TEMPLATE) {
            crate::observability::debug(&format!("[memory] 生成 AGENTS.md 模板失败: {e}"));
        }
    }
}

/// 敏感打码（单遍扫描，替换后不回扫）：sk- 开头的 key、Bearer 令牌、密钥字段值。
/// 2026-10-08 修：键名族改大小写不敏感并扩面——原先是 api_key/apikey/API_KEY
/// 三个字面量精确匹配，api-key/Api-Key/apiKey/Authorization/password/secret/
/// access_key/常见 token 写法全漏，裸令牌原样落进记忆日志（与模块头「默认
/// 脱敏」的宣称不符）。sk- 的键字符集补 '.'（含点的 key 原先只掩一半）。
const SECRET_KEYS: &[&str] = &[
    "api_key", "apikey", "api-key", "authorization", "password", "passwd", "secret",
    "access_key", "private_key", "auth_token", "access_token", "refresh_token",
    "id_token", "client_secret",
];

pub fn sanitize(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(text.len());
    let starts = |i: usize, pat: &str| -> bool {
        let p: Vec<char> = pat.chars().collect();
        i + p.len() <= n && chars[i..i + p.len()] == p[..]
    };
    // 大小写不敏感的前缀匹配（键名族用）
    let starts_ci = |i: usize, pat: &str| -> bool {
        let p: Vec<char> = pat.chars().collect();
        i + p.len() <= n
            && chars[i..i + p.len()]
                .iter()
                .zip(p.iter())
                .all(|(a, b)| a.to_ascii_lowercase() == b.to_ascii_lowercase())
    };
    let mut i = 0usize;
    while i < n {
        // sk-xxxx（≥4 个 key 字符才算，避免误伤普通词）
        if starts(i, "sk-") {
            let mut j = i + 3;
            while j < n
                && (chars[j].is_ascii_alphanumeric()
                    || chars[j] == '-'
                    || chars[j] == '_'
                    || chars[j] == '.')
            {
                j += 1;
            }
            if j - (i + 3) >= 4 {
                out.push_str("sk-***");
                i = j;
                continue;
            }
        }
        // Bearer xxxx
        if starts(i, "Bearer ") {
            out.push_str("Bearer ***");
            let mut j = i + 7;
            while j < n && !(chars[j].is_whitespace() || chars[j] == '"' || chars[j] == '\\') {
                j += 1;
            }
            i = j;
            continue;
        }
        // 密钥字段：名字与分隔符沿用约定，值打码（键名族大小写不敏感 + 词边界——
        // 不查边界会把 "passwords are rotated" 里的 "are" 也掩掉）
        let mut handled = false;
        let at_word_start = i == 0 || {
            let prev = chars[i - 1];
            !(prev.is_ascii_alphanumeric() || prev == '_' || prev == '-')
        };
        if at_word_start {
            for pat in SECRET_KEYS {
                if starts_ci(i, pat) {
                    out.push_str(&chars[i..i + pat.chars().count()].iter().collect::<String>());
                    let mut j = i + pat.chars().count();
                    while j < n && (chars[j] == ':' || chars[j] == '=' || chars[j] == '"' || chars[j].is_whitespace()) {
                        out.push(chars[j]);
                        j += 1;
                    }
                    let mut k = j;
                    while k < n && chars[k] != '"' && chars[k] != ',' && chars[k] != '}' && !chars[k].is_whitespace() {
                        k += 1;
                    }
                    if k > j {
                        out.push_str("***");
                    }
                    i = k;
                    handled = true;
                    break;
                }
            }
        }
        if handled {
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_logger() {
        static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| {
            let dir = std::env::temp_dir().join("agent-test-logs");
            let _ = crate::observability::init(&dir, "memory.jsonl", format!("mem-{}", uuid::Uuid::new_v4()));
        });
    }

    #[test]
    fn sanitize_masks_keys() {
        let s = r#"{"api_key":"abc123XYZ","auth":"Bearer ghp_token123","note":"key=sk-abc123XYZ_-def 和 sk-ab 不是key"}"#;
        let out = sanitize(s);
        assert!(out.contains("sk-***"), "独立 sk key 应打码: {out}");
        assert!(out.contains("Bearer ***"), "Bearer 应打码: {out}");
        assert!(!out.contains("abc123XYZ"), "key 原文不得残留: {out}");
        assert!(!out.contains("ghp_token123"), "token 原文不得残留: {out}");
        assert!(out.contains("sk-ab "), "太短的 sk- 不误伤: {out}");
        // 幂等：再扫一遍不炸不改
        let again = sanitize(&out);
        assert_eq!(again, out);
    }

    /// 2026-10-08 修：键名族扩面 + 大小写不敏感。原先是三个字面量精确匹配，
    /// api-key / Api-Key / apiKey / Authorization / password / secret / token
    /// 等常见写法全漏，裸令牌原样落进记忆日志。
    #[test]
    fn sanitize_covers_key_name_variants() {
        for s in [
            r#"{"api-key":"abc123XYZ"}"#,
            r#"{"Api-Key":"abc123XYZ"}"#,
            r#"{"apiKey":"abc123XYZ"}"#,
            r#"{"Authorization":"abc123XYZ"}"#,
            r#"password = "hunter2hunter2""#,
            r#"{"client_secret": "abc123XYZ"}"#,
            "access_token: abc123XYZ",
        ] {
            let out = sanitize(s);
            assert!(!out.contains("abc123XYZ") && !out.contains("hunter2hunter2"), "密钥必须打码: {s} → {out}");
            assert!(out.contains("***"), "应有打码标记: {s} → {out}");
        }
        // 普通 prose 不误伤（没有密钥字段名/令牌形态的词照旧）
        let plain = sanitize("token 数量是 42，今天没配密码");
        assert!(plain.contains("42"), "普通内容不该被打码: {plain}");
    }

    #[test]
    fn index_register_and_events_log() {
        init_logger();
        let tmp = std::env::temp_dir().join(format!("agent-p7-mem-{}", uuid::Uuid::new_v4().simple()));
        let gtmp = std::env::temp_dir().join(format!("agent-p7-glob-{}", uuid::Uuid::new_v4().simple()));
        let utmp = std::env::temp_dir().join(format!("agent-p7-user-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&tmp).unwrap();
        let cfg = MemoryConfig { enabled: true, persist_sensitive: false };
        let dirs = GlobalDirs { install: gtmp.clone(), user: utmp.clone() };
        let mem = Memory::open_at(&tmp, &cfg, dirs.clone());
        assert!(mem.enabled);
        // 项目身份 = 路径（临时目录非 git 仓库）
        assert!(mem.key.contains("agent-p7-mem"), "key 应含目录名: {}", mem.key);
        // index.json 登记在隔离的安装状态根
        let idx = mem.load_index();
        assert_eq!(idx.projects.len(), 1, "应登记本项目");
        assert_eq!(idx.projects[0].key, mem.key);

        // events.log 追加 + 脱敏
        mem.record("task.start", json!({"text": "修编译错误", "key": "sk-secretvalue123"}));
        mem.record("tool.call", json!({"name": "shell", "danger": 2, "ok": true}));
        let log = std::fs::read_to_string(tmp.join(".agent").join("memory").join("events.log")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("task.start") && lines[0].contains("修编译错误"));
        assert!(lines[0].contains("sk-***") && !lines[0].contains("secretvalue"), "应脱敏: {}", lines[0]);

        // 删掉整个 .agent 后重开 → 自动重建（静默原则）
        std::fs::remove_dir_all(tmp.join(".agent")).unwrap();
        let mem2 = Memory::open_at(&tmp, &cfg, dirs);
        assert!(tmp.join(".agent").join("memory").exists());
        mem2.record("task.finish", json!({"steps": 1}));
        assert!(tmp.join(".agent").join("memory").join("events.log").exists());

        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(&gtmp).ok();
        std::fs::remove_dir_all(&utmp).ok();
    }

    /// 2026-10-09 回滚：两个全局根分离——AGENTS.md 归用户根（~/.agent，按用户），
    /// index.json 归安装状态根（app_root/.agent，按安装）。
    #[test]
    fn global_roots_split_user_vs_install() {
        assert_eq!(install_state_dir(), crate::config::app_root().join(".agent"));
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default();
        if !home.is_empty() {
            assert_eq!(user_global_dir(), std::path::Path::new(&home).join(".agent"));
        }
        assert_ne!(install_state_dir(), user_global_dir(), "两个根不能同址");
    }

    /// 搬根迁移只搬 index.json：AGENTS.md 的语义正主就是用户根（~/.agent），
    /// 回滚后无需迁移；别的一概不碰。
    #[test]
    fn migrate_legacy_global_copies_only_index() {
        let tag = uuid::Uuid::new_v4().simple();
        let old = std::env::temp_dir().join(format!("agent-mig-old-{tag}"));
        let new = std::env::temp_dir().join(format!("agent-mig-new-{tag}"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("AGENTS.md"), "用户亲手写的约定").unwrap();
        std::fs::write(old.join("index.json"), r#"{"projects":[]}"#).unwrap();
        std::fs::create_dir_all(old.join("skills")).unwrap();
        std::fs::write(old.join("skills").join("keep.txt"), "别碰我").unwrap();

        let moved = migrate_legacy_global_at(&new, &old);
        assert_eq!(moved, vec!["index.json"], "只搬 index.json");
        assert!(!new.join("AGENTS.md").exists(), "AGENTS.md 不搬：用户根就是正主");
        assert_eq!(std::fs::read_to_string(new.join("index.json")).unwrap(), r#"{"projects":[]}"#);
        assert!(!new.join("skills").exists(), "记忆文件之外的一概不碰");

        // 幂等 + 绝不覆盖：新根已有 = 这份安装自己的账
        std::fs::write(new.join("index.json"), r#"{"projects":["这份安装自己的"]}"#).unwrap();
        assert!(migrate_legacy_global_at(&new, &old).is_empty(), "新根已有就不搬");
        assert_eq!(std::fs::read_to_string(new.join("index.json")).unwrap(), r#"{"projects":["这份安装自己的"]}"#);

        // 旧根永不删
        assert!(old.join("AGENTS.md").exists() && old.join("index.json").exists());
        // 同根 = 空操作（防自拷贝）
        assert!(migrate_legacy_global_at(&old, &old).is_empty());

        std::fs::remove_dir_all(&old).ok();
        std::fs::remove_dir_all(&new).ok();
    }

    /// 回滚：两份不一致（最危险方向=用户根空、安装根有内容）→ .bak 备份 + 移除副本 + 告知。
    #[test]
    fn rollback_global_agents_conflict_backs_up_and_notifies() {
        let tag = uuid::Uuid::new_v4().simple();
        let install = std::env::temp_dir().join(format!("agent-rb-inst-{tag}"));
        let user = std::env::temp_dir().join(format!("agent-rb-user-{tag}"));
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(install.join("AGENTS.md"), "v0.32.32 时写下的约定").unwrap();

        let notice = rollback_global_agents_at(&install, &user).expect("不一致必须告知");
        assert!(notice.contains("AGENTS.md.bak"), "告知要点明备份去处: {notice}");
        assert!(!install.join("AGENTS.md").exists(), "安装根副本失效");
        assert_eq!(
            std::fs::read_to_string(install.join("AGENTS.md.bak")).unwrap(),
            "v0.32.32 时写下的约定",
            "旧内容备份不删"
        );
        assert!(!user.join("AGENTS.md").exists(), "不往用户根写内容——只告知");

        // 幂等：副本已不在，再来一遍不响不炸
        assert!(rollback_global_agents_at(&install, &user).is_none());

        std::fs::remove_dir_all(&install).ok();
        std::fs::remove_dir_all(&user).ok();
    }

    /// 回滚：一致或两份都空 → 静默移除冗余副本不告知；安装根没有副本 → 无事发生。
    #[test]
    fn rollback_global_agents_silent_when_no_conflict() {
        let tag = uuid::Uuid::new_v4().simple();
        let install = std::env::temp_dir().join(format!("agent-rb2-inst-{tag}"));
        let user = std::env::temp_dir().join(format!("agent-rb2-user-{tag}"));
        std::fs::create_dir_all(&install).unwrap();
        std::fs::create_dir_all(&user).unwrap();

        // 安装根没有 AGENTS.md：无事
        assert!(rollback_global_agents_at(&install, &user).is_none());

        // 两份一致：静默移除副本，用户根原样不动，不留 .bak
        std::fs::write(user.join("AGENTS.md"), "同一份约定").unwrap();
        std::fs::write(install.join("AGENTS.md"), "同一份约定").unwrap();
        assert!(rollback_global_agents_at(&install, &user).is_none(), "一致不告知");
        assert!(!install.join("AGENTS.md").exists(), "冗余副本移除");
        assert!(!install.join("AGENTS.md.bak").exists(), "一致不需要备份");
        assert_eq!(std::fs::read_to_string(user.join("AGENTS.md")).unwrap(), "同一份约定");

        // 两份都空（用户根文件不在读为空白）：静默移除空副本
        std::fs::remove_file(user.join("AGENTS.md")).unwrap();
        std::fs::write(install.join("AGENTS.md"), "").unwrap();
        assert!(rollback_global_agents_at(&install, &user).is_none(), "都空不响");
        assert!(!install.join("AGENTS.md").exists());

        // CRLF 差异不算内容不一致
        std::fs::write(user.join("AGENTS.md"), "行尾差异\r\n第二行\r\n").unwrap();
        std::fs::write(install.join("AGENTS.md"), "行尾差异\n第二行\n").unwrap();
        assert!(rollback_global_agents_at(&install, &user).is_none(), "CRLF 不算不一致");
        assert!(!install.join("AGENTS.md").exists());

        // 同址（开发形态）不删用户唯一那份
        std::fs::write(user.join("AGENTS.md"), "唯一一份").unwrap();
        assert!(rollback_global_agents_at(&user, &user).is_none(), "同址不响");
        assert_eq!(std::fs::read_to_string(user.join("AGENTS.md")).unwrap(), "唯一一份");

        std::fs::remove_dir_all(&install).ok();
        std::fs::remove_dir_all(&user).ok();
    }

    #[test]
    fn disabled_memory_writes_nothing() {
        init_logger();
        let tmp = std::env::temp_dir().join(format!("agent-p7-off-{}", uuid::Uuid::new_v4().simple()));
        let gtmp = std::env::temp_dir().join(format!("agent-p7-offg-{}", uuid::Uuid::new_v4().simple()));
        let utmp = std::env::temp_dir().join(format!("agent-p7-offu-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&tmp).unwrap();
        let dirs = GlobalDirs { install: gtmp.clone(), user: utmp.clone() };
        let mem = Memory::open_at(&tmp, &MemoryConfig { enabled: false, persist_sensitive: false }, dirs);
        mem.record("task.start", json!({"text": "x"}));
        assert!(!tmp.join(".agent").join("memory").join("events.log").exists(), "关闭时不落任何文件");
        assert!(!gtmp.join("index.json").exists(), "关闭时不登记 index");
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(&gtmp).ok();
        std::fs::remove_dir_all(&utmp).ok();
    }

    #[test]
    fn agents_template_created() {
        init_logger();
        let tmp = std::env::temp_dir().join(format!("agent-p7-agents-{}", uuid::Uuid::new_v4().simple()));
        let gtmp = std::env::temp_dir().join(format!("agent-p7-agentsg-{}", uuid::Uuid::new_v4().simple()));
        let utmp = std::env::temp_dir().join(format!("agent-p7-agentsu-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&tmp).unwrap();
        let dirs = GlobalDirs { install: gtmp.clone(), user: utmp.clone() };
        let mem = Memory::open_at(&tmp, &MemoryConfig::default(), dirs);
        mem.ensure_agents_template(&tmp);
        let text = std::fs::read_to_string(tmp.join("AGENTS.md")).unwrap();
        // 模板必须近乎空白：只有标题与自填提示，不含任何预置项目知识
        assert!(text.contains("# AGENTS.md"), "应有标题");
        assert!(!text.contains("cargo") && !text.contains("构建："), "模板不得预填内容: {text}");
        // 已存在时不覆盖
        std::fs::write(tmp.join("AGENTS.md"), "我的约定").unwrap();
        mem.ensure_agents_template(&tmp);
        assert_eq!(std::fs::read_to_string(tmp.join("AGENTS.md")).unwrap(), "我的约定");
        // 未改动的空白模板 = 不注入（含 CRLF 变体）；用户填了才注入
        std::fs::write(tmp.join("AGENTS.md"), AGENTS_TEMPLATE).unwrap();
        assert!(!mem.agents_md().contains("AGENTS.md"), "空白模板不应注入: {:?}", mem.agents_md());
        std::fs::write(tmp.join("AGENTS.md"), AGENTS_TEMPLATE.replace('\n', "\r\n")).unwrap();
        assert!(!mem.agents_md().contains("AGENTS.md"), "CRLF 模板同样不应注入");
        std::fs::write(tmp.join("AGENTS.md"), "构建用 make build").unwrap();
        assert!(mem.agents_md().contains("make build"), "用户设定应注入");
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(&gtmp).ok();
        std::fs::remove_dir_all(&utmp).ok();
    }
}
