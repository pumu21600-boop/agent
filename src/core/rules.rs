//! 细粒度审批规则引擎（P9）
//!
//! 替代“一刀切”的审批机制：
//! - Safe (0): 只读安全操作、查看代码、单元测试、只读 Git，完全静默放行；
//! - Normal (1): 工作区内的正常代码创建与修改，按常规放行并记录改动；
//! - Dangerous (2): 文件删除、工作区外部篡改、git 破坏性操作与高危命令，强制拦截卡点弹窗。

use std::path::Path;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum RiskLevel {
    /// 只读安全（无需弹窗，静默放行）
    Safe = 0,
    /// 常规改动（项目内读写与构建）
    Normal = 1,
    /// 高危破坏（必须弹窗由用户确认）
    Dangerous = 2,
}

impl RiskLevel {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// 评估具体工具调用的风险等级并给出判定依据。
pub fn evaluate_risk(tool_name: &str, args: &Value, workspace_root: &Path) -> (RiskLevel, String) {
    match tool_name {
        // 只读工具：恒定 Safe
        "read_file" | "read_page" | "search" | "web_fetch" => {
            (RiskLevel::Safe, "只读安全查询操作".into())
        }

        // 写文件：区分工作区内/越界/敏感文件
        "write_file" => {
            let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path_str.is_empty() {
                return (RiskLevel::Normal, "写入路径为空".into());
            }

            // 检查敏感关键文件
            let lower = path_str.to_lowercase().replace('\\', "/");
            if lower.contains(".git/") || lower.ends_with(".git") || lower == "settings.json" {
                return (RiskLevel::Dangerous, format!("试图修改系统核心或版本库元数据: {path_str}"));
            }

            // 检查是否越界
            let target_path = workspace_root.join(path_str);
            if path_str.starts_with("..") || path_str.contains("/../") || path_str.contains("\\..\\") {
                return (RiskLevel::Dangerous, format!("检测到跨级路径遍历: {path_str}"));
            }

            // 绝对路径检查：如果以盘符开头且不在工作区内
            if let Ok(canon_target) = target_path.canonicalize() {
                if let Ok(canon_root) = workspace_root.canonicalize() {
                    if !canon_target.starts_with(&canon_root) {
                        return (RiskLevel::Dangerous, format!("试图修改工作区外部文件: {path_str}"));
                    }
                }
            }

            (RiskLevel::Normal, format!("项目工作区文件改写: {path_str}"))
        }

        // Git 操作：区分只读、常规提交与破坏性命令
        "git" => {
            let cmd_args = args.get("args").and_then(|v| v.as_str()).unwrap_or("").trim();
            let subcmd = cmd_args.split_whitespace().next().unwrap_or("").to_lowercase();

            // 破坏性 / 强推 / 硬重置
            if cmd_args.contains("--hard")
                || cmd_args.contains("-f")
                || cmd_args.contains("--force")
                || subcmd == "push"
                || subcmd == "clean"
                || (subcmd == "reset" && cmd_args.contains("hard"))
                || (subcmd == "branch" && cmd_args.contains("-D"))
            {
                return (RiskLevel::Dangerous, format!("Git 破坏性/强推操作: git {cmd_args}"));
            }

            // 只读子命令
            if matches!(
                subcmd.as_str(),
                "status" | "diff" | "log" | "show" | "branch" | "describe" | "rev-parse" | "tag"
            ) {
                return (RiskLevel::Safe, format!("Git 只读状态查询: git {subcmd}"));
            }

            // 其他常规 Git 命令（如 add, commit, stash, checkout 等）
            (RiskLevel::Normal, format!("Git 常规本地操作: git {subcmd}"))
        }

        // Shell 命令行：区分只读诊断命令与破坏性/系统命令
        "shell" => {
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("").trim();
            let lower = command.to_lowercase();

            // 高危破坏性关键词
            let dangerous_keywords = [
                "del ", "del/", "del\\", "rmdir", "rd ", "rm -rf", "rm -r", "format ", "diskpart",
                "reg add", "reg delete", "net user", "net localgroup", "shutdown",
                "taskkill", "drop table", "drop database", "remove-item",
            ];

            for kw in dangerous_keywords {
                if lower.contains(kw) {
                    return (RiskLevel::Dangerous, format!("检测到高危系统/删除命令: {command}"));
                }
            }

            // 安全只读与常规测试构建命令
            let safe_prefixes = [
                "dir", "ls", "type ", "cat ", "echo ", "pwd", "where ", "which ",
                "cargo check", "cargo test", "npm test", "npm run test", "pytest",
                "node -v", "npm -v", "cargo --version", "rustc -v", "git status", "git diff", "git log"
            ];

            for pfx in safe_prefixes {
                if lower == pfx || lower.starts_with(&format!("{pfx} ")) || lower.starts_with(pfx) {
                    return (RiskLevel::Safe, format!("安全只读或自动化测试命令: {command}"));
                }
            }

            // 常规命令
            (RiskLevel::Normal, format!("沙箱命令执行: {command}"))
        }

        // 其他未知工具
        unknown => (RiskLevel::Dangerous, format!("未登记工具调用: {unknown}")),
    }
}

/// 判定当前审批模式下该风险等级是否需要向用户请求审批。
///
/// 模式映射：
/// - "read-only" (原 suggest)：Safe(0) 放行，Normal(1) 及以上需审批；
/// - "auto" (原 auto-edit / full-auto)：Safe(0) 与 Normal(1) 静默放行，仅 Dangerous(2) 需审批；
/// - "full-access" (原 local)：全免审（除非配置显式强制了 unsandboxed_approval）。
pub fn should_ask_approval(mode: &str, risk: RiskLevel) -> bool {
    match mode {
        "full-access" | "local" => false,
        "auto" | "full-auto" | "auto-edit" => risk >= RiskLevel::Dangerous,
        _ => risk >= RiskLevel::Normal, // read-only / suggest / 未知
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use serde_json::json;

    #[test]
    fn test_safe_tools_always_safe() {
        let root = PathBuf::from("C:/test/ws");
        assert_eq!(evaluate_risk("read_file", &json!({"path": "src/main.rs"}), &root).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("search", &json!({"query": "foo"}), &root).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("web_fetch", &json!({"url": "https://example.com"}), &root).0, RiskLevel::Safe);
    }

    #[test]
    fn test_git_readonly_vs_destructive() {
        let root = PathBuf::from("C:/test/ws");
        assert_eq!(evaluate_risk("git", &json!({"args": "status"}), &root).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "diff"}), &root).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "log --oneline -5"}), &root).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "add ."}), &root).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("git", &json!({"args": "commit -m 'feat'"}), &root).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("git", &json!({"args": "reset --hard HEAD~1"}), &root).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("git", &json!({"args": "push origin master -f"}), &root).0, RiskLevel::Dangerous);
    }

    #[test]
    fn test_shell_safe_vs_dangerous() {
        let root = PathBuf::from("C:/test/ws");
        assert_eq!(evaluate_risk("shell", &json!({"command": "cargo test"}), &root).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("shell", &json!({"command": "dir"}), &root).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("shell", &json!({"command": "cargo build"}), &root).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("shell", &json!({"command": "del /f main.rs"}), &root).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("shell", &json!({"command": "rmdir /s /q src"}), &root).0, RiskLevel::Dangerous);
    }

    #[test]
    fn test_write_file_path_traversal() {
        let root = PathBuf::from("C:/test/ws");
        assert_eq!(evaluate_risk("write_file", &json!({"path": "src/lib.rs", "content": "..."}), &root).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("write_file", &json!({"path": "../secret.txt", "content": "..."}), &root).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("write_file", &json!({"path": ".git/config", "content": "..."}), &root).0, RiskLevel::Dangerous);
    }

    #[test]
    fn test_should_ask_approval_tiers() {
        // read-only 模式：Safe 放行，Normal / Dangerous 需审批
        assert!(!should_ask_approval("read-only", RiskLevel::Safe));
        assert!(should_ask_approval("read-only", RiskLevel::Normal));
        assert!(should_ask_approval("read-only", RiskLevel::Dangerous));

        // auto 模式：Safe / Normal 自动放行，Dangerous 需审批
        assert!(!should_ask_approval("auto", RiskLevel::Safe));
        assert!(!should_ask_approval("auto", RiskLevel::Normal));
        assert!(should_ask_approval("auto", RiskLevel::Dangerous));

        // full-access 模式：全部自动放行
        assert!(!should_ask_approval("full-access", RiskLevel::Safe));
        assert!(!should_ask_approval("full-access", RiskLevel::Normal));
        assert!(!should_ask_approval("full-access", RiskLevel::Dangerous));
    }
}
