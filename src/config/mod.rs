//! 配置加载 —— config.toml + 环境变量覆盖（施工单 001 / 任务 B）
//!
//! 硬约束一：base_url 从 config.toml 读，可用环境变量 DEEPSEEK_BASE_URL 覆盖（商汤中转）。
//! 硬约束二：API key 不在这里，只走环境变量 DEEPSEEK_API_KEY。

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub mod settings;

/// 应用根目录：**打包分发的唯一形态是「exe 同级目录」**，开发形态回退编译期源码目录。
///
/// 判定：exe 同级存在 `config.toml` 或 `web` 目录 ⇒ 认定随包运行，一切相对资源
/// （config.toml / settings.json / providers.json / prompts / web/dist / plugins / logs）
/// 都落在 exe 同级。`cargo run` 与 `target\release\agent.exe` 直接跑时同级没有这些
/// 目录，行为与从前（CARGO_MANIFEST_DIR）完全一致，开发流程不受影响。
pub fn app_root() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && (dir.join("config.toml").is_file() || dir.join("web").is_dir())
    {
        return dir.to_path_buf();
    }
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[cfg(test)]
mod app_root_tests {
    use super::*;

    /// 开发形态（test exe 在 target/debug/deps 下）必须回落到源码目录，
    /// 否则 config.toml / providers.json / web/dist 全部读不到，整个服务起不来。
    #[test]
    fn dev_layout_falls_back_to_manifest_dir() {
        let root = app_root();
        assert_eq!(root, std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        assert!(root.join("config.toml").is_file(), "源码根应有 config.toml");
        assert!(root.join("providers.json").is_file(), "源码根应有 providers.json");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub roles: RolesConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub recovery: RecoveryConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub hooks: HooksConfig,
    /// P21 成本台账：全局花费上限（硬闸）等成本红线参数
    #[serde(default)]
    pub cost: CostConfig,
    /// P22-C 子代理并行度上限
    #[serde(default)]
    pub subagent: SubagentConfig,
}

/// 成本红线参数（施工单 030，P21）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostConfig {
    /// 全局花费上限（美元，硬闸）：台账累计花费达到即拒绝新的模型请求；0 = 不限（默认）。
    /// 单价来自 app_root/prices.json（美元 / 1M tokens，带生效时间）。
    #[serde(default)]
    pub spend_cap_usd: f64,
}

/// P8 角色映射（总架构 §8 功能模型映射，数据驱动）：值为 settings.configs 里的
/// 配置名或配置 id；未配置的角色回落主渠道。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RolesConfig {
    /// 滚动摘要（SUMMARY）
    #[serde(default)]
    pub summary: Option<String>,
    /// 会话标题生成（TITLE_GENERATION）
    #[serde(default)]
    pub title: Option<String>,
    /// 向量嵌入（EMBEDDING，记忆库语义检索消费；未配置时用本地哈希向量）
    #[serde(default)]
    pub embedding: Option<String>,
    /// P21 降级渠道：单任务累计 token 超 [hooks] budget_tokens 后，本任务剩余步骤改走它
    /// （未配置 = 维持主渠道 + Notice，行为与 P18 一致）
    #[serde(default)]
    pub budget: Option<String>,
    /// P22-C 子代理专用渠道（未配置 = 回落主渠道，与 B3 一致）；
    /// 典型用法：主会话用旗舰模型，子代理跑量用便宜模型
    #[serde(default)]
    pub subagent: Option<String>,
}

/// P22-C 子代理并行度（config.toml `[subagent]`；docs/subagent-draft.md 第 1 节）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentConfig {
    /// 全局并发子代理上限（0 = 不允许派发子代理）
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// 单个父任务的活跃子代理上限（满员进 FIFO 队列，出队即跑，不丢不串）
    #[serde(default = "default_per_parent")]
    pub per_parent: usize,
}

fn default_max_concurrent() -> usize {
    3
}

fn default_per_parent() -> usize {
    2
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self { max_concurrent: default_max_concurrent(), per_parent: default_per_parent() }
    }
}

/// 记忆系统参数（施工单 009）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// 总开关：false 时 events.log / index / AGENTS.md 注入 / 滚动摘要全停
    #[serde(default = "default_memory_enabled")]
    pub enabled: bool,
    /// 敏感落盘：false（默认）时 events.log 里的 key/token 打码后才写
    #[serde(default)]
    pub persist_sensitive: bool,
}

