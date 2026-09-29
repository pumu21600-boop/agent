//! 第三方插件来源验签（施工单 030，P21 红线 3）—— **pin digest 模型**。
//!
//! 诉求（ROADMAP 红线 3）：第三方插件要能验明来源再跑。真实密码学签名体系（CA / 私钥
//! 分发 / 吊销列表）工程面远超「窄而刚」单阶段容量，且 manifest 里自称的签名字串在无密钥
//! 前提下不构成任何保证——故采用 **SSH 主机密钥同款思路：pin digest**。
//!
//! - canonical digest = sha256(排序后的 `相对路径\n` + sha256(文件内容) + `\n` 序列)，
//!   目录形态与裸文件形态各一把尺子（`digest_dir` / `digest_file`）；
//! - `user` 来源插件（工作区/应用根自装的 plugins、plugins/ 目录与裸文件）**必须先被用户
//!   pin 才能加载**：未 pin → 拒载 + 显式「未受信任」；内容被改 → digest 不符 → 拒载 +
//!   「内容已变，需重新信任」；
//! - pin 表落 settings.json `trusted_plugins{key: digest}`（key = `plugins:<name>` /
//!   `plugins:<name>`），内存态与落盘双向同步（install 在启动/设置变更时调）；
//! - **豁免两类**：`built_in`（随包内置，位置即信任）、`model_generated`（draft 隔离区，
//!   权限已清空 + 内存态）；draft **转正自动 pin**（转正是用户显式动作，即信任决定）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::observability::{try_event, EventType};

/// digest 计算域前缀（换算法必须换前缀，历史 pin 全部失效重pin——与坑 15 同规）。
const DOMAIN: &[u8] = b"zcode-plugin-digest-v1\n";
/// 目录递归深度上限（防病态目录树；插件不该有深层嵌套）。
const MAX_DEPTH: usize = 6;

/// pin 键：`plugins:<name>`（P28 目录合并后唯一 kind；旧 `ui-ext:<name>` 键由
/// settings 迁移改写，这里不再认第二种）。
pub fn key_of(kind: &str, name: &str) -> String {
    format!("{kind}:{name}")
}

/// 目录 canonical digest（递归收集普通文件，按相对路径排序后逐文件哈希）。
pub fn digest_dir(dir: &Path) -> Result<String, String> {
    if !dir.is_dir() {
        return Err(format!("不是目录: {}", dir.display()));
    }
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    collect_files(dir, dir, 0, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = Sha256::new();
    h.update(DOMAIN);
    for (rel, p) in &files {
        let bytes = std::fs::read(p).map_err(|e| format!("读 {} 失败: {e}", p.display()))?;
        h.update(rel.as_bytes());
        h.update(b"\n");
        h.update(Sha256::digest(&bytes));
        h.update(b"\n");
    }
    Ok(hex::encode(h.finalize()))
}

/// 单文件 digest（plugins/ 下裸 .wasm/.wat 旧形态）。
pub fn digest_file(file: &Path) -> Result<String, String> {
    let bytes = std::fs::read(file).map_err(|e| format!("读 {} 失败: {e}", file.display()))?;
    let mut h = Sha256::new();
    h.update(DOMAIN);
    h.update(b"file\n");
    h.update(Sha256::digest(&bytes));
    Ok(hex::encode(h.finalize()))
}

fn collect_files(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("目录层级过深（插件不该有深层嵌套）".into());
    }
    let rd = std::fs::read_dir(dir).map_err(|e| format!("读目录 {} 失败: {e}", dir.display()))?;
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            collect_files(root, &p, depth + 1, out)?;
        } else if ft.is_file() {
            let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            out.push((rel, p));
        }
    }
    Ok(())
}

/// 验签状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TrustState {
    /// 内置/模型自产：不验
    Exempt,
    /// 已 pin 且 digest 一致
    Trusted,
    /// 从未被 pin
    Untrusted { digest: String },
    /// pin 过但内容变了
    Changed { pinned: String, current: String },
}

impl TrustState {
    pub fn trusted(&self) -> bool {
        matches!(self, TrustState::Exempt | TrustState::Trusted)
    }
}

static PINNED: OnceLock<RwLock<BTreeMap<String, String>>> = OnceLock::new();

