//! 配置加载 —— config.toml + 环境变量覆盖（施工单 001 / 任务 B）
//!
//! 硬约束一：base_url 从 config.toml 读，可用环境变量 DEEPSEEK_BASE_URL 覆盖（商汤中转）。
//! 硬约束二：API key 不在这里，只走环境变量 DEEPSEEK_API_KEY。

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub mod settings;
pub mod settings_schema;

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
}

fn default_checkpoint_enabled() -> bool {
    true
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self { checkpoint_enabled: default_checkpoint_enabled() }
    }
}

/// 核心循环参数（施工单 003 / 005）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// 单次任务模型决策步数上限（0 = 不限：2026-10-03 用户拍板，大项目 100+ 次工具
    /// 调用是常态；失控由取消标志/相同调用护栏/花费硬闸管，不由步数砍任务）
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
    // 0 = 不限（2026-10-03 用户拍板：大项目 100+ 次工具调用是常态；失控由取消标志/
    // 花费硬闸两道缰绳管，不由步数把长任务拦腰砍断）
    0
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

/// agent 经 settings 工具改 config.toml 的一个键（2026-10-04 放开权限轮，用户拍板
/// 「除审批档位/API key 全部可改」）。key 形如 `agent.max_steps`（section.field，支持更深）。
/// 用 toml_edit 保注释改写——config.toml 是用户精心维护的注释文件，toml::to_string
/// 重排会把这些注释全冲掉。改完需重启服务才生效（config 不热加载），返回里带提示。
pub fn set_config_key(key: &str, value: &serde_json::Value) -> Result<String, String> {
    let path = app_root().join("config.toml");
    set_config_key_at(&path, key, value)
}

