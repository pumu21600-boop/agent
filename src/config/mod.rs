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
    pub logging: LoggingConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_log_dir")]
    pub dir: String,
    #[serde(default = "default_log_file")]
    pub file: String,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    8321
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
fn default_max_steps() -> u32 {
    8
}
fn default_tool_result_max_chars() -> usize {
    8000
}
fn default_approval() -> String {
    "auto-edit".to_string()
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

impl Default for ServerConfig {
    fn default() -> Self {
        Self { host: default_host(), port: default_port() }
    }
}
impl Default for GatewayConfig {
    fn default() -> Self {
        Self { base_url: default_base_url(), model: default_model() }
    }
}
impl Default for LoggingConfig {
    fn default() -> Self {
        Self { dir: default_log_dir(), file: default_log_file() }
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
        }
    }
}
impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            gateway: GatewayConfig::default(),
            logging: LoggingConfig::default(),
            agent: AgentConfig::default(),
            sandbox: SandboxConfig::default(),
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
