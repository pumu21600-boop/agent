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
    "read_file", "read_page", "read_skill", "search", "web_fetch", "web_search", "write_file",
    "edit_file", "shell", "git",
    // P21 浏览器内核：统一 browser 工具（method 判别；逐方法分级见 evaluate_risk）
    "browser",
    // P22-C 子代理派发（核心循环拦截，实现见 server/subagent.rs）
    "subagent_spawn",
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
    if let Some(rest) = tool_name.strip_prefix(crate::mcp::PREFIX) {
        return match crate::mcp::risk_for(tool_name) {
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
        "read_file" | "read_page" | "search" | "web_fetch" | "web_search" => {
            (RiskLevel::Safe, "只读安全查询操作".into())
        }

        // P21 浏览器内核：统一 browser 工具按 method 分级——只读方法（导航/快照/
        // 列表/等待等）视为只读；click/fill/type/press/select/check/drag/evaluate
        // 会改变远端页面状态或执行页面脚本，按常规改动审批
        "browser" => {
            let method = args.get("method").and_then(|v| v.as_str()).unwrap_or("");
            if crate::browser::method_is_read_only(method) {
                (RiskLevel::Safe, "浏览器只读操作（导航/快照/列表/等待）".into())
            } else {
                (
                    RiskLevel::Normal,
                    "浏览器页面操作（点击/输入/求值，可能改变页面状态）".into(),
                )
            }
        }

        // 读技能：只读 SKILL.md，恒定 Safe
        "read_skill" => (RiskLevel::Safe, "读取技能文件（只读）".into()),

        // P22-C 子代理派发：自身只发起一次独立会话（花销与隔离由服务层并发度/审批策略管），
        // 按常规处理；子代理自己那轮的工具有它自己的审批门（无人值守默认拒绝高危）
        "subagent_spawn" => (RiskLevel::Normal, "派发子代理任务".into()),

        // 写文件 / 局部修改：同一套「敏感文件 > 遍历 > 越界 > 常规」判定（参数名同为 path）
        "write_file" | "edit_file" => risk_write_file(args, workspace_root),

        // Git 操作：区分只读、常规提交与破坏性命令
        "git" => {
            let cmd_args = args.get("args").and_then(|v| v.as_str()).unwrap_or("").trim();
            risk_git(cmd_args, workspace_root)
        }

        // Shell 命令行：拆分后逐条判定，取最高风险
        "shell" => {
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("").trim();
            risk_shell(command, workspace_root)
        }

        // 已登记但暂无专门规则的工具：按常规改动保守处理
        other if KNOWN_TOOLS.contains(&other) => {
            (RiskLevel::Normal, format!("已登记工具无专门规则，按常规处理: {other}"))
        }

        // 未登记工具：一律高危，防止新型/伪造工具名静默执行
        unknown => (RiskLevel::Dangerous, format!("未登记工具调用: {unknown}")),
    }
}

/// write_file / edit_file 风险判定：敏感文件 > 路径遍历 > 越界 > 常规。
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
    // 词面先判：目标文件/父目录还不存在时 canonicalize 必失败，越界判定会被整段跳过——
    // `C:/外部/尚不存在.txt` 这类绝对路径就不能按常规改动静默放行
    if target_path.is_absolute() && !is_within_root(workspace_root, &target_path) {
        return (RiskLevel::Dangerous, format!("试图修改工作区外部文件: {path_str}"));
    }
    if let Ok(canon_target) = target_path.canonicalize()
        && let Ok(canon_root) = workspace_root.canonicalize()
            && !is_within_root(&canon_root, &canon_target) {
                return (RiskLevel::Dangerous, format!("试图修改工作区外部文件: {path_str}"));
            }

    (RiskLevel::Normal, format!("项目工作区文件改写: {path_str}"))
}

