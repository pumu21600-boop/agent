//! OpenAI 兼容端点的兼容性探测 —— 移植自 OpenClaw `packages/ai/src/transports/openai-completions-compat.ts`（v2026.8.2）。
//!
//! 核心思想：不按 provider id 硬编码分支，而是按「端点归属」推导请求行为（thinkingFormat、
//! max_tokens 字段、temperature 收不收等），并允许渠道配置覆盖。
//! 本产品无模型目录，探测输入 = 渠道 provider_id（预设）+ base_url + model id，
//! 三者都认不出时按 OpenAI 标准端点处理——与上游「端点归属 + 配置覆盖」一致。

use super::effort;

/// 思考参数的线上形态（上游 thinkingFormat 的本产品子集——openrouter/together 是聚合平台，
/// 本产品只对接官方网关与本地服务，不收录；zai/deepseek/qwen 全保留）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingFormat {
    /// 极简/禁用：绝不向端点发送任何思考相关参数
    None,
    /// OpenAI 原生：reasoning_effort 档位（经 effort::resolve 按模型降级；无法表达"关"时不发）
    #[default]
    Openai,
    /// DeepSeek/小米：thinking 对象 {type:enabled|disabled}；响应读 reasoning_content；
    /// 历史回放保留 reasoning_content 字段
    Deepseek,
    /// 智谱 GLM：thinking 对象 {type:enabled|disabled}（档位无意义）
    Zai,
    /// 通义千问/混元：顶层布尔 enable_thinking
    Qwen,
    /// 本地推理服务聊天模板形态（vLLM/llama.cpp/ollama 的 Qwen3 系）：chat_template_kwargs.enable_thinking
    QwenChatTemplate,
}

impl ThinkingFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            ThinkingFormat::None => "none",
            ThinkingFormat::Openai => "openai",
            ThinkingFormat::Deepseek => "deepseek",
            ThinkingFormat::Zai => "zai",
            ThinkingFormat::Qwen => "qwen",
            ThinkingFormat::QwenChatTemplate => "qwen-chat-template",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "none" => Some(Self::None),
            "openai" => Some(Self::Openai),
            "deepseek" => Some(Self::Deepseek),
            "zai" => Some(Self::Zai),
            "qwen" => Some(Self::Qwen),
            "qwen-chat-template" => Some(Self::QwenChatTemplate),
            _ => None,
        }
    }
}

/// 传输协议维度（P23）：同一渠道走 OpenAI Chat Completions 还是 Anthropic Messages。
/// 与 ThinkingFormat 正交——Anthropic 协议的思考参数在 anthropic.rs 里按 Messages 规范组装。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    /// OpenAI 兼容 /chat/completions（含全部国产官方网关、中转、本地推理服务）
    #[default]
    OpenAiCompletions,
    /// Anthropic Messages（/v1/messages：system 顶层、tool_use/tool_result 块、x-api-key 鉴权）
    AnthropicMessages,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::OpenAiCompletions => "openai-completions",
            Protocol::AnthropicMessages => "anthropic-messages",
        }
    }
}

/// 端点兼容性档（请求组装与响应解析都按它走）。
#[derive(Debug, Clone)]
pub struct Compat {
    /// 传输协议（P23）：Anthropic 端点走 Messages 适配器，其余走 OpenAI Completions
    pub protocol: Protocol,
    pub thinking_format: ThinkingFormat,
    /// "max_tokens" 或 "max_completion_tokens"（OpenAI 新系模型要求后者）
    pub max_tokens_field: &'static str,
    /// 流式是否带 stream_options.include_usage
    pub supports_usage_in_streaming: bool,
    /// 是否支持 temperature（gpt-5.6 家族拒收 400）
    pub supports_temperature: bool,
    /// 历史回放：assistant 消息是否保留 reasoning_content（DeepSeek/zai 契约）
    pub requires_reasoning_content_replay: bool,
}

/// 提取 base_url 的 host 部分（小写）。
fn host_of(base_url: &str) -> String {
    base_url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_lowercase()
}

