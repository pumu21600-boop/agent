//! 工具系统（总架构 §5）—— 注册表 + 内置工具（施工单 002 / 005）。
//!
//! P4 第一批：read_file / search / web_fetch（只读，danger 0）、write_file（写，danger 1）、
//! shell / git（执行，danger 2，走 P3 沙箱）。审批策略（suggest/auto-edit/full-auto）在核心循环
//! 按 danger_level 判定，工具层只管执行。
//! shell / git 是阻塞的沙箱调用，包在 spawn_blocking 里。

use std::{
    path::{Path, PathBuf},
    sync::{OnceLock, RwLock},
};

use serde_json::{Value, json};
use tokio::task::spawn_blocking;

use crate::config::SandboxConfig;
use crate::sandbox::{self, SandboxLimits, SandboxSpec};

/// 统一工具接口：名称 + 参数 schema + 危险等级。
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
    /// 0=read 1=write 2=exec（审批三档按它判定）
    pub danger_level: u8,
}

static WORKSPACE: OnceLock<RwLock<PathBuf>> = OnceLock::new();

fn workspace_slot() -> &'static RwLock<PathBuf> {
    WORKSPACE.get_or_init(|| RwLock::new(crate::config::app_root()))
}

/// 工作目录根（P8.2 起运行时可切换：/api/workspace 设置，默认工程根）。
pub fn workspace_root() -> PathBuf {
    workspace_slot().read().map(|p| p.clone()).unwrap_or_else(|_| crate::config::app_root())
}

/// 切换工作目录（调用方已校验路径存在）；返回旧值供日志。
pub fn set_workspace_root(path: &Path) -> PathBuf {
    let mut slot = workspace_slot().write().unwrap_or_else(|e| e.into_inner());
    std::mem::replace(&mut *slot, path.to_path_buf())
}

/// 工具执行所需的配置快照。
#[derive(Clone)]
pub struct ExecConfig {
    pub sandbox: SandboxConfig,
    pub max_chars: usize,
    /// 本机直跑（脱沙箱）：shell/git 用完整用户 token + 继承父环境
    /// （AppContainer / 网络出向不在此重复存——run_in_sandbox 按 sandbox.mode 现算）
    pub unsandboxed: bool,
}

fn builtin_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read_file",
            description: "读取工作目录内一个 UTF-8 文本文件的完整内容。路径相对于工作目录根。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "要读取的文件路径（相对工作目录，如 README.md 或 src/main.rs）" }
                },
                "required": ["path"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "search",
            description: "在工作目录内按正则/关键字搜索文件内容，返回匹配行（默认 ripgrep，缺失时 findstr 兜底）。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "搜索关键字或正则" },
                    "path": { "type": "string", "description": "可选：限定搜索的子目录或文件（相对工作目录）" }
                },
                "required": ["query"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "web_fetch",
            description: "抓取一个 http/https 网页并返回去除 HTML 标签后的正文文本（不做 JS 渲染）。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "完整 URL，必须以 http:// 或 https:// 开头" }
                },
                "required": ["url"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "web_search",
            description: "联网搜索：按关键词搜网页，返回前几条结果的标题/网址/摘要（不需要 key）。要找资料先用它，再对感兴趣的网址用 web_fetch 抓正文。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "搜索关键词（尽量具体）" },
                    "count": { "type": "integer", "description": "可选：返回条数（默认 6，上限 10）" }
                },
                "required": ["query"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "read_page",
            description: "按页读取工作目录内的大文本文件（默认 200 行/页）。大文件优先用本工具翻页，不要一次读全。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径（相对工作目录）" },
                    "page": { "type": "integer", "description": "页码，从 1 开始" },
                    "page_size": { "type": "integer", "description": "可选：每页行数（默认 200，上限 1000）" }
                },
                "required": ["path", "page"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "read_skill",
            description: "读取一个技能（Skill）的完整 SKILL.md。系统提示的「可用技能」索引列出全部技能；用户消息中出现 $技能名 或任务与某技能描述相关时，先调用本工具读全文再按其步骤执行。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "技能名（见系统提示「可用技能」索引，如 deploy）" }
                },
                "required": ["name"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "write_file",
            description: "在工作目录内创建/覆盖一个 UTF-8 文本文件（自动创建父目录）。写入前无需确认与否决，按当前审批档位执行。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "目标文件路径（相对工作目录）" },
                    "content": { "type": "string", "description": "要写入的完整内容（覆盖式）" }
                },
                "required": ["path", "content"]
            }),
            danger_level: 1,
        },
        ToolSpec {
            name: "edit_file",
            description: "在工作目录内一个文本文件里做局部替换：把 old_string 精确替换为 new_string（改大文件、改几行时优先用它，不要用 write_file 重写全文）。old_string 必须与文件内容逐字一致（含缩进与换行）且在文件中唯一出现；出现 0 次或多次都会失败并说明原因，可读文件后再试。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "目标文件路径（相对工作目录）" },
                    "old_string": { "type": "string", "description": "要被替换的原文本（与文件逐字一致；多带几行上下文保证唯一）" },
                    "new_string": { "type": "string", "description": "替换后的新文本（删除内容时给空串）" },
                    "replace_all": { "type": "boolean", "description": "可选：old_string 出现多次时全部替换（默认 false，要求唯一匹配）" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
            danger_level: 1,
        },
        ToolSpec {
            name: "shell",
            description: "在沙箱中执行一条 Windows 命令（cmd /C，工作目录=项目根）。输出有超时与整树击杀保护。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "要执行的命令，如 git status 或 cargo build" },
                    "timeout_ms": { "type": "integer", "description": "可选：超时毫秒（上限 300000，默认取配置）" }
                },
                "required": ["command"]
            }),
            danger_level: 2,
        },
        ToolSpec {
            name: "git",
            description: "在沙箱中对工作目录仓库执行 git 命令（如 status / log --oneline -5 / diff）。等价 shell 跑 git，但语义更明确。写操作与破坏性操作（push/clean/reset --hard/tag -d 等）按风险分级走审批，绝不静默执行。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "args": { "type": "string", "description": "git 子命令及参数，如 status 或 log --oneline -5" }
                },
                "required": ["args"]
            }),
            danger_level: 2,
        },
    ]
}

pub fn all() -> &'static [ToolSpec] {
    static TOOLS: OnceLock<Vec<ToolSpec>> = OnceLock::new();
    TOOLS.get_or_init(builtin_tools)
}

pub fn find(name: &str) -> Option<&'static ToolSpec> {
    all().iter().find(|t| t.name == name)
}

/// 发给 OpenAI 兼容请求的 tools 数组（P11：含已启用+已确认的 MCP 动态工具）。
pub fn definitions() -> Vec<Value> {
    let mut out: Vec<Value> = all()
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
        .collect();
    out.extend(crate::plugin::mcp::tool_definitions());
    out.extend(crate::plugin::wasm::tool_definitions());
    out
}

/// 执行工具，返回文本结果；Err 的内容会作为 tool 消息回填给模型（不中断循环）。
pub async fn execute(cfg: &ExecConfig, name: &str, args: &str) -> Result<String, String> {
    // P11：MCP 动态工具分发（进程隔离自带边界，不走本地沙箱/工作区约束）
    if name.starts_with(crate::plugin::mcp::PREFIX) {
        return crate::plugin::mcp::call_tool(name, args).await;
    }
    // P11 017：WASM 插件工具分发（纯计算沙箱，自带边界）
    if name.starts_with(crate::plugin::wasm::PREFIX) {
        return crate::plugin::wasm::call_tool(name, args).await;
    }
    match name {
        "read_file" => read_file(args, cfg.max_chars),
        "read_page" => read_page(args, cfg.max_chars),
        "read_skill" => {
            let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .ok_or("缺少 name 参数")?;
            crate::skills::read(name)
        }
        "write_file" => write_file(args),
        "edit_file" => edit_file(args),
        "search" => search(args, cfg.max_chars),
        "web_fetch" => web_fetch(args, cfg.max_chars).await,
        "web_search" => web_search(args, cfg.max_chars).await,
        "shell" => shell(args, cfg).await,
        "git" => git(args, cfg).await,
        other => Err(format!("未知工具: {other}")),
    }
}

