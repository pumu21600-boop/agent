//! 存储一门（施工单 027）—— 所有落盘的统一入口。
//!
//! 优化报告 6.5 说的「物理隔离」从本模块开始成立：内核保留区（会话 / settings）的落盘
//! 只认这里（原子写 + `storage.write` 审计），插件永远拿不到 raw 句柄，只有自己命名
//! 空间内的结构化读写（`plugin_read / plugin_write / plugin_delete`）。
//!
//! 一句话纪律：**插件可以写自己名下的格子，碰不到昨天存下来的东西。**
//!
//! - 插件数据落在 `<app_root>/plugin_data/<插件名>/<key>.json`：键名白名单
//!   （字母数字与 `- _ .`，1–64 字节，无路径分隔符 ⇒ 无穿越可能），值为 JSON
//!   （序列化后 ≤ 64KB）；
//! - 内核保留区（sessions / settings.json / memory / checkpoints）不在任何插件可见的
//!   写路径上——就算插件传 `../settings` 这类键名，也只会收到拒绝 + `storage.plugin_denied`
//!   审计，写不进任何地方。

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::observability::{try_event, EventType};

/// 插件命名空间数据的根目录。
pub fn plugin_root() -> PathBuf {
    crate::config::app_root().join("plugin_data")
}

/// 单个插件的命名空间目录（`<root>/<插件名>/`）。
pub fn plugin_dir(root: &Path, name: &str) -> PathBuf {
    root.join(name)
}

/// 内核保留区写入口：会话文件。session/mod.rs 的唯一落盘路径。
pub fn write_session(path: &Path, text: &str) -> anyhow::Result<()> {
    crate::fsx::write_atomic_str(path, text)
        .map_err(|e| anyhow::anyhow!("写会话文件失败（{}）: {e}", path.display()))?;
    try_event(EventType::State, serde_json::json!({ "kind": "storage.write", "area": "session" }));
    Ok(())
}

/// 内核保留区写入口：settings.json。config/settings.rs 的唯一落盘路径。
pub fn write_settings(path: &Path, text: &str) -> anyhow::Result<()> {
    crate::fsx::write_atomic_str(path, text).map_err(|e| anyhow::anyhow!("写 settings.json 失败: {e}"))?;
    try_event(EventType::State, serde_json::json!({ "kind": "storage.write", "area": "settings" }));
    Ok(())
}

/// Windows 传统 DOS 保留设备名（不分区大小写；任意扩展名同样保留，故按首个点前缀匹配）。
/// 审计 11：`CON.json` 之类在 Windows 会触发设备重定向，必须整段拦死。
fn is_dos_reserved_name(name: &str) -> bool {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = name.split('.').next().unwrap_or(name);
    RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r))
}

/// 插件名合法（与 plugins 同规：无分隔符、无穿越；额外放行 `- _ .` 组合但不接受 `.`/`..` 本身；
/// DOS 保留设备名整段拒绝——名字会用作 plugin_data 下的目录名）。
fn safe_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && !is_dos_reserved_name(name)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// 插件数据键名合法：字母数字与 `- _`，1–64 字节。硬卡死路径分隔符与 `..`（构造上无穿越）；
/// DOS 保留设备名拒绝（键名会拼成 `<key>.json`，CON 也撞设备名）。
fn safe_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && !is_dos_reserved_name(key)
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 插件值大小上限（序列化后字节）。
pub const MAX_VALUE_BYTES: usize = 64 * 1024;

/// 读插件命名空间数据。返回 None = 键不存在。
pub fn plugin_read_at(root: &Path, name: &str, key: &str) -> Result<Option<Value>, String> {
    if !safe_plugin_name(name) {
        return Err("插件名非法".into());
    }
    if !safe_key(key) {
        return Err("键名非法（只允许字母数字与 - _）".into());
    }
    let path = plugin_dir(root, name).join(format!("{key}.json"));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        // 2026-10-08 修：一切读错误都当「键不存在」——文件被 AV/备份短暂锁住
        // （Windows sharing violation）时插件读到 None，误判无数据并随即
        // plugin_write 覆盖，旧数据被静默冲掉。NotFound 才是真的不存在。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读插件数据失败（{name}/{key}，可能被其它程序占用）: {e}")),
    };
    serde_json::from_str(&text).map(Some).map_err(|e| format!("插件数据损坏（{name}/{key}）: {e}"))
}

/// 写插件命名空间数据（原子写）。返回写入字节数。
pub fn plugin_write_at(root: &Path, name: &str, key: &str, value: &Value) -> Result<usize, String> {
    if let Err(reason) = check_namespace(name, key) {
        try_event(
            EventType::State,
            serde_json::json!({ "kind": "storage.plugin_denied", "plugin": name, "key": key, "reason": reason }),
        );
        return Err(reason);
    }
    let text = serde_json::to_string(value).map_err(|e| format!("值不可序列化: {e}"))?;
    if text.len() > MAX_VALUE_BYTES {
        let reason = format!("值过大（{} 字节，上限 {}）", text.len(), MAX_VALUE_BYTES);
        try_event(
            EventType::State,
            serde_json::json!({ "kind": "storage.plugin_denied", "plugin": name, "key": key, "reason": reason }),
        );
        return Err(reason);
    }
    let dir = plugin_dir(root, name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("建插件数据目录失败: {e}"))?;
    let path = dir.join(format!("{key}.json"));
    crate::fsx::write_atomic(&path, text.as_bytes()).map_err(|e| format!("写插件数据失败: {e}"))?;
    try_event(
        EventType::State,
        serde_json::json!({ "kind": "storage.plugin_write", "plugin": name, "key": key, "bytes": text.len() }),
    );
    Ok(text.len())
}

