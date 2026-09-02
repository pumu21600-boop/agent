//! JSONL 日志地基 —— 施工单 001 / 任务 A
//!
//! 约定：后续所有代码统一走本模块落日志，业务数据禁止直接 println。
//! 每条记录固定 4 个字段：ts / session_id / event_type / payload。
//! 写盘方式：追加写、单行一条 JSON + '\n'、写完立即 flush（落盘即完整，不许半行）。
//! schema 见 docs/jsonl-schema.md。

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
    sync::{Arc, Mutex, OnceLock},
};

use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};

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
}

impl Logger {
    /// 打开（追加）JSONL 文件，目录不存在则创建。
    pub fn open(dir: &Path, file_name: &str, session_id: String) -> anyhow::Result<Arc<Self>> {
        fs::create_dir_all(dir)?;
        let file = OpenOptions::new().create(true).append(true).open(dir.join(file_name))?;
        Ok(Arc::new(Self { file: Mutex::new(file), session_id }))
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
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
        match self.file.lock() {
            Ok(mut f) => {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
            Err(_) => eprintln!("[logger] 日志文件锁中毒，本条未写入"),
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

static LOGGER: OnceLock<Arc<Logger>> = OnceLock::new();

/// 进程启动时调用一次。
pub fn init(dir: &Path, file_name: &str, session_id: String) -> anyhow::Result<()> {
    let logger = Logger::open(dir, file_name, session_id)?;
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

/// 全局控制台调试输出。
pub fn debug(msg: &str) {
    get().debug(msg);
}
