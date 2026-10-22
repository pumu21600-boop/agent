//! 用户设置持久化 —— settings.json（工程根目录）
//!
//! 架构设计（彻底重构）：
//! 1. **单一真理源**：废除顶层冗余镜像字段与位置元组手抄；每套配置独立自治。
//! 2. **Config 为一等实体**：包含渠道信息、模型选择、DPAPI 专属密钥、采样与思考参数、模型多模态能力标记。
//! 3. **RolesSettings 角色路由**：支持主力模型、会话摘要、标题生成及多模态视觉/视频/音频解析路由。
//! 4. **DPAPI 密钥隔离**：按配置独立加密保存（CryptProtectData），不入日志、不出内存。
//!
//! ## 配置面地图（收敛：三类五面，写入口各一）
//!
//! | 面 | 性质 | 写入口（grep 实证唯一） |
//! |----|------|------------------------|
//! | `config.toml` | 静态种子（随包只读） | 无运行时写（启动一次性读入） |
//! | `providers.json` | 预设目录（随包只读） | 无运行时写（`gateway::providers::init` 一次性读入） |
//! | `settings.json` | **唯一运行时用户配置** | `crate::storage::write_settings`（存储一门，原子写 + 审计）|
//! | `mcp_tokens.json` | 运行时状态·机密（DPAPI 逐条加密）| `crate::mcp::oauth` 内部原子写（独立国自持）|
//! | `plugin_data/` | 运行时状态·插件命名空间 KV | `crate::storage::plugin_write_at` / `plugin_delete_at`（键名白名单） |
//!
//! 旧字段名兼容（起）：serde alias 垫片退役，统一走 `parse_settings` →
//! `migrate_legacy_field_names` 单一迁移入口（纯函数、带单测；未来改名只追加一行）。

use std::collections::BTreeMap;

use anyhow::anyhow;
use base64::Engine;
use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};

use crate::gateway::providers;

const ENTROPY: &[u8] = b"agent-gateway-settings-v1";

/// 模型能力标记（：支持工具调用、图片解析、视频解析、音频解析）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    #[serde(default = "default_true")]
    pub tools: bool,
    #[serde(default)]
    pub vision: bool,
    #[serde(default)]
    pub video: bool,
    #[serde(default)]
    pub audio: bool,
}

fn default_true() -> bool {
    true
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            tools: true,
            vision: false,
            video: false,
            audio: false,
        }
    }
}

// （用户拍板）：按模型名「智能推导」能力的机制整体退役——聚合渠道上模型名与真实能力
// 对不上（统计不过来），预测错的徽章/门控比没有更糟。能力一律以用户手动勾选为准：
// 新建/迁移配置用 Default（tools 开、其余关，user_set=false），媒体门控只认勾选值。

/// 角色路由配置（：支持主力、摘要、标题及多模态视觉/视频/音频路由，解决 config.toml 割裂）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RolesSettings {
    /// 滚动摘要专用配置名或 id（未设回落主力渠道）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// 会话标题生成专用配置名或 id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 视觉/图片理解专用配置名或 id（多模态旁路代理）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<String>,
    /// 视频理解专用配置名或 id（多模态旁路代理）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<String>,
    /// 音频理解专用配置名或 id（多模态旁路代理）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    /// 向量嵌入专用配置名或 id（记忆库语义检索；未设时用本地哈希向量，不用主渠道兜底）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<String>,
    /// 预算降级渠道——单任务累计 token 超 [hooks] budget_tokens 后，剩余步骤改走它
    /// （B3 解析链：settings.roles.budget > config.toml [roles].budget > 不降级）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<String>,
    /// 子代理专用渠道（B3 解析链：settings.roles.subagent > config.toml [roles].subagent
    /// > 回落主渠道；派发时的请求级 model 覆盖优先于此）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<String>,
    /// 降级备用配置名列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<String>,
}