/// 供服务层复用（P17 组件总线写操作）：在工作区内写一个相对路径的文件。
/// 边界与 write_file 工具一致——越界硬拦、512KB 上限、原子写。
pub fn write_workspace_relative(rel: &str, content: &str) -> Result<usize, String> {
    if content.len() > MAX_FILE_BYTES {
        return Err(format!("内容过大（{} 字节，上限 512KB）", content.len()));
    }
    let full = resolve_in_workspace(rel, false)?;
    crate::fsx::write_atomic_str(&full, content).map_err(|e| format!("写文件失败: {e}"))?;
    Ok(content.len())
}

fn cap(s: &str, max: usize) -> String {
    // P5 起工具结果统一走头尾压缩（替代旧的单向硬截断）
    crate::context::compress(s, max)
}

/// 解析相对路径并做工作目录穿越校验，返回 canonicalize 后的绝对路径。
/// `must_exist=false` 时对不存在的路径校验其父目录（写文件用）。
fn resolve_in_workspace(rel: &str, must_exist: bool) -> Result<PathBuf, String> {
    if rel.trim().is_empty() {
        return Err("path 为空".to_string());
    }
    let root = workspace_root().canonicalize().map_err(|e| format!("工作目录解析失败: {e}"))?;
    let full = root.join(rel);
    if must_exist {
        let canon = full.canonicalize().map_err(|e| format!("路径不存在或不可访问: {e}"))?;
        if !canon.starts_with(&root) {
            return Err(format!("拒绝访问工作目录外的路径: {rel}"));
        }
        Ok(canon)
    } else {
        // 目标可能还不存在：先确保父目录存在，再对父目录做穿越校验
        let parent = full.parent().ok_or("路径无父目录")?.to_path_buf();
        std::fs::create_dir_all(&parent).map_err(|e| format!("创建父目录失败: {e}"))?;
        let canon_parent = parent.canonicalize().map_err(|e| format!("父目录解析失败: {e}"))?;
        if !canon_parent.starts_with(&root) {
            return Err(format!("拒绝访问工作目录外的路径: {rel}"));
        }
        Ok(canon_parent.join(full.file_name().ok_or("路径无文件名")?))
    }
}

fn read_file(args: &str, max_chars: usize) -> Result<String, String> {    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let rel = v.get("path").and_then(Value::as_str).ok_or("缺少 path 参数")?;
    let full = resolve_in_workspace(rel, true)?;
    let meta = full.metadata().map_err(|e| format!("读取失败: {e}"))?;
    if meta.is_dir() {
        return Err("目标是目录，不是文件".to_string());
    }
    let text = std::fs::read_to_string(&full).map_err(|e| format!("读文件失败（可能非 UTF-8）: {e}"))?;
    Ok(cap(&text, max_chars))
}

/// 分页读取大文件（P5）：按行分页，页码越界自动钳到最后/第一页。
fn read_page(args: &str, max_chars: usize) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let rel = v.get("path").and_then(Value::as_str).ok_or("缺少 path 参数")?;
    let page = v.get("page").and_then(Value::as_u64).ok_or("缺少 page 参数")?;
    if page == 0 {
        return Err("page 从 1 开始".to_string());
    }
    let page_size = v.get("page_size").and_then(Value::as_u64).unwrap_or(200).clamp(10, 1000) as usize;
    let full = resolve_in_workspace(rel, true)?;
    if full.metadata().map_err(|e| format!("读取失败: {e}"))?.is_dir() {
        return Err("目标是目录，不是文件".to_string());
    }
    let text = std::fs::read_to_string(&full).map_err(|e| format!("读文件失败（可能非 UTF-8）: {e}"))?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Ok(format!("{rel} 是空文件"));
    }
    let pages = lines.len().div_ceil(page_size);
    let page = (page as usize).min(pages);
    let start = (page - 1) * page_size;
    let end = (start + page_size).min(lines.len());
    let body = lines[start..end].join("\n");
    let out = format!(
        "{} 第 {page}/{pages} 页（共 {} 行，本页 {}-{} 行）\n{}",
        rel,
        lines.len(),
        start + 1,
        end,
        body
    );
    Ok(cap(&out, max_chars))
}

/// 文件类工具的统一体积上限：`write_file` 的 content、`edit_file` 的目标文件与编辑结果
/// 共用（审查 N2——edit_file 原先无上限，模型误编辑大文件时整文件读入内存）。
const MAX_FILE_BYTES: usize = 512 * 1024;

fn write_file(args: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let rel = v.get("path").and_then(Value::as_str).ok_or("缺少 path 参数")?;
    let content = v.get("content").and_then(Value::as_str).ok_or("缺少 content 参数")?;
    if content.len() > MAX_FILE_BYTES {
        return Err(format!("内容过大（{} 字节，上限 512KB）", content.len()));
    }
    let full = resolve_in_workspace(rel, false)?;
    // 原子写：写到一半失败不能留下半截文件（模型写代码文件时这是最常见的损坏来源）
    crate::fsx::write_atomic_str(&full, content).map_err(|e| format!("写文件失败: {e}"))?;
    Ok(format!("已写入 {} 字节 → {}", content.len(), rel))
}

/// 局部修改（P16）：文件里唯一出现的 old_string → new_string。
/// 大文件改几行用它，省 token 也避免重写全文时误伤其他内容。
fn edit_file(args: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let rel = v.get("path").and_then(Value::as_str).ok_or("缺少 path 参数")?;
    let old = v.get("old_string").and_then(Value::as_str).ok_or("缺少 old_string 参数")?;
    let new = v.get("new_string").and_then(Value::as_str).ok_or("缺少 new_string 参数")?;
    let replace_all = v.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
    if old.is_empty() {
        return Err("old_string 为空：本工具做定位替换，不接受空串（整文件重写请用 write_file）".to_string());
    }
    if old == new {
        return Err("old_string 与 new_string 相同，无需修改".to_string());
    }
    if new.len() > MAX_FILE_BYTES {
        return Err(format!("new_string 过大（{} 字节，上限 512KB）", new.len()));
    }
    let full = resolve_in_workspace(rel, true)?;
    let meta = full.metadata().map_err(|e| format!("读取失败: {e}"))?;
    if meta.is_dir() {
        return Err("目标是目录，不是文件".to_string());
    }
    if meta.len() > MAX_FILE_BYTES as u64 {
        return Err(format!(
            "文件过大（{} 字节，上限 512KB）：edit_file 只用于改文本片段，拒绝整文件读入",
            meta.len()
        ));
    }
    let text = std::fs::read_to_string(&full).map_err(|e| format!("读文件失败（可能非 UTF-8）: {e}"))?;
    let (out, count, line) = apply_edit(&text, old, new, replace_all, rel)?;
    if out.len() > MAX_FILE_BYTES {
        return Err(format!(
            "编辑后文件将达 {} 字节，超过 512KB 上限（原 {} 字节）：请缩小改动或用 write_file 分文件写",
            out.len(),
            text.len()
        ));
    }
    // 与其他落盘点同一原子写：替换一半失败不留半截文件
    crate::fsx::write_atomic_str(&full, &out).map_err(|e| format!("写文件失败: {e}"))?;
    Ok(format!(
        "已编辑 {rel}：{count} 处替换（首处在第 {line} 行，{} → {} 字节）",
        text.len(),
        out.len()
    ))
}

