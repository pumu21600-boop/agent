//! 用户设置持久化 —— settings.json（工程根目录）
//!
//! 架构设计（P8.5 彻底重构）：
//! 1. **单一真理源**：废除顶层冗余镜像字段与位置元组手抄；每套配置独立自治。
//! 2. **Config 为一等实体**：包含渠道信息、模型选择、DPAPI 专属密钥、采样与思考参数、模型多模态能力标记。
//! 3. **RolesSettings 角色路由**：支持主力模型、会话摘要、标题生成及多模态视觉/视频/音频解析路由。
//! 4. **DPAPI 密钥隔离**：按配置独立加密保存（CryptProtectData），不入日志、不出内存。

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, anyhow};
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

/// 模型能力标记（P8.5：支持工具调用、图片解析、视频解析、音频解析）。
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

impl ModelCapabilities {
    /// 根据模型名称智能推导默认能力
    pub fn infer_from_model_id(model_id: &str) -> Self {
        let id = model_id.trim().to_lowercase();
        let is_vision = id.contains("vl")
            || id.contains("vision")
            || id.contains("gemini")
            || id.contains("gpt-4o")
            || id.contains("claude-3")
            || id.contains("omni");
        let is_video = id.contains("video") || id.contains("gemini-2");
        let is_audio = id.contains("audio") || id.contains("realtime") || id.contains("whisper");
        Self {
            tools: true,
            vision: is_vision,
            video: is_video,
            audio: is_audio,
        }
    }
}

/// 角色路由配置（P8.5：支持主力、摘要、标题及多模态视觉/视频/音频路由，解决 config.toml 割裂）。
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
    /// 降级备用配置名列表
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<String>,
}

/// 一套配置（渠道信息 + 模型 + 本配置专属 key + 协议形态与采样参数 + 能力标记）。
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
    /// 思考/协议规范形态覆盖（8大规范全覆盖词表）：
    /// auto/none/openai/deepseek/zai/qwen/claude/moonshot/qwen-chat-template；None=auto 端点探测
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_style: Option<String>,
    /// 模型能力标记（工具调用/图片/视频/音频解析）
    #[serde(default)]
    pub capabilities: ModelCapabilities,
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

    /// DPAPI 解密本配置专属 key（仅内存，绝不序列化、绝不出日志）
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
pub struct Settings {
    /// 当前激活配置 ID（兼容旧 active_profile_id 字段）
    #[serde(default, alias = "active_profile_id")]
    pub active_config_id: Option<String>,

    /// 配置列表（按配置独立保存 provider/base_url/model/key/参数）
    #[serde(default, alias = "profiles")]
    pub configs: Vec<Config>,

    /// 角色路由映射（摘要/标题/多模态旁路/降级链）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<RolesSettings>,

    /// P8.2 工作区：agent 实际操作的项目目录（None=工程根）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,

    /// 最近用过的工作区（新→旧，最多 8 个）
    #[serde(default)]
    pub recent_workspaces: Vec<String>,

    // —— 历史迁移源（仅在反序列化旧版 settings.json 时读入，序列化时不写出） ——
    #[serde(default, skip_serializing)]
    pub keys: BTreeMap<String, String>,
    #[serde(default, skip_serializing)]
    pub key_cipher: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            active_config_id: None,
            configs: Vec::new(),
            roles: None,
            workspace: None,
            recent_workspaces: Vec::new(),
            keys: BTreeMap::new(),
            key_cipher: None,
        }
    }
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
    /// 读取；不存在时用 config.toml 的 gateway 节做种子。
    /// 逐级迁移：旧版单 key / keys 映射 -> configs 专属 key。
    pub fn load(seed_base_url: &str, seed_model: &str) -> Self {
        let path = Self::path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(mut s) = serde_json::from_str::<Settings>(&text) {
                // 若旧版完全没有 configs 数组，从顶层字段挽救一套配置
                if s.configs.is_empty() {
                    if let Ok(legacy) = serde_json::from_str::<LegacyTopLevelFields>(&text) {
                        let provider_id = legacy.provider_id.unwrap_or_else(|| "deepseek".to_string());
                        let base_url = legacy.base_url.unwrap_or_default();
                        let model = legacy.model.unwrap_or_else(|| "deepseek-chat".to_string());
                        let models = legacy.models.unwrap_or_default();
                        let cap = ModelCapabilities::infer_from_model_id(&model);
                        s.configs.push(Config {
                            id: new_config_id(),
                            name: "配置 1".to_string(),
                            provider_id,
                            base_url,
                            model,
                            models,
                            key_cipher: None,
                            context_length: legacy.context_length,
                            max_output_tokens: legacy.max_output_tokens,
                            temperature: legacy.temperature,
                            top_p: legacy.top_p,
                            thinking: legacy.thinking,
                            thinking_style: legacy.thinking_style,
                            capabilities: cap,
                        });
                    }
                }

                // v1 -> v2：key_cipher 挂到当前 provider 作用域
                if s.keys.is_empty() {
                    if let Some(cipher) = s.key_cipher.take() {
                        let pid = s.configs.first().map(|c| c.provider_id.clone()).unwrap_or_else(|| "deepseek".to_string());
                        s.keys.insert(pid, cipher);
                    }
                }
                // v2 -> v2.2：provider 作用域 key 归位到 base_url 作用域
                s.migrate_provider_keys_to_urls();
                // v2.3 -> v2.4：key 从按 base_url 共享改为按配置独占
                s.migrate_url_keys_to_configs();
                // 确保存在有效激活配置
                s.ensure_active_config();
                let _ = s.save();
                return s;
            }
            eprintln!("[settings] 解析失败，重建默认设置");
        }

        let mut s = Self::default();
        let model = if seed_model.is_empty() {
            "deepseek-chat".to_string()
        } else {
            seed_model.to_string()
        };
        let cap = ModelCapabilities::infer_from_model_id(&model);
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
        });
        s.ensure_active_config();
        let _ = s.save();
        s
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(Self::path(), json).context("写 settings.json 失败")
    }

    fn path() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("settings.json")
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