fn map() -> &'static RwLock<BTreeMap<String, String>> {
    PINNED.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// 装入 pin 表（启动 / settings 变更 / 端点 pin 后调；全量同步，测试可直接替换）。
pub fn install(pins: &BTreeMap<String, String>) {
    let lock = map();
    if let Ok(mut m) = lock.write() {
        *m = pins.clone();
    }
}

pub fn pinned_digest(key: &str) -> Option<String> {
    map().read().ok().and_then(|m| m.get(key).cloned())
}

/// 判定一个插件的验签状态。`origin` 为契约来源三值之一。
pub fn state(key: &str, origin: &str, current_digest: &str) -> TrustState {
    if origin != crate::plugin::contract::ORIGIN_USER {
        return TrustState::Exempt; // built_in / model_generated 不验
    }
    match pinned_digest(key) {
        None => TrustState::Untrusted { digest: current_digest.to_string() },
        Some(pinned) if pinned == current_digest => TrustState::Trusted,
        Some(pinned) => TrustState::Changed { pinned, current: current_digest.to_string() },
    }
}

/// 拒载理由（人话，指路操作）。
pub fn reject_reason(state: &TrustState) -> String {
    match state {
        TrustState::Untrusted { .. } => {
            "未受信任的第三方插件：来源验签要求先在设置里点「信任」pin 它的内容指纹，之后才会加载".into()
        }
        TrustState::Changed { .. } => {
            "插件内容已变（与信任时的内容指纹不符）：如是本人改动，在设置里重新点「信任」；否则停止使用".into()
        }
        _ => String::new(),
    }
}

/// pin 键 → 插件位置（P28 合并后单一 kind：目录形态工作区优先，裸 .wat/.wasm 旧形态兜底）。
pub fn resolve(key: &str) -> Option<PathBuf> {
    let (kind, name) = key.split_once(':')?;
    if kind != crate::plugin::ui::DIR || name.is_empty() || name.contains(['/', '\\', ':']) {
        return None;
    }
    let ws = crate::tools::workspace_root().join(crate::plugin::ui::DIR).join(name);
    if ws.is_dir() {
        return Some(ws);
    }
    let app = crate::config::app_root().join(crate::plugin::ui::DIR).join(name);
    if app.is_dir() {
        return Some(app);
    }
    let root = crate::plugin::wasm::plugins_dir()?;
    for ext in ["wat", "wasm"] {
        let f = root.join(format!("{name}.{ext}"));
        if f.is_file() {
            return Some(f);
        }
    }
    None
}

/// 按位置算 digest（目录走目录尺，文件走文件尺）。
pub fn digest_of_path(path: &Path) -> Result<String, String> {
    if path.is_dir() {
        digest_dir(path)
    } else {
        digest_file(path)
    }
}

/// 转正自动 pin：从 promote 落盘的 manifest 路径推导插件目录并算 digest。
/// 返回 `(pin 键, digest)` 列表（调用方写 settings + install）。
pub fn pin_from_manifest_paths(paths: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for p in paths {
        let path = Path::new(p);
        if path.file_name().and_then(|s| s.to_str()) != Some("manifest.json") {
            continue;
        }
        let Some(dir) = path.parent() else { continue };
        // P28 目录合并：plugins/<name>/manifest.json → 唯一 kind=plugins
        let Some(name) = dir.file_name().and_then(|s| s.to_str()) else { continue };
        if let Ok(d) = digest_dir(dir) {
            out.push((key_of("plugins", name), d));
        }
    }
    out
}

/// 验签失败审计（拒载方调用：UI 清单与工具注册表两条路径都经它）。
pub fn audit_blocked(key: &str, state: &TrustState, where_meta: &str) {
    try_event(
        EventType::State,
        serde_json::json!({ "kind": "plugin.trust_blocked", "key": key, "state": state, "via": where_meta }),
    );
}

/// pin / unpin 审计（端点调用）。
pub fn audit_pinned(key: &str, digest: &str, by: &str) {
    try_event(
        EventType::State,
        serde_json::json!({ "kind": "plugin.trust_pinned", "key": key, "digest": digest, "by": by }),
    );
}

