//! JSONL 日志地基 —— 施工单 001 / 任务 A
//!
//! 约定：后续所有代码统一走本模块落日志，业务数据禁止直接 println。
//! 每条记录固定 4 个字段：ts / session_id / event_type / payload。
//! 写盘方式：追加写、单行一条 JSON + '\n'、写完立即 flush（落盘即完整，不许半行）。
//! 轮转（2026-09-14 补）：单文件超过上限就归档成 `<file>.1`（只留一份）再开新文件——
//! 启动时检查一次，运行中每 500 条复查一次；上限走 config.toml [logging].max_mb。
//! schema 见 docs/jsonl-schema.md。

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
};

use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};

/// 运行中体积复查间隔（条）：写满这么多条检查一次，避免每条都 stat。
const ROTATE_CHECK_EVERY: u64 = 500;

/// 事件类型 6 类（schema 见 docs/jsonl-schema.md）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    /// 模型思考 / 推理内容（核心循环接入后使用，本单预留）
    Thought,
    /// 工具调用（本单未实现，预留）
    ToolCall,
    /// 工具结果（本单未实现，预留）
    ToolResult,
    /// 模型流式分片
    ModelChunk,
    /// 状态快照（启动、网关请求/响应、取消等）
    State,
    /// 错误
    Error,
}

impl EventType {
    pub fn as_str(self) -> &'static str {
        match self {
            EventType::Thought => "thought",
            EventType::ToolCall => "tool_call",
            EventType::ToolResult => "tool_result",
            EventType::ModelChunk => "model_chunk",
            EventType::State => "state",
            EventType::Error => "error",
        }
    }
}

pub struct Logger {
    file: Mutex<File>,
    session_id: String,
    /// P10-A：当前日志文件完整路径（trace 查询接口用）
    path: PathBuf,
    /// 轮转上限（字节），0 = 不轮转
    max_bytes: u64,
    /// 已写入条数（周期性体积复查用）
    writes: AtomicU64,
}

impl Logger {
    /// 打开（追加）JSONL 文件，目录不存在则创建；max_bytes > 0 时按上限轮转。
    pub fn open(
        dir: &Path,
        file_name: &str,
        session_id: String,
        max_bytes: u64,
    ) -> anyhow::Result<Arc<Self>> {
        fs::create_dir_all(dir)?;
        let path = dir.join(file_name);
        // 启动即查：超限先把旧文件归档成 .1（只留一份），本次从新文件开始写
        let rotated_from = if max_bytes > 0 {
            rotate_file(&path, max_bytes).ok().flatten()
        } else {
            None
        };
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let logger = Arc::new(Self {
            file: Mutex::new(file),
            session_id,
            path,
            max_bytes,
            writes: AtomicU64::new(0),
        });
        if let Some(bytes) = rotated_from {
            logger.log(
                EventType::State,
                json!({ "kind": "log.rotated", "bytes": bytes, "limit_bytes": max_bytes }),
            );
        }
        Ok(logger)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// P10-A：当前 JSONL 文件路径（未初始化时 None）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 写一条日志：单行 JSON + 换行，写完立即 flush（落盘即完整）。
    pub fn log(&self, event_type: EventType, payload: Value) {
        let record = json!({
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            "session_id": self.session_id,
            "event_type": event_type.as_str(),
            "payload": payload,
        });
        let mut line = match serde_json::to_string(&record) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[logger] 序列化失败: {e}");
                return;
            }
        };
        line.push('\n');
        let mut rotated: Option<u64> = None;
        match self.file.lock() {
            Ok(mut f) => {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
                let n = self.writes.fetch_add(1, Ordering::Relaxed) + 1;
                if self.max_bytes > 0 && n % ROTATE_CHECK_EVERY == 0 {
                    // 持锁检查 + 轮转，避免并发写者踩到换文件
                    if let Ok(meta) = fs::metadata(&self.path) {
                        if meta.len() > self.max_bytes {
                            let backup = backup_path(&self.path);
                            let _ = fs::remove_file(&backup);
                            if fs::rename(&self.path, &backup).is_ok() {
                                if let Ok(nf) =
                                    OpenOptions::new().create(true).append(true).open(&self.path)
                                {
                                    *f = nf;
                                    rotated = Some(meta.len());
                                }
                            }
                        }
                    }
                }
            }
            Err(_) => eprintln!("[logger] 日志文件锁中毒，本条未写入"),
        }
        if let Some(bytes) = rotated {
            // 锁已释放，补一条轮转事件（新文件体积极小，不会再次触发）
            self.log(
                EventType::State,
                json!({ "kind": "log.rotated", "bytes": bytes, "limit_bytes": self.max_bytes }),
            );
        }
    }

    /// 控制台调试输出（带时间戳）；业务日志统一走 log()。
    pub fn debug(&self, msg: &str) {
        println!(
            "[{}] {}",
            Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            msg
        );
    }
}