/// 内置功能开关（插件页「内置」组的可开关功能行）。缺省全开——旧 settings.json
/// 无此字段时行为与从前完全一致；关掉的语义见各字段注释（全部复用现成的退化路径）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureSettings {
    /// 功能桥接（原「角色分工」渠道路由）：关 = 摘要/标题/嵌入/多模态/子代理/预算降级
    /// 全部回主渠道或默认退化（config.toml [roles] 一并不再生效）
    #[serde(default = "default_true")]
    pub roles: bool,
    /// css-ui工具（界面微调引擎 + 手写 CSS）：关 = 收起工具入口，已调好的样式继续生效
    #[serde(default = "default_true")]
    pub css_tool: bool,
    /// 星云入口（记忆库可视化/检索）：关 = 只藏入口，记忆功能本体不受影响
    #[serde(default = "default_true")]
    pub galaxy: bool,
    /// SSE 断线即取消本轮任务（审计 07）：关（默认）= 任务在后台跑完，刷新页面能接着聊；
    /// 开 = 检测到连接断开就取消本轮，不再继续烧 token
    #[serde(default)]
    pub cancel_on_disconnect: bool,
    /// 自改总闸（2026-10-03 用户拍板 B）：关 = agent 所有「改自身」动作（设置/渠道/
    /// 插件技能启停/定时任务/会话管理/面板）一律拒绝并引导用户到插件页打开；
    /// 开 = 按当前审批档位放行（红线仍必问）。只读不闸。
    /// 默认开——与插件行「装着即启用」一致（行开关是唯一真相，toggle 会同步写这里）。
    #[serde(default = "default_true")]
    pub self_mod: bool,
}

impl Default for FeatureSettings {
    fn default() -> Self {
        Self { roles: true, css_tool: true, galaxy: true, cancel_on_disconnect: false, self_mod: true }
    }
}

impl FeatureSettings {
    /// 按键写开关。键名不在 `crate::plugin::ui::FEATURE_KEYS` 里 = 编程错误：
    /// serde 对未知字段静默丢弃，那种「开关写了却没生效」比报错更难查，所以显式报错。
    pub fn set(&mut self, key: &str, value: bool) -> Result<(), String> {
        match key {
            "roles" => self.roles = value,
            "css_tool" => self.css_tool = value,
            "galaxy" => self.galaxy = value,
            "cancel_on_disconnect" => self.cancel_on_disconnect = value,
            "self_mod" => self.self_mod = value,
            other => {
                return Err(format!(
                    "未知功能键 {other}（可选：{}）",
                    crate::plugin::ui::FEATURE_KEYS.join(" / ")
                ));
            }
        }
        Ok(())
    }
}

/// 成本台账设置（插件化后由「用量价格」插件的价格设置页维护）。
/// 单价一律以**美元 / 1M tokens** 规范存储（台账与硬闸的口径）；界面按 `currency` 显示，
/// 人民币值按 `usd_cny_rate` 折算——切币种只换展示，不动历史账。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostSettings {
    /// 展示币种："USD"（默认）| "CNY"
    #[serde(default = "default_currency")]
    pub currency: String,
    /// 汇率：1 USD = N CNY（人民币展示与录入折算用；填 0 或负值按 1 处理）
    #[serde(default = "default_usd_cny_rate")]
    pub usd_cny_rate: f64,
    /// 价格表：一行 = 一个「供应商 host + 模型 id」的三个单价（美元 / 1M tokens）。
    /// host 或 model 可为 `*` = 通配兜底（精确匹配优先于通配）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prices: Vec<ModelPrice>,
}

fn default_currency() -> String {
    "USD".to_string()
}
fn default_usd_cny_rate() -> f64 {
    7.2
}

/// 一条模型价格（美元 / 1M tokens）。`provider` 记渠道 base_url 的 host——
/// **同模型 id 不同供应商是两行**，靠 host 区分，绝不只看模型名。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPrice {
    /// 渠道 host（如 api.deepseek.com；`*` = 任意供应商的通配兜底）
    pub provider: String,
    /// 模型 id（如 deepseek-chat；`*` = 该供应商下任意模型的兜底）
    pub model: String,
    /// 输入单价（未命中缓存部分）
    pub prompt: f64,
    /// 缓存命中输入单价
    #[serde(default)]
    pub cached: f64,
    /// 输出单价
    #[serde(default)]
    pub completion: f64,
}

