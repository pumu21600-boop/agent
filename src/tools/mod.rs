//! 工具系统（总架构 §5）—— 注册表 + 内置工具（施工单 002 / 005）。
//!
//! 第一批：read_file / search / web_fetch（只读，danger 0）、write_file（写，danger 1）、
//! shell / git（执行，danger 2，走沙箱）。审批策略（suggest/auto-edit/full-auto）在核心循环
//! 按 danger_level 判定，工具层只管执行。
//! shell / git 是阻塞的沙箱调用，包在 spawn_blocking 里。

use std::{
    path::{Path, PathBuf},
    sync::{OnceLock, RwLock},
};

use serde_json::{Value, json};

use crate::config::SandboxConfig;

mod fs;
mod git;
mod memory;
mod plugin;
mod search;
mod session;
mod settings;
mod shell;
mod skill;
// ui 工具与 rules.rs「同一批常量不各抄一份」：event kind 白名单/破坏 denlist 跨模块可见
pub(crate) mod ui;
mod web;

pub(crate) use fs::*;
pub(crate) use git::*;
pub(crate) use search::*;
pub(crate) use shell::*;
pub(crate) use web::*;

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

/// 工作目录根（起运行时可切换：/api/workspace 设置，默认工程根）。
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
    /// 整树 CPU 硬上限百分比（0 = 不限；2026-10-06 起由调用方传固定值 SANDBOX_CPU_PERCENT）
    pub cpu_percent: u32,
    /// 文件工具是否允许越出工作区（auto / full-access 档=true；read-only / sandbox 档锁死工作区）。
    /// 相对路径永远以工作区为基准；放行的只是绝对路径与 `..` 出界。
    pub fs_unconfined: bool,
    /// 本轮任务自己的工作区（审计 05）：后台任务（定时/无头/子代理）按触发时的
    /// 工作区执行，前台用户中途切换工作区不会把在跑的任务拽到新工作区去
    pub workspace: PathBuf,
}