/// 测试专用：pin 表是全局静态，跨文件测试共用一把锁（沾 pin 表的测试先锁它，
/// 与 plugin::REGISTRY_TEST_LOCK 同规）。
#[cfg(test)]
pub static TRUST_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-trust-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn digest_dir_is_stable_and_content_sensitive() {
        let root = tmp("stable");
        let d = root.join("board");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("manifest.json"), r#"{"title":"看板"}"#).unwrap();
        std::fs::write(d.join("index.html"), "<html></html>").unwrap();
        std::fs::write(d.join("sub").join("app.js"), "console.log(1)").unwrap();
        let a = digest_dir(&d).unwrap();
        let b = digest_dir(&d).unwrap();
        assert_eq!(a, b, "同样内容 → 同样 digest（pin 才稳定）");
        // 改一个字节 → digest 必须变（内容被改能检出）
        std::fs::write(d.join("sub").join("app.js"), "console.log(2)").unwrap();
        let c = digest_dir(&d).unwrap();
        assert_ne!(a, c, "内容变化必须反映到 digest");
        // 加一个文件也变
        std::fs::write(d.join("evil.js"), "x").unwrap();
        assert_ne!(c, digest_dir(&d).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn state_matrix_pin_unpin_change() {
        install(&BTreeMap::new());
        let key = "plugins:board";
        // 未 pin → Untrusted
        assert_eq!(state(key, crate::plugin::contract::ORIGIN_USER, "abc"), TrustState::Untrusted { digest: "abc".into() });
        assert!(!state(key, crate::plugin::contract::ORIGIN_USER, "abc").trusted());
        assert!(reject_reason(&state(key, crate::plugin::contract::ORIGIN_USER, "abc")).contains("信任"));
        // pin 后一致 → Trusted
        let mut pins = BTreeMap::new();
        pins.insert(key.to_string(), "abc".to_string());
        install(&pins);
        assert_eq!(state(key, crate::plugin::contract::ORIGIN_USER, "abc"), TrustState::Trusted);
        assert!(state(key, crate::plugin::contract::ORIGIN_USER, "abc").trusted());
        // 内容变了 → Changed，理由要提示「重新点信任」
        let changed = state(key, crate::plugin::contract::ORIGIN_USER, "xyz");
        assert!(matches!(changed, TrustState::Changed { .. }));
        assert!(reject_reason(&changed).contains("重新点"), "{:?}", reject_reason(&changed));
        // 内置与模型自产豁免（不验）
        assert_eq!(state(key, crate::plugin::contract::ORIGIN_BUILT_IN, "whatever"), TrustState::Exempt);
        assert_eq!(state(key, crate::plugin::contract::ORIGIN_MODEL, "whatever"), TrustState::Exempt);
        // 清空 pin 表 → 回到 Untrusted
        install(&BTreeMap::new());
        assert!(matches!(state(key, crate::plugin::contract::ORIGIN_USER, "abc"), TrustState::Untrusted { .. }));
    }

    #[test]
    fn pin_from_manifest_paths_single_kind() {
        let root = tmp("promote");
        // P28 合并后一个形态：plugins/<name>/manifest.json → 唯一 kind=plugins
        let dir = root.join("plugins").join("calc");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), "{}").unwrap();
        std::fs::write(dir.join("calc.wat"), "(module)").unwrap();
        let pins = pin_from_manifest_paths(&[dir.join("manifest.json").display().to_string()]);
        assert_eq!(pins.len(), 1, "P28 起单一 kind，不再双表");
        assert_eq!(pins[0].0, "plugins:calc");
        assert!(!pins[0].1.is_empty(), "digest 必须非空（空 digest 无法验签）");
        // 裸文件路径不产 pin（只认 manifest.json）
        let bare = pin_from_manifest_paths(&[dir.join("calc.wat").display().to_string()]);
        assert!(bare.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn digest_file_differs_from_dir_digest() {
        let root = tmp("file");
        let f = root.join("a.wat");
        std::fs::write(&f, "(module)").unwrap();
        let fd = digest_file(&f).unwrap();
        let d = root.join("dir");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.wat"), "(module)").unwrap();
        assert_ne!(fd, digest_dir(&d).unwrap(), "两种形态不同尺子");
        assert!(digest_file(Path::new("nope.wat")).is_err(), "读不动必须报错（不静默给空 digest）");
        let _ = std::fs::remove_dir_all(&root);
    }
}
