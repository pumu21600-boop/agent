//! 工具系统（总架构 §5）—— 注册表 + 内置工具。
//!
//! P2 内置第一个工具 `read_file`（只读、限工作目录内）。
//! shell/写文件等危险工具在 P3/P4 随沙箱与审批接入。

use std::{path::PathBuf, sync::OnceLock};

use serde_json::{Value, json};

/// 统一工具接口：名称 + 参数 schema + 危险等级。
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    /// 0=safe 1=write 2=exec（审批引擎 P4 使用）
    pub danger_level: u8,
}

/// 工作目录根（P2 固定为工程根；P6 会话系统接入后按项目会话切换）。
pub fn workspace_root() -> &'static std::path::Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

fn builtin_tools() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "read_file",
        description: "读取工作目录内一个 UTF-8 文本文件的完整内容。路径相对于工作目录根。",
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "要读取的文件路径（相对工作目录，如 README.md 或 src/main.rs）"
                }
            },
            "required": ["path"]
        }),
        danger_level: 0,
    }]
}

pub fn all() -> &'static [ToolSpec] {
    static TOOLS: OnceLock<Vec<ToolSpec>> = OnceLock::new();
    TOOLS.get_or_init(builtin_tools)
}

pub fn find(name: &str) -> Option<&'static ToolSpec> {
    all().iter().find(|t| t.name == name)
}

/// 发给 OpenAI 兼容请求的 tools 数组。
pub fn definitions() -> Vec<Value> {
    all()
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters.clone(),
                }
            })
        })
        .collect()
}

/// 执行工具，返回文本结果；Err 的内容会作为 tool 消息回填给模型（不中断循环）。
pub fn execute(name: &str, args: &str, max_chars: usize) -> Result<String, String> {
    match name {
        "read_file" => read_file(args, max_chars),
        other => Err(format!("未知工具: {other}")),
    }
}

fn read_file(args: &str, max_chars: usize) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let rel = v.get("path").and_then(Value::as_str).ok_or("缺少 path 参数")?;
    if rel.is_empty() {
        return Err("path 为空".to_string());
    }
    let root = workspace_root().canonicalize().map_err(|e| format!("工作目录解析失败: {e}"))?;
    let full = root.join(rel);
    // 路径穿越校验：canonicalize 后必须仍在工作目录内
    let full = full.canonicalize().map_err(|e| format!("文件不存在或不可读: {e}"))?;
    if !full.starts_with(&root) {
        return Err(format!("拒绝访问工作目录外的路径: {rel}"));
    }
    let meta = full.metadata().map_err(|e| format!("读取失败: {e}"))?;
    if meta.is_dir() {
        return Err("目标是目录，不是文件".to_string());
    }
    let text = std::fs::read_to_string(&full)
        .map_err(|e| format!("读文件失败（可能非 UTF-8）: {e}"))?;
    if text.chars().count() > max_chars {
        let head: String = text.chars().take(max_chars).collect();
        Ok(format!("{head}\n……[已截断，共 {} 字符，仅显示前 {max_chars}]", text.chars().count()))
    } else {
        Ok(text)
    }
}
