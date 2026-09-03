//! 会话管理 —— 总架构 §13 / 施工单 007、008
//!
//! 多会话：每条会话一个文件 `<log_dir>/sessions/<id>.json`（OpenAI 消息数组，不含 system），
//! 新建对话不清空历史；标题取首条用户消息。核心循环每关键点发 Snapshot 事件，server
//! 按 session_id 落盘；前端启动拉 /api/sessions 列表、点选切换。落盘即续命。

use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFile {
    pub id: String,
    /// 首条用户消息截断（列表展示用）
    #[serde(default)]
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub messages: Vec<Value>,
    /// P7 滚动摘要（跨任务延续；无摘要时缺省）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// 创建时所在的工作区（DSH 式侧边栏「按工作区」分组用；旧文件缺省=未分组）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace: String,
    /// DSH 式侧边栏：置顶（列表内沉底排序的最前）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// DSH 式侧边栏：归档（默认列表隐藏，管理模式可见/可取消）
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
}

/// 列表条目（不携带消息体）。
#[derive(Debug, Clone, Serialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub count: usize,
    pub workspace: String,
    pub pinned: bool,
    pub archived: bool,
}

pub struct SessionManager {
    dir: PathBuf,
}

/// session_id 只允许 uuid 安全字符，直接拼文件名防路径穿越。
fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn first_user_title(messages: &[Value]) -> String {
    messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .filter_map(|m| m.get("content").and_then(Value::as_str))
        .find(|s| !s.trim().is_empty())
        .map(|s| {
            let mut s = s.trim();
            // 剥离形如 [Audio understanding...] 或 [audio omitted...] 的前置投影块
            while s.starts_with('[') {
                if let Some(end_idx) = s.find(']') {
                    s = s[end_idx + 1..].trim();
                } else {
                    break;
                }
            }
            if s.is_empty() {
                "媒体理解对话".to_string()
            } else {
                s.chars().take(30).collect::<String>()
            }
        })
        .unwrap_or_else(|| "新对话".to_string())
}

