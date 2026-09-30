//! search 工具:ripgrep 优先、findstr 兜底的工作区内容搜索。

use super::*;

/// ripgrep 可用性（父进程 PATH 里找一次）。
pub(crate) fn rg_available() -> bool {
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

pub(crate) fn search(args: &str, max_chars: usize, unconfined: bool) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let query = v.get("query").and_then(Value::as_str).ok_or("缺少 query 参数")?;
    if query.trim().is_empty() {
        return Err("query 为空".to_string());
    }
    let sub = v.get("path").and_then(Value::as_str).unwrap_or(".");
    let target = resolve_in_workspace(sub, true, unconfined)?;
    // path 指到文件时别报错（模型经常把文件当目录传）——直接对该文件内容搜索
    if target.is_file() {
        let output = if rg_available() {
            Command::new("rg")
                .args(["-n", "--no-heading", "-m", "50", query])
                .arg(&target)
                .current_dir(workspace_root())
                .stdin(Stdio::null())
                .creation_flags(0x0800_0000) // 不闪控制台窗口
                .output()
        } else {
            Command::new("findstr")
                .args(["/n", "/i", &format!("/c:{query}"), &target.to_string_lossy()])
                .current_dir(workspace_root())
                .stdin(Stdio::null())
                .creation_flags(0x0800_0000)
                .output()
        }
        .map_err(|e| format!("启动搜索失败: {e}"))?;
        let text = String::from_utf8_lossy(&output.stdout);
        let err = String::from_utf8_lossy(&output.stderr);
        if text.trim().is_empty() {
            if !err.trim().is_empty() {
                return Err(cap(err.trim(), 500));
            }
            return Ok("（无匹配）".to_string());
        }
        return Ok(cap(text.trim_end(), max_chars));
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