/// 一套配置（渠道信息 + 模型 + 本配置专属 key + 协议形态与采样参数 + 能力标记）。
///
/// B1 清除语义（施工单 012 固化，前后端任何一方改动必须对照本条）：
/// `0`=清除长度类（context_length/max_output_tokens）；负数=清除采样类（temperature/top_p）；
/// 空串/auto=清除思考类（thinking/thinking_style）；clear_key=true=清 key。
/// 「思考档位被吞」事故即源于此约定未成文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub id: String,
    /// 用户起的名字；空则自动补「配置 N」
    #[serde(default)]
    pub name: String,
    pub provider_id: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub models: Vec<String>,
    /// 本配置专属 key（DPAPI 密文 base64）；同地址的不同配置各有各的 key
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_cipher: Option<String>,
    /// 该配置模型的上下文长度（tokens）；影响圆环预算，未设走全局 [context].budget_tokens
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    /// 该配置模型的最大输出长度（tokens）；随请求作为 max_tokens 发给渠道
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// 采样参数：None=不发字段走渠道默认
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// 思考开关/档位："on"/"off"/low/medium/high/xhigh/max/minimal；None=渠道默认
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// 思考/协议规范形态覆盖（思考形态词表，只增不改名——B4 固化）：
    /// 空=auto 端点探测；none/openai/deepseek/zai/qwen/qwen-chat-template；
    /// 旧值（thinking_object/reasoning_effort/enable_thinking*）自动映射。
    /// 新形态入词表必须附真实渠道冒烟记录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_style: Option<String>,
    /// 模型能力标记（工具调用/图片/视频/音频解析）
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    /// A2 能力标记所有权：true=用户显式标记过（勾选框），此后任何模型切换/勾选
    /// 不得被推导覆盖；false=推导态（模型首次成为当前模型时按模型名推导作初值）。
    /// 「按模型名重新推导」按钮复位为 false。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub capabilities_user_set: bool,
}

impl Config {
    /// 生效的 base_url：自定义 > 预设默认
    pub fn effective_base_url(&self) -> String {
        if !self.base_url.trim().is_empty() {
            self.base_url.trim().to_string()
        } else {
            providers::find(&self.provider_id)
                .map(|p| p.default_base_url.clone())
                .unwrap_or_default()
        }
    }

    /// 生效的模型：已选 > 勾选列表的第一个 > 预设默认
    pub fn effective_model(&self) -> String {
        if !self.model.trim().is_empty() {
            self.model.trim().to_string()
        } else if let Some(first) = self.models.first() {
            first.clone()
        } else {
            providers::find(&self.provider_id)
                .map(|p| p.default_model.clone())
                .unwrap_or_default()
        }
    }

    /// 是否已保存 key
    #[allow(dead_code)]
    pub fn has_key(&self) -> bool {
        self.key_cipher.is_some()
    }

    /// key 的尾 4 位提示
    #[allow(dead_code)]
    pub fn key_hint(&self) -> String {
        self.decrypted_key()
            .map(|k| {
                let n = k.chars().count();
                if n > 4 {
                    k.chars().skip(n - 4).collect::<String>()
                } else {
                    k
                }
            })
            .unwrap_or_default()
    }

    /// DPAPI 解密本配置专属 key（仅内存，绝不序列化、绝不出日志）。
    /// B2 key 回落链（施工单 012 固化）：请求显式 key > 激活配置 DPAPI key >
    /// DEEPSEEK_API_KEY 环境变量；key 永不出日志、永不出 JSONL。
    pub fn decrypted_key(&self) -> Option<String> {
        self.key_cipher.as_deref().and_then(Settings::decrypt)
    }

    /// 保存 key 到本配置
    pub fn set_key(&mut self, key: &str) -> anyhow::Result<()> {
        let cipher = Settings::protect(key)?;
        self.key_cipher = Some(cipher);
        Ok(())
    }

    /// 清除本配置的 key
    pub fn clear_key(&mut self) {
        self.key_cipher = None;
    }
}

