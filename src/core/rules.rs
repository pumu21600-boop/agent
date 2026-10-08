//! 细粒度审批规则引擎（v1 转正）
//!
//! 替代“一刀切”的审批机制：
//! - Safe (0): 只读安全操作、查看代码、单元测试、只读 Git，完全静默放行；
//! - Normal (1): 工作区内的正常代码创建与修改，按常规放行并记录改动；
//! - Dangerous (2): 文件删除、工作区外部篡改、git 破坏性操作与高危命令，强制拦截卡点弹窗。
//!
//! v1 相对 v0（提前落地的初版）修正的判定缺陷：
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
/// 的 MCP 工具接入时同样在此登记。
const KNOWN_TOOLS: &[&str] = &[
    "read_file", "read_page", "read_skill", "search", "web_fetch", "web_search", "write_file",
    "edit_file", "shell", "git",
    // 浏览器内核：统一 browser 工具（method 判别；逐方法分级见 evaluate_risk）
    "browser",
    // 子代理派发（核心循环拦截，实现见 server/subagent.rs）
    "subagent_spawn",
    // 2026-10-03 agent 自改设置（get/set；分级与红线见 risk_settings / red_line_hold）
    "settings",
    // 2026-10-03 操作本应用界面（开/关侧栏面板标签；无持久副作用）
    "ui",
    // 2026-10-03 技能建/改/删正门（复用 skills::save/delete；分级见 evaluate_risk 的 skill 臂）
    "skill",
    // 2026-10-03 会话管理（new/open/rename/fork/delete/pin/archive；分级见 session 臂）
    "session",
    // 2026-10-04 放开权限轮：第三方插件信任（先审后装；分级见 evaluate_risk 的 plugin 臂）
    "plugin",
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
/// 刻意不含解释器（python/pytest/npx 等）：`python -c "任删全盘"` 首 token 也是
/// python——解释器见 INTERPRETER_PROGRAMS 单独判，不进这张表（2026-10-04 审计）。
const SAFE_PROGRAMS: &[&str] = &[
    "dir", "ls", "type", "cat", "echo", "pwd", "where", "which", "head", "tail",
    "findstr", "grep", "wc", "sort", "tree", "hostname", "whoami", "date", "time",
    "ver",
];

/// 解释器类程序：能执行任意代码（`-c`/脚本参数/自动发现 conftest/拉包执行），
/// 除版本/帮助查询外一律 Dangerous——宁可 auto 档多问，不静默跑任意代码。
/// （2026-10-04 审计：原 SAFE_PROGRAMS 无条件收 python/pytest/npx，`python -c
/// "shutil.rmtree(...)"` 三档全静默。本机没装 python 时零摩擦，装了才开始弹卡。）
const INTERPRETER_PROGRAMS: &[&str] = &["python", "python3", "pytest", "npx"];

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
];

/// 三段式安全组合（程序 + 子命令 + 参数），用于 `npm run test` 这类。
const SAFE_TRIPLES: &[(&str, &str, &str)] = &[("npm", "run", "test"), ("npm", "run", "test:")];

/// write_file 敏感路径特征（命中即 Dangerous，小写匹配）。
/// 2026-10-04 放开权限轮（用户拍板「除审批档位/API key 全部可改」）摘出 config.toml
/// 与 providers.json——前者走 set_config 通道（default_approval 特判弹卡），后者是
/// 厂商预设清单无密钥。settings.json 保留：它是 api_key（DPAPI 密文）的载体；
/// prompts/system.md 保留：行为契约，agent 改它必须过卡。
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
    "prompts/system.md",
];

/// settings 工具 set 动作的红线键（改渠道指向 = 换自己的大脑）：命中判 Dangerous，
/// 是否弹卡由审批档位决定（2026-10-03 并三档轮起不再「任何档位强制弹卡」——
/// 用户拍板：除撤销检查点外，破坏操作一律按档位走，档位本身就是用户的决定）。
/// 与 tools/settings.rs 的补丁字段对应。
/// 设置红线路径集合（2026-10-03 schema round：改由 config::settings_schema 表派生——
/// 表里 risk="red_line" 的行。新增红线键只改表一处，不再维护第二份名单）。
pub static SETTINGS_REDLINE_KEYS: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(crate::config::settings_schema::red_line_paths);

/// 唯一「任何档位必问」的操作（2026-10-03 用户拍板）：撤销检查点——回滚工作区文件
/// 不可逆，且界面还会弹二次确认。其余破坏操作（删会话/删配置/切工作区/跑定时任务/
/// 删技能/换渠道指向/直写自身配置文件）都按审批档位判，档位即用户意愿。
const ALWAYS_ASK_OPS: &[(&str, &str)] = &[("ui", "undo_checkpoint")];

/// write_file 敏感后缀（密钥/证书类）。
const SENSITIVE_SUFFIXES: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".jks", ".ppk"];