/// 删除插件命名空间数据。返回是否确有删除。
pub fn plugin_delete_at(root: &Path, name: &str, key: &str) -> Result<bool, String> {
    if let Err(reason) = check_namespace(name, key) {
        try_event(
            EventType::State,
            serde_json::json!({ "kind": "storage.plugin_denied", "plugin": name, "key": key, "reason": reason }),
        );
        return Err(reason);
    }
    let path = plugin_dir(root, name).join(format!("{key}.json"));
    let found = std::fs::remove_file(&path).is_ok();
    try_event(
        EventType::State,
        serde_json::json!({ "kind": "storage.plugin_delete", "plugin": name, "key": key, "found": found }),
    );
    Ok(found)
}

/// app_root 版便捷入口（服务运行时用；测试用 *_at 版注入临时目录）。
pub fn plugin_read(name: &str, key: &str) -> Result<Option<Value>, String> {
    plugin_read_at(&plugin_root(), name, key)
}
pub fn plugin_write(name: &str, key: &str, value: &Value) -> Result<usize, String> {
    plugin_write_at(&plugin_root(), name, key, value)
}
pub fn plugin_delete(name: &str, key: &str) -> Result<bool, String> {
    plugin_delete_at(&plugin_root(), name, key)
}

fn check_namespace(name: &str, key: &str) -> Result<(), String> {
    if !safe_plugin_name(name) {
        return Err("插件名非法".into());
    }
    if !safe_key(key) {
        return Err("键名非法（只允许字母数字与 - _）".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmproot(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-storage-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn session_write_is_atomic_and_audited() {
        let root = tmproot("kernel");
        let path = root.join("sessions").join("abc.json");
        write_session(&path, "{\"id\":\"abc\"}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"id\":\"abc\"}");
        // 覆盖写仍完整（原子替换语义）
        write_session(&path, "{\"id\":\"abc\",\"v\":2}").unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"v\":2"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn settings_write_funnels_through_gate() {
        let root = tmproot("settings");
        let path = root.join("settings.json");
        write_settings(&path, "{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dos_reserved_names_rejected() {
        // 审计 11：CON/NUL/COM1 等传统设备名（含带扩展名形态）在 Windows 触发设备重定向
        for bad in ["CON", "con", "Nul", "AUX", "NUL", "COM1", "lpt9", "con.json", "COM8.any"] {
            assert!(!safe_plugin_name(bad), "{bad} 是保留设备名，必须拒绝");
            assert!(!safe_key(bad), "{bad} 是保留设备名，必须拒绝");
        }
        for good in ["constant", "nulp", "com10", "auxiliary", "my.plugin", "key-1"] {
            assert!(safe_plugin_name(good), "{good} 不该被误伤");
        }
        assert!(safe_key("constant") && safe_key("com10") && safe_key("auxiliary"));
    }

    #[test]
    fn plugin_write_read_delete_roundtrip() {
        let root = tmproot("plugin");
        let n = plugin_write_at(&root, "board", "notes", &json!({ "items": [1, 2, 3] })).unwrap();
        assert!(n > 0);
        let got = plugin_read_at(&root, "board", "notes").unwrap().unwrap();
        assert_eq!(got["items"][2], 3);
        assert!(plugin_read_at(&root, "board", "missing").unwrap().is_none());
        assert!(plugin_delete_at(&root, "board", "notes").unwrap());
        assert!(plugin_read_at(&root, "board", "notes").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn namespace_escape_is_denied_and_audited() {
        let root = tmproot("escape");
        // 键名穿越 / 绝对键 / 插件名穿越一律拒绝，且写不进任何地方
        assert!(plugin_write_at(&root, "board", "../settings", &json!(1)).is_err());
        assert!(plugin_write_at(&root, "board", "a/b", &json!(1)).is_err());
        assert!(plugin_write_at(&root, "../evil", "k", &json!(1)).is_err());
        assert!(plugin_write_at(&root, "", "k", &json!(1)).is_err());
        assert!(plugin_read_at(&root, "board", "..\\settings").is_err());
        // 越权尝试没有产生任何文件
        let entries: Vec<PathBuf> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.path()).collect();
        assert!(entries.is_empty(), "拒绝的写不应落任何文件: {entries:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn oversized_value_is_denied() {
        let root = tmproot("big");
        let big = "x".repeat(MAX_VALUE_BYTES + 1);
        assert!(plugin_write_at(&root, "board", "big", &json!(big)).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