/// 定位替换的纯逻辑（可单测）：返回（替换后全文, 替换处数, 首处行号）。
///
/// CRLF 容错：文件是**纯 CRLF** 行尾而 old_string 按 LF 书写时（模型最常见的输入形态），
/// 把全文按 LF 规范化后匹配、替换，再整体还原 CRLF（往返可逆才做，混合行尾不猜测）。
fn apply_edit(
    text: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    rel: &str,
) -> Result<(String, usize, usize), String> {
    let mut work = text.to_string();
    let mut w_old = old.to_string();
    let mut w_new = new.to_string();
    let mut crlf = false;
    if text.matches(old).count() == 0 && text.contains("\r\n") && !old.contains('\r') {
        let lf = text.replace("\r\n", "\n");
        // 纯 CRLF（往返一致）且规范化后能命中，才走这条宽容路径
        if lf.replace('\n', "\r\n") == text && lf.contains(old) {
            w_new = new.replace("\r\n", "\n");
            work = lf;
            w_old = old.to_string();
            crlf = true;
        }
    }
    let n = work.matches(w_old.as_str()).count();
    if n == 0 {
        return Err(format!(
            "old_string 未命中 {rel}{}：old_string 必须与文件内容逐字一致（含缩进、空行与换行），先 read_file/read_page 取准确片段再试",
            near_line_hint(&work, old)
        ));
    }
    if n > 1 && !replace_all {
        return Err(format!(
            "old_string 在 {rel} 中出现 {n} 次，不唯一：上下各多带几行上下文让它唯一，或置 replace_all=true 全部替换"
        ));
    }
    let first = work.find(w_old.as_str()).unwrap_or(0);
    let line = work[..first].matches('\n').count() + 1;
    let out = if replace_all {
        work.replace(w_old.as_str(), w_new.as_str())
    } else {
        work.replacen(w_old.as_str(), w_new.as_str(), 1)
    };
    let out = if crlf { out.replace('\n', "\r\n") } else { out };
    Ok((out, if replace_all { n } else { 1 }, line))
}

/// 未命中时的定位提示：old_string 的首个非空行若在文件中出现，报出它所在行号，
/// 让模型知道「差不多的位置在哪、多半是缩进或空行对不上」。
fn near_line_hint(text: &str, old: &str) -> String {
    let head = old.lines().map(str::trim).find(|l| l.len() >= 8).unwrap_or("");
    if head.is_empty() {
        return String::new();
    }
    match text.find(head) {
        Some(pos) => {
            let line = text[..pos].matches('\n').count() + 1;
            format!("（old_string 首行「{head}」出现在第 {line} 行附近，多半是缩进/空行/相邻行对不上）")
        }
        None => String::new(),
    }
}

/// ripgrep 可用性（父进程 PATH 里找一次）。
fn rg_available() -> bool {
    static RG: OnceLock<bool> = OnceLock::new();
    *RG.get_or_init(|| {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("rg")
            .arg("--version")
            .creation_flags(0x0800_0000) // 不闪控制台窗口
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

fn search(args: &str, max_chars: usize) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let query = v.get("query").and_then(Value::as_str).ok_or("缺少 query 参数")?;
    if query.trim().is_empty() {
        return Err("query 为空".to_string());
    }
    let sub = v.get("path").and_then(Value::as_str).unwrap_or(".");
    let target = resolve_in_workspace(sub, true)?;
    if target.is_file() {
        return Err("path 请给目录（搜索整个文件请用 read_file）".to_string());
    }

    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let output = if rg_available() {
        Command::new("rg")
            .args(["-n", "--no-heading", "-m", "50", query])
            .arg(&target)
            .current_dir(workspace_root())
            .stdin(Stdio::null())
            .creation_flags(0x0800_0000) // 不闪控制台窗口
            .output()
    } else {
        // findstr 兜底：/s 递归 /n 行号 /i 忽略大小写；/c:<query> 必须为单参数
        Command::new("findstr")
            .args(["/s", "/n", "/i", &format!("/c:{query}")])
            .arg("*")
            .current_dir(&target)
            .stdin(Stdio::null())
            .creation_flags(0x0800_0000) // 不闪控制台窗口
            .output()
    }
    .map_err(|e| format!("启动搜索失败: {e}"))?;
    // rg 无匹配退出码 1，属正常
    let text = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    if text.trim().is_empty() {
        if !err.trim().is_empty() {
            return Err(cap(err.trim(), 500));
        }
        return Ok("（无匹配）".to_string());
    }
    Ok(cap(text.trim_end(), max_chars))
}

/// 极简 HTML → 文本：剔 script/style，去标签，解常见实体，压缩空行。
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut skip_until: Option<&str> = None;
    let lower = html.to_lowercase();
    let mut i = 0usize;
    let bytes = html.as_bytes();
    while i < html.len() {
        if let Some(end_tag) = skip_until {
            match lower[i..].find(end_tag) {
                Some(p) => {
                    i += p + end_tag.len();
                    skip_until = None;
                }
                None => break,
            }
            continue;
        }
        match bytes[i] {
            b'<' => {
                let rest = &lower[i..];
                if rest.starts_with("<script") {
                    skip_until = Some("</script>");
                } else if rest.starts_with("<style") {
                    skip_until = Some("</style>");
                } else if let Some(end) = rest.find('>') {
                    i += end + 1;
                    out.push(' ');
                    continue;
                } else {
                    break;
                }
            }
            _ => {
                let ch = html[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
                continue;
            }
        }
    }
    // 实体与空白收尾
    let text = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut collapsed = String::new();
    let mut blank = 0;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank += 1;
            if blank <= 1 {
                collapsed.push('\n');
            }
        } else {
            blank = 0;
            collapsed.push_str(t);
            collapsed.push('\n');
        }
    }
    collapsed.trim().to_string()
}

/// web_fetch / web_search 共用的 HTTP 客户端（浏览器 UA：搜索页与部分站点会按 UA 拦爬虫）。
fn web_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(10))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
        // 搜索引擎按 TLS/协议指纹分流：HTTP/2 客户端会被当成爬虫，回一张「查询对得上、结果
        // 全是别的东西」的页面（真机踩过：查「rust 语言」返回西班牙股票、德国天气）。HTTP/1.1 正常。
        .http1_only()
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))
}

async fn web_fetch(args: &str, max_chars: usize) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let url = v.get("url").and_then(Value::as_str).ok_or("缺少 url 参数")?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("url 必须以 http:// 或 https:// 开头".to_string());
    }
    let client = web_client()?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("抓取失败: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("渠道返回 {status}"));
    }
    let body = resp.text().await.map_err(|e| format!("读响应失败: {e}"))?;
    let is_html = body.trim_start().starts_with('<') || body.contains("<html");
    let text = if is_html { html_to_text(&body) } else { body };
    if text.trim().is_empty() {
        return Err("页面内容为空（可能是纯 JS 渲染页，本工具不做 JS 渲染）".to_string());
    }
    Ok(cap(&text, max_chars))
}

/// 一条搜索结果。
#[derive(Debug, Clone, PartialEq)]
struct SearchHit {
    title: String,
    url: String,
    snippet: String,
}

