//! 用户设置持久化 —— settings.json（工程根目录）。
//!
//! - provider_id / base_url（覆盖预设）/ model / models：明文存 settings.json，重启保留。
//! - api_key：用 Windows DPAPI（CryptProtectData，用户账户绑定）加密成 base64，
//!   按 provider_id 存进 `keys` 映射（schema v2）。本机同账户才能解密；文件里不出现明文 key。
//!   （同账户进程理论上能解——DPAPI 的语义就是"本用户的数据保险箱"，与浏览器保存密码同级）

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
const DEFAULT_PROFILE_ID: &str = "profile-default";

/// 配置方案：一套「供应商 + Base URL + 模型 + 已勾选模型」组合，可保存多套快速切换。
/// key 不随方案存——仍按 provider_id 存（切换方案后各家 key 都在）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub provider_id: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub provider_id: String,
    /// 用户自定义 base_url；空 = 用预设默认
    pub base_url: String,
    pub model: String,
    /// 用户从弹窗勾选保存的模型列表（多选）
    #[serde(default)]
    pub models: Vec<String>,
    /// schema v2：provider_id → DPAPI 加密后的 key（base64）
    #[serde(default)]
    pub keys: BTreeMap<String, String>,
    /// v1 遗留：单把 key 密文，load 时自动迁移进 keys 后置空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_cipher: Option<String>,
    /// 配置方案列表
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// 当前生效方案 id；None = 游离配置（手改字段后与方案脱钩）
    #[serde(default)]
    pub active_profile_id: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            provider_id: "deepseek".to_string(),
            base_url: String::new(),
            model: "deepseek-chat".to_string(),
            models: Vec::new(),
            keys: BTreeMap::new(),
            key_cipher: None,
            profiles: Vec::new(),
            active_profile_id: None,
        }
    }
}

impl Settings {
    /// 读取；不存在时用 config.toml 的 gateway 节做种子（保留施工单 001 的换渠道能力）。
    /// v1 的 key_cipher 自动迁移到 keys[provider_id]，不丢已存 key。
    pub fn load(seed_base_url: &str, seed_model: &str) -> Self {
        let path = Self::path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(mut s) = serde_json::from_str::<Settings>(&text) {
                // v1 → v2 迁移：key_cipher 移进 keys[provider_id]，并落盘
                if s.keys.is_empty() {
                    if let Some(cipher) = s.key_cipher.take() {
                        s.keys.insert(s.provider_id.clone(), cipher);
                        let _ = s.save();
                    }
                }
                // v2 → v2.1 迁移：把当前配置自动登记为「默认配置」方案（不改任何字段）
                if s.profiles.is_empty() && !s.provider_id.trim().is_empty() {
                    s.profiles.push(Profile {
                        id: DEFAULT_PROFILE_ID.into(),
                        name: "默认配置".into(),
                        provider_id: s.provider_id.clone(),
                        base_url: s.base_url.clone(),
                        model: s.model.clone(),
                        models: s.models.clone(),
                    });
                    s.active_profile_id = Some(DEFAULT_PROFILE_ID.into());
                    let _ = s.save();
                }
                return s;
            }
            eprintln!("[settings] 解析失败，重建默认设置");
        }
        let mut s = Self::default();
        s.base_url = seed_base_url.to_string();
        s.model = seed_model.to_string();
        if s.model.is_empty() {
            s.model = "deepseek-chat".to_string();
        }
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

    /// 生效的模型：已选 > 弹窗保存列表的第一个 > 预设默认
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

    /// 解密取出指定 provider 的 key（仅内存）。
    pub fn get_key(&self, provider_id: &str) -> Option<String> {
        let cipher = self.keys.get(provider_id)?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(cipher).ok()?;
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

    /// DPAPI 加密并按 provider 保存。
    pub fn set_key(&mut self, provider_id: &str, key: &str) -> anyhow::Result<()> {
        let data = key.as_bytes();
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let entropy = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
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
        self.keys
            .insert(provider_id.to_string(), base64::engine::general_purpose::STANDARD.encode(cipher));
        self.save()
    }

    pub fn clear_key(&mut self, provider_id: &str) -> anyhow::Result<()> {
        self.keys.remove(provider_id);
        self.save()
    }
}
