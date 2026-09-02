//! 供应商预设 —— 数据驱动（总架构 §8/§16：数据驱动不焊死在 Rust 里）
//!
//! 预设清单在仓库根目录 `providers.json`，启动时加载。
//! 每个预设只带默认 base_url 与默认模型，不带模型清单（模型靠渠道 GET /models 实时获取）。

use std::{path::Path, sync::OnceLock};

use anyhow::{Context, anyhow};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderPreset {
    pub id: String,
    pub name: String,
    pub default_base_url: String,
    pub default_model: String,
}

#[derive(Debug, Deserialize)]
struct ProvidersFile {
    providers: Vec<ProviderPreset>,
}

static REGISTRY: OnceLock<Vec<ProviderPreset>> = OnceLock::new();

/// 启动时调用一次：从 providers.json 加载预设。
pub fn init(path: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读 providers.json 失败: {}", path.display()))?;
    let file: ProvidersFile = serde_json::from_str(&text).context("解析 providers.json 失败")?;
    if file.providers.is_empty() {
        return Err(anyhow!("providers.json 没有任何供应商"));
    }
    let _ = REGISTRY.set(file.providers);
    Ok(())
}

pub fn list() -> &'static [ProviderPreset] {
    REGISTRY.get().map(|v| v.as_slice()).unwrap_or(&[])
}

pub fn find(id: &str) -> Option<&'static ProviderPreset> {
    list().iter().find(|p| p.id == id)
}
