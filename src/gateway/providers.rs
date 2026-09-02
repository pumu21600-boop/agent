//! 供应商预设 —— 数据驱动（总架构 §8/§16：数据驱动不焊死在 Rust 里）
//!
//! 预设清单在仓库根目录 `providers.json`，启动时加载。
//! 每个预设带默认 base_url 与默认模型（不带模型清单，模型靠渠道 GET /models 实时获取），
//! 以及思考/行为适配字段（P8）：网关按这些字段组装请求与解析响应，不按 provider id 分支。

use std::{path::Path, sync::OnceLock};

use anyhow::{Context, anyhow};
use serde::Deserialize;

/// 思考的接线方式（P8 排产字段，按厂商适配）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningStyle {
    /// 思考走独立字段 delta.reasoning_content，请求端不需要特殊参数（DeepSeek/Qwen 官方等）
    #[default]
    ReasoningContent,
    /// 请求端用 thinking 参数族控制开关（GLM：{"thinking":{"type":"enabled|disabled"}}）
    ThinkingParam,
    /// 请求端用 reasoning_effort 控制（OpenAI 风格；o 系无可靠"关"参数，off 时不发）
    EffortParam,
    /// 思考混在正文 `<think>` 标签里（免费反代/本地推理服务），响应端需剥离（P7.7 已兜住）
    InContent,
}

impl ReasoningStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReasoningStyle::ReasoningContent => "reasoning_content",
            ReasoningStyle::ThinkingParam => "thinking_param",
            ReasoningStyle::EffortParam => "effort_param",
            ReasoningStyle::InContent => "in_content",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderPreset {
    pub id: String,
    pub name: String,
    pub default_base_url: String,
    pub default_model: String,
    /// 思考接线方式；缺省 reasoning_content（请求不发参数，响应读独立字段）
    #[serde(default)]
    pub reasoning_style: ReasoningStyle,
    /// 布尔思考开关参数名（Qwen 系 enable_thinking）；与 thinking_param 二选一按渠道配
    #[serde(default)]
    pub enable_thinking_param: Option<String>,
    /// 思考预算参数名（Qwen 系 thinking_budget）
    #[serde(default)]
    pub thinking_budget_param: Option<String>,
    /// 思考是否计入 max_tokens（GLM 实测占、Qwen 官方不占；截断排查与预算提示用）
    #[serde(default)]
    pub reasoning_counts_in_max_tokens: bool,
    /// 思考模式为独立模型的厂商（DeepSeek 官方走 deepseek-reasoner，无开关参数）：
    /// 用户开思考（thinking=on）时网关把模型名换成它
    #[serde(default)]
    pub thinking_model: Option<String>,
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
    fn presets_parse_with_new_fields() {
        let ps = load_test();
        assert!(ps.len() >= 18, "官方网关 13 家 + 本地/自定义 5 家，实际 {}", ps.len());
        // 只保留大模型厂商官方网关与本地推理服务，第三方中转/聚合已移除
        for id in [
            "deepseek", "qwen", "ernie", "spark", "moonshot", "zhipu", "minimax", "doubao",
            "hunyuan", "openai", "anthropic", "gemini", "xai", "ollama", "lmstudio", "mnn",
            "llamacpp", "custom",
        ] {
            assert!(ps.iter().any(|p| p.id == id), "缺预设 {id}");
        }
        for gone in ["sensenova", "siliconflow", "groq", "openrouter"] {
            assert!(!ps.iter().any(|p| p.id == gone), "{gone} 应已移除（第三方中转/聚合，非官方网关）");
        }
        // DeepSeek 思考=独立模型（官方无开关参数）
        let ds = ps.iter().find(|p| p.id == "deepseek").unwrap();
        assert_eq!(ds.thinking_model.as_deref(), Some("deepseek-reasoner"));
        // Qwen：布尔开关 + 预算，官方思考不占 max_tokens（实测）
        let qwen = ps.iter().find(|p| p.id == "qwen").unwrap();
        assert_eq!(qwen.reasoning_style, ReasoningStyle::ReasoningContent);
        assert_eq!(qwen.enable_thinking_param.as_deref(), Some("enable_thinking"));
        assert_eq!(qwen.thinking_budget_param.as_deref(), Some("thinking_budget"));
        assert!(!qwen.reasoning_counts_in_max_tokens, "Qwen 官方思考不占 max_tokens");
        let glm = ps.iter().find(|p| p.id == "zhipu").unwrap();
        assert_eq!(glm.reasoning_style, ReasoningStyle::ThinkingParam);
        assert!(glm.reasoning_counts_in_max_tokens, "GLM 思考占 max_tokens（实测）");
        // 官方网关各家的思考接线样式
        let anthropic = ps.iter().find(|p| p.id == "anthropic").unwrap();
        assert_eq!(anthropic.reasoning_style, ReasoningStyle::EffortParam);
        assert!(anthropic.reasoning_counts_in_max_tokens);
        let doubao = ps.iter().find(|p| p.id == "doubao").unwrap();
        assert_eq!(doubao.reasoning_style, ReasoningStyle::ThinkingParam);
        let hunyuan = ps.iter().find(|p| p.id == "hunyuan").unwrap();
        assert_eq!(hunyuan.enable_thinking_param.as_deref(), Some("enable_thinking"));
        let minimax = ps.iter().find(|p| p.id == "minimax").unwrap();
        assert_eq!(minimax.reasoning_style, ReasoningStyle::InContent, "M2 思考混在正文 <think> 标签");
        for local in ["ollama", "lmstudio", "mnn", "llamacpp", "custom"] {
            assert!(
                ps.iter().find(|p| p.id == local).unwrap().key_optional,
                "{local} 应 key 可空"
            );
        }
    }

    #[test]
    fn old_format_without_new_fields_still_parses() {
        let text = r#"{"providers":[{"id":"x","name":"X","default_base_url":"http://a","default_model":"m"}]}"#;
        let file: ProvidersFile = serde_json::from_str(text).unwrap();
        let p = &file.providers[0];
        assert_eq!(p.reasoning_style, ReasoningStyle::ReasoningContent);
        assert!(!p.reasoning_counts_in_max_tokens);
        assert!(!p.key_optional);
    }
}