/// 根据模型名称智能推荐官方对齐协议规范
#[allow(dead_code)]
pub fn infer_protocol_preset(model_id: &str) -> Option<&'static str> {
    let id = model_id.trim().to_lowercase();
    if id.contains("deepseek") || id.contains("r1") {
        Some("deepseek")
    } else if id.contains("qwen") || id.contains("hunyuan") {
        Some("qwen")
    } else if id.contains("glm") || id.contains("doubao") {
        Some("zai")
    } else if id.contains("gpt-5") || id.contains("o1") || id.contains("o3") || id.contains("o4") {
        Some("openai")
    } else if id.contains("claude") {
        Some("claude")
    } else if id.contains("kimi") || id.contains("moonshot") {
        Some("moonshot")
    } else {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct Settings {
    /// 当前激活配置 ID（起旧字段名 `active_profile_id` 由 parse_settings 迁移收口，垫片退役）
    #[serde(default)]
    pub active_config_id: Option<String>,

    /// 配置列表（按配置独立保存 provider/base_url/model/key/参数；旧名 `profiles` 走迁移入口）
    #[serde(default)]
    pub configs: Vec<Config>,

    /// 角色路由映射（摘要/标题/多模态旁路/降级链）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<RolesSettings>,

    /// 工作区：agent 实际操作的项目目录（None=工程根）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,

    /// 最近用过的工作区（新→旧，最多 8 个）
    #[serde(default)]
    pub recent_workspaces: Vec<String>,

    /// DSH 式侧边栏「重命名项目」：工作区路径 → 显示名（DSH 项目别名；空串=未起名）
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workspace_aliases: BTreeMap<String, String>,

    /// （施工单 016）：MCP server 配置（stdio transport；工具以 mcp__<server>__<tool> 接入）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerCfg>,

    /// 停用的插件名单（plugins/ 目录名；停用的不加载、不参与面板插槽与数据桥）。
    /// 旧字段名 `disabled_extensions` 由 parse_settings 迁移收口（垫片退役），落盘写新名。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_plugins: Vec<String>,

    /// 2026-10-02：停用的技能名单（技能名；停用的不进系统提示索引与 composer 菜单）。
    /// 与 disabled_plugins 同族，skills::set_disabled 进程内副本由启动与 toggle 端点灌入。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_skills: Vec<String>,

    /// 成本台账：展示币种 / 汇率 / 模型价格表（美元 / 1M tokens 规范存储）
    #[serde(default)]
    pub cost: CostSettings,

    /// 内置功能开关（插件页「内置」组的功能行；缺省全开）
    #[serde(default)]
    pub features: FeatureSettings,

    /// 定时任务清单（cron 唤醒无头会话；设置→定时 页签维护）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<crate::schedule::ScheduleTask>,

    /// 完成记录里被用户 × 掉的通知 key（notice_key = ts+task_id）。
    /// notices 缓冲本身重启即失，但桌面壳重启不重建 agent.exe 服务、设置弹窗每次重开
    /// 都重建组件，× 不落盘就会「明明叉掉了又回来」（2026-10-05 用户实锤）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub schedule_dismissed: Vec<String>,

    // —— 历史迁移源（仅在反序列化旧版 settings.json 时读入，序列化时不写出） ——
    #[serde(default, skip_serializing)]
    pub keys: BTreeMap<String, String>,
    #[serde(default, skip_serializing)]
    pub key_cipher: Option<String>,
}

/// 进程级共享句柄：`AppState.settings` 与 agent 的 settings 工具用**同一个 Arc**，
/// 两边看到并写的是同一份状态（工具层不持 AppState，经此取；main 启动时装一次）。
static SHARED_SETTINGS: std::sync::OnceLock<std::sync::Arc<std::sync::RwLock<Settings>>> =
    std::sync::OnceLock::new();

/// main 启动时安装共享句柄（传入的 Arc 必须与 `AppState.settings` 同源）。
pub fn install_shared_settings(handle: std::sync::Arc<std::sync::RwLock<Settings>>) {
    let _ = SHARED_SETTINGS.set(handle);
}

/// 取共享设置句柄；未安装（测试/极早调用）返回 None。
pub(crate) fn shared_settings() -> Option<std::sync::Arc<std::sync::RwLock<Settings>>> {
    SHARED_SETTINGS.get().cloned()
}


/// MCP OAuth 配置（授权码 / client_credentials 两种流程）：
/// authorization_code=浏览器授权码流程（远程托管 MCP 服务登录）；
/// client_credentials=机器凭据直换 token（无浏览器）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOAuthCfg {
    /// "authorization_code" | "client_credentials"
    #[serde(default = "default_oauth_type")]
    pub r#type: String,
    /// 预置 client_id；空=走 DCR 动态注册（授权码流程）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_id: String,
    /// client_credentials 必填；授权码流程仅个别授权服务器要求
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_secret: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope: String,
    /// loopback 回调路径（默认 /callback）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub redirect_path: String,
}

fn default_oauth_type() -> String {
    "authorization_code".to_string()
}