fn builtin_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read_file",
            description: "读取工作目录内一个 UTF-8 文本文件的完整内容。路径相对于工作目录根；自动/完全访问档下也接受绝对路径（可读整台电脑）。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "要读取的文件路径（相对工作目录，如 README.md 或 src/main.rs；自动/完全访问档可为绝对路径）" }
                },
                "required": ["path"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "search",
            description: "在工作目录内按正则/关键字搜索文件内容，返回匹配行（默认 ripgrep，缺失时 findstr 兜底）。自动/完全访问档下 path 也接受绝对路径（可搜整台电脑）。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "搜索关键字或正则" },
                    "path": { "type": "string", "description": "可选：限定搜索的子目录或文件（相对工作目录；自动/完全访问档可为绝对路径）" }
                },
                "required": ["query"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "web_fetch",
            description: "抓取一个 http/https 网页并返回正文文本。默认 auto 档：先静态抓取（快），取不到正文（空/极少，JS 渲染的 SPA 骨架页）时自动回落浏览器渲染后再取。已知是 JS 渲染页可传 render=always 直接走浏览器；浏览器插件被停用时传 render=never 只要静态结果。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "完整 URL，必须以 http:// 或 https:// 开头" },
                    "render": { "type": "string", "enum": ["auto", "always", "never"], "description": "JS 渲染策略：auto（默认）=静态优先、空骨架回落浏览器；always=直接浏览器渲染；never=只要静态抓取" }
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
            description: "按页读取工作目录内的大文本文件（默认 200 行/页）。大文件优先用本工具翻页，不要一次读全。自动/完全访问档下也接受绝对路径。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径（相对工作目录；自动/完全访问档可为绝对路径）" },
                    "page": { "type": "integer", "description": "页码，从 1 开始" },
                    "page_size": { "type": "integer", "description": "可选：每页行数（默认 200，上限 1000）" }
                },
                "required": ["path", "page"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "read_skill",
            description: "读取一个技能（Skill）的完整 SKILL.md；带 ref 参数时读该技能的 references/ 下单篇参考资料（大技能的细节文档按需拉，不必一次全读）。自建技能见系统提示「可用技能」索引，「可用插件」节标「自带技能」的插件包文档同样按名读取；用户消息中出现 /技能名 或任务与某技能描述相关时，先调用本工具读全文再按其步骤执行。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "技能名（见系统提示「可用技能」索引，如 deploy）" },
                    "ref": { "type": "string", "description": "可选：references/ 下的参考文件名（见 SKILL.md 里的引用说明或索引篇数提示）" }
                },
                "required": ["name"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "write_file",
            description: "在工作目录内创建/覆盖一个 UTF-8 文本文件（自动创建父目录）。写入前无需确认与否决，按当前审批档位执行。自动/完全访问档下也接受绝对路径写工作区外（完全访问静默，自动档按高危先询问）；只读档对写操作逐次询问。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "目标文件路径（相对工作目录；自动/完全访问档可为绝对路径）" },
                    "content": { "type": "string", "description": "要写入的完整内容（覆盖式）" }
                },
                "required": ["path", "content"]
            }),
            danger_level: 1,
        },
        ToolSpec {
            name: "edit_file",
            description: "在工作目录内一个文本文件里做局部替换：把 old_string 精确替换为 new_string（改大文件、改几行时优先用它，不要用 write_file 重写全文）。old_string 必须与文件内容逐字一致（含缩进与换行）且在文件中唯一出现；出现 0 次或多次都会失败并说明原因，可读文件后再试。自动/完全访问档下也接受绝对路径。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "目标文件路径（相对工作目录；自动/完全访问档可为绝对路径）" },
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
            description: "在沙箱中执行一条 Windows 命令（cmd /C，工作目录=当前工作区根，见系统提示「工作区」）。输出有超时与整树击杀保护。",
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
        ToolSpec {
            name: "subagent_spawn",
            description: "派发一个子代理：在一条独立会话里跑完你给的子任务，把它的最终回答作为工具结果返回给你（你看不到它的中间过程，成本记在你这条任务上）。适用：可并行的独立调研/多方案对比/批量同类小任务（一轮里发多个 subagent_spawn 会并行跑）。注意：子代理无人值守、没有用户来回答审批——需要审批的操作一律按拒绝处理并回给它自己调整；审批档位不得高于你当前会话的档位（只能更严）。子代理失败/超时会以错误文本返回，你可重派或换路。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "子任务：要它做什么、产出什么（说清边界，它看不到你的对话历史）" },
                    "model": { "type": "string", "description": "覆盖模型（留空=功能桥接子代理席位，未配跟主渠道）" },
                    "approval_mode": { "type": "string", "enum": ["read-only", "auto", "full-access"], "description": "子代理审批档（不得高于父会话，缺省跟随）" },
                    "on_approval": { "type": "string", "enum": ["deny", "approve"], "description": "命中审批时怎么办（仅 deny 生效）" },
                    "timeout_secs": { "type": "integer", "description": "整轮超时秒数（0=默认 1800）" },
                    "label": { "type": "string", "description": "一句话标签，方便在结果里识别" }
                },
                "required": ["task"]
            }),
            danger_level: 1,
        },
        ToolSpec {
            name: "browser",
            description: "内置浏览器（无头 Edge/Chrome，本进程直连控制）——一个工具按 method 覆盖全部动作：navigate/back/forward/reload/snapshot/click/fill/type/press/scroll/hover/select/check/drag/screenshot/get_state/element_info/evaluate/page_text/wait_for/get_dialog/handle_dialog/new_tab/close/close_browser/list/activate_tab/capabilities。定位优先级：ref（快照编号）→ text/label（语义）→ x/y（坐标）。工作流：navigate 后直接读返回的快照；点击/输入前先 snapshot 拿最新 ref；每个观察周期只做一个状态变更动作，做完再 snapshot 验证效果；ref 失效时用 text/label 或坐标兜底，别重试同一个 ref。只要整页正文（不交互）用 page_text（比 evaluate 上限宽，走 max_chars）。用户可在右侧「网页」面板看到同一个浏览器（agent 页带徽章），list 查全部标签页，tab_id 可操作面板页。完整纪律：read_skill(\"browser-use\")。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "method": {
                        "type": "string",
                        "enum": crate::browser::ALL_METHODS.to_vec(),
                        "description": "命令方法（见工具说明）"
                    },
                    "url": { "type": "string", "description": "navigate：URL" },
                    "ref": { "type": "string", "description": "快照元素编号，如 e12" },
                    "text": { "type": "string", "description": "click：可见文本；wait_for：要出现的文本" },
                    "label": { "type": "string", "description": "type：输入框名/占位符" },
                    "value": { "type": "string", "description": "fill：填入文本" },
                    "key": { "type": "string", "description": "press：按键名（Enter/Tab/Escape…）" },
                    "x": { "type": "number", "description": "视口横坐标" },
                    "y": { "type": "number", "description": "视口纵坐标" },
                    "delta_x": { "type": "number", "description": "scroll：水平增量" },
                    "delta_y": { "type": "number", "description": "scroll：垂直增量（正数向下）" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "click：按键（默认 left）" },
                    "double_click": { "type": "boolean", "description": "click：是否双击" },
                    "modifiers": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["alt", "control", "meta", "shift"] },
                        "description": "click/press：修饰键"
                    },
                    "selector": { "type": "string", "description": "wait_for：CSS selector" },
                    "text_gone": { "type": "string", "description": "wait_for：要消失的文本" },
                    "timeout_ms": { "type": "integer", "description": "wait_for：超时 ms（默认 5000，上限 30000）" },
                    "full_page": { "type": "boolean", "description": "screenshot：整页截图" },
                    "expression": { "type": "string", "description": "evaluate：页面 JS 表达式" },
                    "values": { "type": "array", "items": { "type": "string" }, "description": "select：option 的 value/文本" },
                    "checked": { "type": "boolean", "description": "check：勾选态（默认 true）" },
                    "from_ref": { "type": "string", "description": "drag：起点 ref" },
                    "to_ref": { "type": "string", "description": "drag：终点 ref" },
                    "from": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } }, "description": "drag：起点坐标" },
                    "to": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } }, "description": "drag：终点坐标" },
                    "accept": { "type": "boolean", "description": "handle_dialog：接受/取消" },
                    "prompt_text": { "type": "string", "description": "handle_dialog：prompt 输入内容" },
                    "tab_id": { "type": "string", "description": "指定标签页（缺省 agent 专用页）" }
                },
                "required": ["method"]
            }),
            danger_level: 1,
        },
        ToolSpec {
            name: "computer",
            description: "操作电脑：截屏看屏幕 + 键鼠控制（SendInput）+ 窗口聚焦。一个工具按 method 覆盖：screenshot 截虚拟屏 PNG（结果里内联图片，你直接能看见屏幕；全尺寸存工作区 screenshots/ 供用户看）；cursor/screen_size/windows 坐标与窗口清单（只读）；focus 按标题或句柄把窗口带到前台；click/move/scroll/drag 鼠标；type 灌文本（Unicode 直输，中文没问题）；key 组合键（\"ctrl+c\"、\"alt+f4\"、\"enter\"）。坐标用屏幕绝对坐标（截图上的像素位置即坐标）。工作流：先 screenshot 看当前屏幕 → 对目标元素 click/type → 再 screenshot 验证效果；键盘没反应时先看返回里的焦点窗口诊断。键鼠动作按会话审批档走闸（只读档每次问）。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "method": {
                        "type": "string",
                        "enum": crate::computer::ALL_METHODS.to_vec(),
                        "description": "命令方法（见工具说明）"
                    },
                    "x": { "type": "number", "description": "click/move/scroll：屏幕 x 坐标" },
                    "y": { "type": "number", "description": "click/move/scroll：屏幕 y 坐标" },
                    "button": { "type": "string", "enum": ["left", "right", "middle"], "description": "click/drag：鼠标键（缺省 left）" },
                    "clicks": { "type": "integer", "description": "click：连击次数（1-3，缺省 1）" },
                    "dx": { "type": "number", "description": "scroll：横向滚动格数（正=右）" },
                    "dy": { "type": "number", "description": "scroll：纵向滚动格数（正=上）" },
                    "from": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } }, "description": "drag：起点" },
                    "to": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } }, "description": "drag：终点" },
                    "text": { "type": "string", "description": "type：要输入的文本" },
                    "combo": { "type": "string", "description": "key：组合键，如 \"ctrl+c\"、\"ctrl+shift+t\"、\"enter\"" },
                    "title": { "type": "string", "description": "focus：窗口标题子串（大小写不敏感）" },
                    "hwnd": { "type": "integer", "description": "focus：窗口句柄（windows 方法清单里给）" }
                },
                "required": ["method"]
            }),
            danger_level: 1,
        },
        ToolSpec {
            name: "settings",
            description: "读取或修改本应用自身的运行时设置（settings.json）与 config.toml。get：全部设置快照（key 只给尾号）。schema（只读不受闸）：全部可改路径的自描述（路径/类型/可选值/默认/风险档）——set 之前先读它，别瞎猜键名。set 配 patch（顶层键或点号子路径，如 features.galaxy=false、roles.summary=主力配置名）：普通键静默生效；换大脑键（base_url/model/provider_id/active_config_id）同档按审批走。set_config 配 key+value 改 config.toml（保注释，重启生效）；default_approval 必问。config 配 op：new=新建渠道配置并激活；delete+id=删配置（删密钥，按档位问）。mcp op=refresh+name 重连 MCP；workspace path 切换工作区（按档位问）；alias name 给工作区改显示名（细则见系统提示【工作区】节）；schedule_run id 立即跑定时任务（真花钱，按档位问）；memory op=reindex 重建记忆索引。范围外直接报错：api_key/clear_key（key 永不经模型之手，换 key 请用户到设置页）。审批档位 agent 没有任何通道可改，用户要切档请他去输入框上方的档位菜单手动切。第三方插件信任走 plugin 工具。除 get/schema/set_config 外受「中枢」闸（见系统提示「自我管理与探索纪律」）。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["schema", "get", "set", "set_config", "config", "mcp", "workspace", "alias", "schedule_run", "memory"], "description": "设置动作（每个 action 配什么参数见工具说明）" },
                    "patch": { "type": "object", "description": "set 时必填：要改的键与新值。顶层键 + 点号子路径均可（点号如 features.galaxy / roles.summary；合法路径全集见 action=schema）；整表替换的键（mcp_servers/schedules/roles/features）先 get 拿现状" },
                    "key": { "type": "string", "description": "set_config：config.toml 的键路径（形如 agent.max_steps，section.field）" },
                    "value": { "description": "set_config：新值（字符串/数字/布尔/一维数组）" },
                    "op": { "type": "string", "enum": ["new", "delete", "refresh", "reindex"], "description": "config：new/delete；mcp：refresh；memory：reindex" },
                    "id": { "type": "string", "description": "config=配置 id；schedule_run=任务 id；mcp=server 名" },
                    "name": { "type": "string", "description": "mcp refresh：server 名；alias：工作区显示名（留空=恢复文件夹名）" },
                    "path": { "type": "string", "description": "workspace：目标目录绝对路径；alias：要改别名的项目路径（省略=当前工作区）" }
                },
                "required": ["action"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "ui",
            description: "操作本应用自己的界面。语义铁律：「打开/展开」=让目标显示，「收起/关闭/隐藏」=让目标隐藏——是状态设置不是切换语义；指令含糊（没说哪个/没说方向）先问用户，猜错方向比多问一句严重。各 method 的参数配套见参数 schema。补充：面板插件停用着打不开（指路 settings 开插件）；终端与右侧面板互斥；inject_css 与设置页 CSS 注入框同路、不依赖「css-ui工具」开关、重开也在（clear=true 清空、append=true 追加、默认覆盖，保留原规则先 state 读 custom_css）；open_memory 入口关着会拒并指路 settings 开 features.galaxy；state/list_panels 读界面状态、面板启用态与界面自报 ops 清单（kind/title/risk/args）；fire_event（event.kind 必填）准入=白名单∪ops 清单 safe/normal 项，投前先 state 读 ops，破坏类永远不收必须走具名方法，未登记 kind 被拒并回带可用清单——能用具名方法就用具名方法；undo_checkpoint 回滚任务改动，不可逆、任何档位必问。除 list_panels/state 外受「中枢」闸（见系统提示「自我管理与探索纪律」）。会话管理用 session 工具。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "method": { "type": "string", "enum": ["open_panel", "close_panel", "list_panels", "open_file", "open_memory", "open_sidebar", "close_sidebar", "open_terminal", "close_terminal", "open_settings", "close_settings", "set_theme", "open_tab", "close_tab", "state", "inject_css", "fire_event", "undo_checkpoint"], "description": "open_panel/close_panel/open_sidebar/close_sidebar 配 name/side；open_tab 配 name；close_tab 配 key；open_settings 可配 tab；set_theme 配 mode；inject_css 配 css；fire_event 配 event；undo_checkpoint 配 id；open_file 配 path；list_panels/state/open_memory 不需要额外参数" },
                    "name": { "type": "string", "enum": ["trace", "board", "usage", "artifacts", "git", "web"], "description": "面板插件 id（trace=时间线/board=项目文件/usage=用量价格）或内置标签（artifacts=产物/git/web），按 method 区分" },
                    "side": { "type": "string", "enum": ["left", "right"], "description": "open_sidebar/close_sidebar：left=左侧会话列表边栏，right=右侧面板边栏" },
                    "tab": { "type": "string", "enum": ["appearance", "model", "roles", "memory", "skills", "mcp", "ext", "schedule"], "description": "open_settings：跳哪个设置页签（appearance=外观/model=模型/roles=功能桥接/memory=记忆/skills=技能/mcp/ext=插件/schedule=定时）" },
                    "mode": { "type": "string", "enum": ["dark", "light", "system"], "description": "set_theme：dark=深色、light=浅色、system=跟随系统" },
                    "key": { "type": "string", "description": "close_tab：标签 key（artifacts/git/web/ext:usage/file:<路径>）" },
                    "path": { "type": "string", "description": "open_file：要预览的文件路径（工作区相对或绝对）" },
                    "event": { "type": "object", "description": "fire_event：事件对象（kind 必填，其余字段按事件类型带，如 {\"kind\":\"theme\",\"mode\":\"light\"}）；准入规则见工具说明，投递前先 method=state 读 ops 清单" },
                    "id": { "type": "string", "description": "undo_checkpoint：检查点 id（/api/checkpoints 可列）" }
                },
                "required": ["method"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "session",
            description: "会话管理：method=list 列出全部会话（id/标题/工作区/置顶归档态，只读不受闸）；new=新建会话并切过去；open+id=切换到某条会话；rename+id+title=改标题；fork+id=分叉；delete+id=删除（不可逆，必问）；pin/archive+id(+on)=置顶/归档。除 list 外受「中枢」开关闸（见系统提示「自我管理与探索纪律」）。注意：切换/新建后 agent 自己的回复仍落在原会话，结果里要向用户交代这一点。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "method": { "type": "string", "enum": ["list", "new", "open", "rename", "fork", "delete", "pin", "archive"], "description": "list/new 不需要 id；open/rename/fork/delete/pin/archive 配 id；rename 再配 title；pin/archive 可配 on（默认 true）" },
                    "id": { "type": "string", "description": "会话 id（method=list 查）" },
                    "title": { "type": "string", "description": "rename：新标题" },
                    "on": { "type": "boolean", "description": "pin/archive：true=执行，false=取消（默认 true）" }
                },
                "required": ["method"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "plugin",
            description: "模型自产插件安装与删除（受「中枢」开关闸，见系统提示「自我管理与探索纪律」）。action=install 配 name + manifest + files（files 是 {相对路径: 文本内容} 映射，至少要有入口页或工具文件；manifest 按 manifest schema v1：apiVersion=1、title、ui/tools/permissions 声明——模型生成的插件权限一律清空，装完用户可在插件页按需开）：落位到插件目录即加载生效。action=remove 配 name 删除插件（连目录带文件一起删，不可恢复，判 Dangerous 必弹审批卡；只删 user 来源，内置拒删并说人话——想停用内置插件就引导用户关它的开关，别删）。删除前把插件名念给用户确认。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["install", "remove"], "description": "install 配 name/manifest/files；remove 配 name" },
                    "name": { "type": "string", "description": "install/remove：插件名（仅字母数字与 - _ .）" },
                    "manifest": { "type": "object", "description": "install：manifest.json 内容（apiVersion=1 + title + ui/tools 声明）" },
                    "files": { "type": "object", "description": "install：{相对路径: 文本内容}，如 {\"index.html\": \"<html>…\"}" }
                },
                "required": ["action"]
            }),
            danger_level: 1,
        },
        ToolSpec {
            name: "skill",
            description: "技能管理正门：action=list 列出全部技能（含插件自带的，标注 source/plugin）；action=save 新建或覆盖一个技能（name + description 一句话 + content 正文：触发时机与步骤，markdown；落全局技能根，下一轮对话进系统提示索引）；action=delete 删除自建技能（插件自带技能拒删，想覆盖用 save 写同名自建版）。要创建技能就用本工具——不用也不要去文件系统找 SKILL.md 的位置。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "save", "delete"], "description": "list 不需要其他参数；save/delete 配 name，save 再配 description/content" },
                    "name": { "type": "string", "description": "技能名（save 用它做目录名与 frontmatter name；中文可用）" },
                    "description": { "type": "string", "description": "save：一句话描述（索引里给模型看的触发依据）" },
                    "content": { "type": "string", "description": "save：技能正文（markdown 步骤；已有 frontmatter 的话会被保留，description 行自动同步）" }
                },
                "required": ["action"]
            }),
            danger_level: 0,
        },
        ToolSpec {
            name: "memory",
            description: "记忆两用：检索以前聊过什么 + 往自动记忆 profile.md 写条目。检索：search 配 query（语义+关键词混合，跨全部工作区）→ 标题/工作区/时间/命中片段/id；recent 配 limit（默认 10）+可选 workspace（项目名或路径片段）；similar 配 id 找相近会话；read 配 id 读原文。别翻 logs/、别解析 library.json、别读源码考古。写入：write 配 op+section——何时记/记什么/怎么提炼看系统提示「边聊边记（自动记忆）」节；op=add(text)/replace(match+text)/remove(match)/merge(match，confirm=true 表二次踩坑)；section=prefs/lessons；match 必须一字不差，不上会回带该区条目重试；含敏感信息（密钥/手机号/身份证/邮箱等）或到字符硬顶会被拒，别原样重试。只读档下每次记记忆先问用户。「记忆库」插件关着会被拒并指路 设置→插件。",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["search", "recent", "similar", "read", "write"], "description": "search 配 query；recent 可配 limit/workspace；similar/read 配 id；write 配 op+section" },
                    "query": { "type": "string", "description": "search：关键词或自然语言描述" },
                    "workspace": { "type": "string", "description": "recent 可选：只搜某个项目（项目名或路径片段）" },
                    "limit": { "type": "integer", "description": "recent：返回条数（默认 10，上限 50）" },
                    "id": { "type": "string", "description": "similar/read：会话 id（从 search/recent 结果里拿，别自己编）" },
                    "op": { "type": "string", "enum": ["add", "replace", "remove", "merge"], "description": "write：add 加一条；replace 换一条；remove 删一条；merge 合并重复条目" },
                    "section": { "type": "string", "enum": ["prefs", "lessons"], "description": "write：prefs=偏好区；lessons=教训区" },
                    "text": { "type": "string", "description": "write：add/replace 的条目正文（一句话 ≤40 字，别原句照搬）" },
                    "match": { "type": "string", "description": "write：replace/remove/merge 的目标条目原文，一字不差" },
                    "confirm": { "type": "boolean", "description": "write：merge 可选，true=第二次踩坑（观察中→已确认）" }
                },
                "required": ["action"]
            }),
            danger_level: 0,
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