/// 剥掉 git 全局参数，取出真正的子命令与其后的参数。
/// `git -C repo push` / `git -c core.pager=cat log` 不能被误判成子命令 `-C` / `-c`。
fn git_subcommand(cmd_args: &str) -> (String, Vec<String>) {
    let toks: Vec<&str> = cmd_args.split_whitespace().collect();
    let mut i = 0usize;
    while i < toks.len() {
        let t = toks[i];
        if matches!(t, "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path") {
            i += 2; // 选项 + 值
            continue;
        }
        let inline = t.starts_with("--git-dir=")
            || t.starts_with("--work-tree=")
            || t.starts_with("--namespace=")
            || t.starts_with("--exec-path=")
            || (t.starts_with("-C") && t.len() > 2)
            || (t.starts_with("-c") && t.len() > 2);
        if inline
            || matches!(
                t,
                "--no-pager" | "-p" | "--paginate" | "--bare" | "--literal-pathspecs"
                    | "--no-optional-locks" | "--no-replace-objects" | "--version" | "--help"
            )
        {
            i += 1;
            continue;
        }
        break;
    }
    let sub = toks.get(i).map(|s| s.to_lowercase()).unwrap_or_default();
    let rest = toks.iter().skip(i + 1).map(|s| s.to_string()).collect();
    (sub, rest)
}

/// 参数里是否出现某个精确 token（`--force` 不会命中 `--force-with-lease` 这类前缀）。
fn git_flag(rest: &[String], names: &[&str]) -> bool {
    rest.iter().any(|t| names.contains(&t.as_str()))
}

/// 不带 `-` 的参数（子命令的第二级动作、参数值）。
fn git_words(rest: &[String]) -> Vec<String> {
    rest.iter().filter(|t| !t.starts_with('-')).map(|t| t.to_lowercase()).collect()
}

/// 任何位置出现这些 token 都按高危处理：强制/丢弃/删除语义明确。
const GIT_FORCE_TOKENS: &[&str] = &[
    "-f", "--force", "--hard", "-D", "--delete", "--discard-changes", "-B",
    "--prune=now", "--expire=now",
];