fn default_memory_enabled() -> bool {
    true
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self { enabled: default_memory_enabled(), persist_sensitive: false }
    }
}

/// 恢复与续命参数（施工单 007）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryConfig {
    /// shadow repo checkpoint 总开关（danger≥1 工具执行前拍快照）
    #[serde(default = "default_checkpoint_enabled")]
    pub checkpoint_enabled: bool,
    /// 上下文占用达到预算该比例时自动生成交接文档 HANDOFF.md
    #[serde(default = "default_handoff_ratio")]
    pub handoff_token_ratio: f32,
}

fn default_checkpoint_enabled() -> bool {
    true
}
fn default_handoff_ratio() -> f32 {
    0.8
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self { checkpoint_enabled: default_checkpoint_enabled(), handoff_token_ratio: default_handoff_ratio() }
    }
}

/// 核心循环参数（施工单 003 / 005）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// 单次任务最大循环步数（防失控）
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    /// 工具结果回填模型的截断上限（上下文工程雏形，P5 补全）
    #[serde(default = "default_tool_result_max_chars")]
    pub tool_result_max_chars: usize,
    /// 默认审批档位：suggest / auto-edit / full-auto / local（请求级可覆盖）
    #[serde(default = "default_approval")]
    pub default_approval: String,
    /// 本机直跑总开关：false 时 local 档回退 full-auto（脱沙箱=完整用户权限，必须显式开）
    #[serde(default)]
    pub allow_unsandboxed: bool,
    /// 本机直跑档下，执行类(danger 2)是否仍要审批（false=全自动）
    #[serde(default)]
    pub unsandboxed_approval: bool,
}

/// 执行沙箱参数（施工单 004，P4 shell 工具也走这套限额）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// 墙钟超时（ms）：到点整树击杀
    #[serde(default = "default_sandbox_timeout_ms")]
    pub timeout_ms: u32,
    /// 进程树内单进程 commit 内存上限（MB）
    #[serde(default = "default_sandbox_memory_mb")]
    pub memory_mb: u32,
    /// 进程树内活跃进程数上限
    #[serde(default = "default_sandbox_active_processes")]
    pub active_processes: u32,
    /// P12 019：隔离模式 "restricted"（默认，受限 token）| "appcontainer"（LowBox + 能力 SID，默认断网；实验特性）
    #[serde(default = "default_sandbox_mode")]
    pub mode: String,
    /// P12 019：启用 AppContainer 模式（mode=appcontainer 时有效）
    #[serde(default)]
    pub appcontainer: bool,
    /// P12 019：AppContainer 网络出向（仅 appcontainer 模式生效）
    #[serde(default)]
    pub allow_network: bool,
}
fn default_sandbox_mode() -> String {
    "restricted".into()
}

/// 钩子配置（施工单 027，P18）—— 用户不改源码即可在固定挂点插入自定义动作。
/// 承载形式：本地命令（stdin 收 JSON 事件、stdout 回 JSON 决定）；空串 = 未配置。
/// 语义四条（定案 7）：4 挂点 / before_tool 仅否决权 / 超时放行 / JSONL 审计。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HooksConfig {
    /// 工具执行前：stdout `{"decision":"deny","reason":"…"}` = 拦下并回喂模型；其余 = 放行
    #[serde(default)]
    pub before_tool: String,
    /// 工具执行后（观测性通知，stdout 不参与决策）
    #[serde(default)]
    pub after_tool: String,
    /// 任务结束（沉淀记忆等收尾动作）
    #[serde(default)]
    pub session_end: String,
    /// 超预算通知（budget_tokens>0 且单任务累计 token 超限时触发一次）
    #[serde(default)]
    pub budget_exceeded: String,
    /// 挂点限时（ms）：到点视为放行，绝不卡主循环
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
    /// 单任务累计 token 预算（0 = 不启用）
    #[serde(default)]
    pub budget_tokens: u64,
}

fn default_hook_timeout_ms() -> u64 {
    5000
}