/// 发给 OpenAI 兼容请求的 tools 数组（：含已启用+已确认的 MCP 动态工具）。
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
    out.extend(crate::mcp::tool_definitions());
    out.extend(crate::plugin::wasm::tool_definitions());
    out
}

/// 执行工具，返回文本结果；Err 的内容会作为 tool 消息回填给模型（不中断循环）。
pub async fn execute(cfg: &ExecConfig, name: &str, args: &str) -> Result<String, String> {
    // MCP 动态工具分发（进程隔离自带边界，不走本地沙箱/工作区约束）
    if name.starts_with(crate::mcp::PREFIX) {
        return crate::mcp::call_tool(name, args).await;
    }
    // 017：WASM 插件工具分发（沙箱自带边界；fs_read 宿主函数按本任务钉住的
    // 工作区判界，不走全局根——后台任务运行中切工作区不串界）
    if name.starts_with(crate::plugin::wasm::PREFIX) {
        return crate::plugin::wasm::call_tool(name, args, &cfg.workspace).await;
    }
    match name {
        "read_file" => read_file(&cfg.workspace, args, cfg.max_chars, cfg.fs_unconfined),
        "read_page" => read_page(&cfg.workspace, args, cfg.max_chars, cfg.fs_unconfined),
        "read_skill" => {
            let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .ok_or("缺少 name 参数")?;
            // 带 ref 读 references/ 单篇；不带读 SKILL.md 全文（生态轮：头部带技能
            // 目录——插件技能的脚本资产要用绝对路径跑）
            match v.get("ref").and_then(|x| x.as_str()) {
                Some(r) => crate::skills::read_ref(name, r),
                None => crate::skills::read_for_agent(name),
            }
        }
        "write_file" => write_file(&cfg.workspace, args, cfg.fs_unconfined),
        "edit_file" => edit_file(&cfg.workspace, args, cfg.fs_unconfined),
        "search" => search(&cfg.workspace, args, cfg.max_chars, cfg.fs_unconfined),
        "web_fetch" => web_fetch(args, cfg.max_chars).await,
        "web_search" => web_search(args, cfg.max_chars).await,
        "shell" => shell(args, cfg).await,
        "git" => git(args, cfg).await,
        // 浏览器内核：统一 browser 工具（method 判别，统一命令协议）。
        // Chromium 自带站点隔离即边界，不进 Windows 沙箱；审批门按 method 分级
        // （只读 Safe / 页面操作 Normal，见 core/rules.rs）。
        "browser" => crate::browser::command(args, cfg.max_chars).await,
        // 操作电脑（截图/键鼠/窗口；2026-10-20 第 4 轮）
        "computer" => crate::computer::execute(args, cfg.max_chars).await,
        // 2026-10-03 agent 自改设置：读/改 settings.json（分级与红线见 core/rules.rs）
        "settings" => settings::run(args).await,
        // 2026-10-04 放开权限轮：第三方插件信任交给 agent（先审后装，force 必问）
        "plugin" => plugin::run(args).await,
        // 2026-10-03：操作本应用界面（开/关侧栏面板标签，走 /api/ui/events 事件流）
        "ui" => ui::run(args).await,
        // 2026-10-03：技能建/改/删正门（复用 skills::save/delete，模型不用找文件）
        "skill" => skill::run(args).await,
        // 2026-10-03：会话管理（new/open/rename/fork/delete/pin/archive，事件驱动前端）
        "session" => session::run(args).await,
        // 2026-10-08：记忆库检索（search/recent/similar/read，全部只读）
        "memory" => memory::run(args).await,
        other => Err(format!("未知工具: {other}")),
    }
}

