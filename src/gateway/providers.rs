//! 供应商预设 —— 数据驱动（总架构 §8/§16：数据驱动不焊死在 Rust 里）
//!
//! 预设清单在仓库根目录 `providers.json`，启动时加载。
//! 每个预设带默认 base_url 与默认模型（不带模型清单，模型靠渠道 GET /models 实时获取），
//! 以及可选的 `thinking_format` 覆盖（OpenClaw thinkingFormat 词表）。
//! 思考参数的组装不再按预设字段逐个开关：端点探测（compat::resolve）给出主判定，
//! 预设的 thinking_format 只在端点域名认不出时兜底——与 OpenClaw「端点归属 + 配置覆盖」一致。

use std::{path::Path, sync::OnceLock};

use anyhow::{Context, anyhow};
use serde::Deserialize;

use super::compat::ThinkingFormat;

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderPreset {
    pub id: String,
    pub name: String,
    pub default_base_url: String,
    pub default_model: String,
    /// 思考参数形态覆盖（OpenClaw 词表：openai/deepseek/zai/qwen/qwen-chat-template）。
    /// None=完全交给端点探测（官方域名都能认出；custom 中转认不出时按 openai 格式）。
    #[serde(default)]
    pub thinking_format: Option<ThinkingFormat>,
    /// 本渠道 key 可空（本地推理服务：Ollama/LM Studio/MNN/llama.cpp/自定义）
    #[serde(default)]
    pub key_optional: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn load_test() -> Vec<ProviderPreset> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("providers.json");
        let text = std::fs::read_to_string(path).unwrap();
        serde_json::from_str::<ProvidersFile>(&text).unwrap().providers
    }

    #[test]
    fn presets_parse_with_thinking_format() {
        let ps = load_test();
        assert!(ps.len() >= 18, "官方网关 13 家 + 本地/自定义 5 家，实际 {}", ps.len());
        for id in [
            "deepseek", "qwen", "ernie", "spark", "moonshot", "zhipu", "minimax", "doubao",
            "hunyuan", "openai", "anthropic", "gemini", "xai", "ollama", "lmstudio", "mnn",
            "llamacpp", "custom",
        ] {
            assert!(ps.iter().any(|p| p.id == id), "缺预设 {id}");
        }
        // 本地/自定义 key 可空
        for local in ["ollama", "lmstudio", "mnn", "llamacpp", "custom"] {
            assert!(
                ps.iter().find(|p| p.id == local).unwrap().key_optional,
                "{local} 应 key 可空"
            );
        }
        // thinking_format 只给端点域名认不出的厂商兜底（豆包官方域名带 ark.* 看不出 GLM 式）
        let doubao = ps.iter().find(|p| p.id == "doubao").unwrap();
        assert_eq!(doubao.thinking_format, Some(ThinkingFormat::Zai), "豆包 Seed 思考=GLM 式 thinking 对象");
        // 官方域名可探测的厂商不写覆盖（数据驱动交给探测）
        for probeable in ["deepseek", "zhipu", "qwen", "openai"] {
            let p = ps.iter().find(|p| p.id == probeable).unwrap();
            assert_eq!(p.thinking_format, None, "{probeable} 官方域名可探测，不写覆盖");
        }
    }

    #[test]
    fn old_format_without_optional_fields_still_parses() {
        let text = r#"{"providers":[{"id":"x","name":"X","default_base_url":"http://a","default_model":"m"}]}"#;
        let file: ProvidersFile = serde_json::from_str(text).unwrap();
        let p = &file.providers[0];
        assert_eq!(p.thinking_format, None);
        assert!(!p.key_optional);
    }
}
