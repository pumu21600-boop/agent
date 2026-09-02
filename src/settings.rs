//! 用户设置持久化 —— settings.json（工程根目录）。
//!
//! - provider_id / base_url（覆盖预设）/ model：明文存 settings.json，重启保留。
//! - api_key：用 Windows DPAPI（CryptProtectData，用户账户绑定）加密成 base64 存进
//!   settings.json 的 key_cipher 字段。本机同账户才能解密；文件里不出现明文 key。
//!   （同账户进程理论上能解——DPAPI 的语义就是"本用户的数据保险箱"，与浏览器保存密码同级）

use std::path::Path;

use anyhow::{Context, anyhow};
use base64::Engine;
use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};

use crate::providers;

const ENTROPY: &[u8] = b"agent-gateway-settings-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub provider_id: String,
    /// 用户自定义 base_url；空 = 用预设默认
    pub base_url: String,
    pub model: String,
    /// 用户从弹窗勾选保存的模型列表（多选）
    #[serde(default)]
    pub models: Vec<String>,
    /// DPAPI 加密后的 key（base64）；空 = 未设置
    #[serde(default)]
    pub key_cipher: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            provider_id: "deepseek".to_string(),
            base_url: String::new(),
            model: "deepseek-chat".to_string(),
            models: Vec::new(),
            key_cipher: None,
        }
    }
}

impl Settings {
    /// 读取；不存在时用 config.toml 的 gateway 节做种子（保留施工单 001 的换渠道能力）。
    pub fn load(seed_base_url: &str, seed_model: &str) -> Self {
        let path = Self::path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(s) = serde_json::from_str::<Settings>(&text) {
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
                .map(|p| p.default_base_url.to_string())
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
                .map(|p| p.default_model.to_string())
                .unwrap_or_default()
        }
    }

    /// 解密取出 key（仅内存）。
    pub fn get_key(&self) -> Option<String> {
        let cipher = self.key_cipher.as_ref()?;
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

    /// DPAPI 加密 key 并保存。
    pub fn set_key(&mut self, key: &str) -> anyhow::Result<()> {
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
        self.key_cipher =
            Some(base64::engine::general_purpose::STANDARD.encode(cipher));
        self.save()
    }

    pub fn clear_key(&mut self) -> anyhow::Result<()> {
        self.key_cipher = None;
        self.save()
    }
}