/// git 子命令风险判定（工具参数与 shell 里的 git 命令共用同一套语义）。
///
/// v2 修 v1 的「只读白名单过宽」：白名单只认**不带写参数**的形式
/// （`git tag -d` 不再因为子命令是 tag 就放行），未知子命令一律常规档，不静默放行。
fn risk_git(cmd_args: &str, ws: &Path) -> (RiskLevel, String) {
    let (sub, rest) = git_subcommand(cmd_args.trim());
    let say = |r: RiskLevel, why: &str| (r, format!("{why}: git {}", cmd_args.trim()));
    if sub.is_empty() {
        return say(RiskLevel::Safe, "Git 只读查询"); // 裸 git 打印帮助
    }
    if git_flag(&rest, GIT_FORCE_TOKENS) {
        return say(RiskLevel::Dangerous, "Git 破坏性/强推操作");
    }
    let words = git_words(&rest);
    let sub2 = words.first().map(String::as_str).unwrap_or("");
    let read = |why: &str| say(RiskLevel::Safe, why);
    let norm = || say(RiskLevel::Normal, "Git 常规本地操作");

    match sub.as_str() {
        // —— 纯只读 ——
        "status" | "diff" | "log" | "show" | "describe" | "rev-parse" | "ls-files" | "ls-tree"
        | "cat-file" | "blame" | "shortlog" | "grep" | "whatchanged" | "merge-base" | "name-rev"
        | "count-objects" | "verify-pack" | "var" | "help" | "version" | "archive" | "bisect" => {
            read("Git 只读状态查询")
        }

        "reflog" => match sub2 {
            "delete" | "expire" => say(RiskLevel::Dangerous, "Git 破坏性/删除 reflog"),
            _ => read("Git 只读状态查询"),
        },

        // 丢弃 stash / 删工作树 / 移除子模块都不可逆
        "stash" => match sub2 {
            "" | "list" | "show" => read("Git 只读状态查询"),
            "drop" | "clear" => say(RiskLevel::Dangerous, "Git 破坏性/丢弃 stash"),
            _ => norm(),
        },
        "worktree" => match sub2 {
            "" | "list" => read("Git 只读状态查询"),
            "remove" => say(RiskLevel::Dangerous, "Git 破坏性/删除工作树"),
            _ => norm(),
        },
        "submodule" => match sub2 {
            "" | "status" | "summary" => read("Git 只读状态查询"),
            "deinit" => say(RiskLevel::Dangerous, "Git 破坏性/移除子模块"),
            _ => norm(),
        },

        // 远端配置能悄悄改写后续 push/fetch 的去向，按高危处理
        "remote" => match sub2 {
            "" | "show" | "get-url" => read("Git 只读状态查询"),
            "add" | "set-url" | "rename" | "rm" | "remove" | "set-head" => {
                say(RiskLevel::Dangerous, "Git 远端配置变更")
            }
            _ => norm(),
        },

        // 建/删标签：只有「列表查询」形式算只读
        "tag" => {
            if rest.is_empty() || git_flag(&rest, &["-l", "--list", "-n", "-v", "--verify", "--contains", "--points-at"]) {
                read("Git 只读状态查询")
            } else if git_flag(&rest, &["-d", "--delete"]) {
                say(RiskLevel::Dangerous, "Git 破坏性/删除标签")
            } else {
                norm()
            }
        }

        "branch" => {
            if git_flag(&rest, &["-d", "--delete"]) {
                say(RiskLevel::Dangerous, "Git 破坏性/删除分支")
            } else if rest.is_empty()
                || git_flag(&rest, &["-l", "--list", "-a", "-r", "-v", "-vv", "--show-current", "--contains", "--merged", "--no-merged", "--points-at", "--format"])
            {
                read("Git 只读状态查询")
            } else {
                norm() // 新建 / 改名 / 设置上游
            }
        }

        "config" => {
            let only_read = rest.is_empty()
                || git_flag(&rest, &["--get", "--get-all", "--get-regexp", "--list", "-l", "--show-origin"]);
            if only_read {
                read("Git 只读状态查询")
            } else if git_flag(&rest, &["--global", "--system"]) {
                say(RiskLevel::Dangerous, "Git 全局配置变更（工作区之外）")
            } else {
                norm()
            }
        }

        "checkout" => {
            // 还原路径 = 丢弃工作区改动；切分支 = 常规。路径判定结合工作区实际存在与否，
            // 避免把 feature/x 这类带斜杠的分支名误判成路径。
            let paths = rest.iter().position(|t| t == "--").map(|i| rest[i + 1..].to_vec());
            let discard = match paths {
                Some(p) => !p.is_empty(),
                None => words.iter().any(|w| w == "." || ws.join(w).exists()),
            };
            if discard {
                say(RiskLevel::Dangerous, "Git 丢弃工作区改动")
            } else {
                norm()
            }
        }

        "restore" => {
            let staged_only = git_flag(&rest, &["--staged", "-S", "--cached"])
                && !git_flag(&rest, &["--worktree", "-W"]);
            if staged_only {
                norm() // 只动索引，不碰工作区文件
            } else {
                say(RiskLevel::Dangerous, "Git 丢弃工作区改动")
            }
        }

        "reset" => {
            if git_flag(&rest, &["--merge"]) {
                say(RiskLevel::Dangerous, "Git 重置并丢弃本地改动")
            } else {
                norm() // --soft / --mixed / 默认 只动 HEAD 与索引
            }
        }

        "rm" => {
            if git_flag(&rest, &["--cached"]) && !git_flag(&rest, &["-f", "--force"]) {
                norm() // 只从索引移除，文件保留
            } else {
                say(RiskLevel::Dangerous, "Git 删除工作区文件")
            }
        }

        "update-ref" => {
            if git_flag(&rest, &["-d", "--delete"]) {
                say(RiskLevel::Dangerous, "Git 删除引用")
            } else {
                norm()
            }
        }

        // 历史改写 / 强推 / 清理：不可逆
        "push" => say(RiskLevel::Dangerous, "Git 推送远端（需你逐次确认）"),
        "clean" => say(RiskLevel::Dangerous, "Git 清理未跟踪文件"),
        "rebase" | "filter-branch" | "filter-repo" => say(RiskLevel::Dangerous, "Git 改写历史"),

        // 其余本地操作（add / commit / merge / cherry-pick / fetch / pull / init / clone 等）
        _ => norm(),
    }
}