/// 单个 MCP server 的连接与策略配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerCfg {
    /// 服务名（工具名 mcp__<name>__<tool>；禁止包含 "__"）
    pub name: String,
    /// transport："stdio"（默认，本地子进程）| "http"（MCP Streamable HTTP 远端）
    #[serde(default = "default_transport")]
    pub transport: String,
    /// stdio：启动命令（如 npx / node / 绝对路径 exe）；http 时忽略
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// http：服务端点（形如 https://host/mcp）；stdio 时忽略
    #[serde(default)]
    pub url: String,
    /// http：自定义请求头（鉴权等；值只在内存与 settings.json，绝不进日志）
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// http：OAuth 登录配置（None=用手工 headers 或无需鉴权）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthCfg>,
    /// 启停（false 时工具不下发、不可调用）
    #[serde(default = "crate::config::settings::yes")]
    pub enabled: bool,
}
fn yes() -> bool {
    true
}
fn default_transport() -> String {
    "stdio".into()
}

/// key 存储作用域归一化：小写、去首尾空白与尾部 '/'。
pub fn normalize_base(url: &str) -> String {
    url.trim().trim_end_matches('/').to_lowercase()
}

/// 用于反序列化旧版 v0/v1 settings.json 顶层字段的过渡结构
#[derive(Debug, Deserialize)]
struct LegacyTopLevelFields {
    #[serde(default)]
    provider_id: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    thinking_style: Option<String>,
}

impl Settings {
    /// 写功能总开关（插件开关联动用，见 server::plugins::toggle_plugin）。
    /// 未知键返回 Err（调用方落审计/报错，不静默）。
    pub fn set_feature(&mut self, key: &str, value: bool) -> Result<(), String> {
        self.features.set(key, value)
    }

