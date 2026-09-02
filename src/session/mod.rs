//! 会话管理 —— 总架构 §13 / 施工单 007
//!
//! v1：单会话 + 落盘 + 恢复。会话消息数组（OpenAI 格式、不含 system）落
//! `<log_dir>/session.json`，核心循环每个关键点发 Snapshot 事件，server 收到即落盘；
//! 前端启动时 GET /api/session 重建对话视图。落盘即续命：进程崩了消息还在。

use std::path::PathBuf;

use anyhow::Context;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFile {
    /// 进程 session_id（JSONL 日志里的同一个）
    pub id: String,
    pub updated_at: String,
    pub messages: Vec<Value>,
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    /// dir 为日志目录（与 JSONL 同盘同目录）；文件名固定 session.json。
    pub fn new(dir: &std::path::Path) -> Self {
        Self { path: dir.join("session.json") }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 原子落盘：先写 tmp 再 rename 覆盖（Windows rename 可覆盖已存在文件）。
    pub fn save(&self, session_id: &str, messages: &[Value]) -> anyhow::Result<()> {
        let file = SessionFile {
            id: session_id.to_string(),
            updated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            messages: messages.to_vec(),
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("建目录失败: {}", parent.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string(&file).context("会话序列化失败")?;
        std::fs::write(&tmp, text).with_context(|| format!("写临时文件失败: {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).with_context(|| format!("替换会话文件失败: {}", self.path.display()))?;
        Ok(())
    }

    /// 读取落盘会话；文件不存在或损坏返回 None（损坏时打日志，不 panic）。
    pub fn load(&self) -> Option<SessionFile> {
        let text = match std::fs::read_to_string(&self.path) {
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

    /// 清盘（新会话）。
    pub fn clear(&self) -> anyhow::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("删除会话文件失败: {}", self.path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_store(tag: &str) -> (tempdir::TempDir, Store) {
        let dir = tempdir::TempDir::new(tag);
        let store = Store::new(&dir.path());
        (dir, store)
    }

    // 极简临时目录：测试结束自动删除
    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU32, Ordering};

        static SEQ: AtomicU32 = AtomicU32::new(0);

        pub struct TempDir(PathBuf);
        impl TempDir {
            pub fn new(tag: &str) -> Self {
                let n = SEQ.fetch_add(1, Ordering::SeqCst);
                let p = std::env::temp_dir().join(format!(
                    "agent-p6-{tag}-{}-{}",
                    std::process::id(),
                    n
                ));
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

    #[test]
    fn save_load_roundtrip_and_clear() {
        let (_d, store) = temp_store("session");
        assert!(store.load().is_none(), "未落盘时应为 None");
        let msgs = vec![
            json!({"role": "user", "content": "你好"}),
            json!({"role": "assistant", "content": "嗨", "tool_calls": []}),
        ];
        store.save("sid-1", &msgs).unwrap();
        let f = store.load().unwrap();
        assert_eq!(f.id, "sid-1");
        assert_eq!(f.messages, msgs);
        assert!(!f.updated_at.is_empty());
        // 原子覆盖
        store.save("sid-2", &msgs[..1]).unwrap();
        let f = store.load().unwrap();
        assert_eq!(f.id, "sid-2");
        assert_eq!(f.messages.len(), 1);
        store.clear().unwrap();
        assert!(store.load().is_none());
        // clear 幂等
        store.clear().unwrap();
    }

    #[test]
    fn load_corrupted_returns_none() {
        let _ = crate::observability::init(
            &std::env::temp_dir().join("agent-test-logs"),
            "session.jsonl",
            format!("session-test-{}", uuid::Uuid::new_v4()),
        );
        let (_d, store) = temp_store("session-bad");
        std::fs::write(store.path(), "{not json").unwrap();
        assert!(store.load().is_none());
    }
}