/// 评估具体工具调用的风险等级并给出判定依据。
pub fn evaluate_risk(tool_name: &str, args: &Value, workspace_root: &Path) -> (RiskLevel, String) {
    // （施工单 016）：MCP 动态工具——档位由 plugin 层按「首装确认 + server 配置」判定：
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

        // 浏览器内核：统一 browser 工具按 method 分级——只读方法（导航/快照/
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

        // 子代理派发：自身只发起一次独立会话（花销与隔离由服务层并发度/审批策略管），
        // 按常规处理；子代理自己那轮的工具有它自己的审批门（无人值守默认拒绝高危）
        "subagent_spawn" => (RiskLevel::Normal, "派发子代理任务".into()),

        // 2026-10-03 agent 自改设置：get 只读；set 按补丁键分级（红线键见 SETTINGS_REDLINE_KEYS）
        "settings" => risk_settings(args),

        // 2026-10-03 操作本应用界面：开/关侧栏面板标签，无持久副作用，恒定 Safe
        // 2026-10-03 操作本应用界面：panel/sidebar/terminal/settings/theme/tab/css/
        // fire_event（白名单只收非破坏 kind）都无持久破坏面；undo_checkpoint 回滚工作区
        // 文件不可逆 → 高危必问（fire_event 白名单刻意不收它，防绕过分级）
        "ui" => {
            let method = args.get("method").and_then(|v| v.as_str()).unwrap_or("");
            if method == "undo_checkpoint" {
                (RiskLevel::Dangerous, "撤销检查点：回滚工作区文件（不可逆）".into())
            } else if method == "fire_event" {
                // 准入与工具层 event_kind_allowed 同源复核（同一批常量，不各抄一份）：
                // 破坏 denylist → Dangerous；静态白名单 → Safe；前端 UI_OPS 清单登记的
                // → 按清单 risk 映射；查无此 kind → Dangerous（未登记默认高危；清单是
                // 广告不是闸——/api/ui/state 无鉴权可投毒，规则不因它放松）。
                let kind = args
                    .get("event")
                    .and_then(|e| e.get("kind"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if kind.is_empty() {
                    (RiskLevel::Dangerous, "fire_event 缺少 event.kind".into())
                } else if crate::tools::ui::DESTRUCTIVE_EVENT_KINDS.contains(&kind) {
                    (
                        RiskLevel::Dangerous,
                        format!("界面事件 {kind}：破坏类只走具名方法，不进通用通道"),
                    )
                } else if crate::tools::ui::SAFE_EVENT_KINDS.contains(&kind) {
                    (RiskLevel::Safe, format!("界面事件投递: {kind}"))
                } else {
                    match crate::server::ui_events::ops_risk(kind).as_deref() {
                        Some("safe") => {
                            (RiskLevel::Safe, format!("界面事件投递（界面清单登记）: {kind}"))
                        }
                        Some("normal") => (
                            RiskLevel::Normal,
                            format!("界面事件投递（界面清单登记，有持久副作用）: {kind}"),
                        ),
                        _ => (
                            RiskLevel::Dangerous,
                            format!(
                                "界面事件 {kind} 未在当前界面清单登记（无页面在报或前端没有）——按未登记高危兜底"
                            ),
                        ),
                    }
                }
            } else {
                (RiskLevel::Safe, "应用界面操作（面板/边栏/主题/标签页/CSS 等无持久破坏面）".into())
            }
        }

        // 2026-10-03 技能管理：list 只读；save 常规（校验统一在 skills::save，落全局技能根）；
        // delete 不可逆 → 高危（插件自带技能工具层本就拒删）
        "skill" => match args.get("action").and_then(|v| v.as_str()).unwrap_or("") {
            "list" => (RiskLevel::Safe, "列出技能（只读）".into()),
            "save" => (RiskLevel::Normal, "保存技能（写全局技能根）".into()),
            "delete" => (RiskLevel::Dangerous, "删除技能（不可逆）".into()),
            _ => (RiskLevel::Dangerous, "skill 工具缺少合法 action（list/save/delete）".into()),
        },

        // 2026-10-03 会话管理：list 只读；删除不可逆 → 高危；其余界面级动作常规
        "session" => match args.get("method").and_then(|v| v.as_str()).unwrap_or("") {
            "list" => (RiskLevel::Safe, "列出会话（只读）".into()),
            "delete" => (RiskLevel::Dangerous, "删除会话（不可逆）".into()),
            "new" | "open" | "rename" | "fork" | "pin" | "archive" => {
                (RiskLevel::Normal, "会话管理（界面动作）".into())
            }
            _ => (RiskLevel::Dangerous, "session 工具缺少合法 method".into()),
        },

        // 2026-10-04 放开权限轮：第三方插件安装交给 agent（落位即加载；2026-10-08
        // 审核关与 pin 验签按用户拍板移除）。安装改插件面走 Normal；2026-10-06 加
        // remove：删文件不可逆 → Dangerous 必问（与界面删除按钮走同一道 ConfirmModal 纪律）。
        "plugin" => match args.get("action").and_then(|v| v.as_str()).unwrap_or("") {
            "install" => (RiskLevel::Normal, "安装插件（落位即加载）".into()),
            "remove" => (RiskLevel::Dangerous, "删除插件（连目录带文件，不可恢复）".into()),
            _ => (RiskLevel::Dangerous, "plugin 工具缺少合法 action（install/remove）".into()),
        },

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

/// settings 工具风险分级（2026-10-03 agent 自改设置轮）：
/// get 只读 Safe；set 普通键常规改动（auto 档静默），红线键（渠道指向） Dangerous；
/// 是否弹卡由审批档位决定（2026-10-03 并三档轮起 red_line_hold 不再强制任何档位）。
fn risk_settings(args: &Value) -> (RiskLevel, String) {
    match args.get("action").and_then(|v| v.as_str()).unwrap_or("") {
        "get" => (RiskLevel::Safe, "读取应用设置（只读快照，key 只给尾号）".into()),
        // 2026-10-04 真机补：阶段3 给 settings 加了 action=schema（只读合法路径清单），
        // 工具层放行但判级器漏了同名分支——纯只读查询掉进兜底判 Dangerous 弹卡。
        "schema" => (RiskLevel::Safe, "读取可改设置路径清单（只读 manifest）".into()),
        // 2026-10-04 放开权限轮：config.toml 键改写走 set_config（toml_edit 保注释）。
        // 唯一红线是审批档位的持久形态 default_approval——其余键（花费上限/钩子/步数）
        // 按用户拍板全部放开，Normal 按档位走。
        "set_config" => {
            let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            if key.is_empty() {
                (RiskLevel::Dangerous, "set_config 缺少 key（形如 agent.max_steps）".into())
            } else if key.contains("approval") {
                (RiskLevel::Dangerous, format!("试图改 config.toml 的 {key}（审批档位相关，红线）"))
            } else {
                (RiskLevel::Normal, format!("改 config.toml 的 {key}（重启后生效）"))
            }
        }
        "set" => {
            let keys: Vec<&str> = args
                .get("patch")
                .and_then(|p| p.as_object())
                .map(|o| o.keys().map(|s| s.as_str()).collect())
                .unwrap_or_default();
            // 2026-10-03 schema round：分级查表——顶层键与点号子路径同一套口径。
            // 红线键（渠道指向）/ 表外路径（宁可多问不静默）判 Dangerous；表内普通键 Normal。
            // 纵深：工具解析层会先拒表外路径，这里仍自洽兜底。
            let hit: Vec<&str> = keys
                .iter()
                .copied()
                .filter(|k| {
                    crate::config::settings_schema::risk_of(k) == Some("red_line")
                        || SETTINGS_REDLINE_KEYS.contains(k)
                })
                .collect();
            let off_table: Vec<&str> = keys
                .iter()
                .copied()
                .filter(|k| {
                    crate::config::settings_schema::risk_of(k).is_none()
                        && !SETTINGS_REDLINE_KEYS.contains(k)
                })
                .collect();
            if !hit.is_empty() {
                (
                    RiskLevel::Dangerous,
                    format!("试图修改红线配置（渠道指向: {}，等于给 agent 换大脑）", hit.join("/")),
                )
            } else if !off_table.is_empty() {
                (
                    RiskLevel::Dangerous,
                    format!(
                        "设置路径不在可改清单里（{}）——合法路径先 action=schema 看",
                        off_table.join("/")
                    ),
                )
            } else if keys.is_empty() {
                (RiskLevel::Dangerous, "settings set 缺少 patch".into())
            } else {
                (RiskLevel::Normal, format!("修改应用常规设置: {}", keys.join("/")))
            }
        }
        // 渠道配置增删：新建=常规（起点复制当前，行为不变）；删除=高危（可能带 key）
        "config" => match args.get("op").and_then(|v| v.as_str()).unwrap_or("") {
            "delete" => (RiskLevel::Dangerous, "删除渠道配置（含密钥，不可逆）".into()),
            "new" => (RiskLevel::Normal, "新建渠道配置（复制当前并激活）".into()),
            _ => (RiskLevel::Dangerous, "config 缺少合法 op（new/delete）".into()),
        },
        // MCP 重连：只重连+重新发现工具，不动配置
        "mcp" => (RiskLevel::Normal, "重连 MCP server（重新发现工具）".into()),
        // 立即跑定时任务：无头会话真跑真花钱
        "schedule_run" => (RiskLevel::Dangerous, "立即执行定时任务（无头会话真实运行）".into()),
        // 重建记忆索引：本地重建+向量计算，常规运维
        "memory" => (RiskLevel::Normal, "重建记忆库索引".into()),
        // 切换工作区：Dangerous，按档位弹卡（full-access 放行——档位即用户意愿）
        "workspace" => (RiskLevel::Dangerous, "切换工作区（改自己的文件边界）".into()),
        // 工作区别名：只改侧栏显示名，不碰目录与文件边界，可逆 → Normal 按档位走。
        // 与 action=workspace 分开判——「改名」在用户语义里就是显示层的事（真机实锤：
        // 用户说「把工作区名改成测试」要的是侧栏那一行字，不是把目录挪走）。
        "alias" => (RiskLevel::Normal, "设置工作区别名（侧栏显示名，不动目录）".into()),
        _ => (RiskLevel::Dangerous, "settings 工具缺少合法 action".into()),
    }
}

/// 红线判定（2026-10-03 并三档轮改版）：命中即**无论审批档位**（含完全访问/全免审）
/// 都必须弹卡由用户确认。审批判定单点 `ApprovalGate::needs_approval_for` 在最开头调用
/// 本函数——红线凌驾于档位之上。
///
/// 用户拍板（2026-10-03）：红线只保留「撤销检查点」一项——回滚工作区文件不可逆、界面
/// 还会弹二次确认。其余破坏操作（换渠道指向、删会话/删配置/删技能、切工作区、跑定时
/// 任务、shell 直写自身配置文件）全部按审批档位判：read-only/auto 问、full-access
/// 放行。档位本身就是用户的决定，「任何档位强制弹卡」的面就此收窄到一项。
/// （shell 写敏感文件的判级从本函数搬进了 `risk_shell`——档位矩阵照样管得住。）
pub fn red_line_hold(tool_name: &str, args: &Value, _workspace_root: &Path) -> bool {
    ALWAYS_ASK_OPS.iter().any(|(t, method)| {
        *t == tool_name && args.get("method").and_then(|v| v.as_str()).unwrap_or("") == *method
    })
}

/// 按空白切 token，但**尊重双引号**：`-c foo.bar="a b"` 的值是一个完整 token。
/// git 自身与 cmd 都按引号切参数，判级侧用 `split_whitespace` 不尊重引号时，
/// `-c x="a b" reset` 会被切成 `["-c", "x=\"a", "b\"", "reset"]`，`i += 2` 跳过
/// `b"` 后把 `reset` 当成普通参数——破坏性操作的子命令整个从判定里消失，
/// 按 `_ => norm()` 静默放行（2026-10-08 实测：`git -c x="a b" push` 判 Normal）。
/// 返回 None = 引号不配对：解析不可靠，调用方按高危处理（不静默放行）。
fn split_git_tokens(s: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut has = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_q = !in_q;
                has = true; // 空引号 "" 也是 token（git 认它）
            }
            c if c.is_whitespace() && !in_q => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if in_q {
        return None;
    }
    if has {
        out.push(cur);
    }
    Some(out)
}

/// 剥掉 git 全局参数，取出真正的子命令与其后的参数。
/// `git -C repo push` / `git -c core.pager=cat log` 不能被误判成子命令 `-C` / `-c`。
/// None = 引号不配对无法可靠解析（调用方按高危处理）。
fn git_subcommand(cmd_args: &str) -> Option<(String, Vec<String>)> {
    let toks = split_git_tokens(cmd_args)?;
    let mut i = 0usize;
    while i < toks.len() {
        let t = &toks[i];
        if matches!(t.as_str(), "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path") {
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
                t.as_str(),
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
    let rest = toks.iter().skip(i + 1).cloned().collect();
    Some((sub, rest))
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
    let say = |r: RiskLevel, why: &str| (r, format!("{why}: git {}", cmd_args.trim()));
    let Some((sub, rest)) = git_subcommand(cmd_args.trim()) else {
        // 引号不配对：git 与 cmd 的解析都不确定，判级侧不许猜——按高危处理
        return say(RiskLevel::Dangerous, "Git 参数引号不配对，无法可靠判级");
    };
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

    // 敏感文件升级（2026-10-03 并三档轮从 red_line_hold 搬来）：命令点到应用自身
    // 配置/密钥类文件名且不是纯只读形态（`type config.toml` 照旧 Safe 不拦； echo 带
    // 重定向写它们本就是 Normal）——写它们 = 动 key/花费上限/审批默认档，判 Dangerous
    // 按档位弹卡（read-only/auto 问、full-access 放行）
    if worst != RiskLevel::Safe {
        let lower_all = command.to_lowercase();
        // 审批档位的持久形态：config.toml 的 default_approval（2026-10-04 放开轮保留的
        // 红线之一）——shell 碰 config.toml 本身已放开，但带着 default_approval 写必问
        if lower_all.contains("config.toml") && lower_all.contains("default_approval") {
            return (
                RiskLevel::Dangerous,
                "命令要写 config.toml 的 default_approval（审批档位的持久形态，红线）".into(),
            );
        }
        if let Some(f) = SENSITIVE_PATTERNS.iter().find(|p| lower_all.contains(**p)) {
            return (
                RiskLevel::Dangerous,
                format!("命令涉及敏感文件 {f}（应用自身配置/密钥类，写入即高危）: {command}"),
            );
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

    // 3.5) powershell -EncodedCommand：base64 载荷判级完全不可见，盲判高危
    if lower.contains("-encodedcommand") || lower.contains(" -enc ") {
        return (
            RiskLevel::Dangerous,
            "powershell -EncodedCommand 内容无法判级（base64），一律按高危弹卡".into(),
        );
    }

    // 3.6) 解释器（python/pytest/npx）：可执行任意代码，除版本/帮助查询外一律高危
    if INTERPRETER_PROGRAMS.contains(&prog.as_str()) {
        if matches!(second.as_str(), "-v" | "--version" | "-h" | "--help" | "-vv") {
            return (RiskLevel::Safe, "解释器版本/帮助查询".into());
        }
        return (
            RiskLevel::Dangerous,
            format!("解释器 {prog} 可执行任意代码（脚本/-c/拉包都算），按档位弹卡"),
        );
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

    // 6) 包装/复合形态兜底（2026-10-04 审计）：首 token 判级对 cmd 语法不设防——
    // `if exist x (del x)`、`cmd /c del x`、`for ... do del %i`、`powershell "Remove-Item"`
    // 的程序名都不是 del，DANGEROUS_PROGRAMS 全部失效后落 Normal（auto 档静默）。
    // 这里对非安全程序的命令串做高危名 token 扫描（词级、剥括号引号），
    // 任何位置出现高危程序即 Dangerous；安全程序（echo/findstr 等打印/搜索
    // "del" 字样）跳过避免误伤。
    // 2026-10-08 收两头残余盲区：① `d^el`/`de^l` 这类 ^ 转义——cmd 解析时 ^ 被
    // 吃掉，执行的就是 del，token 原样不命中，剥掉所有 ^ 再比一次；② cmd 延迟
    // 展开（/V:ON 或 setlocal enabledelayedexpansion）下 `!x!!y!` 运行时才展开，
    // 静态文本里没有任何高危 token 却真实执行 del——开了延迟展开且带 !标识符!
    // 形态即判高危。仍剩的盲区：`%var%` 间接展开（var 值来自文件/环境时不可知），
    // 注释挂账。
    if !SAFE_PROGRAMS.contains(&prog.as_str()) {
        for tok in trimmed.split(['(', ')', '"', '\'', ' ', '\t', ',', '=']) {
            let t = tok
                .trim_matches(['&', '|', ';', '^'])
                .to_lowercase();
            // ^ 是 cmd 转义符：`d^el` 执行时就是 del
            let unescaped = t.replace('^', "");
            if DANGEROUS_PROGRAMS.contains(&t.as_str())
                || DANGEROUS_PROGRAMS.contains(&unescaped.as_str())
            {
                return (
                    RiskLevel::Dangerous,
                    format!("复合命令中出现高危程序 {}（包装/嵌套/转义形态也拦）", if t.is_empty() { unescaped } else { t }),
                );
            }
        }
        if has_delayed_expansion(trimmed) {
            return (
                RiskLevel::Dangerous,
                "命令开了 cmd 延迟展开（/V:ON 或 setlocal enabledelayedexpansion，!var! 运行时才展开），实际执行的程序不可静态判定".into(),
            );
        }
    }

    // 7) 其余命令（含安全程序带重定向、cargo build 等）
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

/// cmd 延迟展开启用判定（2026-10-08 修）：`/V:ON` 开关或
/// `setlocal enabledelayedexpansion` 开启后，`!var!` 在命令行解析结束后才展开——
/// `set x=de& set y=l& !x!!y! /f 目标` 静态文本里没有任何高危 token，
/// 执行的却是 del。单独出现（不开延迟展开时 ! 是普通字符）不误伤。
fn has_delayed_expansion(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    if !lower.contains("/v:on") && !lower.contains("enabledelayedexpansion") {
        return false;
    }
    // !标识符! 形态（`!x!!y!` 连续展开也算：前一个 ! 后紧跟字母即启动匹配）
    let b = lower.as_bytes();
    (0..b.len()).any(|i| {
        b[i] == b'!'
            && i + 1 < b.len()
            && (b[i + 1].is_ascii_alphabetic() || b[i + 1] == b'_')
            && lower[i + 1..].contains('!')
    })
}

/// 按命令分隔符拆分（`&&` `||` `&` `;` `|` 换行），双引号内的分隔符不拆。
/// 2026-10-04 审计两处修正：① 单 `&` 也是 cmd 合法分隔符（`dir & del x`），
/// 原先只认 `&&` 导致整条按首个程序判级；② cmd 不认单引号（只有 `"` 是引号），
/// 原先把 `'` 当引号反而保护了 `' & del x & '` 里的分隔符不被拆——拆开才算得对。
fn split_commands(cmd: &str) -> Vec<String> {
    let chars: Vec<char> = cmd.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_double = false;
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            in_double = !in_double;
        }
        if !in_double {
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
            if c == ';' || c == '|' || c == '&' || c == '\n' {
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

/// 「同类操作」的范围标识（A3：会话内同类允许 的记忆键）。
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
        // settings：按补丁键归类（read-only 档下「放行改 thinking」不代表放行改 roles）；
        // Dangerous（红线键）本就不参与会话内记忆，这里只影响普通键
        "settings" => {
            let mut keys: Vec<String> = args
                .get("patch")
                .and_then(|p| p.as_object())
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            keys.sort();
            format!("settings:{tag}:{}", keys.join(","))
        }
        other => format!("{other}:{tag}"),
    }
}

/// 判定当前审批模式下该风险等级是否需要向用户请求审批。
///
/// 三档映射（2026-10-03 并三档轮：原 read-only/sandbox/auto/full-access 四档合并为
/// 三档——sandbox 档的审批矩阵与 read-only 相同，取消后旧值迁移为 read-only）：
/// - "read-only" (原 suggest / sandbox)：Safe(0) 放行，Normal(1) 及以上需审批；
///   shell/git 执行恒在沙箱内，文件工具锁死工作区；
/// - "auto" (原 auto-edit / full-auto)：Safe(0) 与 Normal(1) 静默放行，仅 Dangerous(2) 需审批；
///   工作区外写入判 Dangerous（见 `risk_write_file`），因此在 auto 档「能动电脑但高危先问」；
/// - "full-access" (原 local)：全免审（除非配置显式强制了 unsandboxed_approval）。
/// 唯一「任何档位必问」是撤销检查点（见 `red_line_hold`）——档位即用户意愿，其余不强制。
pub fn should_ask_approval(mode: &str, risk: RiskLevel) -> bool {
    match mode {
        "full-access" | "local" => false,
        "auto" | "full-auto" | "auto-edit" => risk >= RiskLevel::Dangerous,
        // read-only / sandbox(旧值) / suggest / 未知：Normal 及以上逐次询问
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

    // —— 2026-10-08 全局选项的值里带引号+空格时，split_whitespace 会把值拆开，
    // `i += 2` 跳过中段后真子命令落进 rest 只参与 flag 检查，破坏性操作降级 Normal ——
    #[test]
    fn test_git_quoted_global_value_cannot_mask_subcommand() {
        // 引号值是一个完整 token（git/cmd 都这么解析），子命令必须被认出来
        assert_eq!(evaluate_risk("git", &json!({"args": r#"-c foo.bar="a b" push origin main"#}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("git", &json!({"args": r#"-c foo.bar="a b" clean -fd"#}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("git", &json!({"args": r#"-C "my repo" reset --hard HEAD"#}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("git", &json!({"args": r#"--git-dir="a b" rebase -i"#}), &ws()).0, RiskLevel::Dangerous);
        // 带引号值的只读子命令照旧 Safe（不许为修洞把只读全打成高危）
        assert_eq!(evaluate_risk("git", &json!({"args": r#"-c foo.bar="a b" status"#}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("git", &json!({"args": r#"-c foo.bar="a b" log --oneline"#}), &ws()).0, RiskLevel::Safe);
        // shell 路径同源
        assert_eq!(evaluate_risk("shell", &json!({"command": r#"git -c foo.bar="a b" push"#}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("shell", &json!({"command": r#"git -c foo.bar="a b" status"#}), &ws()).0, RiskLevel::Safe);
        // 引号不配对：解析不可靠 → 高危（不静默放行）
        assert_eq!(evaluate_risk("git", &json!({"args": r#"-c foo.bar="a b push"#}), &ws()).0, RiskLevel::Dangerous);
        // 提交信息带引号+空格：子命令仍是 commit（Normal），不能误判
        assert_eq!(evaluate_risk("git", &json!({"args": r#"commit -m "fix: a b""#}), &ws()).0, RiskLevel::Normal);
    }

    // —— v2：push 从 shell 与 git 工具走，语义必须一致（都需审批，不能一边硬禁一边放行）——
    #[test]
    fn test_git_push_uniform_risk() {
        assert_eq!(evaluate_risk("git", &json!({"args": "push origin main"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("git", &json!({"args": "push --force-with-lease"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("shell", &json!({"command": "git push"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("shell", &json!({"command": "git.exe push"}), &ws()).0, RiskLevel::Dangerous);
    }

    // `checkout --`（-- 是最后一个 token）不能 panic：路径列表为空 → 按切分支判
    #[test]
    fn test_git_checkout_bare_double_dash_no_panic() {
        assert_eq!(evaluate_risk("git", &json!({"args": "checkout --"}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("git", &json!({"args": "checkout -- ."}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("git", &json!({"args": "checkout main"}), &ws()).0, RiskLevel::Normal);
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

    // —— 2026-10-04 审计修复：cmd 语法对首 token 判级的成片绕过 ——
    #[test]
    fn test_shell_cmd_syntax_hardening() {
        // 单 `&` 是 cmd 合法分隔符：dir 判 Safe 但后面的 del 必须被看到
        let r = evaluate_risk("shell", &json!({"command": "dir & del /f main.rs"}), &ws());
        assert_eq!(r.0, RiskLevel::Dangerous, "单 & 串联的删除不得按首程序判 Safe: {}", r.1);
        // cmd 不认单引号：原先 `'` 被当引号保护了 `& del &` 不拆分，整条判 echo Safe
        let r = evaluate_risk("shell", &json!({"command": "echo ' & del /f main.rs & echo '"}), &ws());
        assert_eq!(r.0, RiskLevel::Dangerous, "单引号内的 & 分隔符必须拆开判: {}", r.1);
        // 包装/嵌套形态：程序名不是 del 但命令串里出现高危程序 token
        for cmd in [
            "if exist x (del x)",
            "cmd /c del x",
            "C:\\Windows\\System32\\cmd.exe /c del x",
            "for %i in (*) do del %i",
            "call del x",
            "start \"\" cmd /c del x",
            "powershell -Command \"Remove-Item x\"",
            "set d=del && %d% /f x",
        ] {
            let r = evaluate_risk("shell", &json!({ "command": cmd }), &ws());
            assert_eq!(r.0, RiskLevel::Dangerous, "包装/嵌套删除应判高危: {cmd}: {}", r.1);
        }
        // base64 载荷判级不可见，盲判高危
        assert_eq!(
            evaluate_risk("shell", &json!({"command": "powershell -EncodedCommand ZGVsIHg="}), &ws()).0,
            RiskLevel::Dangerous
        );
        // 误伤护栏：安全程序（echo/findstr/dir）打印、搜索、列举"del"字样不拦
        assert_eq!(evaluate_risk("shell", &json!({"command": "findstr del notes.txt"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("shell", &json!({"command": "dir del*"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("shell", &json!({"command": "echo it's ok"}), &ws()).0, RiskLevel::Safe);
    }

    /// 2026-10-08 修：注释里挂账的两处残余盲区实测可利用，收口。
    /// ① `^` 转义：cmd 解析时吃掉 ^，`d^el /f x` 执行的就是 del；
    /// ② 延迟展开：`cmd /V:ON /C "set x=de& set y=l& !x!!y! /f 目标"` 静态
    /// 文本里没有任何高危 token，真实执行 del——auto 档原先静默放行。
    #[test]
    fn test_shell_caret_and_delayed_expansion_not_bypassed() {
        for cmd in [
            "d^el /f main.rs",
            "de^l /f main.rs",
            "cmd /c d^el /f main.rs",
            "cmd /V:ON /C \"set x=de& set y=l& !x!!y! /f C:\\target.txt\"",
            "cmd /v:on /c \"set a=ta&set b=skkill& !a!sb^! /f x\"",
            "cmd /c \"setlocal enabledelayedexpansion & set p=del& !p! /f x\"",
        ] {
            let r = evaluate_risk("shell", &json!({ "command": cmd }), &ws());
            assert_eq!(r.0, RiskLevel::Dangerous, "转义/延迟展开删除应判高危: {cmd}: {}", r.1);
        }
        // 误伤护栏：没开延迟展开时 ! 是普通字符；安全程序打印 ^ / ! 不拦。
        // 注：`echo hello^|world` 的 | 被 split_commands 当管道拆成两条
        // （echo Safe + world Normal），结论 Normal——判级从紧但不误升危。
        assert_eq!(evaluate_risk("shell", &json!({"command": "echo hello^|world"}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("shell", &json!({"command": "echo !important! done"}), &ws()).0, RiskLevel::Safe);
        // 开了延迟展开但没有 !标识符! 形态（纯 /V:ON 无害用法）不拦
        assert_eq!(evaluate_risk("shell", &json!({"command": "cmd /V:ON /C echo hi"}), &ws()).0, RiskLevel::Normal);
    }

    // —— 2026-10-04 审计修复：解释器不再无条件 Safe ——
    #[test]
    fn test_shell_interpreters_not_blanket_safe() {
        // `python -c "<任意代码>"` 原先三档全静默，现按高危弹卡
        assert_eq!(
            evaluate_risk("shell", &json!({"command": "python -c \"import shutil; shutil.rmtree(r'C:\\Users\\me')\""}), &ws()).0,
            RiskLevel::Dangerous
        );
        assert_eq!(evaluate_risk("shell", &json!({"command": "pytest"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("shell", &json!({"command": "npx some-pkg"}), &ws()).0, RiskLevel::Dangerous);
        // 版本/帮助查询仍然 Safe
        assert_eq!(evaluate_risk("shell", &json!({"command": "python -V"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("shell", &json!({"command": "npx --version"}), &ws()).0, RiskLevel::Safe);
        // node 不在解释器名单（本机日常工具链），除版本 Safe 外维持 Normal 原口径
        assert_eq!(evaluate_risk("shell", &json!({"command": "node -v"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("shell", &json!({"command": "node script.js"}), &ws()).0, RiskLevel::Normal);
    }

    #[test]
    fn test_write_file_path_traversal() {
        assert_eq!(evaluate_risk("write_file", &json!({"path": "src/lib.rs", "content": "..."}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("write_file", &json!({"path": "../secret.txt", "content": "..."}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("write_file", &json!({"path": ".git/config", "content": "..."}), &ws()).0, RiskLevel::Dangerous);
    }

    // —— edit_file 与 write_file 共用同一套路径判定（敏感/遍历/越界）——
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

    // —— 2026-10-03 agent 自改设置轮：settings 工具分级 ——
    #[test]
    fn test_settings_tool_risk() {
        // get 只读
        assert_eq!(evaluate_risk("settings", &json!({"action": "get"}), &ws()).0, RiskLevel::Safe);
        // schema 只读清单（2026-10-04 补：阶段3 工具层加了 action，判级器漏分支掉兜底弹卡）
        assert_eq!(evaluate_risk("settings", &json!({"action": "schema"}), &ws()).0, RiskLevel::Safe);
        // 普通键常规改动（auto 档静默放行）
        let (r, why) = evaluate_risk("settings", &json!({"action": "set", "patch": {"thinking": "high"}}), &ws());
        assert_eq!(r, RiskLevel::Normal, "{why}");
        // 渠道键（原红线四件套）2026-10-04 放开为 Normal——档位即用户意愿
        for k in ["base_url", "model", "provider_id", "active_config_id"] {
            let (r, why) = evaluate_risk("settings", &json!({"action": "set", "patch": {k: "x"}}), &ws());
            assert_eq!(r, RiskLevel::Normal, "{k} 应判常规: {why}");
        }
        // set_config：普通键 Normal；审批档位的持久形态 default_approval 必问
        assert_eq!(evaluate_risk("settings", &json!({"action": "set_config", "key": "agent.max_steps", "value": 0}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("settings", &json!({"action": "set_config", "key": "cost.spend_cap_usd", "value": 5.0}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("settings", &json!({"action": "set_config", "key": "agent.default_approval", "value": "auto"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("settings", &json!({"action": "set_config"}), &ws()).0, RiskLevel::Dangerous);
        // mcp_servers 入表（放开权限轮）：Normal
        assert_eq!(evaluate_risk("settings", &json!({"action": "set", "patch": {"mcp_servers": []}}), &ws()).0, RiskLevel::Normal);
        // 非法 action / 缺 patch：宁可多问不静默
        assert_eq!(evaluate_risk("settings", &json!({"action": "delete"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("settings", &json!({"action": "set"}), &ws()).0, RiskLevel::Dangerous);
    }

    // —— 2026-10-04 放开权限轮：plugin 工具判级（install 常规 / remove 必问）——
    #[test]
    fn test_plugin_tool_grading() {
        // 常规安装 Normal（落位即加载，无前置门）
        let (r, why) = evaluate_risk("plugin", &json!({"action": "install", "name": "calc", "manifest": {}, "files": {"index.html": "<p>x</p>"}}), &ws());
        assert_eq!(r, RiskLevel::Normal, "安装应判常规: {why}");
        // 2026-10-06 remove：删文件不可逆 → 必问（与界面 ConfirmModal 同纪律）
        let (r, why) = evaluate_risk("plugin", &json!({"action": "remove", "name": "calc"}), &ws());
        assert_eq!(r, RiskLevel::Dangerous, "删插件不可逆必问: {why}");
        assert_eq!(evaluate_risk("plugin", &json!({"action": "nope"}), &ws()).0, RiskLevel::Dangerous);
    }

    // —— 2026-10-03 schema round：settings 分级查表，点号子路径同吃一套 ——
    #[test]
    fn test_settings_dot_path_grading() {
        // 测试模块已有 ws() helper：假工作区根（分级逻辑不碰真实文件系统，只为统一签名）
        let ws = ws();
        // 点号子路径全部 normal（含自改总闸——它不是红线，关它只是让自改动作全拒）
        for (k, v) in [
            ("features.self_mod", json!(false)),
            ("features.galaxy", json!(false)),
            ("roles.summary", json!("cfg-a")),
            ("roles.fallbacks", json!(["cfg-a"])),
        ] {
            let (r, why) = evaluate_risk("settings", &json!({"action": "set", "patch": {k: v}}), &ws);
            assert_eq!(r, RiskLevel::Normal, "{k} 表内 normal 点号路径应判常规: {why}");
        }
        // 表外路径兜底高危：未知点号前缀/未知角色字段一律 Dangerous（宁可多问不静默）。
        // 注意 roles.cancel_on_disconnect_x_invalid 这个键不在表里——同样断言为 Dangerous（未知路径兜底）。
        for k in [
            "features.nope_key",
            "roles.nope",
            "roles.cancel_on_disconnect_x_invalid",
        ] {
            let (r, why) = evaluate_risk("settings", &json!({"action": "set", "patch": {k: "x"}}), &ws);
            assert_eq!(r, RiskLevel::Dangerous, "{k} 表外路径应兜底高危: {why}");
        }
        // 顶层渠道键 2026-10-04 放开为 Normal（与 test_settings_tool_risk 一致，防回归）
        let (r, why) = evaluate_risk(
            "settings",
            &json!({"action": "set", "patch": {"base_url": "https://x/v1"}}),
            &ws,
        );
        assert_eq!(r, RiskLevel::Normal, "base_url 放开后应判常规: {why}");
        // 顶层普通键仍 Normal
        let (r, why) = evaluate_risk("settings", &json!({"action": "set", "patch": {"thinking": "low"}}), &ws);
        assert_eq!(r, RiskLevel::Normal, "thinking 普通键应判常规: {why}");
        // 表内顶层键的枚举：SCHEMA 里 risk=="normal" 的项逐个用 {"<path>": null} 断言 Normal。
        // null 是合法 JSON，分级只看键名；roles/features 两个 object 型给 null 也照样 Normal。
        for e in crate::config::settings_schema::SCHEMA.iter().filter(|e| e.risk == "normal") {
            let (r, why) = evaluate_risk("settings", &json!({"action": "set", "patch": {e.path: null}}), &ws);
            assert_eq!(r, RiskLevel::Normal, "{} 表内 normal 路径应判常规: {why}", e.path);
        }
    }

    // —— 2026-10-03：红线判定（任何档位必问，含完全访问）——
    #[test]
    fn test_red_line_hold() {
        // 2026-10-03 并三档轮：红线收缩为「撤销检查点」一项（任何档位必问）
        assert!(red_line_hold("ui", &json!({"method": "undo_checkpoint", "id": "cp-1"}), &ws()));
        // 其余破坏操作不再任何档位强制弹卡——按档位判（档位即用户意愿）
        assert!(!red_line_hold("settings", &json!({"action": "set", "patch": {"base_url": "https://x/v1"}}), &ws()));
        assert!(!red_line_hold("settings", &json!({"action": "workspace", "path": "C:/x"}), &ws()));
        // 文件工具直写应用自身配置：不再强制，按档位判——2026-10-04 放开轮后
        // config.toml/providers.json 降 Normal（走 set_config/表内键都行），其余仍 Dangerous
        for p in ["settings.json", "prompts/system.md"] {
            assert!(
                !red_line_hold("write_file", &json!({"path": p, "content": "x"}), &ws()),
                "{p} 直写改按档位判（Dangerous），不再任何档位强制"
            );
            assert_eq!(
                evaluate_risk("write_file", &json!({"path": p, "content": "x"}), &ws()).0,
                RiskLevel::Dangerous,
                "{p} 直写仍须判高危（auto 档问、full-access 放行）"
            );
        }
        for p in ["config.toml", "providers.json"] {
            assert_eq!(
                evaluate_risk("write_file", &json!({"path": p, "content": "x"}), &ws()).0,
                RiskLevel::Normal,
                "{p} 放开权限轮后直写判 Normal"
            );
        }
        assert_eq!(
            evaluate_risk("write_file", &json!({"path": "src/main.rs", "content": "x"}), &ws()).0,
            RiskLevel::Normal
        );
        // Windows 反斜杠路径同样命中敏感面
        assert_eq!(
            evaluate_risk("edit_file", &json!({"path": "prompts\\system.md", "old_string": "a", "new_string": "b"}), &ws()).0,
            RiskLevel::Dangerous
        );
        // shell：settings.json 仍敏感；config.toml 放开（带 default_approval 例外=审批档位红线）
        assert_eq!(
            evaluate_risk("shell", &json!({"command": "echo key=xxx > settings.json"}), &ws()).0,
            RiskLevel::Dangerous,
            "echo 重定向写 settings.json（api_key 载体）= 高危"
        );
        assert_eq!(
            evaluate_risk("shell", &json!({"command": "copy evil.json settings.json"}), &ws()).0,
            RiskLevel::Dangerous
        );
        assert_eq!(
            evaluate_risk("shell", &json!({"command": "echo x > config.toml"}), &ws()).0,
            RiskLevel::Normal,
            "config.toml 放开权限轮后 shell 写判 Normal"
        );
        assert_eq!(
            evaluate_risk("shell", &json!({"command": "echo default_approval = \"auto\" > config.toml"}), &ws()).0,
            RiskLevel::Dangerous,
            "写 config.toml 的 default_approval（审批档位持久形态）= 红线必问"
        );
        // 纯读与不相干命令不受影响（type 只读不拦；敏感面不误伤普通命令）
        assert_eq!(evaluate_risk("shell", &json!({"command": "type config.toml"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("shell", &json!({"command": "cargo build"}), &ws()).0, RiskLevel::Normal);
        // 其余工具一律不碰红线
        assert!(!red_line_hold("session", &json!({"method": "delete", "id": "x"}), &ws()));
        assert!(!red_line_hold("shell", &json!({"command": "del x.txt"}), &ws()));
    }

    // —— 2026-10-03：settings 的同类允许粒度按键区分 ——
    #[test]
    fn test_settings_approval_scope_by_keys() {
        let n = RiskLevel::Normal;
        let scope = |patch| approval_scope("settings", &json!({"action": "set", "patch": patch}), n);
        assert_eq!(scope(json!({"thinking": "high"})), scope(json!({"thinking": "low"})), "同键不同值 = 同类");
        assert_ne!(scope(json!({"thinking": "high"})), scope(json!({"roles": {}})), "不同键不得归为同类");
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

    // —— 绝对路径越界且目标尚不存在——canonicalize 必失败，词面判定必须兜住 ——
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

    // —— 2026-10-03 自改大轮：session/settings 新动作分级 + workspace 红线 ——
    #[test]
    fn test_session_and_settings_new_actions() {
        assert_eq!(evaluate_risk("session", &json!({"method": "list"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("session", &json!({"method": "open", "id": "x"}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("session", &json!({"method": "fork", "id": "x"}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("session", &json!({"method": "delete", "id": "x"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("settings", &json!({"action": "config", "op": "new"}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("settings", &json!({"action": "config", "op": "delete", "id": "x"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("settings", &json!({"action": "mcp", "op": "refresh", "name": "fs"}), &ws()).0, RiskLevel::Normal);
        assert_eq!(evaluate_risk("settings", &json!({"action": "schedule_run", "id": "t"}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("settings", &json!({"action": "memory", "op": "reindex"}), &ws()).0, RiskLevel::Normal);
        // workspace：Dangerous 按档位问（2026-10-03 并三档轮起不再任何档位强制红线）
        let (r, why) = evaluate_risk("settings", &json!({"action": "workspace", "path": "C:/x"}), &ws());
        assert_eq!(r, RiskLevel::Dangerous, "{why}");
        assert!(
            !red_line_hold("settings", &json!({"action": "workspace", "path": "C:/x"}), &ws()),
            "切工作区改按档位判定（full-access 放行），不再任何档位强制弹卡"
        );
        // 别名：只改侧栏显示名（不动目录/文件边界）→ Normal 按档位走，也不是红线
        let (r, why) = evaluate_risk("settings", &json!({"action": "alias", "name": "测试"}), &ws());
        assert_eq!(r, RiskLevel::Normal, "{why}");
        assert!(!red_line_hold("settings", &json!({"action": "alias", "name": "测试"}), &ws()));
        // 审批档位映射：auto 档静默放行、read-only 档要问（ Normal 的口径）
        assert!(!should_ask_approval("auto", RiskLevel::Normal));
        assert!(should_ask_approval("read-only", RiskLevel::Normal));
        assert!(!red_line_hold("settings", &json!({"action": "config", "op": "new"}), &ws()));
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

    // —— 2026-10-03 重构轮 Phase 2：ui 工具按 method/kind 判级（清单无关分支，进程有无页面在报都不影响）——
    #[test]
    fn test_ui_tool_risk_by_method_and_kind() {
        // 缺 event.kind：宁可多问不静默
        assert_eq!(evaluate_risk("ui", &json!({"method": "fire_event", "event": {}}), &ws()).0, RiskLevel::Dangerous);
        assert_eq!(evaluate_risk("ui", &json!({"method": "fire_event"}), &ws()).0, RiskLevel::Dangerous);
        // 破坏 denlist：任何页面自报清单都改不了判级（清单是广告不是闸——判级先于清单查询）
        for k in crate::tools::ui::DESTRUCTIVE_EVENT_KINDS {
            let (r, why) = evaluate_risk("ui", &json!({"method": "fire_event", "event": {"kind": k}}), &ws());
            assert_eq!(r, RiskLevel::Dangerous, "{k} 须高危: {why}");
        }
        // 静态白名单：Safe（分支先于清单查询）
        assert_eq!(evaluate_risk("ui", &json!({"method": "fire_event", "event": {"kind": "sidebar"}}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("ui", &json!({"method": "fire_event", "event": {"kind": "pin_session"}}), &ws()).0, RiskLevel::Safe);
        // 撤销检查点：恒定高危（任何审批档位必问）
        assert_eq!(evaluate_risk("ui", &json!({"method": "undo_checkpoint", "id": "cp-1"}), &ws()).0, RiskLevel::Dangerous);
        // 其余具名界面动作照旧 Safe
        assert_eq!(evaluate_risk("ui", &json!({"method": "open_panel", "name": "usage"}), &ws()).0, RiskLevel::Safe);
        assert_eq!(evaluate_risk("ui", &json!({"method": "list_panels"}), &ws()).0, RiskLevel::Safe);
    }
}