fn host_has(base_url: &str, needle: &str) -> bool {
    host_of(base_url).contains(needle)
}

/// 按域名归类端点：native=官方网关，local=本地推理服务，default=中转/未识别。
fn endpoint_class(provider_id: Option<&str>, base_url: Option<&str>) -> &'static str {
    let base = base_url.unwrap_or("");
    if base.trim().is_empty() {
        // 无地址（纯函数调用/未填渠道）：按预设 id 归类
        return match provider_id {
            Some(
                "deepseek" | "zhipu" | "qwen" | "doubao" | "moonshot" | "openai" | "anthropic"
                | "gemini" | "xai" | "minimax" | "hunyuan" | "ernie" | "spark",
            ) => "native",
            Some("ollama" | "lmstudio" | "mnn" | "llamacpp") => "local",
            _ => "default",
        };
    }
    let host = host_of(base);
    // 剥端口再判本地（localhost:11434 / 127.0.0.1:8080；IPv6 [::1]:port 保留括号形态）
    let bare_host = if host.starts_with("[::1]") {
        host.clone()
    } else {
        host.split(':').next().unwrap_or("").to_string()
    };
    let is_local = bare_host == "localhost" || bare_host.starts_with("127.") || bare_host.starts_with("[::1]");
    // 官方域名优先（custom 渠道指到官方域名也算 native）
    const OFFICIAL_DOMAINS: [(&str, &str); 13] = [
        ("deepseek.com", "deepseek"),
        ("bigmodel.cn", "zhipu"),
        ("aliyuncs.com", "qwen"),
        ("volces.com", "doubao"),
        ("moonshot", "moonshot"),
        ("openai.com", "openai"),
        ("anthropic.com", "anthropic"),
        ("googleapis.com", "gemini"),
        ("x.ai", "xai"),
        ("minimax", "minimax"),
        ("tencent.com", "hunyuan"),
        ("baidubce.com", "ernie"),
        ("xf-yun.com", "spark"),
    ];
    for (domain, _) in OFFICIAL_DOMAINS {
        if host.contains(domain) {
            return "native";
        }
    }
    if is_local {
        // 本地端口：预设认得出用预设 id，认不出也算 local（custom=旧「自定义端点」预设 id，保留兼容）
        if matches!(provider_id, Some("ollama" | "lmstudio" | "mnn" | "llamacpp" | "openai_compat" | "custom"))
            || provider_id.is_none()
            || provider_id == Some("custom")
        {
            return "local";
        }
    }
    "default"
}

