//! 用户设置持久化 —— settings.json（工程根目录）
//!
//! 设计（配置即生效）：顶层 provider_id/base_url/model/models 是**当前激活配置的镜像**，
//! 网关只读镜像；`configs` 是多套「供应商+Base URL+模型+已勾选模型」组合，切换即改镜像。
//! api_key：Windows DPAPI（CryptProtectData，用户账户绑定）加密成 base64，
//! **按归一化 base_url 存**（换地址=独立一格 key，互不串）。本机同账户才能解密；
//! 文件里不出现明文 key，也绝不写日志。（同账户进程理论上能解——DPAPI 的语义就是
//! "本用户的数据保险箱"，与浏览器保存密码同级）

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

/// 一套配置（名称 + 供应商 + Base URL + 模型 + 勾选的模型清单）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub id: String,
    /// 用户起的名字；空则自动补「配置 N」
    #[serde(default)]
    pub name: String,
    pub provider_id: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// 以下为当前激活配置的镜像（网关直接读这些）
    pub provider_id: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub models: Vec<String>,
    /// 归一化 base_url → DPAPI 加密后的 key（base64）
    #[serde(default)]
    pub keys: BTreeMap<String, String>,
    /// 旧版遗留（v1 单 key 密文 / v2 provider 作用域），load 时自动迁移后置空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_cipher: Option<String>,
    /// 配置列表（v2.1 的 profiles 字段自动读入）
    #[serde(default, alias = "profiles")]
    pub configs: Vec<Config>,
    /// 当前激活配置 id（兼容旧 active_profile_id 字段）
    #[serde(default, alias = "active_profile_id")]
    pub active_config_id: Option<String>,
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
            configs: Vec::new(),
            active_config_id: None,
        }
    }
}

/// key 存储作用域归一化：小写、去首尾空白与尾部 '/'。
pub fn normalize_base(url: &str) -> String {
    url.trim().trim_end_matches('/').to_lowercase()
}

impl Settings {
    /// 读取；不存在时用 config.toml 的 gateway 节做种子（保留施工单 001 的换渠道能力）。
    /// 逐级迁移：v1 key_cipher → provider 作用域 → base_url 作用域 → configs 列表。
    pub fn load(seed_base_url: &str, seed_model: &str) -> Self {
        let path = Self::path();
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(mut s) = serde_json::from_str::<Settings>(&text) {
                // v1 → v2：key_cipher 挂到当前 provider 作用域
                if s.keys.is_empty() {
                    if let Some(cipher) = s.key_cipher.take() {
                        s.keys.insert(s.provider_id.clone(), cipher);
                    }
                }
                // v2 → v2.2：provider 作用域 key 归位到 base_url 作用域
                s.migrate_provider_keys_to_urls();
                // v2.2 → v2.3：确保存在激活配置（没有就用镜像造一套）
                s.ensure_active_config();
                if s.key_cipher.is_some() {
                    s.key_cipher = None;
                }
                let _ = s.save();
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

    /// 把遗留的"按 provider 存"的 key 迁移到"按 base_url 存"。
    /// 目标地址优先级：同 provider 配置的 base_url > 预设默认 > 当前生效地址。
    fn migrate_provider_keys_to_urls(&mut self) {
        let mut moved = Vec::new();
        let mut added = Vec::new();
        for (slot, cipher) in &self.keys {
            if slot.contains("://") {
                continue; // 已是 url 作用域
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
                .or_else(|| {
                    if slot == &self.provider_id {
                        Some(self.effective_base_url())
                    } else {
                        None
                    }
                })
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

    /// 确保 configs 非空且有激活项；镜像与激活配置对齐；缺名的补「配置 N」。
    pub fn ensure_active_config(&mut self) {
        if self.configs.is_empty() && !self.provider_id.trim().is_empty() {
            self.configs.push(Config {
                id: new_config_id(),
                name: String::new(),
                provider_id: self.provider_id.clone(),
                base_url: self.base_url.clone(),
                model: self.model.clone(),
                models: self.models.clone(),
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

    /// 当前激活配置（镜像即其值）。
    pub fn active(&self) -> Option<&Config> {
        let id = self.active_config_id.as_deref()?;
        self.configs.iter().find(|c| c.id == id)
    }

    /// 把镜像字段写回激活配置（无激活则新建）。
    pub fn sync_config_from_mirror(&mut self) {
        self.ensure_active_config();
        let snap = (
            self.provider_id.clone(),
            self.base_url.clone(),
            self.model.clone(),
            self.models.clone(),
        );
        if let Some(c) = self.active_mut_() {
            c.provider_id = snap.0;
            c.base_url = snap.1;
            c.model = snap.2;
            c.models = snap.3;
        }
    }

    fn active_mut_(&mut self) -> Option<&mut Config> {
        let id = self.active_config_id.clone()?;
        self.configs.iter_mut().find(|c| c.id == id)
    }

    /// 切换激活配置：镜像换成该配置的值。返回是否成功。
    pub fn activate(&mut self, id: &str) -> bool {
        let Some(c) = self.configs.iter().find(|c| c.id == id).cloned() else { return false };
        self.provider_id = c.provider_id;
        self.base_url = c.base_url;
        self.model = c.model;
        self.models = c.models;
        self.active_config_id = Some(id.to_string());
        true
    }

    /// 新建一套配置（复制当前镜像值作起点）并激活。
    pub fn new_config(&mut self) -> String {
        let id = new_config_id();
        let name = format!("配置 {}", self.next_name_seq());
        self.configs.push(Config {
            id: id.clone(),
            name,
            provider_id: self.provider_id.clone(),
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            models: self.models.clone(),
        });
        self.active_config_id = Some(id.clone());
        id
    }

    /// 删除一套配置；删的是激活项则自动切到剩余第一套（没有则保留镜像）。
    pub fn delete_config(&mut self, id: &str) -> bool {
        let before = self.configs.len();
        self.configs.retain(|c| c.id != id);
        if self.configs.len() == before {
            return false;
        }
        if self.active_config_id.as_deref() == Some(id) {
            self.active_config_id = None;
            self.ensure_active_config();
            if let Some(c) = self.active() {
                let (p, b, m, ms) =
                    (c.provider_id.clone(), c.base_url.clone(), c.model.clone(), c.models.clone());
                self.provider_id = p;
                self.base_url = b;
                self.model = m;
                self.models = ms;
            }
        }
        true
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

    /// 按归一化 base_url 解密取出 key（仅内存）。
    pub fn get_key(&self, base_url: &str) -> Option<String> {
        let cipher = self.keys.get(&normalize_base(base_url))?;
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

    /// DPAPI 加密并按 base_url 保存。
    pub fn set_key(&mut self, base_url: &str, key: &str) -> anyhow::Result<()> {
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
            .insert(normalize_base(base_url), base64::engine::general_purpose::STANDARD.encode(cipher));
        self.save()
    }

    pub fn clear_key(&mut self, base_url: &str) -> anyhow::Result<()> {
        self.keys.remove(&normalize_base(base_url));
        self.save()
    }
}

fn new_config_id() -> String {
    format!("cfg-{}", uuid::Uuid::new_v4().simple())
}