fn new_config_id() -> String {
    format!("cfg-{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_inference() {
        let c_vision = ModelCapabilities::infer_from_model_id("qwen-vl-max");
        assert!(c_vision.vision);
        assert!(c_vision.tools);

        let c_text = ModelCapabilities::infer_from_model_id("deepseek-chat");
        assert!(!c_text.vision);
        assert!(!c_text.video);
        assert!(c_text.tools);

        let c_gemini = ModelCapabilities::infer_from_model_id("gemini-2.0-flash");
        assert!(c_gemini.vision);
        assert!(c_gemini.video);
    }

    #[test]
    fn protocol_preset_inference() {
        assert_eq!(infer_protocol_preset("deepseek-v4-flash"), Some("deepseek"));
        assert_eq!(infer_protocol_preset("qwen-plus"), Some("qwen"));
        assert_eq!(infer_protocol_preset("glm-5.2"), Some("zai"));
        assert_eq!(infer_protocol_preset("gpt-5.1-codex"), Some("openai"));
        assert_eq!(infer_protocol_preset("claude-3-7-sonnet"), Some("claude"));
        assert_eq!(infer_protocol_preset("kimi-k2-thinking"), Some("moonshot"));
        assert_eq!(infer_protocol_preset("unknown-model"), None);
    }

    #[test]
    fn single_source_of_truth_and_activation() {
        let mut s = Settings::default();
        s.configs.push(Config {
            id: "cfg-1".to_string(),
            name: "商汤".to_string(),
            provider_id: "deepseek".to_string(),
            base_url: "https://token.sensenova.cn/v1".to_string(),
            model: "deepseek-v4-flash".to_string(),
            models: vec!["deepseek-v4-flash".to_string()],
            key_cipher: None,
            context_length: Some(100000),
            max_output_tokens: Some(8192),
            temperature: Some(0.7),
            top_p: Some(0.9),
            thinking: Some("on".to_string()),
            thinking_style: Some("deepseek".to_string()),
            capabilities: ModelCapabilities::default(),
        });
        s.configs.push(Config {
            id: "cfg-2".to_string(),
            name: "千问".to_string(),
            provider_id: "qwen".to_string(),
            base_url: String::new(),
            model: "qwen-plus".to_string(),
            models: vec!["qwen-plus".to_string()],
            key_cipher: None,
            context_length: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            thinking: Some("off".to_string()),
            thinking_style: Some("qwen".to_string()),
            capabilities: ModelCapabilities::default(),
        });
        s.active_config_id = Some("cfg-1".to_string());

        // 测试单一真理源：修改 active_mut 即可生效，无需手写 mirror 同步
        if let Some(c) = s.active_mut() {
            c.temperature = Some(0.2);
            c.thinking = Some("off".to_string());
        }
        assert_eq!(s.active().unwrap().temperature, Some(0.2));
        assert_eq!(s.active().unwrap().thinking.as_deref(), Some("off"));

        // 测试切换激活配置
        assert!(s.activate("cfg-2"));
        assert_eq!(s.active().unwrap().name, "千问");
        assert_eq!(s.active().unwrap().thinking_style.as_deref(), Some("qwen"));
        assert_eq!(s.effective_model(), "qwen-plus");
    }

    #[test]
    fn backward_compat_legacy_json_deserialization() {
        // 模拟 v2.4 现存真实 settings.json 格式（带顶层镜像字段和旧 keys 映射）
        let legacy_json = r#"{
            "provider_id": "deepseek",
            "base_url": "https://token.sensenova.cn/v1",
            "model": "deepseek-v4-flash",
            "models": ["deepseek-v4-flash", "glm-5.2"],
            "keys": {
                "https://token.sensenova.cn/v1": "dummy_cipher"
            },
            "configs": [
                {
                    "id": "cfg-sensenova",
                    "name": "商汤",
                    "provider_id": "deepseek",
                    "base_url": "https://token.sensenova.cn/v1",
                    "model": "deepseek-v4-flash",
                    "models": ["deepseek-v4-flash", "glm-5.2"]
                }
            ],
            "active_config_id": "cfg-sensenova",
            "workspace": "D:\\test\\ws"
        }"#;

        let mut s = serde_json::from_str::<Settings>(legacy_json).expect("旧格式反序列化必须成功");
        assert_eq!(s.configs.len(), 1);
        assert_eq!(s.active().unwrap().name, "商汤");
        // 验证 URL key 迁移至 config
        s.migrate_url_keys_to_configs();
        assert_eq!(s.configs[0].key_cipher.as_deref(), Some("dummy_cipher"));
        assert_eq!(s.workspace.as_deref(), Some("D:\\test\\ws"));

        // 验证持久化输出已自动剔除 keys 和顶层镜像
        let serialized = serde_json::to_string(&s).unwrap();
        assert!(!serialized.contains("\"keys\""));
        let val: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert!(val.get("provider_id").is_none(), "顶层镜像 provider_id 不再序列化");
        assert!(val.get("base_url").is_none(), "顶层镜像 base_url 不再序列化");
    }
}