/// 联网搜索（026）：Bing（cn 域名，国内可直连）为主，取不到结果时回落 DuckDuckGo HTML。
/// 两个引擎都不需要 key；搜到网址后用 web_fetch 抓正文。
async fn web_search(args: &str, max_chars: usize) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let query = v.get("query").and_then(Value::as_str).map(str::trim).unwrap_or("");
    if query.is_empty() {
        return Err("query 为空".to_string());
    }
    let count = v.get("count").and_then(Value::as_u64).unwrap_or(6).clamp(1, 10) as usize;
    let client = web_client()?;
    let q = urlencode(query);
    // 引擎顺序按「直连可用性」排：www.bing.com 直连会 302 到 region 站、走代理也能出结果，
    // cn.bing.com 直连可用但走代理会被重定向成首页，DDG 只在墙外好用——三个都试一遍。
    // 引擎顺序：Bing 的 **RSS 输出**优先——那是给阅读器用的接口，结果里是干净的真 URL
    // （HTML 版的结果链接被包成 ck/a 跳转，且对程序化客户端可能返回「标题对、内容不是这回事」
    // 的页面，真机踩过）。RSS 不行再退回 HTML 版，最后 DuckDuckGo（墙外可用）。
    let engines: [(&str, String, &str); 5] = [
        ("Bing 中国站 RSS", format!("https://cn.bing.com/search?q={q}&format=rss&count={count}"), "rss"),
        ("Bing RSS", format!("https://www.bing.com/search?q={q}&format=rss&count={count}"), "rss"),
        ("Bing 中国站", format!("https://cn.bing.com/search?q={q}&count={}", count * 2), "bing"),
        ("Bing", format!("https://www.bing.com/search?q={q}&count={}", count * 2), "bing"),
        ("DuckDuckGo", format!("https://html.duckduckgo.com/html/?q={q}"), "ddg"),
    ];
    let mut hits = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for (name, url, kind) in engines {
        match client.get(url).send().await {
            Ok(r) if r.status().is_success() => {
                // 关键：只认落在「搜索结果页」上的响应。Bing 会把请求重定向到首页（/?q=…），
                // 首页上的推荐链接也带同类 CSS 类名，照解析就会把推荐位当搜索结果（真机踩过）。
                let effective = r.url().clone();
                let html = r.text().await.unwrap_or_default();
                let got = match kind {
                    "rss" => parse_bing_rss(&html, count),
                    "bing" => parse_bing(&html, count),
                    _ => parse_ddg(&html, count),
                };
                // 调试开关：设了 AGENT_WEB_DEBUG_DIR 就把原始页面落盘（排查搜索/抓取解析问题用）
                if let Ok(dir) = std::env::var("AGENT_WEB_DEBUG_DIR") {
                    let file = format!("web_{}.html", name.replace(' ', "_"));
                    let _ = std::fs::write(std::path::Path::new(&dir).join(file), &html);
                }
                // 诊断信息常驻日志（搜索出问题时靠它定位：哪个引擎、落到哪个 URL、页面标题是什么）
                crate::observability::try_event(
                    crate::observability::EventType::State,
                    json!({
                        "kind": "web.search",
                        "engine": name,
                        "url": effective.as_str(),
                        "page_title": html_title(&html),
                        "bytes": html.len(),
                        "hits": got.len(),
                    }),
                );
                if kind != "rss" && !is_search_url(effective.path()) {
                    notes.push(format!("{name}: 被重定向到 {}（不是结果页）", effective));
                } else if got.is_empty() {
                    notes.push(format!("{name}: 页面里没解析到结果（多半是反爬或代理中转页）"));
                } else {
                    hits = got;
                    break;
                }
            }
            Ok(r) => notes.push(format!("{name}: HTTP {}", r.status())),
            Err(e) => notes.push(format!("{name}: 请求失败 {e}")),
        }
    }
    if hits.is_empty() {
        return Err(format!(
            "搜索没有取到结果（{}）：换关键词再试，或直接用 web_fetch 抓已知网址",
            notes.join("；")
        ));
    }
    let mut out = format!("搜索「{query}」取到 {} 条：\n", hits.len());
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, h.title, h.url));
        if !h.snippet.is_empty() {
            out.push_str(&format!("   {}\n", h.snippet));
        }
    }
    Ok(cap(&out, max_chars))
}

/// Bing RSS 输出解析（给阅读器用的接口，链接是干净的真 URL，还带摘要）。
fn parse_bing_rss(xml: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut from = 0usize;
    while hits.len() < limit {
        let Some(rel) = xml[from..].find("<item>") else { break };
        let start = from + rel + "<item>".len();
        let Some(end_rel) = xml[start..].find("</item>") else { break };
        let item = &xml[start..start + end_rel];
        from = start + end_rel + "</item>".len();
        let title = xml_tag(item, "title");
        let url = xml_tag(item, "link");
        if title.is_empty() || url.is_empty() {
            continue;
        }
        hits.push(SearchHit {
            title: clean_text(&title),
            url: clean_text(&url),
            snippet: clean_text(&xml_tag(item, "description")),
        });
    }
    hits
}

/// 取 XML 里某个标签的文本（含实体解码；找不到返回空串）。
fn xml_tag(item: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(i) = item.find(&open) else { return String::new() };
    let rest = &item[i + open.len()..];
    let Some(j) = rest.find(&close) else { return String::new() };
    decode_entities(rest[..j].trim())
}

/// 页面 `<title>`（诊断用：判断拿到的到底是结果页还是反爬/中转页）。
fn html_title(html: &str) -> String {
    extract_tag(html, "<title", "</title>").map(|t| clean_text(&t)).unwrap_or_default()
}

/// 是不是「搜索结果页」的路径（Bing 首页 / 各种中转页都排除掉）。
fn is_search_url(path: &str) -> bool {
    matches!(path, "/search" | "/html/" | "/lite/" | "/html" | "/lite")
}

/// Bing 结果页解析：`class="b_algo"` 切块，块内 `<h2>` 拿标题与链接、`b_lineclamp` 拿摘要。
fn parse_bing(html: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut from = 0usize;
    while hits.len() < limit {
        let Some(rel) = html[from..].find("class=\"b_algo\"") else { break };
        let start = from + rel;
        let block_end = html[start + 1..]
            .find("class=\"b_algo\"")
            .map(|p| start + 1 + p)
            .unwrap_or(html.len());
        let block = &html[start..block_end];
        from = block_end;
        let Some(h2_start) = block.find("<h2") else { continue };
        let Some(h2_end) = block[h2_start..].find("</h2>").map(|p| h2_start + p) else { continue };
        let h2 = &block[h2_start..h2_end];
        let Some(href_at) = h2.find("href=\"") else { continue };
        let raw = h2[href_at + 6..].split('"').next().unwrap_or("").trim();
        let url = bing_real_url(raw);
        let title = clean_text(h2);
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let snippet = extract_after_class(block, "class=\"b_lineclamp")
            .or_else(|| extract_tag(block, "<p", "</p>"))
            .map(|s| clean_text(&s))
            .unwrap_or_default();
        hits.push(SearchHit { title, url, snippet });
    }
    hits
}

/// DuckDuckGo HTML 版解析：`result__a` 是标题与链接（链接常是 /l/?uddg= 跳转，取真身）。
fn parse_ddg(html: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut from = 0usize;
    while hits.len() < limit {
        let Some(rel) = html[from..].find("class=\"result__a\"") else { break };
        let start = from + rel;
        let Some(gt) = html[start..].find('>').map(|p| start + p) else { break };
        let Some(end) = html[gt..].find("</a>").map(|p| gt + p) else { break };
        let anchor = &html[start..gt];
        let url = anchor
            .find("href=\"")
            .map(|h| ddg_real_url(&anchor[h + 6..]))
            .unwrap_or_default();
        let title = clean_text(&html[gt + 1..end]);
        from = end + 4;
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let snippet = extract_after_class(&html[from..], "class=\"result__snippet")
            .map(|s| clean_text(&s))
            .unwrap_or_default();
        hits.push(SearchHit { title, url, snippet });
    }
    hits
}

/// Bing 的跳转链接还原：结果链接常是 `bing.com/ck/a?…&u=a1<base64url 真 URL>&…`
/// （`u=` 的值前缀 `a1` 是 Bing 的标记，后面才是编码过的网址）。
fn bing_real_url(href: &str) -> String {
    if href.contains("bing.com/ck/a")
        && let Some(i) = href.find("u=")
    {
        let val = href[i + 2..].split('&').next().unwrap_or("");
        let enc = val.strip_prefix("a1").unwrap_or(val);
        if let Some(url) = base64url_decode(enc) {
            return url;
        }
    }
    href.to_string()
}

