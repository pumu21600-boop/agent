//! 细粒度审批规则引擎（P9，v1 转正）
//!
//! 替代“一刀切”的审批机制：
//! - Safe (0): 只读安全操作、查看代码、单元测试、只读 Git，完全静默放行；
//! - Normal (1): 工作区内的正常代码创建与修改，按常规放行并记录改动；
//! - Dangerous (2): 文件删除、工作区外部篡改、git 破坏性操作与高危命令，强制拦截卡点弹窗。
//!
//! v1 相对 v0（P8.5 提前落地的初版）修正的判定缺陷：
//! 1. shell 命令不再整体前缀匹配——先按命令分隔符拆成多条子命令，逐条判定取最高风险，
//!    堵住 `cargo test && del /f x` 这类串联绕过；
//! 2. 高危/安全名单改按「程序名 token」精确匹配，不再用子串——
//!    `echo "del x"` 不再误判高危，`dirname` 不再因 `dir` 前缀被误判安全；
//! 3. shell 里跑 git 时复用 git 子命令判定（`git push` 不再是常规）；
//! 4. write_file 敏感清单扩展（.env / 密钥文件 / .agent 记忆层等）；
//! 5. 工作区越界判定在 Windows 上大小写不敏感（盘符/目录名大小写差异不再误判）。

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

    /// 前端展示用的档位名（中文，审批卡显示判定依据时一并使用）
    pub fn label(self) -> &'static str {
        match self {
            RiskLevel::Safe => "安全",
            RiskLevel::Normal => "常规改动",
            RiskLevel::Dangerous => "高危操作",
        }
    }
}

/// 已知工具登记表。
///
/// **锚点**：与 `src/tools/mod.rs` 的注册表保持一致——那边新增工具，这边必须同步登记，
/// 否则未登记工具会走 Dangerous 默认策略（见 `evaluate_risk` 末尾分支）。
/// P11 的 MCP 工具接入时同样在此登记。
const KNOWN_TOOLS: &[&str] = &[
    "read_file", "read_page", "search", "web_fetch", "write_file", "shell", "git",
];

/// 高危程序名（按第一条子命令的程序名 token 精确匹配，小写）。
/// 覆盖删除、磁盘、注册表、账号、服务、进程终止等不可逆操作。
const DANGEROUS_PROGRAMS: &[&str] = &[
    "del", "erase", "rm", "rmdir", "rd", "format", "diskpart", "cipher",
    "reg", "net", "shutdown", "taskkill", "remove-item", "ri", "del-item", "rm-item",
    "takeown", "icacls", "bcdedit", "schtasks", "sc", "wmic", "mklink", "attrib",
];

/// 只读/诊断类程序（无论参数如何都判 Safe）。
/// 注意是精确匹配：`dirname` 不会因 `dir` 而被误判安全。
const SAFE_PROGRAMS: &[&str] = &[
    "dir", "ls", "type", "cat", "echo", "pwd", "where", "which", "head", "tail",
    "findstr", "grep", "wc", "sort", "tree", "hostname", "whoami", "date", "time",
    "ver", "python", "python3", "pytest", "npx",
];

/// 需要看子命令才能判安全的程序（程序 + 第二个 token 精确匹配）。
/// 不在表内的同程序命令（如 `cargo build`、`npm run build`）保持 Normal。
const SAFE_SUBCOMMANDS: &[(&str, &str)] = &[
    ("cargo", "check"),
    ("cargo", "test"),
    ("cargo", "fmt"),
    ("cargo", "clippy"),
    ("cargo", "--version"),
    ("cargo", "-v"),
    ("npm", "test"),
    ("npm", "--version"),
    ("npm", "-v"),
    ("node", "-v"),
    ("node", "--version"),
    ("rustc", "--version"),
    ("rustc", "-v"),
    ("rustup", "--version"),
    ("python", "-v"),
    ("python", "--version"),
];

/// 三段式安全组合（程序 + 子命令 + 参数），用于 `npm run test` 这类。
const SAFE_TRIPLES: &[(&str, &str, &str)] = &[("npm", "run", "test"), ("npm", "run", "test:")];