/// 上下文工程参数（施工单 006）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    /// 全 messages 的 token 预算（估算值），超出触发裁剪；给回复留足余量
    #[serde(default = "default_budget_tokens")]
    pub budget_tokens: u64,
    /// 最近 N 个工具结果完整保留，更早的归档
    #[serde(default = "default_recent_kept")]
    pub recent_tool_results_kept: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// 渠道 base_url：config.toml 读，DEEPSEEK_BASE_URL 环境变量可覆盖。
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// 本地限流（P8）：每分钟最多发起多少次模型请求；0=不限（默认）。
    /// 全渠道共用一个节流器，超过时排队等待（只影响节奏，不报错）。
    #[serde(default)]
    pub max_rpm: u32,
    /// 降级链（P8）：主渠道当轮调用失败时按序切换的配置名（settings.configs 的 name）。
    /// 每次切换发 Notice；全部失败才报错。取消标志置位时立即停，不降级。
    #[serde(default)]
    pub fallback_configs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_dir")]
    pub dir: String,
    #[serde(default = "default_log_file")]
    pub file: String,
    /// 单文件轮转上限（MB）：超过就归档成 `<file>.1`，0 = 不轮转
    #[serde(default = "default_log_max_mb")]
    pub max_mb: u64,
    /// 归档保留份数：`<file>.1`（最新）… `<file>.N`（最旧），更旧的丢弃；0 = 归档即删
    #[serde(default = "default_log_keep_archives")]
    pub keep_archives: usize,
}

fn default_log_keep_archives() -> usize {
    crate::observability::DEFAULT_KEEP_ARCHIVES
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    5173
}
fn default_base_url() -> String {
    "https://api.deepseek.com".to_string()
}
fn default_model() -> String {
    "deepseek-chat".to_string()
}
fn default_log_dir() -> String {
    "logs".to_string()
}
fn default_log_file() -> String {
    "agent.jsonl".to_string()
}
fn default_log_max_mb() -> u64 {
    20
}
fn default_max_steps() -> u32 {
    // 16：8 步在「读文件 → 改文件 → 跑测试 → 修 → 再跑」这类真实工程任务上会撞顶中断
    // （施工单 022 采纳审查建议上调；config.toml 里显式值优先）
    16
}
fn default_tool_result_max_chars() -> usize {
    8000
}
fn default_approval() -> String {
    "auto".to_string()
}
fn default_sandbox_timeout_ms() -> u32 {
    10_000
}
fn default_sandbox_memory_mb() -> u32 {
    1024
}
fn default_sandbox_active_processes() -> u32 {
    64
}
fn default_budget_tokens() -> u64 {
    96_000
}
fn default_recent_kept() -> usize {
    6
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { host: default_host(), port: default_port() }
    }
}
impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            model: default_model(),
            max_rpm: 0,
            fallback_configs: Vec::new(),
        }
    }
}
impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            dir: default_log_dir(),
            file: default_log_file(),
            max_mb: default_log_max_mb(),
            keep_archives: default_log_keep_archives(),
        }
    }
}
impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: default_max_steps(),
            tool_result_max_chars: default_tool_result_max_chars(),
            default_approval: default_approval(),
            allow_unsandboxed: false,
            unsandboxed_approval: false,
        }
    }
}
impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            timeout_ms: default_sandbox_timeout_ms(),
            memory_mb: default_sandbox_memory_mb(),
            active_processes: default_sandbox_active_processes(),
            mode: default_sandbox_mode(),
            appcontainer: false,
            allow_network: false,
        }
    }
}
impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            budget_tokens: default_budget_tokens(),
            recent_tool_results_kept: default_recent_kept(),
        }
    }
}

/// 读取 config.toml（不存在则落一份默认），再应用环境变量覆盖。
pub fn load(path: &Path) -> anyhow::Result<Config> {
    if !path.exists() {
        let default_text =
            toml::to_string_pretty(&Config::default()).context("生成默认配置失败")?;
        crate::fsx::write_atomic_str(path, &default_text)
            .with_context(|| format!("写默认配置失败: {}", path.display()))?;
        eprintln!("[config] 已生成默认 config.toml: {}", path.display());
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("读配置失败: {}", path.display()))?;
    let mut config: Config = toml::from_str(&text).context("解析 config.toml 失败")?;
    // 环境变量覆盖（商汤中转等渠道：不改代码、不改文件即可切渠道）
    if let Ok(override_url) = std::env::var("DEEPSEEK_BASE_URL") {
        config.gateway.base_url = override_url;
    }
    Ok(config)
}