/// base64url 解码（无填充也接受）；失败返回 None。
fn base64url_decode(s: &str) -> Option<String> {
    use base64::Engine;
    let mut t: String = s
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    while !t.len().is_multiple_of(4) {
        t.push('=');
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(t.as_bytes()).ok()?;
    String::from_utf8(bytes).ok()
}

/// DuckDuckGo 的跳转链接还原：`//duckduckgo.com/l/?uddg=<百分号编码的真 URL>&rut=…`。
fn ddg_real_url(href: &str) -> String {
    let href = href.trim();
    if let Some(i) = href.find("uddg=") {
        let rest = &href[i + 5..];
        let enc = rest.split('&').next().unwrap_or("");
        return percent_decode(enc);
    }
    if let Some(stripped) = href.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    href.to_string()
}

/// 取 `class="<cls>` 所在元素的文本：跳过开标签（后面可能还有别的属性），
/// 一直取到该元素的闭合标签（摘要里常夹 `<b>` 等内联标签，取到第一个 `<` 会截断）。
fn extract_after_class(html: &str, cls: &str) -> Option<String> {
    let i = html.find(cls)?;
    let gt = html[i..].find('>')? + i;
    let tail = &html[gt + 1..];
    let end = ["</p>", "</div>", "</a>", "</span>", "</li>"]
        .iter()
        .filter_map(|c| tail.find(c))
        .min()?;
    Some(tail[..end].to_string())
}

/// 取某个开标签到闭合标签之间的文本。
fn extract_tag(html: &str, open: &str, close: &str) -> Option<String> {
    let i = html.find(open)?;
    let gt = html[i..].find('>')? + i;
    let end = html[gt..].find(close)? + gt;
    Some(html[gt + 1..end].to_string())
}

/// 去标签 → 解实体 → 压空白（搜索结果的标题/摘要用）。
fn clean_text(s: &str) -> String {
    decode_entities(&strip_tags(s)).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// HTML 实体解码（搜索页里 `&ensp;` / `&#0183;` 这类很多）。
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let window = &tail[..tail.len().min(12)];
        let Some(semi) = window.find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let ent = &tail[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" | "ensp" | "emsp" | "thinsp" => Some(' '),
            "hellip" => Some('…'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "middot" => Some('·'),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
            }
            _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 查询串编码（空格用 %20：`+` 只在表单编码里有「空格」含义，百分号更无歧义）。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 百分号解码（UTF-8 字节流）。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'%'
            && i + 3 <= b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// shell / git 共用：沙箱（或本机直跑）内跑命令行，格式化输出。
async fn run_in_sandbox(
    cfg: &ExecConfig,
    application: PathBuf,
    command_line: String,
    timeout_ms: u32,
) -> Result<String, String> {
    let spec = SandboxSpec {
        application,
        command_line,
        working_dir: workspace_root().to_path_buf(),
        limits: SandboxLimits {
            timeout_ms,
            memory_mb: cfg.sandbox.memory_mb,
            active_processes: cfg.sandbox.active_processes,
        },
        restricted: !cfg.unsandboxed,
        appcontainer: !cfg.unsandboxed && cfg.sandbox.mode == "appcontainer",
        allow_network: cfg.sandbox.allow_network,
    };
    let out = spawn_blocking(move || sandbox::run(&spec))
        .await
        .map_err(|e| format!("沙箱任务失败: {e}"))?
        .map_err(|e| format!("沙箱拉起失败: {e}"))?;
    let mut text = format!("exit={} timed_out={}\n", out.exit_code.unwrap_or(0), out.timed_out);
    if !out.stdout.trim().is_empty() {
        text.push_str("--- stdout ---\n");
        text.push_str(out.stdout.trim_end());
        text.push('\n');
    }
    if !out.stderr.trim().is_empty() {
        text.push_str("--- stderr ---\n");
        text.push_str(out.stderr.trim_end());
        text.push('\n');
    }
    Ok(cap(text.trim_end(), cfg.max_chars))
}

async fn shell(args: &str, cfg: &ExecConfig) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let command = v.get("command").and_then(Value::as_str).ok_or("缺少 command 参数")?;
    if command.trim().is_empty() {
        return Err("command 为空".to_string());
    }
    let timeout_ms = v
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .map(|t| t.clamp(1, 300_000) as u32)
        .unwrap_or(cfg.sandbox.timeout_ms);
    let cmd = sandbox::system_cmd();
    let command_line = format!("\"{}\" /C {command}", cmd.display());
    run_in_sandbox(cfg, cmd, command_line, timeout_ms).await
}

/// git.exe 发现链（开箱即用）：AGENT_GIT_PATH 环境变量 → 父进程 PATH（where.exe）→
/// 注册表（Git for Windows 安装时登记的 InstallPath）→ 常见安装位置 → 随分发捆绑的 tools/git。
/// 找到后把相关目录登记进沙箱 PATH（shell 里按名调 git / git 内部 helper 才可用）。
pub(crate) fn git_exe() -> Result<PathBuf, String> {
    static GIT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    GIT.get_or_init(|| {
        // 1) 显式环境变量
        if let Ok(p) = std::env::var("AGENT_GIT_PATH") {
            let pb = PathBuf::from(p);
            if pb.is_file() {
                register_git_dirs(&pb);
                return Ok(pb);
            }
        }
        // 2) 父进程 PATH
        if let Some(p) = where_git() {
            register_git_dirs(&p);
            return Ok(p);
        }
        // 3) 注册表：Git for Windows 的 InstallPath（HKLM / HKCU，含 Wow6432Node 兜底）
        use std::os::windows::process::CommandExt;
        for hive in [
            "HKLM\\SOFTWARE\\GitForWindows",
            "HKLM\\SOFTWARE\\WOW6432Node\\GitForWindows",
            "HKCU\\SOFTWARE\\GitForWindows",
        ] {
            if let Ok(out) = std::process::Command::new("reg.exe")
                .args(["query", hive, "/v", "InstallPath"])
                .creation_flags(0x0800_0000) // 不闪控制台窗口
                .stdin(std::process::Stdio::null())
                .output()
            {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some(line) = text.lines().find(|l| l.contains("InstallPath"))
                    && let Some(dir) = line.split("REG_SZ").last().map(|s| s.trim()) {
                        for sub in ["cmd\\git.exe", "bin\\git.exe"] {
                            let pb = PathBuf::from(dir).join(sub);
                            if pb.is_file() {
                                register_git_dirs(&pb);
                                return Ok(pb);
                            }
                        }
                    }
            }
        }
        // 4) 常见安装位置（Git for Windows / PortableGit 习惯路径）
        let program_files = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into());
        let program_files_x86 =
            std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".into());
        let local_appdata = std::env::var("LOCALAPPDATA").unwrap_or_default();
        let mut candidates: Vec<PathBuf> = Vec::new();
        for base in [&program_files, &program_files_x86, &local_appdata] {
            if base.is_empty() {
                continue;
            }
            for rel in [
                "Git\\cmd\\git.exe",
                "Git\\bin\\git.exe",
                "PortableGit\\cmd\\git.exe",
                "PortableGit\\bin\\git.exe",
            ] {
                candidates.push(PathBuf::from(base).join(rel));
            }
        }
        // 4.5) 已知版本化便携布局：~/.workbuddy/binaries/PortableGit/versions/<最高版>/cmd/git.exe
        //      与 scoop shim（current 链接在部分 API 下不可穿透，取最高版本号目录）
        if let Ok(up) = std::env::var("USERPROFILE") {
            let vroot = PathBuf::from(&up).join(".workbuddy\\binaries\\PortableGit\\versions");
            if let Ok(rd) = std::fs::read_dir(&vroot) {
                let mut best: Option<(Vec<u32>, PathBuf)> = None;
                for e in rd.filter_map(|e| e.ok()) {
                    let fname = e.file_name().to_string_lossy().to_string();
                    if !e.path().is_dir() {
                        continue;
                    }
                    let ver: Vec<u32> = fname.split('.').filter_map(|x| x.parse::<u32>().ok()).collect();
                    if ver.is_empty() {
                        continue;
                    }
                    if best.as_ref().map(|(v, _)| ver > *v).unwrap_or(true) {
                        best = Some((ver, e.path()));
                    }
                }
                if let Some((_, dir)) = best {
                    candidates.push(dir.join("cmd\\git.exe"));
                    candidates.push(dir.join("bin\\git.exe"));
                }
            }
            candidates.push(PathBuf::from(&up).join("scoop\\apps\\git\\current\\cmd\\git.exe"));
        }
        // 5) 随分发捆绑：exe 同级 tools/git/（放入 PortableGit 即开箱可用）
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent() {
                for rel in ["tools\\git\\cmd\\git.exe", "tools\\git\\bin\\git.exe"] {
                    candidates.push(dir.join(rel));
                }
            }
        for p in candidates {
            if p.is_file() {
                register_git_dirs(&p);
                return Ok(p);
            }
        }
        // 6) 内置运行时（懒解压）：以上都没有（用户机器上没装 git）时，把随包携带的 MinGit
        //    压缩包释放到用户目录再用——不联网、不需要管理员；用户自己装了 git 就永远走不到这里。
        if let Some(p) = ensure_bundled_git() {
            register_git_dirs(&p);
            return Ok(p);
        }
        Err("未找到 git.exe：请安装 Git for Windows（https://git-scm.com/downloads），或设置环境变量 AGENT_GIT_PATH 指向 git.exe 后重启服务".into())
    })
    .clone()
}

fn where_git() -> Option<PathBuf> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    Command::new("where.exe")
        .arg("git")
        .stdin(Stdio::null())
        .creation_flags(0x0800_0000) // 不闪控制台窗口
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(String::from))
        .map(PathBuf::from)
        .filter(|p| p.exists())
}