/// write_file 敏感路径特征（命中即 Dangerous，小写匹配）。
const SENSITIVE_PATTERNS: &[&str] = &[
    ".git/",
    ".agent/",
    ".ssh/",
    ".aws/",
    "settings.json",
    ".env",
    ".npmrc",
    ".netrc",
    "credentials",
    ".htpasswd",
    "id_rsa",
    "id_ed25519",
    ".keystore",
];

/// write_file 敏感后缀（密钥/证书类）。
const SENSITIVE_SUFFIXES: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".jks", ".ppk"];

/// 评估具体工具调用的风险等级并给出判定依据。
pub fn evaluate_risk(tool_name: &str, args: &Value, workspace_root: &Path) -> (RiskLevel, String) {
    // P11（施工单 016）：MCP 动态工具——档位由 plugin 层按「首装确认 + server 配置」判定：
    // 未确认/禁用/未知 → Dangerous（总架构 §7 tool poisoning 防线）；确认后 Normal/Dangerous。
    if let Some(rest) = tool_name.strip_prefix(crate::plugin::wasm::PREFIX) {
        return match crate::plugin::wasm::risk_for(tool_name) {
            Some(2) => (RiskLevel::Dangerous, format!("WASM 插件工具（加载失败/未知）: {rest}")),
            Some(_) => (RiskLevel::Normal, format!("WASM 插件工具（本地可信目录）: {rest}")),
            None => (RiskLevel::Dangerous, format!("未知 WASM 插件工具: {tool_name}")),
        };
    }
    if let Some(rest) = tool_name.strip_prefix(crate::plugin::mcp::PREFIX) {
        return match crate::plugin::mcp::risk_for(tool_name) {
            Some(2) => (
                RiskLevel::Dangerous,
                format!("MCP 工具（高危档或未确认接入）: {rest}"),
            ),
            Some(_) => (
                RiskLevel::Normal,
                format!("MCP 工具（已确认接入，按常规改动审批）: {rest}"),
            ),
            None => (RiskLevel::Dangerous, format!("未知 MCP 工具: {tool_name}")),
        };
    }
    match tool_name {
        // 只读工具：恒定 Safe
        "read_file" | "read_page" | "search" | "web_fetch" => {
            (RiskLevel::Safe, "只读安全查询操作".into())
        }

        // 写文件：区分工作区内/越界/敏感文件
        "write_file" => risk_write_file(args, workspace_root),

        // Git 操作：区分只读、常规提交与破坏性命令
        "git" => {
            let cmd_args = args.get("args").and_then(|v| v.as_str()).unwrap_or("").trim();
            risk_git(cmd_args)
        }

        // Shell 命令行：拆分后逐条判定，取最高风险
        "shell" => {
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("").trim();
            risk_shell(command)
        }

        // 已登记但暂无专门规则的工具：按常规改动保守处理
        other if KNOWN_TOOLS.contains(&other) => {
            (RiskLevel::Normal, format!("已登记工具无专门规则，按常规处理: {other}"))
        }

        // 未登记工具：一律高危，防止新型/伪造工具名静默执行
        unknown => (RiskLevel::Dangerous, format!("未登记工具调用: {unknown}")),
    }
}

/// write_file 风险判定：敏感文件 > 路径遍历 > 越界 > 常规。
fn risk_write_file(args: &Value, workspace_root: &Path) -> (RiskLevel, String) {
    let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    if path_str.is_empty() {
        return (RiskLevel::Normal, "写入路径为空".into());
    }

    let lower = path_str.to_lowercase().replace('\\', "/");

    // 1) 敏感关键文件（版本库元数据 / 密钥 / 记忆层 / 凭据）
    if SENSITIVE_PATTERNS.iter().any(|p| lower.contains(p))
        || SENSITIVE_SUFFIXES.iter().any(|s| lower.ends_with(s))
    {
        return (RiskLevel::Dangerous, format!("试图修改敏感或系统关键文件: {path_str}"));
    }

    // 2) 跨级路径遍历
    if path_str.starts_with("..") || lower.contains("/../") || path_str.contains("\\..\\") {
        return (RiskLevel::Dangerous, format!("检测到跨级路径遍历: {path_str}"));
    }

    // 3) 绝对路径越界（Windows 大小写不敏感比较，避免盘符/目录名大小写差异误判）
    let target_path = workspace_root.join(path_str);
    if let Ok(canon_target) = target_path.canonicalize() {
        if let Ok(canon_root) = workspace_root.canonicalize() {
            if !is_within_root(&canon_root, &canon_target) {
                return (RiskLevel::Dangerous, format!("试图修改工作区外部文件: {path_str}"));
            }
        }
    }

    (RiskLevel::Normal, format!("项目工作区文件改写: {path_str}"))
}