/// 供服务层复用（组件总线写操作）：在工作区内写一个相对路径的文件。
/// 边界与 write_file 工具一致——越界硬拦、512KB 上限、原子写。
pub fn write_workspace_relative(rel: &str, content: &str) -> Result<usize, String> {
    if content.len() > MAX_FILE_BYTES {
        return Err(format!("内容过大（{} 字节，上限 512KB）", content.len()));
    }
    let full = resolve_in_workspace(&workspace_root(), rel, false, false)?;
    crate::fsx::write_atomic_str(&full, content).map_err(|e| format!("写文件失败: {e}"))?;
    Ok(content.len())
}

/// 供服务层复用（文件预览/图片直出/目录列举）：把工作区相对路径解析成绝对路径，
/// 工作区边界与沙箱档照旧硬拦（rel 为空 = 工作区根，目录列举用）。
pub fn resolve_workspace_path(rel: &str) -> Result<PathBuf, String> {
    if rel.trim().is_empty() {
        return Ok(workspace_root());
    }
    resolve_in_workspace(&workspace_root(), rel, true, false)
}

fn cap(s: &str, max: usize) -> String {
    // 工具结果统一走头尾压缩（替代旧的单向硬截断）
    crate::context::compress(s, max)
}

/// 自改插件闸（2026-10-03 用户拍板 B/A/A/A；插件名「中枢」，2026-10-06 改名）：
/// 所有「改自身」动作要求插件页「中枢」开关打开；读（settings get / skill list /
/// ui list_panels / session list）不闸。插件关着时给人话指路（Q4=A 口头引导，
/// agent 不能自己开自己的闸）。放行后的审批仍走原档位规则（Q2=A 跟随档位），红线照旧必问（Q3=A）。
pub(crate) fn selfmod_gate() -> Result<(), String> {
    let on = crate::config::settings::shared_settings()
        .map(|h| h.read().map(|s| s.features.self_mod).unwrap_or(false))
        .unwrap_or(false);
    if on {
        return Ok(());
    }
    Err(
        "「中枢」插件没开，我做不了这件事。请到 设置→插件 找到「中枢」一行把开关打开，然后再叫我重做即可。"
            .to_string(),
    )
}