/// 设置面板「插件」分区的 git 运行时状态：可用性 / 版本 / 来源 / 内置包情况。
/// 每次调用实时取（git_exe 的结果本身是进程内缓存：新装 git 需重启服务才会重新检测）。
pub fn git_info() -> Value {
    let rt = bundled_runtime_dir();
    let zip = bundled_zip_candidates().into_iter().find(|z| z.is_file());
    let runtime_dir = rt.as_ref().map(|d| d.display().to_string());
    let runtime_ready = rt
        .as_ref()
        .map(|d| d.join("cmd\\git.exe").is_file())
        .unwrap_or(false);
    match git_exe() {
        Ok(p) => {
            let path = p.display().to_string();
            let source = if rt.as_ref().map(|d| path_is_under(&p, d)).unwrap_or(false) {
                "bundled" // 内置 zip 解压出来的
            } else if path.replace('/', "\\").contains("\\tools\\git\\") {
                "portable" // 手动放在 exe 同级 tools\git\
            } else {
                "system" // 用户自己装的
            };
            json!({
                "ok": true,
                "path": path,
                "version": git_version(&p),
                "source": source,
                "runtime_dir": runtime_dir,
                "runtime_ready": runtime_ready,
                "zip_available": zip.is_some(),
            })
        }
        Err(e) => json!({
            "ok": false,
            "error": e,
            "runtime_dir": runtime_dir,
            "runtime_ready": runtime_ready,
            "zip_available": zip.is_some(),
        }),
    }
}

/// 路径是否位于某目录下（Windows 大小写不敏感，分隔符归一）。
fn path_is_under(child: &Path, parent: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().to_lowercase().replace('/', "\\");
    norm(child).starts_with(&norm(parent))
}

/// `git --version` → 版本号字符串（失败返回 "unknown"）。
fn git_version(git: &Path) -> String {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(git)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .creation_flags(0x0800_0000) // 不闪控制台窗口
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .map(|s| s.trim_start_matches("git version ").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// 内置 git 运行时目录（懒解压目标）：`%LOCALAPPDATA%\agent\runtime\git`。
/// 放用户目录而不是程序目录：程序可能装在只读位置（Program Files），且卸载重装不丢。
fn bundled_runtime_dir() -> Option<PathBuf> {
    std::env::var("LOCALAPPDATA")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|d| PathBuf::from(d).join("agent").join("runtime").join("git"))
}

/// 随包携带的 MinGit 压缩包候选位置：
/// `AGENT_GIT_ZIP` 环境变量 → exe 同级 `tools\git-runtime.zip`（打包分发布局）
/// → exe 上两级 `runtime\git-runtime.zip`（开发布局：target\release\agent.exe → 仓库根）。
fn bundled_zip_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("AGENT_GIT_ZIP")
        && !p.trim().is_empty() {
            v.push(PathBuf::from(p));
        }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent() {
            v.push(dir.join("tools").join("git-runtime.zip"));
            if let Some(root) = dir.parent().and_then(Path::parent) {
                v.push(root.join("runtime").join("git-runtime.zip"));
            }
        }
    v
}