/// 归档路径：`<file>.1`。
fn backup_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.1", path.display()))
}

/// 超限则把 path 归档为 `<file>.1`（覆盖旧归档），返回被归档的体积；未超限/不存在返回 None。
fn rotate_file(path: &Path, max_bytes: u64) -> std::io::Result<Option<u64>> {
    let size = match fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return Ok(None), // 文件还不存在
    };
    if size <= max_bytes {
        return Ok(None);
    }
    let backup = backup_path(path);
    let _ = fs::remove_file(&backup);
    fs::rename(path, &backup)?;
    Ok(Some(size))
}

static LOGGER: OnceLock<Arc<Logger>> = OnceLock::new();

/// 进程启动时调用一次（默认轮转上限 20MB；生产走 init_with_max，本入口供测试用）。
#[cfg(test)]
pub fn init(dir: &Path, file_name: &str, session_id: String) -> anyhow::Result<()> {
    init_with_max(dir, file_name, session_id, 20 * 1024 * 1024)
}

/// 带轮转上限的初始化；max_bytes = 0 表示不轮转。
pub fn init_with_max(
    dir: &Path,
    file_name: &str,
    session_id: String,
    max_bytes: u64,
) -> anyhow::Result<()> {
    let logger = Logger::open(dir, file_name, session_id, max_bytes)?;
    let _ = LOGGER.set(logger);
    Ok(())
}

pub fn get() -> &'static Arc<Logger> {
    LOGGER.get().expect("logger 未初始化：先调用 logging::init")
}

/// 全局便捷入口：后续所有代码统一走它。
pub fn event(event_type: EventType, payload: Value) {
    get().log(event_type, payload);
}

/// 安全入口：logger 未初始化时静默跳过（早期启动代码 / 单测里可用，不 panic）。
pub fn try_event(event_type: EventType, payload: Value) {
    if let Some(l) = LOGGER.get() {
        l.log(event_type, payload);
    }
}

/// P10-A：当前 JSONL 日志文件路径（logger 未初始化时 None）。
pub fn log_path() -> Option<&'static Path> {
    LOGGER.get().map(|l| l.path())
}

pub mod trace;
pub use trace::read_tasks;

/// 全局控制台调试输出。
pub fn debug(msg: &str) {
    get().debug(msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("agent-log-{}-{}", tag, uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("a.jsonl")
    }

    #[test]
    fn rotate_over_limit_archives_and_reports_size() {
        let path = tmp_path("over");
        fs::write(&path, vec![b'x'; 100]).unwrap();
        assert_eq!(rotate_file(&path, 50).unwrap(), Some(100));
        assert!(!path.exists(), "原文件应已被归档");
        assert!(backup_path(&path).exists(), "归档文件 <file>.1 应存在");
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rotate_under_limit_keeps_file() {
        let path = tmp_path("under");
        fs::write(&path, b"short").unwrap();
        assert_eq!(rotate_file(&path, 5000).unwrap(), None);
        assert!(path.exists());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn rotate_missing_file_is_noop() {
        let path = tmp_path("missing");
        assert_eq!(rotate_file(&path, 10).unwrap(), None);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