/// 推导端点兼容档（上游 resolveOpenAICompletionsCompatDefaults 的本产品版）。
/// `provider_id`/`base_url` 供域名判定；`model_id` 供模型族判定（gpt-5* 等）；
/// `format_override`：providers.json/渠道配置显式指定（优先级最高）。
pub fn resolve(
    provider_id: Option<&str>,
    base_url: Option<&str>,
    model_id: &str,
    format_override: Option<ThinkingFormat>,
) -> Compat {
    let class = endpoint_class(provider_id, base_url);
    // 传输协议（P23）：provider_id=anthropic 或官方域 → Messages 协议，其余 OpenAI Completions
    let protocol = if super::anthropic::is_anthropic_endpoint(provider_id, base_url) {
        Protocol::AnthropicMessages
    } else {
        Protocol::OpenAiCompletions
    };

    // 显式覆盖优先（providers.json 预设或渠道配置）
    if let Some(f) = format_override {
        return Compat {
            protocol,
            thinking_format: f,
            max_tokens_field: max_tokens_field_for(model_id),
            supports_usage_in_streaming: true,
            supports_temperature: !effort::is_gpt_5_6_model(model_id),
            requires_reasoning_content_replay: matches!(f, ThinkingFormat::Deepseek | ThinkingFormat::Zai),
        };
    }

    // 本地推理服务：聊天模板形态思考开关；宽收 stream_options
    if class == "local" {
        return Compat {
            protocol,
            thinking_format: ThinkingFormat::QwenChatTemplate,
            max_tokens_field: "max_tokens",
            supports_usage_in_streaming: true,
            supports_temperature: true,
            requires_reasoning_content_replay: false,
        };
    }

    // 端点归属判定：选了什么供应商就用什么规范（用户定调）
    // 无论 Base URL 是否是中转代理，只要供应商选定了，端点契约严格对齐该供应商！
    let is_deepseek = provider_id == Some("deepseek") || (class == "native" && host_has(base_url.unwrap_or(""), "deepseek.com"));
    let is_xiaomi = class == "native" && host_has(base_url.unwrap_or(""), "xiaomi");
    let is_zai = provider_id == Some("zhipu") || provider_id == Some("doubao") || (class == "native" && (host_has(base_url.unwrap_or(""), "bigmodel.cn") || host_has(base_url.unwrap_or(""), "volces.com")));
    let is_qwen = provider_id == Some("qwen") || provider_id == Some("hunyuan") || (class == "native" && (host_has(base_url.unwrap_or(""), "aliyuncs.com") || host_has(base_url.unwrap_or(""), "tencent.com")));

    let thinking_format = if is_deepseek || is_xiaomi {
        ThinkingFormat::Deepseek
    } else if is_zai {
        ThinkingFormat::Zai
    } else if is_qwen {
        ThinkingFormat::Qwen
    } else {
        ThinkingFormat::Openai
    };

    Compat {
        protocol,
        thinking_format,
        max_tokens_field: max_tokens_field_for(model_id),
        supports_usage_in_streaming: true,
        supports_temperature: !effort::is_gpt_5_6_model(model_id),
        requires_reasoning_content_replay: matches!(
            thinking_format,
            ThinkingFormat::Deepseek | ThinkingFormat::Zai
        ),
    }
}