/// shell 命令风险判定：按分隔符拆成子命令，逐条判定取最高风险。
/// `ws` 供路径存在性判定（`git checkout <path>` 这类）使用。
fn risk_shell(command: &str, ws: &Path) -> (RiskLevel, String) {
    if command.trim().is_empty() {
        return (RiskLevel::Safe, "空命令".into());
    }

    let parts = split_commands(command);
    let mut worst = RiskLevel::Safe;
    let mut worst_reason = String::from("沙箱命令执行");

    for p in parts {
        let (risk, reason) = risk_simple_command(&p, ws);
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
fn risk_simple_command(cmd: &str, ws: &Path) -> (RiskLevel, String) {
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
        return risk_git(&git_args, ws);
    }

    // 2) 高危程序（精确 token 匹配，避免正文/参数里的同名子串误伤）
    if DANGEROUS_PROGRAMS.contains(&prog.as_str()) {
        return (RiskLevel::Dangerous, "检测到高危系统/删除命令".to_string());
    }

    // 3) SQL 类破坏性语句（非程序名场景，保留子串兜底）
    if lower.contains("drop table") || lower.contains("drop database") {
        return (RiskLevel::Dangerous, "检测到高危数据库删除语句".to_string());
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
        return (RiskLevel::Safe, "安全只读或自动化测试命令".to_string());
    }

    // 6) 其余命令（含安全程序带重定向、cargo build 等）
    (RiskLevel::Normal, "沙箱命令执行".to_string())
}

/// 取子命令的程序名：剥离路径与扩展名，小写。
/// `C:\Tools\Del.EXE /f x` → `del`；`del/f x` → `del`；`dirname x` → `dirname`。
fn program_name(cmd: &str) -> String {
    let tok = cmd
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
        // 写文件与局部修改：按文件所在目录归类（两者分开记：放行「改」不等于放行「整文件重写」）
        "write_file" | "edit_file" => {
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let dir = p
                .replace('\\', "/")
                .rsplit_once('/')
                .map(|(d, _)| d.to_string())
                .unwrap_or_default();
            format!("{tool_name}:{tag}:{dir}")
        }
        "git" => {
            let sub = args
                .get("args")
                .and_then(|v| v.as_str())
                .unwrap_or("")
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
/// 四档映射（P19，Codex 式三档扩为四档）：
/// - "read-only" (原 suggest)：Safe(0) 放行，Normal(1) 及以上需审批；
/// - "sandbox"：执行仅限沙箱内（shell/git 恒沙箱），工作区文件写入在进审批前已被
///   `ApprovalGate::tier_block_reason` 直接驳回——能走到这里的 Normal/Dangerous
///   只有沙箱内执行类（git 提交、重定向写等），照 read-only 语义逐次询问；
/// - "auto" (原 auto-edit / full-auto)：Safe(0) 与 Normal(1) 静默放行，仅 Dangerous(2) 需审批；
///   工作区外写入判 Dangerous（见 `risk_write_file`），因此在 auto 档「能动电脑但高危先问」；
/// - "full-access" (原 local)：全免审（除非配置显式强制了 unsandboxed_approval）。
pub fn should_ask_approval(mode: &str, risk: RiskLevel) -> bool {
    match mode {
        "full-access" | "local" => false,
        "auto" | "full-auto" | "auto-edit" => risk >= RiskLevel::Dangerous,
        // read-only / sandbox / suggest / 未知：Normal 及以上逐次询问
        _ => risk >= RiskLevel::Normal,
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
        // 列表查询是只读，不该要审批
        assert_eq!(evaluate_risk("git", &json!({"args": "stash list"}), &ws()).0, RiskLevel::Safe);
    }

    // —— v2：只读白名单收窄，堵住「子命令名对就放行」的漏洞 ——
    #[test]
    fn test_git_v2_closes_readonly_loopholes() {
        // v1 会把这四个判成 Safe（子命令在只读表里）或漏判，全是写/删操作
        assert_eq!(evaluate_risk("git", &json!({"args": "tag -d v1.0"}), &ws()).0, RiskLevel::Dangerous, "删标签");
        assert_eq!(evaluate_risk("git", &json!({"args": "tag v1.0"}), &ws()).0, RiskLevel::Normal, "建标签");
        assert_eq!(evaluate_risk("git", &json!({"args": "remote add origin https://x/y.git"}), &ws()).0, RiskLevel::Dangerous, "加远端");
        assert_eq!(evaluate_risk("git", &json!({"args": "remote set-url origin https://evil/x.git"}), &ws()).0, RiskLevel::Dangerous, "改远端");
        assert_eq!(evaluate_risk("git", &json!({"args": "restore ."}), &ws()).0, RiskLevel::Dangerous, "丢弃工作区改动");
        assert_eq!(evaluate_risk("git", &json!({"args": "restore --staged ."}), &ws()).0, RiskLevel::Normal, "只动索引");
        assert_eq!(evaluate_risk("git", &json!({"args": "rm src/main.rs"}), &ws()).0, RiskLevel::Dangerous, "删文件");
        assert_eq!(evaluate_risk("git", &json!({"args": "rm --cached x.txt"}), &ws()).0, RiskLevel::Normal, "只出索引");
        assert_eq!(evaluate_risk("git", &json!({"args": "config --global user.email a@b.c"}), &ws()).0, RiskLevel::Dangerous, "改全局配置");
        assert_eq!(evaluate_risk("git", &json!({"args": "config --get user.email"}), &ws()).0, RiskLevel::Safe, "读配置");
        assert_eq!(evaluate_risk("git", &json!({"args": "reflog expire --all"}), &ws()).0, RiskLevel::Dangerous, "清 reflog");
        // 只读的仍要放行，别把白名单收得误伤
        assert_eq!(evaluate_risk("git", &json!({"args": "status"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "log --oneline -5"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "tag"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "tag -l"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "branch --show-current"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "reflog"}), &ws()).0, RiskLevel::Safe);
    }

    // —— v2：全局参数不能被当成子命令（`git -C repo push` 必须仍是高危）——
    #[test]
    fn test_git_global_flags_do_not_mask_subcommand() {
        assert_eq!(evaluate_risk("git", &json!({"args": "-C . push origin main"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("git", &json!({"args": "-c core.pager=cat log --oneline"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "--no-pager status"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": "--git-dir=.git push"}), &ws()).0, RiskLevel::Dangerous);
        // shell 里跑的同样要剥
        assert_eq!(evaluate_risk("shell", &json!({"command": "git -C . push"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("shell", &json!({"command": "git -C . status"}), &ws()).0, RiskLevel::Safe);
    }

    // —— v2：push 从 shell 与 git 工具走，语义必须一致（都需审批，不能一边硬禁一边放行）——
    #[test]
    fn test_git_push_uniform_risk() {
        assert_eq!(evaluate_risk("git", &json!({"args": "push origin main"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("git", &json!({"args": "push --force-with-lease"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("shell", &json!({"command": "git push"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("shell", &json!({"command": "git.exe push"}), &ws()).0, RiskLevel::Dangerous);
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

    // —— P16：edit_file 与 write_file 共用同一套路径判定（敏感/遍历/越界）——
    #[test]
    fn test_edit_file_path_rules_match_write_file() {
        let args = |p: &str| json!({"path": p, "old_string": "a", "new_string": "b"});
        assert_eq!(evaluate_risk("edit_file", &args("src/lib.rs"), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("edit_file", &args("../secret.txt"), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("edit_file", &args(".git/config"), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("edit_file", &args("deploy/key.pem"), &ws()).0, RiskLevel::Dangerous);
        // 审批档位语义与 write_file 一致：read-only 档 Normal 也要问，auto 档静默
        assert!(should_ask_approval("read-only", RiskLevel::Normal));
        assert!(!should_ask_approval("auto", RiskLevel::Normal));
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
        assert!(KNOWN_TOOLS.contains(&"edit_file"));
        assert_eq!(KNOWN_TOOLS.len(), crate::tools::all().len(), "新增内置工具须同步登记到 KNOWN_TOOLS");
    }

    #[test]
    fn test_should_ask_approval_tiers() {
        // read-only 模式：Safe 放行，Normal / Dangerous 需审批
        assert!(!should_ask_approval("read-only", RiskLevel::Safe));
        assert!(should_ask_approval("read-only", RiskLevel::Normal));
        assert!(should_ask_approval("read-only", RiskLevel::Dangerous));

        // sandbox 档：Safe 放行，Normal / Dangerous 需审批
        // （write_file/edit_file 在进审批前已被档位硬拦截，能走到这里的只有沙箱内执行类）
        assert!(!should_ask_approval("sandbox", RiskLevel::Safe));
        assert!(should_ask_approval("sandbox", RiskLevel::Normal));
        assert!(should_ask_approval("sandbox", RiskLevel::Dangerous));

        // auto 模式：Safe / Normal 自动放行，Dangerous 需审批
        assert!(!should_ask_approval("auto", RiskLevel::Safe));
        assert!(!should_ask_approval("auto", RiskLevel::Normal));
        assert!(should_ask_approval("auto", RiskLevel::Dangerous));

        // full-access 模式：全部自动放行
        assert!(!should_ask_approval("full-access", RiskLevel::Safe));
        assert!(!should_ask_approval("full-access", RiskLevel::Normal));
        assert!(!should_ask_approval("full-access", RiskLevel::Dangerous));
    }

    // —— P19：绝对路径越界且目标尚不存在——canonicalize 必失败，词面判定必须兜住 ——
    #[test]
    fn test_write_file_absolute_outside_is_dangerous_even_if_missing() {
        let r = evaluate_risk(
            "write_file",
            &json!({"path": "C:/test/absent-dir/absent.txt", "content": "x"}),
            &ws(),
        );
        assert_eq!(r.0, RiskLevel::Dangerous, "{}", r.1);
        // 工作区内的绝对路径（哪怕文件还没建）不判越界
        let r = evaluate_risk("write_file", &json!({"path": "C:/test/ws/src/new.rs", "content": "x"}), &ws());
        assert_eq!(r.0, RiskLevel::Normal, "{}", r.1);
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
        // edit_file：按目录归类，且与 write_file 分开记（放行「改」不等于放行「整文件重写」）
        assert_eq!(
            approval_scope("edit_file", &json!({"path": "src/a.rs", "old_string": "x"}), n),
            approval_scope("edit_file", &json!({"path": "src/b.rs", "old_string": "y"}), n)
        );
        assert_ne!(
            approval_scope("edit_file", &json!({"path": "src/a.rs", "old_string": "x"}), n),
            approval_scope("write_file", &json!({"path": "src/a.rs"}), n),
            "两种写工具不得归为同类"
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