/// 用系统 tar.exe（Win10 1803+ 自带 bsdtar）解压 zip：零新依赖，不引入 zip crate。
fn extract_zip(zip: &Path, dest: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    std::fs::create_dir_all(dest).map_err(|e| format!("建目录失败: {e}"))?;
    let tar = std::env::var("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32").join("tar.exe"))
        .unwrap_or_else(|_| PathBuf::from("tar.exe"));
    let out = std::process::Command::new(&tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(dest)
        .stdin(std::process::Stdio::null())
        .creation_flags(0x0800_0000) // 不闪控制台窗口
        .output()
        .map_err(|e| format!("tar 启动失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "解压失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// 第 6 步：确保内置 git 可用——已解压过直接用；否则解压随包压缩包（一次性，只发生在没装 git 的机器上）。
fn ensure_bundled_git() -> Option<PathBuf> {
    let rt = bundled_runtime_dir()?;
    ensure_bundled_git_at(&rt, &bundled_zip_candidates())
}

/// 纯逻辑（可单测）：先看运行时目录是否已有 git；没有则用第一个存在的 zip 解压；返回可用的 git.exe。
fn ensure_bundled_git_at(rt: &Path, zips: &[PathBuf]) -> Option<PathBuf> {
    const RELS: [&str; 3] = ["cmd\\git.exe", "bin\\git.exe", "mingw64\\bin\\git.exe"];
    for rel in RELS {
        let p = rt.join(rel);
        if p.is_file() {
            return Some(p);
        }
    }
    let zip = zips.iter().find(|z| z.is_file())?;
    match extract_zip(zip, rt) {
        Ok(()) => {
            let found = RELS.iter().map(|rel| rt.join(rel)).find(|p| p.is_file());
            crate::observability::try_event(
                crate::observability::EventType::State,
                json!({
                    "kind": "git.runtime_extracted",
                    "zip": zip.display().to_string(),
                    "dest": rt.display().to_string(),
                    "ok": found.is_some(),
                }),
            );
            found
        }
        Err(e) => {
            crate::observability::try_event(
                crate::observability::EventType::State,
                json!({
                    "kind": "git.runtime_extract_failed",
                    "zip": zip.display().to_string(),
                    "error": e,
                }),
            );
            None
        }
    }
}

/// 把 git.exe 相关目录登记进沙箱 PATH（cmd 为主，bin/mingw64 为 helper 兜底）。
fn register_git_dirs(git_exe: &Path) {
    let cmd_dir = git_exe.parent().map(Path::to_path_buf);
    if let Some(d) = &cmd_dir {
        crate::sandbox::register_path_dir(d);
    }
    if let Some(root) = git_exe.parent().and_then(Path::parent) {
        for rel in ["bin", "mingw64\\bin", "usr\\bin"] {
            crate::sandbox::register_path_dir(&root.join(rel));
        }
    }
}

async fn git(args: &str, cfg: &ExecConfig) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let gargs = v.get("args").and_then(Value::as_str).ok_or("缺少 args 参数")?;
    if gargs.trim().is_empty() {
        return Err("args 为空".to_string());
    }
    // push / clean / rebase 这类破坏性操作不在工具层硬禁——那样只堵住 git 工具，
    // 从 shell 跑 git 照样能绕过去，反而造成两套标准。统一交给风险引擎判高危 +
    // 用户逐次审批（见 core/rules.rs risk_git），两条路径语义一致。
    let timeout_ms = cfg.sandbox.timeout_ms.clamp(1, 300_000);
    let git = git_exe()?;
    let command_line = format!("\"{}\" {gargs}", git.display());
    run_in_sandbox(cfg, git, command_line, timeout_ms).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SandboxConfig;

    fn init_logger() {
        static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| {
            let dir = std::env::temp_dir().join("agent-test-logs");
            let _ = crate::observability::init(&dir, "tools.jsonl", format!("tools-{}", uuid::Uuid::new_v4()));
        });
    }

    fn cfg() -> ExecConfig {
        ExecConfig { sandbox: SandboxConfig::default(), max_chars: 8000, unsandboxed: false }
    }

    #[tokio::test]
    async fn write_read_roundtrip_and_traversal() {
        init_logger();
        // 正常写入（含自动建父目录）+ 读回
        let r = execute(&cfg(), "write_file", r#"{"path":".p4-test/sub/x.txt","content":"hello-p4"}"#).await;
        assert!(r.as_deref().unwrap_or("").contains("hello-p4") || r.is_ok(), "{r:?}");
        let r = execute(&cfg(), "read_file", r#"{"path":".p4-test/sub/x.txt"}"#).await;
        assert_eq!(r.unwrap(), "hello-p4");
        // 越界拒绝
        let r = execute(&cfg(), "write_file", r#"{"path":"../outside.txt","content":"x"}"#).await;
        assert!(r.is_err() && r.unwrap_err().contains("拒绝"), "越界应被拒绝");
        let r = execute(&cfg(), "read_file", r#"{"path":"C:\\Windows\\win.ini"}"#).await;
        assert!(r.is_err(), "绝对路径越界应被拒绝");
        std::fs::remove_dir_all(workspace_root().join(".p4-test")).ok();
    }

    #[tokio::test]
    async fn edit_file_unique_replace_and_failure_hints() {
        init_logger();
        let dir = workspace_root().join(".p16-test-edit");
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(dir.join("a.txt"), "alpha_one\nbeta_two\ngamma_three\n").ok();

        // 唯一替换：内容正确落盘，结果带处数与行号
        let r = execute(&cfg(), "edit_file", r#"{"path":".p16-test-edit/a.txt","old_string":"beta_two","new_string":"BETA_TWO"}"#).await.unwrap();
        assert!(r.contains("1 处替换") && r.contains("第 2 行"), "{r}");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "alpha_one\nBETA_TWO\ngamma_three\n");

        // 未命中：拒绝并带定位提示（首行「gamma_three」在文件第 3 行）
        let e = execute(&cfg(), "edit_file", r#"{"path":".p16-test-edit/a.txt","old_string":"gamma_three\nnope_tail","new_string":"x"}"#).await.unwrap_err();
        assert!(e.contains("未命中") && e.contains("第 3 行附近"), "{e}");

        // 不唯一：要求加长上下文；replace_all 放行
        std::fs::write(dir.join("b.txt"), "dup\ndup\n").ok();
        let e = execute(&cfg(), "edit_file", r#"{"path":".p16-test-edit/b.txt","old_string":"dup","new_string":"D"}"#).await.unwrap_err();
        assert!(e.contains("不唯一") && e.contains("2 次"), "{e}");
        let r = execute(&cfg(), "edit_file", r#"{"path":".p16-test-edit/b.txt","old_string":"dup","new_string":"D","replace_all":true}"#).await.unwrap();
        assert!(r.contains("2 处替换"), "{r}");
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "D\nD\n");

        // 空 old_string / 新旧相同：直接拒绝（不做猜测）
        let e = execute(&cfg(), "edit_file", r#"{"path":".p16-test-edit/a.txt","old_string":"","new_string":"x"}"#).await.unwrap_err();
        assert!(e.contains("为空"), "{e}");
        let e = execute(&cfg(), "edit_file", r#"{"path":".p16-test-edit/a.txt","old_string":"alpha","new_string":"alpha"}"#).await.unwrap_err();
        assert!(e.contains("无需修改"), "{e}");

        // 越界与目录：拒绝（不存在的外界路径先被「路径不存在」拦下；真实存在的外部文件走「拒绝」分支）
        let e = execute(&cfg(), "edit_file", r#"{"path":"../outside.txt","old_string":"a","new_string":"b"}"#).await.unwrap_err();
        assert!(e.contains("路径不存在") || e.contains("拒绝"), "{e}");
        let e = execute(&cfg(), "edit_file", r#"{"path":"C:\\Windows\\win.ini","old_string":"a","new_string":"b"}"#).await.unwrap_err();
        assert!(e.contains("拒绝"), "{e}");
        let e = execute(&cfg(), "edit_file", r#"{"path":".p16-test-edit","old_string":"a","new_string":"b"}"#).await.unwrap_err();
        assert!(e.contains("目录"), "{e}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn edit_file_tolerates_lf_old_string_on_crlf_file() {
        init_logger();
        let dir = workspace_root().join(".p16-test-edit-crlf");
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(dir.join("c.txt"), "one\r\ntwo\r\nthree\r\n").ok();
        // 文件纯 CRLF、old_string 按 LF 写：规范化匹配，结果整体保持 CRLF
        let r = execute(&cfg(), "edit_file", r#"{"path":".p16-test-edit-crlf/c.txt","old_string":"two\nthree","new_string":"TWO\nTHREE"}"#).await.unwrap();
        assert!(r.contains("第 2 行"), "{r}");
        assert_eq!(std::fs::read_to_string(dir.join("c.txt")).unwrap(), "one\r\nTWO\r\nTHREE\r\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn edit_file_rejects_oversized_file_and_result() {
        init_logger();
        let dir = workspace_root().join(".p16-test-edit-big");
        std::fs::create_dir_all(&dir).ok();

        // 目标文件超上限：拒绝整文件读入（N2），原文件一字节不动
        let big = "a".repeat(512 * 1024 + 1);
        std::fs::write(dir.join("big.txt"), &big).ok();
        let e = execute(&cfg(), "edit_file", r#"{"path":".p16-test-edit-big/big.txt","old_string":"a","new_string":"b"}"#).await.unwrap_err();
        assert!(e.contains("文件过大") && e.contains("512KB"), "{e}");
        assert_eq!(std::fs::read_to_string(dir.join("big.txt")).unwrap(), big);

        // 编辑结果超上限：拒绝落盘（与 write_file 同一上限）
        std::fs::write(dir.join("grow.txt"), format!("x{}", "z".repeat(200))).ok();
        let payload = format!(
            r#"{{"path":".p16-test-edit-big/grow.txt","old_string":"x","new_string":"{}"}}"#,
            "y".repeat(512 * 1024)
        );
        let e = execute(&cfg(), "edit_file", &payload).await.unwrap_err();
        assert!(e.contains("编辑后") && e.contains("512KB"), "{e}");
        assert_eq!(std::fs::read_to_string(dir.join("grow.txt")).unwrap().len(), 201);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn search_hits_created_file() {
        init_logger();
        // 用独立目录，避免与 write 测试并行时互删素材
        let dir = workspace_root().join(".p4-test-search");
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(dir.join("hay.txt"), "needle_marker_p4 here\nnope\n").ok();
        let r = execute(&cfg(), "search", r#"{"query":"needle_marker_p4","path":".p4-test-search"}"#).await;
        assert!(r.unwrap().contains("needle_marker_p4"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 搜索页解析（离线夹具）：Bing 的 b_algo 块、DDG 的 result__a/跳转链接还原、实体解码。
    #[test]
    fn web_search_parsers_extract_hits() {
        // Bing：块内 <h2> 拿标题与链接，b_lineclamp 拿摘要（含 &ensp; 与 &#0183; 这类实体）
        let bing = r#"
        <li class="b_algo" data-id iid=SERP.1><h2><a href="https://rust-lang.org/">Rust Programming &amp; Language</a></h2>
        <p class="b_lineclamp2">Rust is <b>fast</b>: no runtime.&ensp;&#0183;&ensp;Memory safe.</p></li>
        <li class="b_algo" data-id iid=SERP.2><h2><a href="https://example.com/b">Second</a></h2>
        <p class="b_lineclamp3">Snippet two</p></li>
        "#;
        let hits = parse_bing(bing, 5);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].url, "https://rust-lang.org/");
        assert_eq!(hits[0].title, "Rust Programming & Language");
        assert_eq!(hits[0].snippet, "Rust is fast: no runtime. · Memory safe.");
        assert_eq!(hits[1].url, "https://example.com/b");
        // 上限生效
        assert_eq!(parse_bing(bing, 1).len(), 1);

        // Bing 结果链接是 ck/a 跳转（u=a1<base64url>），要还原成真 URL
        let ck = "https://www.bing.com/ck/a?!&amp;&amp;p=xx&amp;u=a1aHR0cHM6Ly9ydXN0LWxhbmcub3JnL3poLUNOLw&amp;ntb=1";
        assert_eq!(bing_real_url(ck), "https://rust-lang.org/zh-CN/");
        assert_eq!(bing_real_url("https://example.com/plain"), "https://example.com/plain");

        // DuckDuckGo：result__a 的 href 是 /l/?uddg= 跳转，要还原成真 URL
        let ddg = r#"
        <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&amp;rut=xx">Example &hellip; Title</a>
        <a class="result__snippet" href="https://example.com/a?b=1">Snippet <b>here</b>.</a>
        "#;
        let hits = parse_ddg(ddg, 5);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].url, "https://example.com/a?b=1");
        assert_eq!(hits[0].title, "Example … Title");
        assert_eq!(hits[0].snippet, "Snippet here.");
    }

    /// Bing RSS 解析：真 URL、实体解码、条数上限。
    #[test]
    fn web_search_parses_bing_rss() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>必应：rust</title>
        <item><title>Rust Programming Language</title><link>https://rust-lang.org/</link><description>Rust is &lt;fast&gt; &amp; safe.</description></item>
        <item><title>Rust 教程 | 菜鸟教程</title><link>https://www.runoob.com/rust/rust-tutorial.html?x=1&amp;y=2</link><description>中文摘要</description></item>
        <item><title>Bad</title><description>没有 link，应被跳过</description></item>
        </channel></rss>"#;
        let hits = parse_bing_rss(xml, 6);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].title, "Rust Programming Language");
        assert_eq!(hits[0].url, "https://rust-lang.org/");
        // 描述里是转义过的 HTML：先解实体再剥标签（和 HTML 版一致）
        assert_eq!(hits[0].snippet, "Rust is & safe.");
        assert_eq!(hits[1].url, "https://www.runoob.com/rust/rust-tutorial.html?x=1&y=2");
        assert_eq!(parse_bing_rss(xml, 1).len(), 1);
    }

    #[test]
    fn urlencode_and_percent_decode_roundtrip() {
        let raw = "rust 语言 & 中文/符号";
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("中文"), "%E4%B8%AD%E6%96%87");
        assert_eq!(percent_decode(&urlencode(raw)), raw);
        // 不完整/非法的百分号序列原样保留，不 panic
        assert_eq!(percent_decode("%zz%"), "%zz%");
    }

    #[tokio::test]
    async fn web_fetch_rejects_non_http() {
        init_logger();
        let r = execute(&cfg(), "web_fetch", r#"{"url":"ftp://example.com"}"#).await;
        assert!(r.is_err() && r.unwrap_err().contains("http"));
    }

    #[tokio::test]
    async fn shell_runs_in_sandbox() {
        init_logger();
        let r = execute(&cfg(), "shell", r#"{"command":"echo p4-sandbox-ok"}"#).await;
        let out = r.unwrap();
        assert!(out.contains("p4-sandbox-ok") && out.contains("exit=0"), "{out}");
    }

    #[tokio::test]
    async fn git_version_runs() {
        init_logger();
        let r = execute(&cfg(), "git", r#"{"args":"--version"}"#).await;
        assert!(r.unwrap().contains("git version"));
        // push 的拦截归属：工具层不再硬禁（否则 shell 里跑 git push 能绕过，形成两套标准），
        // 统一由 core::rules 判高危 + 逐次审批 —— 判定见 rules::tests::test_git_push_uniform_risk。
    }

    #[tokio::test]
    async fn read_page_paging_and_bounds() {
        init_logger();
        let dir = workspace_root().join(".p5-test-page");
        std::fs::create_dir_all(&dir).ok();
        let body: String = (1..=250).map(|i| format!("line-{i}\n")).collect();
        std::fs::write(dir.join("big.txt"), &body).ok();
        // 第 1 页：200 行
        let r = execute(&cfg(), "read_page", r#"{"path":".p5-test-page/big.txt","page":1}"#).await.unwrap();
        assert!(r.contains("1/2 页") && r.contains("line-1") && r.contains("line-200"));
        // 第 2 页：50 行
        let r = execute(&cfg(), "read_page", r#"{"path":".p5-test-page/big.txt","page":2}"#).await.unwrap();
        assert!(r.contains("2/2 页") && r.contains("line-201") && r.contains("line-250"));
        // 页码越界钳到最后页
        let r = execute(&cfg(), "read_page", r#"{"path":".p5-test-page/big.txt","page":99}"#).await.unwrap();
        assert!(r.contains("2/2 页"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn unsandboxed_inherits_full_env() {
        init_logger();
        // 差分断言：父进程环境里的变量，本机直跑子进程能看到（沙箱白名单看不到）
        unsafe { std::env::set_var("AGENT_P4_UNSAFE_MARKER", "local-run-ok") };
        let mut c = cfg();
        c.unsandboxed = true;
        let r = execute(&c, "shell", r#"{"command":"echo %AGENT_P4_UNSAFE_MARKER%"}"#).await.unwrap();
        assert!(r.contains("local-run-ok"), "本机直跑应继承完整环境: {r}");

        let r2 = execute(&cfg(), "shell", r#"{"command":"echo %AGENT_P4_UNSAFE_MARKER%"}"#).await;
        let out2 = r2.unwrap();
        assert!(
            !out2.contains("local-run-ok"),
            "沙箱模式不得泄漏父环境变量: {out2}"
        );
    }

    #[test]
    fn bundled_zip_candidates_cover_known_layouts() {
        let cands = bundled_zip_candidates();
        assert!(!cands.is_empty(), "至少应有 exe 相对布局候选");
        let norm: Vec<String> = cands
            .iter()
            .map(|p| p.to_string_lossy().replace('/', "\\"))
            .collect();
        assert!(
            norm.iter().any(|p| p.ends_with("tools\\git-runtime.zip")),
            "打包布局（exe 同级 tools\\git-runtime.zip）应在候选里: {norm:?}"
        );
        assert!(
            norm.iter().any(|p| p.ends_with("runtime\\git-runtime.zip")),
            "开发布局（上两级 runtime\\git-runtime.zip）应在候选里: {norm:?}"
        );
    }

    #[test]
    fn bundled_runtime_dir_is_under_localappdata() {
        if std::env::var("LOCALAPPDATA").map(|s| s.is_empty()).unwrap_or(true) {
            return; // 无 LOCALAPPDATA 的环境跳过
        }
        let d = bundled_runtime_dir().expect("有 LOCALAPPDATA 就应有运行时目录");
        assert!(d.ends_with(Path::new("agent").join("runtime").join("git")), "{d:?}");
    }

    #[test]
    fn extract_zip_roundtrip_with_system_tar() {
        let dir = std::env::temp_dir().join(format!("agent-zip-{}", uuid::Uuid::new_v4()));
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("hello.txt"), b"hi").unwrap();
        let zip = dir.join("t.zip");
        // 用系统 tar 造一个 zip（造不出来=环境无 bsdtar，直接跳过，不算失败）
        let tar = std::env::var("SystemRoot")
            .map(|r| PathBuf::from(r).join("System32").join("tar.exe"))
            .unwrap_or_else(|_| PathBuf::from("tar.exe"));
        let made = std::process::Command::new(&tar)
            .arg("-a")
            .arg("-cf")
            .arg(&zip)
            .arg("-C")
            .arg(&src)
            .arg(".")
            .output();
        match made {
            Ok(o) if o.status.success() && zip.is_file() => {}
            _ => {
                let _ = std::fs::remove_dir_all(&dir);
                return; // 环境不支持则跳过
            }
        }
        let dest = dir.join("out");
        extract_zip(&zip, &dest).expect("解压应成功");
        assert!(dest.join("hello.txt").is_file(), "解压后应能看到文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bundled_git_lazy_extract_end_to_end() {
        // 用随包携带的真实 MinGit zip（开发机上有；干净克隆里没有就跳过）
        let zip = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("runtime")
            .join("git-runtime.zip");
        if !zip.is_file() {
            return;
        }
        let rt = std::env::temp_dir().join(format!("agent-git-rt-{}", uuid::Uuid::new_v4()));
        // 第一次：应解压并找到 git.exe
        let found = ensure_bundled_git_at(&rt, &[zip]).expect("应能从随包 zip 解压出 git.exe");
        assert!(found.is_file(), "{found:?}");
        // 第二次：运行时目录已有，应直接复用（zips 传空也能命中）
        let again = ensure_bundled_git_at(&rt, &[]).expect("第二次应复用已解压目录");
        assert!(again.is_file(), "{again:?}");
        let _ = std::fs::remove_dir_all(&rt);
    }
}