impl SessionManager {
    /// dir 为日志目录，会话文件落 `<dir>/sessions/`。
    pub fn new(log_dir: &Path) -> Self {
        Self { dir: log_dir.join("sessions") }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// 新建对话：只发 id，首条消息落盘时才建文件（空会话不进列表）。
    pub fn new_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// 原子写（临时文件 + rename），save / rename / set_meta / fork 共用。
    fn write(&self, f: &SessionFile) -> anyhow::Result<()> {
        let tmp = self.path(&f.id).with_extension("json.tmp");
        let text = serde_json::to_string(f).context("会话序列化失败")?;
        std::fs::write(&tmp, text).with_context(|| format!("写临时文件失败: {}", tmp.display()))?;
        std::fs::rename(&tmp, self.path(&f.id))
            .with_context(|| format!("替换会话文件失败: {}", self.path(&f.id).display()))?;
        Ok(())
    }

    /// 落盘（原子写）：已存在则保留 created_at/标题/置顶/归档/工作区，否则标题取首条
    /// 用户消息、工作区记为当前工作区。summary 传 Some 才更新（None 保留旧值）。
    pub fn save(&self, id: &str, messages: &[Value], summary: Option<&str>) -> anyhow::Result<()> {
        if !safe_id(id) {
            anyhow::bail!("非法 session id");
        }
        std::fs::create_dir_all(&self.dir).with_context(|| format!("建会话目录失败: {}", self.dir.display()))?;
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let (title, created_at, old_summary, workspace, pinned, archived) = match self.get(id) {
            Some(old) => (old.title, old.created_at, old.summary, old.workspace, old.pinned, old.archived),
            None => (
                first_user_title(messages),
                now.clone(),
                None,
                crate::tools::workspace_root().display().to_string(),
                false,
                false,
            ),
        };
        let file = SessionFile {
            id: id.to_string(),
            title,
            created_at,
            updated_at: now,
            messages: messages.to_vec(),
            summary: summary.map(|s| s.to_string()).or(old_summary),
            workspace,
            pinned,
            archived,
        };
        self.write(&file)
    }

    pub fn get(&self, id: &str) -> Option<SessionFile> {
        if !safe_id(id) {
            return None;
        }
        let text = match std::fs::read_to_string(self.path(id)) {
            Ok(t) => t,
            Err(_) => return None,
        };
        match serde_json::from_str::<SessionFile>(&text) {
            Ok(f) => Some(f),
            Err(e) => {
                crate::observability::debug(&format!("会话文件损坏，忽略: {e}"));
                None
            }
        }
    }

    /// 最近更新的会话（启动时默认恢复它）。
    pub fn latest(&self) -> Option<SessionFile> {
        self.list().first().and_then(|m| self.get(&m.id))
    }

    /// 全部会话元信息：置顶优先，其余按更新时间新→旧（归档项一并返回，由前端过滤）。
    pub fn list(&self) -> Vec<SessionMeta> {
        let mut metas: Vec<SessionMeta> = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return metas;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            let Ok(f) = serde_json::from_str::<SessionFile>(&text) else { continue };
            metas.push(SessionMeta {
                id: f.id,
                title: f.title,
                created_at: f.created_at,
                updated_at: f.updated_at,
                count: f.messages.len(),
                workspace: f.workspace,
                pinned: f.pinned,
                archived: f.archived,
            });
        }
        metas.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(b.updated_at.cmp(&a.updated_at)));
        metas
    }

    /// 删除一条会话（其余不受影响）；不存在也返回 Ok。
    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        if !safe_id(id) {
            anyhow::bail!("非法 session id");
        }
        match std::fs::remove_file(self.path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("删除会话失败: {}", self.path(id).display())),
        }
    }

    /// 改会话标题（P8 TITLE 角色 / 侧边栏重命名）；会话不存在时静默成功（标题晚于首落盘的竞态下无能为力）。
    pub fn rename(&self, id: &str, title: &str) -> anyhow::Result<()> {
        let Some(mut f) = self.get(id) else { return Ok(()) };
        f.title = title.chars().take(60).collect();
        f.updated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        self.write(&f)
    }

    /// DSH 式侧边栏：置顶 / 归档标记（其余字段原样保留；不动 updated_at）。
    pub fn set_flags(&self, id: &str, pinned: Option<bool>, archived: Option<bool>) -> anyhow::Result<()> {
        let Some(mut f) = self.get(id) else {
            anyhow::bail!("会话不存在: {id}");
        };
        if let Some(p) = pinned {
            f.pinned = p;
        }
        if let Some(a) = archived {
            f.archived = a;
        }
        self.write(&f)
    }

    /// DSH 式侧边栏「分叉会话」：复制消息/摘要/工作区为新会话，标题加「（分叉）」。
    /// 返回新会话 id；分叉不带走置顶/归档状态。
    pub fn fork(&self, id: &str) -> anyhow::Result<String> {
        let Some(f) = self.get(id) else {
            anyhow::bail!("会话不存在: {id}");
        };
        let new_id = self.new_id();
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let title = format!("{}（分叉）", f.title).chars().take(60).collect();
        let file = SessionFile {
            id: new_id.clone(),
            title,
            created_at: now.clone(),
            updated_at: now,
            messages: f.messages,
            summary: f.summary,
            workspace: f.workspace,
            pinned: false,
            archived: false,
        };
        self.write(&file)?;
        Ok(new_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        pub struct TempDir(PathBuf);
        impl TempDir {
            pub fn new(tag: &str) -> Self {
                let n = SEQ.fetch_add(1, Ordering::SeqCst);
                let p = std::env::temp_dir().join(format!("agent-p61-{tag}-{}-{}", std::process::id(), n));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                TempDir(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn mgr(tag: &str) -> (tempdir::TempDir, SessionManager) {
        let d = tempdir::TempDir::new(tag);
        let m = SessionManager::new(d.path());
        (d, m)
    }

    #[test]
    fn multi_session_lifecycle() {
        let _ = crate::observability::init(
            &std::env::temp_dir().join("agent-test-logs"),
            "sessions.jsonl",
            format!("session-test-{}", uuid::Uuid::new_v4()),
        );
        let (_d, m) = mgr("multi");

        // 空目录：无会话
        assert!(m.list().is_empty());
        assert!(m.latest().is_none());

        // 两条会话互不影响；标题取首条用户消息
        let a = m.new_id();
        let b = m.new_id();
        assert_ne!(a, b);
        m.save(&a, &[json!({"role":"user","content":"帮我修编译错误，这是会话A的开头"})], None).unwrap();
        m.save(&b, &[json!({"role":"user","content":"第二条会话"})], None).unwrap();
        let list = m.list();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|s| s.id == a && s.title.contains("会话A")));
        assert!(list.iter().any(|s| s.id == b));

        // 更新 A：标题与 created_at 保留，消息数变化；P7 摘要随存
        m.save(
            &a,
            &[
                json!({"role":"user","content":"帮我修编译错误，这是会话A的开头"}),
                json!({"role":"assistant","content":"好的"}),
            ],
            Some("任务目标：修编译错误；已定位到 main.rs"),
        )
        .unwrap();
        let fa = m.get(&a).unwrap();
        assert_eq!(fa.messages.len(), 2);
        assert_eq!(fa.title, "帮我修编译错误，这是会话A的开头");
        assert!(fa.summary.as_deref().unwrap_or("").contains("main.rs"), "P7 摘要应持久化");
        // summary 传 None 时保留旧值
        m.save(&a, &fa.messages, None).unwrap();
        assert!(m.get(&a).unwrap().summary.is_some());

        // 删除 A，B 仍在
        m.delete(&a).unwrap();
        assert!(m.get(&a).is_none());
        assert_eq!(m.list().len(), 1);
        // 删除幂等
        m.delete(&a).unwrap();
    }

    #[test]
    fn rejects_bad_ids_and_missing() {
        let _ = crate::observability::init(
            &std::env::temp_dir().join("agent-test-logs"),
            "sessions.jsonl",
            format!("session-test-{}", uuid::Uuid::new_v4()),
        );
        let (_d, m) = mgr("badid");
        assert!(m.save("../evil", &[], None).is_err());
        assert!(m.get("../evil").is_none());
        assert!(m.delete("../evil").is_err());
        assert!(m.get("不存在的会话").is_none());
    }

    #[test]
    fn pin_archive_fork_lifecycle() {
        // DSH 式侧边栏三操作：置顶排序、归档标记、分叉复制
        let _ = crate::observability::init(
            &std::env::temp_dir().join("agent-test-logs"),
            "sessions.jsonl",
            format!("session-test-{}", uuid::Uuid::new_v4()),
        );
        let (_d, m) = mgr("pinfork");
        let a = m.new_id();
        m.save(&a, &[json!({"role":"user","content":"原始会话"})], None).unwrap();
        // 新会话记录当前工作区
        assert!(!m.get(&a).unwrap().workspace.is_empty(), "新会话应记录创建时的工作区");

        // 置顶 → 列表置前；save 续写后置顶保留
        m.set_flags(&a, Some(true), None).unwrap();
        assert!(m.list()[0].pinned);
        m.save(&a, &[json!({"role":"user","content":"原始会话"}), json!({"role":"assistant","content":"好"})], None).unwrap();
        assert!(m.get(&a).unwrap().pinned, "save 不得丢置顶标记");

        // 归档 → 标记可见（前端负责过滤）
        m.set_flags(&a, None, Some(true)).unwrap();
        assert!(m.get(&a).unwrap().archived);
        assert!(m.list()[0].archived);

        // 取消置顶+取消归档
        m.set_flags(&a, Some(false), Some(false)).unwrap();
        let fa = m.get(&a).unwrap();
        assert!(!fa.pinned && !fa.archived);

        // 分叉：消息/摘要/工作区复制，标题加（分叉），新 id，无置顶归档
        m.set_flags(&a, Some(true), None).unwrap();
        m.rename(&a, "原始标题").unwrap();
        let forked = m.fork(&a).unwrap();
        let ff = m.get(&forked).unwrap();
        assert_eq!(ff.title, "原始标题（分叉）");
        assert_eq!(ff.messages.len(), m.get(&a).unwrap().messages.len());
        assert_eq!(ff.workspace, m.get(&a).unwrap().workspace);
        assert!(!ff.pinned && !ff.archived, "分叉不带走置顶/归档");
        assert_ne!(forked, a);

        // 不存在的会话
        assert!(m.set_flags("ghost", Some(true), None).is_err());
        assert!(m.fork("ghost").is_err());
    }
}