/// 解析路径并做工作目录穿越校验，返回 canonicalize 后的绝对路径。
/// `must_exist=false` 时对不存在的路径校验其父目录（写文件用）。
/// `unconfined=true`（auto/full-access 档）允许绝对路径越出工作区——整台电脑可读写；
/// read-only / sandbox 档传 false，工作区边界照旧硬拦。
fn resolve_in_workspace(root: &Path, rel: &str, must_exist: bool, unconfined: bool) -> Result<PathBuf, String> {
    if rel.trim().is_empty() {
        return Err("path 为空".to_string());
    }
    let root = root.canonicalize().map_err(|e| format!("工作目录解析失败: {e}"))?;
    // 放行档的绝对路径：原样使用，不做工作区归属检查（相对路径仍以工作区为基准）
    if unconfined && Path::new(rel).is_absolute() {
        if must_exist {
            return Path::new(rel)
                .canonicalize()
                .map_err(|e| format!("路径不存在或不可访问: {e}"));
        }
        let parent = Path::new(rel).parent().ok_or("路径无父目录")?.to_path_buf();
        std::fs::create_dir_all(&parent).map_err(|e| format!("创建父目录失败: {e}"))?;
        return Ok(Path::new(rel).to_path_buf());
    }
    let full = root.join(rel);
    if must_exist {
        let canon = full.canonicalize().map_err(|e| format!("路径不存在或不可访问: {e}"))?;
        if !unconfined && !canon.starts_with(&root) {
            return Err(format!("拒绝访问工作目录外的路径: {rel}"));
        }
        Ok(canon)
    } else {
        // 校验先行、副作用殿后（审计 06）：先把 rel 词汇规范化、完成工作区归属检查，
        // 通过了才允许 create_dir_all——旧实现「先建父目录再校验」，越界路径会在工作区
        // 外残留空目录。canonicalize 不认不存在的段，所以向上找最深的已存在祖先做真实
        // 解析（junction 只可能存在于已存在段，不存在的尾段词汇拼接即真实路径）。
        let mut parts: Vec<std::ffi::OsString> = Vec::new();
        let mut escapes: usize = 0;
        for seg in Path::new(rel).components() {
            match seg {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    if parts.pop().is_none() {
                        escapes += 1;
                    }
                }
                std::path::Component::Normal(n) => parts.push(n.to_os_string()),
                // 绝对路径/带前缀在 confined 档一律拒绝（unconfined 的绝对路径已在上面放行）
                _ => return Err(format!("路径含非法组件: {rel}")),
            }
        }
        if escapes > 0 && !unconfined {
            return Err(format!("拒绝访问工作目录外的路径: {rel}"));
        }
        let base = root
            .ancestors()
            .nth(escapes)
            .ok_or_else(|| format!("路径越出文件系统根: {rel}"))?;
        let target = parts.iter().fold(base.to_path_buf(), |acc, seg| acc.join(seg));
        // 向上找最深的已存在祖先：已存在前缀里可能藏 junction，必须 canonicalize 真实解析
        let mut anc = target;
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        while !anc.exists() {
            match (anc.file_name(), anc.parent()) {
                (Some(name), Some(p)) if !name.is_empty() => {
                    tail.push(name.to_os_string());
                    anc = p.to_path_buf();
                }
                _ => return Err(format!("路径无法解析: {rel}")),
            }
        }
        let canon_anc = anc.canonicalize().map_err(|e| format!("父目录解析失败: {e}"))?;
        let canon = tail.iter().rev().fold(canon_anc, |acc, seg| acc.join(seg));
        if !unconfined && !canon.starts_with(&root) {
            return Err(format!("拒绝访问工作目录外的路径: {rel}"));
        }
        if let Some(parent) = canon.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建父目录失败: {e}"))?;
        }
        Ok(canon)
    }
}

