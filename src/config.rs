//! 配置加载 —— config.toml + 环境变量覆盖（施工单 001 / 任务 B）
//!
//! 硬约束一：base_url 从 config.toml 读，可用环境变量 DEEPSEEK_BASE_URL 覆盖（商汤中转）。
//! 硬约束二：API key 不在这里，只走环境变量 DEEPSEEK_API_KEY。

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
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
impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            gateway: GatewayConfig::default(),
            logging: LoggingConfig::default(),
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
