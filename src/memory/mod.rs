//! 记忆系统 v1 —— 总架构 §9 / 施工单 009
//!
//! 双层存储：全局 `~/.agent/` + 项目 `<workspace>/.agent/memory/`（与 P6 影子仓库同根）。
//! - index.json（全局层）：项目身份登记（git remote 优先，无则 git 根路径），防串项目。
//! - events.log（项目层）：任务/工具/交接/检查点的事件流水，跟 git 仓库走沉淀"经验"；
//!   默认脱敏（sk-/Bearer/api_key 打码），`persist_sensitive=true` 才存原文。
//! - AGENTS.md：全局 + 项目双层读取注入 system prompt；项目缺失时生成模板（开箱即用）。
//!
//! 语义记忆/向量检索留接口（P8 EMBEDDING 再补），本单只做"流水 + 摘要 + 约定"。

pub mod library;

use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::MemoryConfig;

/// 全局层目录：HOME（Windows 回退 USERPROFILE）下的 `.agent`。
pub fn global_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".agent")
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

/// 打开的记忆层（持有路径与开关，核心循环/server 共享一个实例）。
pub struct Memory {
    pub enabled: bool,
    persist_sensitive: bool,
    global_dir: PathBuf,
    project_dir: PathBuf,
    ws: PathBuf,
    pub key: String,
    pub name: String,
    pub remote: Option<String>,
}

impl Memory {
    /// 计算项目身份、建目录、登记 index.json（enabled 时）。
    pub fn open(ws: &Path, cfg: &MemoryConfig) -> Self {
        Self::open_at(ws, cfg, global_dir())
    }

    /// 指定全局层目录（测试隔离用；生产走 open()）。
    pub fn open_at(ws: &Path, cfg: &MemoryConfig, global_dir: PathBuf) -> Self {
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
            global_dir,
            project_dir: ws.join(".agent").join("memory"),
            ws: ws.to_path_buf(),
            key,
            name,
            remote,
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
        self.global_dir.join("index.json")
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
        if let Err(e) = std::fs::create_dir_all(&self.global_dir) {
            crate::observability::debug(&format!("[memory] 建全局目录失败: {e}"));
            return;
        }
        let text = serde_json::to_string_pretty(&idx).unwrap_or_default();
        if let Err(e) = std::fs::write(self.index_path(), text) {
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
    pub fn agents_md(&self) -> String {
        let mut out = String::new();
        out.push_str(&agents_text("全局约定", &self.global_dir.join("AGENTS.md")));
        out.push_str(&agents_text("项目约定", &self.ws.join("AGENTS.md")));
        out
    }

    // —— P10.5 设置面板「记忆」分区（只读状态 + AGENTS.md 编辑）——
    pub fn global_dir(&self) -> &Path {
        &self.global_dir
    }
    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }
    /// 全局层 AGENTS.md 路径（~/.agent/AGENTS.md）。
    pub fn global_agents_path(&self) -> PathBuf {
        self.global_dir.join("AGENTS.md")
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
            "global" => self.global_dir.join("AGENTS.md"),
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
    pub fn save_agents(&self, scope: &str, content: &str) -> anyhow::Result<PathBuf> {
        let path = match scope {
            "global" => {
                std::fs::create_dir_all(&self.global_dir)?;
                self.global_dir.join("AGENTS.md")
            }
            _ => self.ws.join("AGENTS.md"),
        };
        std::fs::write(&path, content)?;
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
        if let Err(e) = std::fs::write(&path, AGENTS_TEMPLATE) {
            crate::observability::debug(&format!("[memory] 生成 AGENTS.md 模板失败: {e}"));
        }
    }
}

/// 敏感打码（单遍扫描，替换后不回扫）：sk- 开头的 key、Bearer 令牌、api_key 字段值。
pub fn sanitize(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(text.len());
    let starts = |i: usize, pat: &str| -> bool {
        let p: Vec<char> = pat.chars().collect();
        i + p.len() <= n && chars[i..i + p.len()] == p[..]
    };
    let mut i = 0usize;
    while i < n {
        // sk-xxxx（≥4 个 key 字符才算，避免误伤普通词）
        if starts(i, "sk-") {
            let mut j = i + 3;
            while j < n && (chars[j].is_ascii_alphanumeric() || chars[j] == '-' || chars[j] == '_') {
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
        // api_key / apikey / API_KEY 字段：名字与分隔符照抄，值打码
        let mut handled = false;
        for pat in ["api_key", "apikey", "API_KEY"] {
            if starts(i, pat) {
                out.push_str(pat);
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

    #[test]
    fn index_register_and_events_log() {
        init_logger();
        let tmp = std::env::temp_dir().join(format!("agent-p7-mem-{}", uuid::Uuid::new_v4().simple()));
        let gtmp = std::env::temp_dir().join(format!("agent-p7-glob-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&tmp).unwrap();
        let cfg = MemoryConfig { enabled: true, persist_sensitive: false };
        let mem = Memory::open_at(&tmp, &cfg, gtmp.clone());
        assert!(mem.enabled);
        // 项目身份 = 路径（临时目录非 git 仓库）
        assert!(mem.key.contains("agent-p7-mem"), "key 应含目录名: {}", mem.key);
        // index.json 登记在隔离的全局层
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
        let mem2 = Memory::open_at(&tmp, &cfg, gtmp.clone());
        assert!(tmp.join(".agent").join("memory").exists());
        mem2.record("task.finish", json!({"steps": 1}));
        assert!(tmp.join(".agent").join("memory").join("events.log").exists());

        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(&gtmp).ok();
    }

    #[test]
    fn disabled_memory_writes_nothing() {
        init_logger();
        let tmp = std::env::temp_dir().join(format!("agent-p7-off-{}", uuid::Uuid::new_v4().simple()));
        let gtmp = std::env::temp_dir().join(format!("agent-p7-offg-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mem = Memory::open_at(&tmp, &MemoryConfig { enabled: false, persist_sensitive: false }, gtmp.clone());
        mem.record("task.start", json!({"text": "x"}));
        assert!(!tmp.join(".agent").join("memory").join("events.log").exists(), "关闭时不落任何文件");
        assert!(!gtmp.join("index.json").exists(), "关闭时不登记 index");
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(&gtmp).ok();
    }

    #[test]
    fn agents_template_created() {
        init_logger();
        let tmp = std::env::temp_dir().join(format!("agent-p7-agents-{}", uuid::Uuid::new_v4().simple()));
        let gtmp = std::env::temp_dir().join(format!("agent-p7-agentsg-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mem = Memory::open_at(&tmp, &MemoryConfig::default(), gtmp.clone());
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
    }
}