/// OpenAI 新系模型（gpt-5*/o 系）要求 max_completion_tokens；其余一律 max_tokens
/// （国产/中转端点普遍只认 max_tokens；上游 usesMaxTokens 名单是它们目录的镜像）。
fn max_tokens_field_for(model_id: &str) -> &'static str {
    let id = model_id.trim().to_lowercase();
    if id.starts_with("gpt-5") || id.starts_with("o1") || id.starts_with("o3") || id.starts_with("o4") {
        "max_completion_tokens"
    } else {
        "max_tokens"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_endpoints_get_native_formats() {
        let c = resolve(Some("zhipu"), Some("https://open.bigmodel.cn/api/paas/v4"), "glm-4.6", None);
        assert_eq!(c.thinking_format, ThinkingFormat::Zai);
        // 上游 shouldPreserveReasoningContentReplay：deepseek 与 zai 形态都保留回放
        assert!(c.requires_reasoning_content_replay);

        let c = resolve(Some("deepseek"), Some("https://api.deepseek.com"), "deepseek-chat", None);
        assert_eq!(c.thinking_format, ThinkingFormat::Deepseek);
        assert!(c.requires_reasoning_content_replay);

        let c = resolve(Some("qwen"), Some("https://dashscope.aliyuncs.com/compatible-mode/v1"), "qwen-plus", None);
        assert_eq!(c.thinking_format, ThinkingFormat::Qwen);

        let c = resolve(Some("openai"), Some("https://api.openai.com/v1"), "gpt-5", None);
        assert_eq!(c.thinking_format, ThinkingFormat::Openai);
        assert_eq!(c.max_tokens_field, "max_completion_tokens");
        assert!(c.supports_temperature, "gpt-5.6 家族才拒收；gpt-5 应支持");
        let c = resolve(Some("openai"), Some("https://api.openai.com/v1"), "gpt-5.6", None);
        assert!(!c.supports_temperature);

        let c = resolve(Some("openai"), Some("https://api.openai.com/v1"), "gpt-4o-mini", None);
        assert_eq!(c.max_tokens_field, "max_tokens");
    }

    #[test]
    fn local_endpoints_use_chat_template() {
        let c = resolve(Some("ollama"), Some("http://localhost:11434/v1"), "qwen3:8b", None);
        assert_eq!(c.thinking_format, ThinkingFormat::QwenChatTemplate);
        // custom 渠道指到本地端口也按 local
        let c = resolve(Some("custom"), Some("http://127.0.0.1:8080/v1"), "qwen3", None);
        assert_eq!(c.thinking_format, ThinkingFormat::QwenChatTemplate);
    }

    #[test]
    fn custom_endpoint_falls_back_to_openai_format() {
        let c = resolve(Some("custom"), Some("https://relay.example.com/v1"), "mystery-model", None);
        assert_eq!(c.thinking_format, ThinkingFormat::Openai);
        assert_eq!(c.max_tokens_field, "max_tokens");
        // 模型 id 自证身份只在 native 端点生效；中转域名（default）仍按 openai 格式，
        // 想要 deepseek 契约必须显式覆盖
        let c = resolve(Some("custom"), Some("https://relay.example.com/v1"), "deepseek-reasoner", None);
        assert_eq!(c.thinking_format, ThinkingFormat::Openai, "中转域名探测不出 → openai 格式");
        // 显式覆盖生效（预设 thinking_format / 渠道配置）
        let c = resolve(Some("custom"), Some("https://relay.example.com/v1"), "mystery-model", Some(ThinkingFormat::Deepseek));
        assert_eq!(c.thinking_format, ThinkingFormat::Deepseek);
        assert!(c.requires_reasoning_content_replay);
    }

    #[test]
    fn doubao_and_provider_dictate_protocol() {
        // 豆包供应商直接识别为 Zai 形态
        let c = resolve(Some("doubao"), Some("https://ark.cn-beijing.volces.com/api/v3"), "doubao-seed-1.6", None);
        assert_eq!(c.thinking_format, ThinkingFormat::Zai);

        // 选了什么供应商就用什么规范：即便地址是第三方中转，只要选了 deepseek，就走 deepseek 契约
        let c = resolve(Some("deepseek"), Some("https://token.sensenova.cn/v1"), "deepseek-v4-flash", None);
        assert_eq!(c.thinking_format, ThinkingFormat::Deepseek);
        assert!(c.requires_reasoning_content_replay);

        let c = resolve(Some("qwen"), Some("https://relay.custom.io/v1"), "qwen-max", None);
        assert_eq!(c.thinking_format, ThinkingFormat::Qwen);
    }

    #[test]
    fn anthropic_endpoints_get_messages_protocol() {
        // 预设 id 与官方域都判 Anthropic Messages 协议；OpenAI 形状字段与之正交
        let c = resolve(Some("anthropic"), Some("https://api.anthropic.com"), "claude-sonnet-4-5", None);
        assert_eq!(c.protocol, Protocol::AnthropicMessages);
        assert_eq!(c.max_tokens_field, "max_tokens");
        // 域名判定：自定义渠道指到 anthropic.com 也是 Messages 协议
        let c = resolve(Some("openai_compat"), Some("https://api.anthropic.com/v1"), "claude-3-5-haiku", None);
        assert_eq!(c.protocol, Protocol::AnthropicMessages);
        // 自建 Anthropic 兼容端点（路径含 /anthropic）：智谱 GLM Coding Plan
        let c = resolve(Some("zhipu"), Some("https://open.bigmodel.cn/api/anthropic"), "GLM-5.3", None);
        assert_eq!(c.protocol, Protocol::AnthropicMessages);
        // 中转域名不带 anthropic 标识 → OpenAI 协议（要 Messages 就选 anthropic 预设）
        let c = resolve(Some("openai_compat"), Some("https://relay.example.com/v1"), "claude-sonnet-4-5", None);
        assert_eq!(c.protocol, Protocol::OpenAiCompletions);
        // 国产官方域不受影响
        let c = resolve(Some("zhipu"), Some("https://open.bigmodel.cn/api/paas/v4"), "glm-4.6", None);
        assert_eq!(c.protocol, Protocol::OpenAiCompletions);
    }
}
