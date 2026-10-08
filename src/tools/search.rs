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

pub(crate) fn search(ws: &Path, args: &str, max_chars: usize, unconfined: bool) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let query = v.get("query").and_then(Value::as_str).ok_or("缺少 query 参数")?;
    if query.trim().is_empty() {
        return Err("query 为空".to_string());
    }
    let sub = v.get("path").and_then(Value::as_str).unwrap_or(".");
    let target = resolve_in_workspace(ws, sub, true, unconfined)?;
    // path 指到文件时别报错（模型经常把文件当目录传）——直接对该文件内容搜索
    if target.is_file() {
        let output = if rg_available() {
            Command::new("rg")
                // `-e` 显式声明模式：query 以 `-` 开头时（`--pre=del.exe` 这类）
                // 裸传会被 rg 当选项解析——`--pre` 让 rg 对搜到的每个文件
                // spawn 指定程序，等于借搜索通道拿任意命令执行（2026-10-08 修）
                .args(["-n", "--no-heading", "-m", "50", "-e", query])
                .arg(&target)
                .current_dir(ws)
                .stdin(Stdio::null())
                .creation_flags(0x0800_0000) // 不闪控制台窗口
                .output()
        } else {
            Command::new("findstr")
                .args(["/n", "/i", &format!("/c:{query}"), &target.to_string_lossy()])
                .current_dir(ws)
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
            // 同上：`-e` 防 query 被当选项解析（`--pre=` 任意命令执行）
            .args(["-n", "--no-heading", "-m", "50", "-e", query])
            .arg(&target)
            .current_dir(ws)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-08 修：query 以 `-` 开头时不得被 rg 当选项解析（`--pre=<程序>` 会让
    /// rg 对每个命中文件 spawn 该程序 = 借搜索通道任意命令执行）。
    /// 修后按普通模式匹配：文件里没有 "--version" 字样 → 无匹配。
    #[test]
    fn query_starting_with_dash_is_not_parsed_as_option() {
        if !rg_available() {
            return; // 无 rg 的机器走 findstr 分支（/c: 单参数，无此面）
        }
        let dir = std::env::temp_dir().join(format!("agent-search-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello world\n").unwrap();
        let out = search(&dir, r#"{"query":"--version"}"#, 4000, false).unwrap();
        assert_eq!(out, "（无匹配）", "query 不应被 rg 当选项解析: {out}");
        let hit = search(&dir, r#"{"query":"hello"}"#, 4000, false).unwrap();
        assert!(hit.contains("a.txt") && hit.contains("hello"), "正常搜索应命中: {hit}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