    /// 读取；不存在时用 config.toml 的 gateway 节做种子。
    /// 逐级迁移：旧版单 key / keys 映射 -> configs 专属 key。
    pub fn load(seed_base_url: &str, seed_model: &str) -> Self {
        let path = Self::path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            match settings_from_text(&text) {
                Ok(s) => {
                    // 落盘前留一份 .bak（2026-10-04 真机教训：settings.json 丢过一次
                    // 整个 mcp_servers，触发原因没定位到，先保证任何丢失可回退一份）
                    let _ = std::fs::copy(&path, path.with_extension("json.bak"));
                    let _ = s.save();
                    return s;
                }
                Err(e) => {
                    // 解析失败：坏文件先改名备份，绝不静默用种子覆盖用户配置
                    eprintln!("[settings] 解析失败（{e}），坏文件备份后重建默认设置");
                    let bak = path.with_extension(format!(
                        "json.corrupt-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    ));
                    let _ = std::fs::rename(&path, &bak);
                }
            }
        }
        // 文件不存在：首次启动走下方种子

        let mut s = Self::default();
        let model = if seed_model.is_empty() {
            "deepseek-chat".to_string()
        } else {
            seed_model.to_string()
        };
        let cap = ModelCapabilities::default();
        s.configs.push(Config {
            id: new_config_id(),
            name: "配置 1".to_string(),
            provider_id: "deepseek".to_string(),
            base_url: seed_base_url.to_string(),
            model,
            models: Vec::new(),
            key_cipher: None,
            context_length: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            thinking: None,
            thinking_style: None,
            capabilities: cap,
            capabilities_user_set: false,
        });
        s.ensure_active_config();
        let _ = s.save();
        s
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        // 存储一门：settings.json 落盘只走 `storage::write_settings`（原子写 + 审计
        // 都在那里面）；这里是仓内唯一调用点。
        crate::storage::write_settings(&Self::path(), &json)
    }

    /// settings.json 路径（外部监听与 load 共用；暴露成 pub(crate) 供 server 的
    /// 外部改动监听轮询同一个文件，别在两处各写一份路径拼装）。
    pub(crate) fn path_pub() -> std::path::PathBuf {
        crate::config::app_root().join("settings.json")
    }

    fn path() -> std::path::PathBuf {
        crate::config::app_root().join("settings.json")
    }
    /// 迁移历史按 provider 存储的 key 到 base_url
    fn migrate_provider_keys_to_urls(&mut self) {
        let mut moved = Vec::new();
        let mut added = Vec::new();
        for (slot, cipher) in &self.keys {
            if slot.contains("://") {
                continue;
            }
            let target = self
                .configs
                .iter()
                .rev()
                .find(|p| p.provider_id == *slot)
                .map(|p| p.base_url.clone())
                .filter(|u| !u.trim().is_empty())
                .or_else(|| providers::find(slot).map(|p| p.default_base_url.clone()))
                .filter(|u| !u.trim().is_empty())
                .map(|u| normalize_base(&u));
            let Some(url) = target else { continue };
            if !self.keys.contains_key(&url) {
                added.push((url, cipher.clone()));
            }
            moved.push(slot.clone());
        }
        for slot in &moved {
            self.keys.remove(slot);
        }
        for (url, cipher) in added {
            self.keys.insert(url, cipher);
        }
    }

    /// 迁移按 base_url 存储的 key 到各配置的 key_cipher
    fn migrate_url_keys_to_configs(&mut self) {
        for c in &mut self.configs {
            if c.key_cipher.is_some() {
                continue;
            }
            let url = if !c.base_url.trim().is_empty() {
                c.base_url.clone()
            } else {
                providers::find(&c.provider_id)
                    .map(|p| p.default_base_url.clone())
                    .unwrap_or_default()
            };
            if url.trim().is_empty() {
                continue;
            }
            if let Some(cipher) = self.keys.get(&normalize_base(&url)) {
                c.key_cipher = Some(cipher.clone());
            }
        }
    }

    /// 确保 configs 非空且有激活项；缺名的补「配置 N」。
    pub fn ensure_active_config(&mut self) {
        if self.configs.is_empty() {
            self.configs.push(Config {
                id: new_config_id(),
                name: "配置 1".to_string(),
                provider_id: "deepseek".to_string(),
                base_url: String::new(),
                model: "deepseek-chat".to_string(),
                models: Vec::new(),
                key_cipher: None,
                context_length: None,
                max_output_tokens: None,
                temperature: None,
                top_p: None,
                thinking: None,
                thinking_style: None,
                capabilities: ModelCapabilities::default(),
                capabilities_user_set: false,
            });
        }
        self.assign_missing_names();
        if self
            .active_config_id
            .as_ref()
            .is_none_or(|id| !self.configs.iter().any(|c| &c.id == id))
        {
            self.active_config_id = self.configs.first().map(|c| c.id.clone());
        }
    }

    /// 给没有名字的配置按序号补名（配置 1、配置 2…跳过已占用序号）。
    fn assign_missing_names(&mut self) {
        let mut n = 1;
        let mut i = 0;
        while i < self.configs.len() {
            if self.configs[i].name.trim().is_empty() {
                while self.configs.iter().any(|o| o.name == format!("配置 {n}")) {
                    n += 1;
                }
                self.configs[i].name = format!("配置 {n}");
            }
            i += 1;
        }
    }

    /// 下一个可用「配置 N」序号。
    fn next_name_seq(&self) -> u32 {
        let mut n = 1;
        while self.configs.iter().any(|c| c.name == format!("配置 {n}")) {
            n += 1;
        }
        n
    }

    /// 改当前激活配置的名字（空名回退自动编号）。
    pub fn set_active_name(&mut self, name: &str) {
        let name = name.trim().to_string();
        let Some(id) = self.active_config_id.clone() else { return };
        if let Some(c) = self.configs.iter_mut().find(|c| c.id == id) {
            c.name = name;
        }
        self.assign_missing_names();
    }

    /// 当前激活配置。
    pub fn active(&self) -> Option<&Config> {
        let id = self.active_config_id.as_deref()?;
        self.configs.iter().find(|c| c.id == id)
    }

    /// 当前激活配置的可变引用。
    pub fn active_mut(&mut self) -> Option<&mut Config> {
        let id = self.active_config_id.clone()?;
        self.configs.iter_mut().find(|c| c.id == id)
    }

    /// 保证返回一个可用配置引用
    pub fn active_or_default(&self) -> &Config {
        self.active()
            .or_else(|| self.configs.first())
            .expect("Settings 必须保证至少有一套配置")
    }

    /// 切换激活配置。返回是否成功。
    pub fn activate(&mut self, id: &str) -> bool {
        if self.configs.iter().any(|c| c.id == id) {
            self.active_config_id = Some(id.to_string());
            true
        } else {
            false
        }
    }

    /// 新建一套配置并激活：供应商/Base URL 复制当前作起点，模型与 key 一律从零。
    pub fn new_config(&mut self) -> String {
        let (provider_id, base_url) = self
            .active()
            .map(|c| (c.provider_id.clone(), c.base_url.clone()))
            .unwrap_or_else(|| ("deepseek".to_string(), String::new()));
        let id = new_config_id();
        let name = format!("配置 {}", self.next_name_seq());
        self.configs.push(Config {
            id: id.clone(),
            name,
            provider_id,
            base_url,
            model: String::new(),
            models: Vec::new(),
            key_cipher: None,
            context_length: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            thinking: None,
            thinking_style: None,
            capabilities: ModelCapabilities::default(),
            capabilities_user_set: false,
        });
        self.active_config_id = Some(id.clone());
        id
    }

    /// 删除一套配置；删的是激活项则自动切到剩余第一套。
    pub fn delete_config(&mut self, id: &str) -> bool {
        let before = self.configs.len();
        self.configs.retain(|c| c.id != id);
        if self.configs.len() == before {
            return false;
        }
        if self.active_config_id.as_deref() == Some(id) {
            self.active_config_id = self.configs.first().map(|c| c.id.clone());
            self.ensure_active_config();
        }
        true
    }

    // —— 便捷委托方法（供 main.rs 与业务调用） ——

    pub fn provider_id(&self) -> String {
        self.active().map(|c| c.provider_id.clone()).unwrap_or_else(|| "deepseek".to_string())
    }

    pub fn effective_base_url(&self) -> String {
        self.active_or_default().effective_base_url()
    }

    pub fn effective_model(&self) -> String {
        self.active_or_default().effective_model()
    }

    pub fn config_key(&self, c: &Config) -> Option<String> {
        c.decrypted_key()
    }

    pub fn get_key(&self) -> Option<String> {
        self.active().and_then(|c| c.decrypted_key())
    }

    pub fn set_key(&mut self, key: &str) -> anyhow::Result<()> {
        self.ensure_active_config();
        if let Some(c) = self.active_mut() {
            c.set_key(key)?;
        }
        self.save()
    }

    pub fn clear_key(&mut self) -> anyhow::Result<()> {
        if let Some(c) = self.active_mut() {
            c.clear_key();
        }
        self.save()
    }

    /// DPAPI 解密（密文 base64）。
    pub(crate) fn decrypt(cipher_b64: &str) -> Option<String> {
        let bytes = base64::engine::general_purpose::STANDARD.decode(cipher_b64).ok()?;
        let input = CRYPT_INTEGER_BLOB { cbData: bytes.len() as u32, pbData: bytes.as_ptr() as *mut u8 };
        let entropy = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 || output.pbData.is_null() {
            return None;
        }
        let plain = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe { LocalFree(output.pbData as *mut _) };
        String::from_utf8(plain).ok()
    }

    /// DPAPI 加密成 base64。
    pub(crate) fn protect(key: &str) -> anyhow::Result<String> {
        let data = key.as_bytes();
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let entropy = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 || output.pbData.is_null() {
            return Err(anyhow!("DPAPI 加密 key 失败"));
        }
        let cipher = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe { LocalFree(output.pbData as *mut _) };
        Ok(base64::engine::general_purpose::STANDARD.encode(cipher))
    }
}

/// settings.json 文本 → 规范化 Settings（纯函数，无 IO 无落盘）。
///
/// `load`（启动/首次读）与**外部改动重载**（agent 或用户拿文件工具直接改了
/// settings.json）共用这一把尺子——两边迁移语义必须完全一致，否则「同一份文件，
/// 重启后是一种状态、热加载后是另一种」比不热加载更难查。历史迁移链：
/// 旧字段名 → 老版无 configs 挽救 → key 三级归位 → ensure_active_config
/// （信任键合并已随验签机制移除）。解析失败返回 Err（调用方各自处理：load 备份
/// 坏文件，重载保持内存现状）。
///
/// 全史格式考古定案（2026-10-22，自 09-02 施工单 001 起 47 个提交逐代对账）：
/// - v0（09-02）：顶层 provider_id/base_url/model/models/key_cipher 五字段，无
///   configs——走下方挽救路径，顶层 key 直接挂到挽救出的配置（2026-10-22 垫片改：
///   原先绕三跳靠 providers.json 默认 base_url 兜底，预设 id 收敛删除的场合 key
///   会卡死在过渡槽静默丢失；三跳链保留给 v1/v2 时代带 configs 的顶层 key）。
/// - profiles 时代（09-02）：profiles/active_profile_id → migrate_legacy_field_names。
/// - configs 时代（09-02 起）：key 从顶层共享逐级收紧到配置独占（v1/v2/v2.4）。
/// - Config.thinking_budget（09-03 一天窗口，P8.2 加入、同日 P8.4 照抄 OpenClaw
///   删除）：**有意不迁**——数值语义已被档位化 thinking + thinking_style 形态
///   取代，无迁移目标；serde 忽略未知字段，读旧文件静默丢弃该键。
/// - trusted_plugins 与 mcp_servers[].confirmed/risk：随验签/每服务审批档机制
///   移除（2026-10-08 用户拍板），旧键 serde 自然忽略，不迁。
/// - mcp_servers[].transport：09-14 时代只有 stdio 无此键；serde default="stdio"。
///
/// 未来字段改名只改 migrate_legacy_field_names（旧名→新名一行），勿另开入口。
pub(crate) fn settings_from_text(text: &str) -> Result<Settings, serde_json::Error> {
    let mut s = parse_settings(text)?;
    // 若旧版完全没有 configs 数组，从顶层字段挽救一套配置
    if s.configs.is_empty()
        && let Ok(legacy) = serde_json::from_str::<LegacyTopLevelFields>(text)
    {
        let provider_id = legacy.provider_id.unwrap_or_else(|| "deepseek".to_string());
        let base_url = legacy.base_url.unwrap_or_default();
        let model = legacy.model.unwrap_or_else(|| "deepseek-chat".to_string());
        let models = legacy.models.unwrap_or_default();
        let cap = ModelCapabilities::default();
        // v0 顶层 key 直接挂到挽救出的这套配置——它本来就属于这套配置，不必绕
        // providers 预设兜底的 provider→url→config 三跳（那条路在预设 id 被收敛
        // 删除的场合会把 key 卡死在过渡槽里静默丢失）；三跳链留给 v1/v2 时代
        // 带 configs 的顶层 key（见下方两段）。
        let key_cipher = s.key_cipher.take();
        s.configs.push(Config {
            id: new_config_id(),
            name: "配置 1".to_string(),
            provider_id,
            base_url,
            model,
            models,
            key_cipher,
            context_length: legacy.context_length,
            max_output_tokens: legacy.max_output_tokens,
            temperature: legacy.temperature,
            top_p: legacy.top_p,
            thinking: legacy.thinking,
            thinking_style: legacy.thinking_style,
            capabilities: cap,
            capabilities_user_set: false,
        });
    }

    // v1 -> v2：key_cipher 挂到当前 provider 作用域
    if s.keys.is_empty()
        && let Some(cipher) = s.key_cipher.take()
    {
        let pid = s.configs.first().map(|c| c.provider_id.clone()).unwrap_or_else(|| "deepseek".to_string());
        s.keys.insert(pid, cipher);
    }
    // v2 -> v2.2：provider 作用域 key 归位到 base_url 作用域
    s.migrate_provider_keys_to_urls();
    // v2.3 -> v2.4：key 从按 base_url 共享改为按配置独占
    s.migrate_url_keys_to_configs();
    // 确保存在有效激活配置
    s.ensure_active_config();
    Ok(s)
}

fn new_config_id() -> String {
    format!("cfg-{}", uuid::Uuid::new_v4().simple())
}

/// settings.json 解析唯一入口：先过一遍旧字段名迁移（纯函数），再按类型反序列化。
/// serde alias 垫片退役——旧字段名兼容只走这一条路（load 与单测共用）。
fn parse_settings(text: &str) -> Result<Settings, serde_json::Error> {
    let mut v: serde_json::Value = serde_json::from_str(text)?;
    migrate_legacy_field_names(&mut v);
    serde_json::from_value(v)
}

/// 配置面收敛：settings.json 旧字段名一次迁净（新旧并存以新名为准，旧键删除）。
/// 纯函数，load 与单测共用；未来改名只在此追加一行。
fn migrate_legacy_field_names(v: &mut serde_json::Value) {
    let Some(obj) = v.as_object_mut() else { return };
    for (old, new) in [
        ("active_profile_id", "active_config_id"),
        ("profiles", "configs"),
        ("disabled_extensions", "disabled_plugins"),
    ] {
        if let Some(val) = obj.remove(old)
            && !obj.contains_key(new)
        {
            obj.insert(new.to_string(), val);
        }
    }
}

#[cfg(test)]
mod tests;