/// git 子命令风险判定（工具参数与 shell 里的 git 命令共用同一套语义）。
fn risk_git(cmd_args: &str) -> (RiskLevel, String) {
    let cmd_args = cmd_args.trim();
    let mut it = cmd_args.split_whitespace();
    let subcmd = it.next().unwrap_or("").to_lowercase();
    let second = it.next().unwrap_or("").to_lowercase();
    let lower = cmd_args.to_lowercase();

    // 1) 破坏性 / 强推 / 硬重置 / 丢弃改动
    if lower.contains("--hard")
        || lower.contains(" -f ")
        || lower.contains("--force")
        || subcmd == "push"
        || subcmd == "clean"
        || (subcmd == "reset" && lower.contains("hard"))
        || (subcmd == "branch" && (lower.contains("-d") || lower.contains("--delete")))
        || subcmd == "rebase"
        || (subcmd == "stash" && (second == "drop" || second == "clear"))
        || (subcmd == "checkout" && (second == "." || second == "--" || second == "-f"))
        || (subcmd == "remote" && second == "remove")
    {
        return (RiskLevel::Dangerous, format!("Git 破坏性/强推操作: git {cmd_args}"));
    }

    // 2) 只读子命令
    if matches!(
        subcmd.as_str(),
        "status" | "diff" | "log" | "show" | "branch" | "describe" | "rev-parse" | "tag" | "remote"
    ) && second != "-v"
        || (subcmd == "branch" && second.is_empty())
        || (subcmd == "remote" && second == "-v")
    {
        return (RiskLevel::Safe, format!("Git 只读状态查询: git {subcmd}"));
    }

    // 3) 其他常规 Git 命令（add / commit / stash / checkout 分支等）
    (RiskLevel::Normal, format!("Git 常规本地操作: git {subcmd}"))
}

/// shell 命令风险判定：按分隔符拆成子命令，逐条判定取最高风险。
fn risk_shell(command: &str) -> (RiskLevel, String) {
    if command.trim().is_empty() {
        return (RiskLevel::Safe, "空命令".into());
    }

    let parts = split_commands(command);
    let mut worst = RiskLevel::Safe;
    let mut worst_reason = String::from("沙箱命令执行");

    for p in parts {
        let (risk, reason) = risk_simple_command(&p);
        if risk > worst {
            worst = risk;
            worst_reason = reason;
        }
        if worst == RiskLevel::Dangerous {
            break; // 已到顶，无需再看
        }
    }

    (worst, format!("{worst_reason}: {command}"))
}

/// 单条子命令的风险判定（不含串联分隔符）。
fn risk_simple_command(cmd: &str) -> (RiskLevel, String) {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return (RiskLevel::Safe, "空子命令".into());
    }
    let lower = trimmed.to_lowercase();
    let prog = program_name(trimmed);
    let rest: Vec<&str> = trimmed.split_whitespace().collect();
    let second = rest.get(1).copied().unwrap_or("").to_lowercase();
    let third_token = rest.get(2).copied().unwrap_or("").to_lowercase();

    // 1) shell 里跑 git：复用 git 子命令语义（git push 不能算常规）
    if prog == "git" {
        let git_args: String = rest.iter().skip(1).copied().collect::<Vec<_>>().join(" ");
        return risk_git(&git_args);
    }

    // 2) 高危程序（精确 token 匹配，避免正文/参数里的同名子串误伤）
    if DANGEROUS_PROGRAMS.contains(&prog.as_str()) {
        return (RiskLevel::Dangerous, format!("检测到高危系统/删除命令"));
    }

    // 3) SQL 类破坏性语句（非程序名场景，保留子串兜底）
    if lower.contains("drop table") || lower.contains("drop database") {
        return (RiskLevel::Dangerous, format!("检测到高危数据库删除语句"));
    }

    // 4) 重定向/管道写文件：即便程序只读也算常规改动（可能覆盖文件）
    let writes = lower.contains('>') || lower.contains("| out-file") || lower.contains("tee ");

    // 5) 安全程序（精确匹配，且无写文件副作用）
    let safe = SAFE_PROGRAMS.contains(&prog.as_str())
        || SAFE_SUBCOMMANDS.contains(&(prog.as_str(), second.as_str()))
        || SAFE_TRIPLES
            .iter()
            .any(|(p, s, t)| prog == *p && second == *s && third_token.starts_with(t));

    if safe && !writes {
        return (RiskLevel::Safe, format!("安全只读或自动化测试命令"));
    }

    // 6) 其余命令（含安全程序带重定向、cargo build 等）
    (RiskLevel::Normal, format!("沙箱命令执行"))
}

