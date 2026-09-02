//! 供应商预设注册表 —— 内置主流供应商网关 + OpenAI 兼容兜底。
//!
//! 每个预设只带默认 base_url（用户可覆盖）与默认模型（用户可改）；
//! 不带模型清单——模型通过渠道的 GET /models 实时获取。

pub struct ProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub default_base_url: &'static str,
    pub default_model: &'static str,
}

/// 主流供应商预设（全部走 OpenAI 兼容 /chat/completions 协议）。
pub static PROVIDERS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "deepseek",
        name: "DeepSeek（深度求索）",
        default_base_url: "https://api.deepseek.com",
        default_model: "deepseek-chat",
    },
    ProviderPreset {
        id: "openai",
        name: "OpenAI",
        default_base_url: "https://api.openai.com/v1",
        default_model: "gpt-4o-mini",
    },
    ProviderPreset {
        id: "sensenova",
        name: "商汤日日新（SenseNova）",
        default_base_url: "https://api.sensenova.cn/compatible-mode/v1",
        default_model: "DeepSeek-V3",
    },
    ProviderPreset {
        id: "dashscope",
        name: "阿里云百炼（通义千问）",
        default_base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        default_model: "qwen-plus",
    },
    ProviderPreset {
        id: "zhipu",
        name: "智谱 AI（GLM）",
        default_base_url: "https://open.bigmodel.cn/api/paas/v4",
        default_model: "glm-4-flash",
    },
    ProviderPreset {
        id: "moonshot",
        name: "Moonshot（Kimi）",
        default_base_url: "https://api.moonshot.cn/v1",
        default_model: "moonshot-v1-8k",
    },
    ProviderPreset {
        id: "baidu",
        name: "百度千帆",
        default_base_url: "https://qianfan.baidubce.com/v2",
        default_model: "ernie-4.0-8k",
    },
    ProviderPreset {
        id: "spark",
        name: "讯飞星火",
        default_base_url: "https://spark-api-open.xf-yun.com/v1",
        default_model: "generalv3.5",
    },
    ProviderPreset {
        id: "siliconflow",
        name: "硅基流动 SiliconFlow",
        default_base_url: "https://api.siliconflow.cn/v1",
        default_model: "deepseek-ai/DeepSeek-V3",
    },
    ProviderPreset {
        id: "groq",
        name: "Groq",
        default_base_url: "https://api.groq.com/openai/v1",
        default_model: "llama-3.3-70b-versatile",
    },
    ProviderPreset {
        id: "xai",
        name: "xAI（Grok）",
        default_base_url: "https://api.x.ai/v1",
        default_model: "grok-2-latest",
    },
    ProviderPreset {
        id: "ollama",
        name: "Ollama（本地）",
        default_base_url: "http://localhost:11434/v1",
        default_model: "qwen2.5:7b",
    },
    ProviderPreset {
        id: "custom",
        name: "OpenAI 兼容（自定义）",
        default_base_url: "",
        default_model: "",
    },
];

pub fn find(id: &str) -> Option<&'static ProviderPreset> {
    PROVIDERS.iter().find(|p| p.id == id)
}