/// 同上，路径可注入（单测用）。
pub fn set_config_key_at(path: &std::path::Path, key: &str, value: &serde_json::Value) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("读 config.toml 失败: {e}"))?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("config.toml 不是合法 TOML: {e}"))?;
    // 类型对齐（真机实锤：模型常把数字发成字符串，"0" 写进整数键会让重启时 TOML
    // 反序列化失败、服务起不来）——键已存在时按既有值的 TOML 类型强制转换，
    // 转不动（如把 "abc" 塞给整数键）直接报错拒绝写，不让坏类型落盘。
    let value = coerce_to_existing(&doc, key, value)?;
    // JSON 值 → TOML 值（字符串/布尔/整数/浮点/数组；null 视为删除该键）
    let item = match &value {
        serde_json::Value::Null => toml_edit::Item::None,
        serde_json::Value::String(s) => toml_edit::value(s.clone()),
        serde_json::Value::Bool(b) => toml_edit::value(b.clone()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml_edit::value(i)
            } else {
                toml_edit::value(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::Array(arr) => {
            let mut a = toml_edit::Array::new();
            for v in arr {
                match v {
                    serde_json::Value::String(s) => a.push(s.clone()),
                    serde_json::Value::Number(n) if n.is_i64() => a.push(n.as_i64().unwrap_or(0)),
                    serde_json::Value::Number(n) => a.push(n.as_f64().unwrap_or(0.0)),
                    serde_json::Value::Bool(b) => a.push(*b),
                    _ => return Err("数组里只支持字符串/数字/布尔".into()),
                }
            }
            toml_edit::value(a)
        }
        serde_json::Value::Object(_) => {
            return Err("set_config 只支持标量与一维数组值（结构体键请逐键改）".into());
        }
    };
    let mut cur = doc.as_table_mut();
    let segs: Vec<&str> = key.split('.').collect();
    for (i, seg) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        if last {
            if matches!(value, serde_json::Value::Null) {
                cur.remove(seg);
            } else {
                cur[seg] = item.clone();
            }
        } else {
            if !cur.contains_key(seg) {
                cur[seg] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            cur = cur[seg]
                .as_table_mut()
                .ok_or_else(|| format!("config.toml 的 {seg} 不是小节，无法往下钻"))?;
        }
    }
    crate::fsx::write_atomic_str(path, &doc.to_string())
        .map_err(|e| format!("写 config.toml 失败: {e}"))?;
    Ok(format!("{key} 已写入 config.toml（重启服务后生效）"))
}

/// 按 config.toml 里既有键的 TOML 类型把新值对齐（键不存在时原样返回）。
/// 实锤动机：模型发 `"0"` 给整数键 max_steps，写出的 `max_steps = "0"` 会让重启时
/// TOML 反序列化失败、服务起不来——字符串数字/布尔在写前翻回目标标量类型，
/// 实在对不上就报错拒写。
fn coerce_to_existing(
    doc: &toml_edit::DocumentMut,
    key: &str,
    value: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut cur: &toml_edit::Table = doc.as_table();
    let segs: Vec<&str> = key.split('.').collect();
    for (i, seg) in segs.iter().enumerate() {
        let Some(item) = cur.get(*seg) else {
            return Ok(value.clone());
        };
        if i + 1 == segs.len() {
            let Some(v) = item.as_value() else { return Ok(value.clone()) };
            let bad = || {
                Err(format!(
                    "config.toml 的 {key} 是 {}，不能写成 {}（类型对不上会让重启失败）",
                    if v.is_integer() { "整数" } else if v.is_float() { "浮点数" } else if v.is_bool() { "布尔" } else { "字符串" },
                    value
                ))
            };
            return match (value, v.is_str(), v.is_integer(), v.is_float(), v.is_bool()) {
                (serde_json::Value::String(s), false, true, _, _) => serde_json::from_str::<i64>(s)
                    .map(serde_json::Value::from)
                    .or_else(|_| bad()),
                (serde_json::Value::String(s), false, _, true, _) => serde_json::from_str::<f64>(s)
                    .map(serde_json::Value::from)
                    .or_else(|_| bad()),
                (serde_json::Value::String(s), false, _, _, true) => match s.trim() {
                    "true" => Ok(serde_json::Value::Bool(true)),
                    "false" => Ok(serde_json::Value::Bool(false)),
                    _ => bad(),
                },
                (serde_json::Value::Number(n), true, _, _, _) => Ok(serde_json::json!(n.to_string())),
                (serde_json::Value::Bool(b), true, _, _, _) => Ok(serde_json::json!(b.to_string())),
                // 2026-10-08 修：下面几类原先落 `_ => Ok(clone)` 原样落盘——
                // ① 浮点写整数键：TOML 写成 `max_steps = 100.0`，serde 的 u32 字段
                //    遇 visit_f64 直接 invalid type → config::load 失败 → **服务重启
                //    起不来**，且写时零告警（str→int 有专门防护并注释「真机实锤」，
                //    浮点/布尔/数组是同款漏洞的漏网口子）
                // ② 布尔写数值键：同理炸启动
                // ③ 数组元素类型不校验：一维数组里塞错类型元素，同样起不来
                (serde_json::Value::Number(n), _, true, _, _) => {
                    // 整数键：整数原样；浮点必须是无小数部分的（100.0 → 100），否则拒
                    n.as_i64()
                        .map(serde_json::Value::from)
                        .or_else(|| {
                            n.as_f64()
                                .filter(|f| f.fract() == 0.0)
                                .map(|f| serde_json::json!(f as i64))
                        })
                        .ok_or_else(|| {
                            format!(
                                "config.toml 的 {key} 是整数，不能写成 {n}（小数部分会被丢掉，且会让重启失败）"
                            )
                        })
                }
                (serde_json::Value::Number(_), _, _, true, _) => Ok(value.clone()), // 整数写浮点键：serde 的 f64 收 visit_i64，安全
                (serde_json::Value::Bool(_), _, true, _, _) | (serde_json::Value::Bool(_), _, _, true, _) => {
                    bad()
                }
                (serde_json::Value::Array(new), _, _, _, _) => {
                    // 一维数组：元素类型逐個对（str→str/num→num/bool→bool）
                    let Some(old) = v.as_array() else { return Ok(value.clone()) };
                    // toml_edit::Value 与 serde_json::Value 的判定 API 名字不同，分别取
                    let toml_kind = |x: &toml_edit::Value| {
                        if x.is_str() {
                            "s"
                        } else if x.is_bool() {
                            "b"
                        } else if x.is_integer() || x.is_float() {
                            "n"
                        } else {
                            "?"
                        }
                    };
                    let json_kind = |x: &serde_json::Value| {
                        if x.is_string() {
                            "s"
                        } else if x.is_boolean() {
                            "b"
                        } else if x.is_number() {
                            "n"
                        } else {
                            "?"
                        }
                    };
                    let old_kind = old.iter().next().map(toml_kind).unwrap_or("?");
                    if new.iter().any(|x| json_kind(x) != old_kind) {
                        return Err(format!(
                            "config.toml 的 {key} 是{old_kind}数组，不能写成 {value}（元素类型对不上会让重启失败）"
                        ));
                    }
                    Ok(value.clone())
                }
                _ => Ok(value.clone()),
            };
        }
        cur = item.as_table().ok_or_else(|| format!("config.toml 的 {} 不是小节", seg))?;
    }
    Ok(value.clone())
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

#[cfg(test)]
mod set_config_tests {
    use super::*;

    #[test]
    fn set_config_coerces_to_existing_type_and_keeps_comments() {
        let dir = std::env::temp_dir().join(format!("agent_cfg_coerce_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "[agent]\n# 步数上限注释要保住\nmax_steps = 0\n").unwrap();
        // 模型发字符串 "200" 给整数键 → 应对齐为整数落盘（真机实锤的坑）
        set_config_key_at(&p, "agent.max_steps", &serde_json::json!("200")).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("max_steps = 200"), "应写为整数: {text}");
        assert!(text.contains("# 步数上限注释要保住"), "注释应保留: {text}");
        // 数字发给字符串键 → 翻成字符串
        std::fs::write(&p, "[server]\nhost = \"127.0.0.1\"\n").unwrap();
        set_config_key_at(&p, "server.host", &serde_json::json!(127999)).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("host = \"127999\""), "{text}");
        // 类型实在对不上 → 拒写
        std::fs::write(&p, "[agent]\nmax_steps = 0\n").unwrap();
        let err = set_config_key_at(&p, "agent.max_steps", &serde_json::json!("abc")).unwrap_err();
        assert!(err.contains("类型对不上"), "{err}");
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("max_steps = 0"), "拒写后文件应保持原样: {text}");
        // 键不存在 → 按 JSON 原类型新建
        set_config_key_at(&p, "agent.brand_new", &serde_json::json!("text")).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("brand_new = \"text\""), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 2026-10-08 修：浮点/布尔/数组元素类型原先落 `_ => Ok(clone)` 原样落盘——
    /// `max_steps = 100.0` 让 serde 的 u32 字段解析失败，**服务重启起不来**，
    /// 且写时零告警（str→int 有防护，这几个是同款漏洞的漏网口子）。
    #[test]
    fn set_config_rejects_type_mismatches_that_break_next_boot() {
        let dir = std::env::temp_dir().join(format!("agent_cfg_coerce2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "[agent]\nmax_steps = 0\n").unwrap();
        // 浮点写整数键：无小数部分的收成整数（100.0 → 100）
        set_config_key_at(&p, "agent.max_steps", &serde_json::json!(100.0)).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("max_steps = 100"), "无小数浮点应收成整数: {text}");
        // 带小数的浮点 → 拒写（写下去重启必炸）
        let err = set_config_key_at(&p, "agent.max_steps", &serde_json::json!(100.5)).unwrap_err();
        assert!(err.contains("整数"), "应说明键是整数: {err}");
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("max_steps = 100"), "拒写后保持原值: {text}");
        // 布尔写整数键 → 拒
        let err = set_config_key_at(&p, "agent.max_steps", &serde_json::json!(true)).unwrap_err();
        assert!(err.contains("对不上"), "{err}");
        // 数组元素类型不符 → 拒
        std::fs::write(&p, "[gateway]\nfallback_configs = [\"a\", \"b\"]\n").unwrap();
        let err = set_config_key_at(&p, "gateway.fallback_configs", &serde_json::json!([1, 2])).unwrap_err();
        assert!(err.contains("数组"), "应点名数组元素类型: {err}");
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("fallback_configs = [\"a\", \"b\"]"), "拒写后保持原值: {text}");
        // 同类型数组照旧放行
        set_config_key_at(&p, "gateway.fallback_configs", &serde_json::json!(["x"])).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("fallback_configs = [\"x\"]"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