/// 取子命令的程序名：剥离路径与扩展名，小写。
/// `C:\Tools\Del.EXE /f x` → `del`；`del/f x` → `del`；`dirname x` → `dirname`。
fn program_name(cmd: &str) -> String {
    let tok = cmd
        .trim()
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches('"')
        .trim_matches('\'');
    // 剥离目录路径
    let file = tok.rsplit(['\\', '/']).next().unwrap_or(tok);
    // 剥离 `程序/参数` 紧邻写法（如 del/f）
    let file = file.split('/').next().unwrap_or(file);
    let lower = file.to_lowercase();
    // 剥离可执行文件扩展名
    for ext in [".exe", ".cmd", ".bat", ".ps1", ".com"] {
        if let Some(stripped) = lower.strip_suffix(ext) {
            return stripped.to_string();
        }
    }
    lower
}

/// 按命令分隔符拆分（`&&` `||` `;` `|` 换行），引号内的分隔符不拆。
fn split_commands(cmd: &str) -> Vec<String> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ => {}
        }
        if !in_single && !in_double {
            let two = if i + 1 < chars.len() {
                format!("{c}{}", chars[i + 1])
            } else {
                String::new()
            };
            if two == "&&" || two == "||" {
                if !cur.trim().is_empty() {
                    out.push(cur.clone());
                }
                cur.clear();
                i += 2;
                continue;
            }
            if c == ';' || c == '|' || c == '\n' {
                if !cur.trim().is_empty() {
                    out.push(cur.clone());
                }
                cur.clear();
                i += 1;
                continue;
            }
        }
        cur.push(c);
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(cmd.to_string());
    }
    out
}

/// target 是否位于 root 之内（Windows 上大小写不敏感，避免盘符/目录名差异误判）。
fn is_within_root(root: &Path, target: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('\\', "/").trim_end_matches('/').to_lowercase();
    let r = norm(root);
    let t = norm(target);
    if r.is_empty() {
        return true; // 无工作区约束时不判越界
    }
    t == r || t.starts_with(&format!("{r}/"))
}