/// 一条搜索结果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SearchHit {
    title: String,
    url: String,
    snippet: String,
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
        ExecConfig { sandbox: SandboxConfig::default(), max_chars: 8000, unsandboxed: false, fs_unconfined: false, cpu_percent: 0, workspace: workspace_root() }
    }

    /// 审计 05：ExecConfig.workspace 钉住的工作区优先于全局——后台任务跑到一半
    /// 用户切换工作区，任务的文件操作仍落在自己触发时的那个工作区，不被拽走
    #[tokio::test]
    async fn tools_use_pinned_workspace_not_global() {
        init_logger();
        let pinned = std::env::temp_dir().join(format!("agent-ws-pinned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&pinned);
        std::fs::create_dir_all(&pinned).unwrap();
        // 2026-10-04：原实现会 set_workspace_root 切全局再还原——并发跑时把别的测试
        // 的工作区根拽走/提前还原，是全量偶发 FAILED 的源头。钉住语义本身只依赖
        // ExecConfig.workspace，不碰全局同样能证：写读全落 pinned、与全局根无关。
        let mut c = cfg();
        c.workspace = pinned.clone();
        let r = execute(&c, "write_file", r#"{"path":"pinned.txt","content":"钉住"}"#).await;
        assert!(r.is_ok(), "{r:?}");
        assert!(pinned.join("pinned.txt").is_file(), "文件必须落在钉住的工作区");
        assert!(!workspace_root().join("pinned.txt").exists(), "不许落到全局工作区");
        let r = execute(&c, "read_file", r#"{"path":"pinned.txt"}"#).await;
        assert_eq!(r.unwrap(), "钉住");
        let _ = std::fs::remove_dir_all(&pinned);
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
    async fn traversal_rejection_leaves_no_dirs_outside() {
        init_logger();
        // 审计 06：越界路径必须在任何 create_dir_all 之前被拒——旧实现先建父目录再校验，
        // 拒绝后工作区外残留空目录。默认工作区 = 工程根，../ 落在其父目录。
        let residue = workspace_root().parent().unwrap().join("audit06-residue");
        std::fs::remove_dir_all(&residue).ok();
        let r = execute(&cfg(), "write_file", r#"{"path":"../audit06-residue/deep/x.txt","content":"x"}"#).await;
        assert!(r.is_err() && r.unwrap_err().contains("拒绝"), "越界应被拒绝");
        assert!(!residue.exists(), "越界拒绝后不得在工作区外残留目录: {}", residue.display());
        // 工作区内不存在的多级路径照常自动建父目录（正常路径行为不回归）
        let r = execute(&cfg(), "write_file", r#"{"path":".audit06/inner/x.txt","content":"ok"}"#).await;
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(std::fs::read_to_string(workspace_root().join(".audit06").join("inner").join("x.txt")).unwrap(), "ok");
        std::fs::remove_dir_all(workspace_root().join(".audit06")).ok();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn junction_escape_rejected() {
        init_logger();
        // 审计 06 的存在段变体：已存在的 junction 把路径真实引到工作区外，
        // 校验必须 canonicalize 已存在前缀才能抓到（词汇 starts_with 抓不到）
        let link = workspace_root().join(".audit06-link");
        let outside = workspace_root().parent().unwrap().join("audit06-junction-target");
        std::fs::remove_dir_all(&link).ok();
        std::fs::create_dir_all(&outside).unwrap();
        let st = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&outside)
            .status()
            .unwrap();
        assert!(st.success(), "mklink /J 应成功（无需特权）");
        let r = execute(&cfg(), "write_file", r#"{"path":".audit06-link/x.txt","content":"x"}"#).await;
        assert!(r.is_err() && r.unwrap_err().contains("拒绝"), "经 junction 越界必须被拒绝");
        assert!(!outside.join("x.txt").exists(), "junction 外不得写入文件");
        std::fs::remove_dir_all(&link).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    // —— fs_unconfined（auto / full-access 档）放行绝对路径读写整台电脑 ——
    #[tokio::test]
    async fn fs_unconfined_reaches_outside_workspace() {
        init_logger();
        let outside = std::env::temp_dir().join("p19-unconfined-probe.txt");
        std::fs::write(&outside, "outside-ok").ok();
        let path = outside.to_string_lossy().replace('\\', "/");
        let args = format!(r#"{{"path":"{path}"}}"#);
        let wargs = format!(r#"{{"path":"{path}","content":"p19-write"}}"#);

        // 放行档：工作区外绝对路径可读可写
        let mut c = cfg();
        c.fs_unconfined = true;
        let r = execute(&c, "read_file", &args).await;
        assert_eq!(r.unwrap(), "outside-ok", "放行档应可读工作区外文件");
        let r = execute(&c, "write_file", &wargs).await;
        assert!(r.is_ok(), "放行档应可写工作区外文件: {r:?}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "p19-write");

        // 收紧回 read-only / sandbox 档：同一绝对路径被硬拦
        let r = execute(&cfg(), "read_file", &args).await;
        assert!(r.is_err() && r.unwrap_err().contains("拒绝"), "收紧行后越界应被拒绝");
        std::fs::remove_file(&outside).ok();
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
        // 宿主起不了受限 token 子进程（托管 runner）时自检跳过，本机照常全量断言
        if let Some(reason) = crate::sandbox::probes::restricted_host_limitation() {
            eprintln!("[skip] shell_runs_in_sandbox：{reason}");
            return;
        }
        let r = execute(&cfg(), "shell", r#"{"command":"echo p4-sandbox-ok"}"#).await;
        let out = r.unwrap();
        assert!(out.contains("p4-sandbox-ok") && out.contains("exit=0"), "{out}");
    }

    #[tokio::test]
    async fn git_version_runs() {
        init_logger();
        if let Some(reason) = crate::sandbox::probes::restricted_host_limitation() {
            eprintln!("[skip] git_version_runs：{reason}");
            return;
        }
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

    /// 2026-10-08 修：read_file 无脑整读，GB 级日志/构建产物把进程撑爆。
    /// 现在 >2MB 明确拒（指路 read_page/shell），read_page 流式分页照旧能读。
    #[tokio::test]
    async fn read_file_rejects_huge_file_and_read_page_still_pages() {
        init_logger();
        let dir = workspace_root().join(".p8-test-readbig");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).ok();
        // 2MB 出头，行数少而单行长——整读该拒、分页该过
        let big_line = "x".repeat(4096);
        let mut body = String::new();
        while body.len() < 2 * 1024 * 1024 + 4096 {
            body.push_str(&big_line);
            body.push('\n');
        }
        std::fs::write(dir.join("huge.log"), &body).ok();
        let e = execute(&cfg(), "read_file", r#"{"path":".p8-test-readbig/huge.log"}"#).await.unwrap_err();
        assert!(e.contains("文件过大") && e.contains("read_page"), "应拒并指路: {e}");
        let r = execute(&cfg(), "read_page", r#"{"path":".p8-test-readbig/huge.log","page":1,"page_size":10}"#).await.unwrap();
        assert!(r.contains("第 1/") && r.contains("本页 1-10 行"), "分页头应正确: {}", &r[..r.len().min(120)]);
        // 空文件与无尾换行文件的语义不变
        std::fs::write(dir.join("empty.txt"), "").ok();
        let r = execute(&cfg(), "read_page", r#"{"path":".p8-test-readbig/empty.txt","page":1}"#).await.unwrap();
        assert!(r.contains("空文件"));
        std::fs::write(dir.join("notrail.txt"), "a\nb").ok();
        let r = execute(&cfg(), "read_page", r#"{"path":".p8-test-readbig/notrail.txt","page":1}"#).await.unwrap();
        assert!(r.contains("共 2 行") && r.contains("本页 1-2 行"), "无尾换行应算 2 行: {r}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn unsandboxed_inherits_full_env() {
        init_logger();
        // 差分断言：父进程环境里的变量，本机直跑子进程能看到（沙箱白名单看不到）
        unsafe { std::env::set_var("PAWCH_AGENT_P4_UNSAFE_MARKER", "local-run-ok") };
        let mut c = cfg();
        c.unsandboxed = true;
        let r = execute(&c, "shell", r#"{"command":"echo %PAWCH_AGENT_P4_UNSAFE_MARKER%"}"#).await.unwrap();
        assert!(r.contains("local-run-ok"), "本机直跑应继承完整环境: {r}");

        let r2 = execute(&cfg(), "shell", r#"{"command":"echo %PAWCH_AGENT_P4_UNSAFE_MARKER%"}"#).await;
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
        // 改名迁移轮（2026-10-22）：新路径 pawch-agentuntimegit；旧路径
        // agentuntimegit 的已解压缓存存在时兼容复用一个版本
        let ok = d.ends_with(Path::new("pawch-agent").join("runtime").join("git"))
            || d.ends_with(Path::new("agent").join("runtime").join("git"));
        assert!(ok, "{d:?}");
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

    // —— 2026-10-03 自改插件闸（Phase 0 口径补测试）：关着全拒+人话指路，开着放行 ——
    #[test]
    fn selfmod_gate_blocks_when_plugin_off_and_passes_when_on() {
        use crate::config::settings::{install_shared_settings, shared_settings};
        use std::sync::{Arc, RwLock};
        // shared_settings 是进程级 OnceLock：已被别的测试装过就不抢（未来并行装载时静默跳过）
        if shared_settings().is_some() {
            return;
        }
        let h = Arc::new(RwLock::new(crate::config::settings::Settings::default()));
        install_shared_settings(h.clone());
        // 关：拒，且指路「设置→插件」开中枢（Q4=A 口头引导，不许 agent 自开闸）
        h.write().unwrap().features.self_mod = false;
        let e = selfmod_gate().unwrap_err();
        assert!(e.contains("中枢") && e.contains("设置→插件"), "{e}");
        // 开：放行（放行后的审批仍走档位规则，这里只验闸本身）
        h.write().unwrap().features.self_mod = true;
        assert!(selfmod_gate().is_ok());
        // 收尾留回与 prod 默认一致（self_mod 默认 true），别给并行读者留非默认态
        h.write().unwrap().features.self_mod = true;
    }
}
