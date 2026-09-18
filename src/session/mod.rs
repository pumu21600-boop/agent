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

/// RFC3339 → 本地「月-日 时:分」，给项目现状摘要用（解析失败原样返回，不报错）。
fn fmt_ts(iso: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|_| iso.to_string())
}

impl SessionManager {
    /// dir 为日志目录，会话文件落 `<dir>/sessions/`。
    pub fn new(log_dir: &Path) -> Self {
        Self { dir: log_dir.join("sessions") }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// 新建对话 id（不落盘；首条消息时 save 才建文件）。
    pub fn new_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// 新建一条空会话并立即落盘：点「新会话」就在侧栏当前项目下留一条，
    /// 不用等首条消息。同一工作区已存在空会话时直接复用，避免连点堆出一排空会话。
    pub fn create_empty(&self, workspace: &str, title: &str) -> anyhow::Result<SessionFile> {
        if let Some(m) = self
            .list()
            .into_iter()
            .find(|m| m.count == 0 && !m.archived && m.workspace == workspace)
            && let Some(f) = self.get(&m.id) {
                return Ok(f);
            }
        std::fs::create_dir_all(&self.dir).with_context(|| format!("建会话目录失败: {}", self.dir.display()))?;
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let f = SessionFile {
            id: self.new_id(),
            title: title.to_string(),
            created_at: now.clone(),
            updated_at: now,
            messages: Vec::new(),
            summary: None,
            workspace: workspace.to_string(),
            pinned: false,
            archived: false,
        };
        self.write(&f)?;
        Ok(f)
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

    /// 同项目（workspace 相同）里最近更新的一条已落盘会话，排除 current_id。
    /// 新会话开场「上次做到哪」的来源；没有则返回 None。
    pub fn previous_in_workspace(&self, workspace: &str, current_id: &str) -> Option<SessionFile> {
        if workspace.trim().is_empty() {
            return None;
        }
        let mut best: Option<SessionFile> = None;
        for m in self.list() {
            if m.id == current_id || m.workspace != workspace {
                continue;
            }
            let Some(f) = self.get(&m.id) else { continue };
            if best.as_ref().map(|b| f.updated_at > b.updated_at).unwrap_or(true) {
                best = Some(f);
            }
        }
        best
    }

    /// 新会话开场的「项目现状」摘要：上一条同项目会话的标题/滚动摘要 + 工作区未提交改动量。
    /// 没有任何可写事实时返回 None（不注入，避免占上下文）。
    /// `changes` = (文件数, 新增行, 删除行)，相对影子仓库最近检查点。
    pub fn project_brief(&self, current_id: &str, changes: Option<(usize, u64, u64)>) -> Option<String> {
        let ws = crate::tools::workspace_root();
        let ws_path = ws.display().to_string();
        let ws_name = ws.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut lines: Vec<String> = Vec::new();
        if let Some(p) = self.previous_in_workspace(&ws_path, current_id) {
            lines.push(format!("- 上一条会话：「{}」（{}）", p.title, fmt_ts(&p.updated_at)));
            if let Some(sm) = p.summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                lines.push(format!("- 上次进展：{}", sm.chars().take(400).collect::<String>()));
            }
        }
        if let Some((files, add, del)) = changes
            && files > 0 {
                lines.push(format!("- 工作区已有 {files} 个文件改动（+{add}/-{del}，相对最近检查点），内容都在磁盘上"));
            }
        if lines.is_empty() {
            return None;
        }
        Some(format!(
            "\n\n【项目现状 · 新对话接手】\n项目目录：{ws_path}（{ws_name}）\n{}\n这是同一项目里新开的对话，之前的改动已落盘。动手前先确认现状，不要重做上一条会话已完成的工作。\n",
            lines.join("\n")
        ))
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

    /// 点「新会话」立即落盘并进列表；同工作区复用空会话，有消息的不算空。
    #[test]
    fn create_empty_session_is_listed_and_reused() {
        let (_d, m) = mgr("empty-new");
        let ws = "C:\\ws\\demo";
        let f = m.create_empty(ws, "新会话").unwrap();
        assert!(f.messages.is_empty());
        assert_eq!(f.workspace, ws);
        let list = m.list();
        assert_eq!(list.len(), 1, "空会话也要进列表（侧栏立即可见）");
        assert_eq!(list[0].count, 0);
        assert_eq!(list[0].title, "新会话");

        // 同工作区再次新建 → 复用同一条，连点不堆空会话
        let f2 = m.create_empty(ws, "新会话").unwrap();
        assert_eq!(f2.id, f.id);
        assert_eq!(m.list().len(), 1);

        // 换工作区 → 另建一条
        let f3 = m.create_empty("C:\\ws\\other", "新会话").unwrap();
        assert_ne!(f3.id, f.id);
        assert_eq!(m.list().len(), 2);

        // 有消息的会话不再被当作可复用空会话
        m.save(&f.id, &[json!({"role":"user","content":"开始干活"})], None).unwrap();
        let f4 = m.create_empty(ws, "新会话").unwrap();
        assert_ne!(f4.id, f.id, "有消息的会话不该被复用");
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

    #[test]
    fn project_brief_carries_prior_session() {
        let _ = crate::observability::init(
            &std::env::temp_dir().join("agent-test-logs"),
            "sessions.jsonl",
            format!("session-test-{}", uuid::Uuid::new_v4()),
        );
        let (_d, m) = mgr("brief");
        let ws = crate::tools::workspace_root().display().to_string();

        // 项目里还没有别的会话：没有可写事实 → 不注入
        let fresh = m.new_id();
        assert!(m.project_brief(&fresh, None).is_none(), "空项目不该注入现状");

        // 上一条会话有摘要 + 工作区有未提交改动 → 摘要里两者都要出现
        let prev = m.new_id();
        m.save(&prev, &[json!({"role":"user","content":"把登录接口改成 JWT"})], None).unwrap();
        m.save(
            &prev,
            &[
                json!({"role":"user","content":"把登录接口改成 JWT"}),
                json!({"role":"assistant","content":"已改完"}),
            ],
            Some("任务目标：登录改 JWT；已完成签发与校验，待补刷新逻辑"),
        )
        .unwrap();

        let brief = m.project_brief(&fresh, Some((2, 15, 4))).expect("应生成项目现状");
        assert!(brief.contains("把登录接口改成 JWT"), "带上一条会话标题: {brief}");
        assert!(brief.contains("待补刷新逻辑"), "带上一条会话摘要: {brief}");
        assert!(brief.contains("2 个文件改动"), "带未提交改动量: {brief}");
        assert!(brief.contains(&ws), "带项目目录: {brief}");

        // 排除自己：当前会话不该被当成「上一条」
        assert!(m.previous_in_workspace(&ws, &prev).is_none());

        // 别的项目不算同项目
        assert!(m.previous_in_workspace("C:\\somewhere\\else", &fresh).is_none());
        assert!(m.project_brief(&fresh, None).is_some(), "换基线仍有上一条会话可写");
    }
}