/// 「同类操作」的范围标识（P9 A3：会话内同类允许 的记忆键）。
///
/// 粒度取「工具名 + 风险档 + 参数特征」，既不能粗到「放行整个 shell」，
/// 也不细到「只放行这一条命令」：
/// - shell → 程序名（同程序、同风险档的后续命令不再问）；
/// - write_file → 文件所在目录（同目录下的后续写入不再问）；
/// - git → 子命令（同子命令、同风险档不再问）；
/// - 其他 → 仅工具名 + 风险档。
///
/// 风险档参与 key：同一程序在 Normal 档获批，不代表 Dangerous 档也放行。
pub fn approval_scope(tool_name: &str, args: &Value, risk: RiskLevel) -> String {
    let tag = risk.as_u8();
    match tool_name {
        "shell" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            format!("shell:{tag}:{}", program_name(cmd))
        }
        "write_file" => {
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let dir = p
                .replace('\\', "/")
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default();
            format!("write_file:{tag}:{dir}")
        }
        "git" => {
            let sub = args
                .get("args")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_lowercase();
            format!("git:{tag}:{sub}")
        }
        other => format!("{other}:{tag}"),
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

    fn ws() -> PathBuf {
        PathBuf::from("C:/test/ws")
    }

    #[test]
    fn test_safe_tools_always_safe() {
        assert_eq!(evaluate_risk("read_file", &json!({"path": "src/main.rs"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("search", &json!({"query": "foo"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("web_fetch", &json!({"url": "https://example.com"}), &ws()).0, RiskLevel::Safe);
    }

    #[test]
    fn test_git_readonly_vs_destructive() {
        assert_eq!(evaluate_risk("git", &json!({"args": "status"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "diff"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "log --oneline -5"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "branch"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "remote -v"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "add ."}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("git", &json!({"args": "commit -m 'feat'"}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("git", &json!({"args": "reset --hard HEAD~1"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("git", &json!({"args": "push origin master -f"}), &ws()).0, RiskLevel::Dangerous);
    }

    // —— v1 新增：git 破坏性补充场景 ——
    #[test]
    fn test_git_extra_destructive_cases() {
        // rebase 改写历史、stash 丢弃、checkout 丢弃工作区改动、删除分支
        assert_eq!(evaluate_risk("git", &json!({"args": "rebase main"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("git", &json!({"args": "stash drop"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("git", &json!({"args": "checkout ."}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("git", &json!({"args": "branch -d old"}), &ws()).0, RiskLevel::Dangerous);
        // 常规操作不受影响
        assert_eq!(evaluate_risk("git", &json!({"args": "checkout feature/x"}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("git", &json!({"args": "stash list"}), &ws()).0, RiskLevel::Normal);
    }

    #[test]
    fn test_shell_safe_vs_dangerous() {
        assert_eq!(evaluate_risk("shell", &json!({"command": "cargo test"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("shell", &json!({"command": "dir"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("shell", &json!({"command": "npm run test"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("shell", &json!({"command": "cargo build"}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("shell", &json!({"command": "del /f main.rs"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("shell", &json!({"command": "rmdir /s /q src"}), &ws()).0, RiskLevel::Dangerous);
    }

    // —— v1 修复 1：串联命令不得绕过（v0 下 `cargo test && del /f x` 被判 Safe）——
    #[test]
    fn test_shell_chained_command_not_bypassed() {
        let r = evaluate_risk("shell", &json!({"command": "cargo test && del /f main.rs"}), &ws());
        assert_eq!(r.0, RiskLevel::Dangerous, "串联删除不得因首条安全命令被放行: {}", r.1);
        // 管道与分号同理
        assert_eq!(evaluate_risk("shell", &json!({"command": "dir | del /f x"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("shell", &json!({"command": "echo hi; rm -rf src"}), &ws()).0, RiskLevel::Dangerous);
        // 全安全串联仍为 Safe
        assert_eq!(evaluate_risk("shell", &json!({"command": "cargo test && cargo fmt"}), &ws()).0, RiskLevel::Safe);
        // 含 Normal 的串联取较高档
        assert_eq!(evaluate_risk("shell", &json!({"command": "dir && cargo build"}), &ws()).0, RiskLevel::Normal);
    }

    // —— v1 修复 2：正文/参数里的高危字样不得误伤 ——
    #[test]
    fn test_shell_quoted_text_not_false_positive() {
        let r = evaluate_risk("shell", &json!({"command": "echo \"del x\""}), &ws());
        assert_eq!(r.0, RiskLevel::Safe, "echo 一段含 del 的文本不应判高危: {}", r.1);
        assert_eq!(evaluate_risk("shell", &json!({"command": "echo taskkill not really"}), &ws()).0, RiskLevel::Safe);
        // 真删除仍然高危
        assert_eq!(evaluate_risk("shell", &json!({"command": "del x"}), &ws()).0, RiskLevel::Dangerous);
    }

    // —— v1 修复 3：程序名精确匹配，`dirname` 不因 `dir` 前缀被误判安全 ——
    #[test]
    fn test_shell_program_prefix_not_confused() {
        assert_eq!(evaluate_risk("shell", &json!({"command": "dir"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("shell", &json!({"command": "dirname src/main.rs"}), &ws()).0, RiskLevel::Normal);
        // 带路径与扩展名的删除命令同样识别
        assert_eq!(evaluate_risk("shell", &json!({"command": "C:\\Tools\\Del.EXE /f x"}), &ws()).0, RiskLevel::Dangerous);
    }

    // —— v1 修复 4：shell 里跑 git push 复用 git 语义 ——
    #[test]
    fn test_shell_git_push_is_dangerous() {
        assert_eq!(evaluate_risk("shell", &json!({"command": "git push origin main"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("shell", &json!({"command": "git status"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("shell", &json!({"command": "git commit -m x"}), &ws()).0, RiskLevel::Normal);
    }

    // —— v1 修复 5：重定向写文件降级为常规改动 ——
    #[test]
    fn test_shell_redirect_is_normal() {
        assert_eq!(evaluate_risk("shell", &json!({"command": "echo hi > out.txt"}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("shell", &json!({"command": "dir > list.txt"}), &ws()).0, RiskLevel::Normal);
    }

    #[test]
    fn test_write_file_path_traversal() {
        assert_eq!(evaluate_risk("write_file", &json!({"path": "src/lib.rs", "content": "..."}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("write_file", &json!({"path": "../secret.txt", "content": "..."}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("write_file", &json!({"path": ".git/config", "content": "..."}), &ws()).0, RiskLevel::Dangerous);
    }

    // —— v1 修复 6：write_file 敏感清单扩展 ——
    #[test]
    fn test_write_file_sensitive_expanded() {
        for p in [".env", "config/.env.local", ".agent/index.json", "deploy/key.pem", "server.key", "settings.json"] {
            let r = evaluate_risk("write_file", &json!({"path": p, "content": "x"}), &ws());
            assert_eq!(r.0, RiskLevel::Dangerous, "{p} 应判高危: {}", r.1);
        }
        // 普通文件不受影响
        assert_eq!(evaluate_risk("write_file", &json!({"path": "src/env_util.rs", "content": "x"}), &ws()).0, RiskLevel::Normal);
    }

    // —— v1 修复 7：Windows 路径大小写差异不得造成越界误判 ——
    #[test]
    fn test_is_within_root_case_insensitive() {
        use std::path::Path;
        let root = Path::new("C:/Test/WS");
        assert!(is_within_root(root, Path::new("c:/test/ws/src/main.rs")));
        assert!(is_within_root(root, Path::new("C:/TEST/WS/README.md")));
        assert!(!is_within_root(root, Path::new("C:/test/other/x.rs")));
    }

    // —— v1 修复 8：工具登记表——已登记按常规，未登记一律高危 ——
    #[test]
    fn test_tool_registry_default_policy() {
        assert_eq!(evaluate_risk("mystery_tool", &json!({}), &ws()).0, RiskLevel::Dangerous);
        // 已登记但无专门规则的工具（登记表里存在）按常规处理，不再一律高危
        assert!(KNOWN_TOOLS.contains(&"write_file"));
        assert_eq!(KNOWN_TOOLS.len(), 7, "新增内置工具须同步登记到 KNOWN_TOOLS");
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

    #[test]
    fn test_risk_level_label() {
        assert_eq!(RiskLevel::Safe.label(), "安全");
        assert_eq!(RiskLevel::Normal.label(), "常规改动");
        assert_eq!(RiskLevel::Dangerous.label(), "高危操作");
    }

    // —— A3：会话内同类允许 的范围粒度 ——
    #[test]
    fn test_approval_scope_granularity() {
        let n = RiskLevel::Normal;
        let d = RiskLevel::Dangerous;
        // shell：按程序名归类，同程序同档 = 同类
        assert_eq!(
            approval_scope("shell", &json!({"command": "cargo build"}), n),
            approval_scope("shell", &json!({"command": "cargo build --release"}), n)
        );
        assert_ne!(
            approval_scope("shell", &json!({"command": "cargo build"}), n),
            approval_scope("shell", &json!({"command": "npm run build"}), n),
            "不同程序不得归为同类"
        );
        assert_ne!(
            approval_scope("shell", &json!({"command": "cargo build"}), n),
            approval_scope("shell", &json!({"command": "cargo build"}), d),
            "风险档不同即不同类——Normal 获批不放行 Dangerous"
        );
        // write_file：按所在目录归类
        assert_eq!(
            approval_scope("write_file", &json!({"path": "src/a.rs"}), n),
            approval_scope("write_file", &json!({"path": "src/b.rs"}), n)
        );
        assert_ne!(
            approval_scope("write_file", &json!({"path": "src/a.rs"}), n),
            approval_scope("write_file", &json!({"path": "docs/x.md"}), n)
        );
        // git：按子命令归类
        assert_eq!(
            approval_scope("git", &json!({"args": "commit -m a"}), n),
            approval_scope("git", &json!({"args": "commit -m b"}), n)
        );
        assert_ne!(
            approval_scope("git", &json!({"args": "commit -m a"}), n),
            approval_scope("git", &json!({"args": "push"}), d)
        );
    }
}
