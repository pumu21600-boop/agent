//! 配置加载 —— config.toml + 环境变量覆盖（施工单 001 / 任务 B）
//!
//! 硬约束一：base_url 从 config.toml 读，可用环境变量 DEEPSEEK_BASE_URL 覆盖（商汤中转）。
//! 硬约束二：API key 不在这里，只走环境变量 DEEPSEEK_API_KEY。

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub mod settings;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// 单文件轮转上限（MB）：超过就归档成 `<file>.1`（只留一份），0 = 不轮转
    #[serde(default = "default_log_max_mb")]
    pub max_mb: u64,
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
    8
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
impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            gateway: GatewayConfig::default(),
            roles: RolesConfig::default(),
            logging: LoggingConfig::default(),
            agent: AgentConfig::default(),
            sandbox: SandboxConfig::default(),
            context: ContextConfig::default(),
            recovery: RecoveryConfig::default(),
            memory: MemoryConfig::default(),
        }
    }
}

/// 读取 config.toml（不存在则落一份默认），再应用环境变量覆盖。
pub fn load(path: &Path) -> anyhow::Result<Config> {
    if !path.exists() {
        let default_text =
            toml::to_string_pretty(&Config::default()).context("生成默认配置失败")?;
        std::fs::write(path, default_text)
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
