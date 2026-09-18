//! 模型自产插件 draft 隔离区（施工单 027，P18）。
//!
//! `source=model_generated` 的插件走 draft 隔离区：**只在内存**（不入盘，服务重启即失），
//! **权限一律清空**（最低权限，连 notify 都没有），工具可编译进 WASM 引擎供当前使用。
//! 用户一键「转正」后落盘持久化、来源改 `user`。
//!
//! 与定案的差异（如实记录）：定案说「仅当前会话生效」；v1 的生命周期是「到重启」——
//! 多会话同见。会话级隔离要给清单/面板/总线全链路加 session 过滤，在还没有真实生产者
//! （模型生成插件的工具链属 P22）之前是纯成本，故先做重启粒度，语义留口。
//!
//! 隔离边界：draft 插件能做什么由「权限清空 + WASM 纯计算沙箱（零宿主函数）」物理保证，
//! 不靠自觉；面板 iframe 不透明源、postMessage 白名单校验权限（全部为空 ⇒ 全拒）。

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

use serde_json::{json, Value};

use crate::observability::{try_event, EventType};

/// 单个 draft 插件：规范化后的清单 + 内存文件（相对路径 → 内容，全部文本）。
#[derive(Debug, Clone)]
pub struct Draft {
    pub name: String,
    /// 已规范化：apiVersion=1、source=model_generated、permissions=[]
    pub manifest: Value,
    pub files: BTreeMap<String, Vec<u8>>,
    /// 已编译进 WASM 引擎的工具文件名（None = 纯面板草稿）
    pub tool_file: Option<String>,
    /// 转正时工具文件应去的目录（由 wasm 引擎登记时回填）
    pub tool_loaded: bool,
}

static DRAFTS: OnceLock<RwLock<BTreeMap<String, Draft>>> = OnceLock::new();

fn store() -> &'static RwLock<BTreeMap<String, Draft>> {
    DRAFTS.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// 单文件大小上限与数量上限（draft 是模型现场生成的，卡死爆炸面）。
const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_FILES: usize = 32;

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// 提交一个 draft（模型自产插件的唯一入口）。规范化清单并做完整性校验；
/// 带 tool_file 时现场编译，编译失败整体拒绝。
pub fn submit(name: &str, manifest: &Value, files: &BTreeMap<String, String>) -> Result<(), String> {
    if !safe_name(name) {
        return Err("插件名非法（仅字母数字与 - _ .，且不能是 . 或 ..）".into());
    }
    if !manifest.is_object() {
        return Err("manifest 必须是 JSON 对象".into());
    }
    // 契约校验：apiVersion 不匹配直接拒（与磁盘插件同一把尺子）
    crate::plugin::contract::check_api_version(manifest)?;
    // 与磁盘插件重名 → 拒（避免「同名覆盖内置」的迷惑性）
    if crate::plugin::ui::resolve_asset(name, "manifest.json").is_ok() {
        return Err(format!("已存在同名插件（{name}），换一个名字"));
    }
    if crate::plugin::wasm::has_plugin(name) {
        return Err(format!("已存在同名工具插件（{name}），换一个名字"));
    }
    if files.is_empty() {
        return Err("files 为空：至少要有入口页或工具文件".into());
    }
    if files.len() > MAX_FILES {
        return Err(format!("文件数超上限（{MAX_FILES}）"));
    }
    let mut bin: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (rel, content) in files {
        if rel.is_empty() || rel.len() > 128 || rel.contains("..") || rel.contains('\\') || rel.starts_with('/') {
            return Err(format!("文件名非法: {rel}"));
        }
        if content.len() > MAX_FILE_BYTES {
            return Err(format!("文件过大（{rel}，上限 {MAX_FILE_BYTES} 字节）"));
        }
        bin.insert(rel.clone(), content.as_bytes().to_vec());
    }
    // 规范化：来源强制 model_generated，权限一律清空（最低权限，定案 3/隔离边界）
    let mut mf = manifest.clone();
    mf["source"] = json!(crate::plugin::contract::ORIGIN_MODEL);
    mf["permissions"] = json!([]);

    // 面板完整性：声明了 ui 插槽或 entry 时，入口页必须真实存在
    let entry = mf.get("entry").and_then(Value::as_str).unwrap_or("index.html").to_string();
    let declares_panel = mf.get("ui").and_then(Value::as_array).is_some_and(|a| !a.is_empty())
        || mf.get("entry").is_some();
    if declares_panel && !bin.contains_key(&entry) {
        return Err(format!("入口页 {entry} 不在 files 里"));
    }

    // 工具：tool_file 指向 files 里的 .wat/.wasm，现场编译（失败整体拒绝）
    let tool_file = mf
        .get("tool_file")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut tool_loaded = false;
    if let Some(tf) = &tool_file {
        let bytes = bin
            .get(tf)
            .ok_or_else(|| format!("tool_file {tf} 不在 files 里"))?
            .clone();
        crate::plugin::wasm::load_inline(name, &bytes).map_err(|e| format!("工具编译失败: {e}"))?;
        tool_loaded = true;
    }

    let draft = Draft { name: name.to_string(), manifest: mf, files: bin, tool_file, tool_loaded };
    store()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string(), draft);
    try_event(
        EventType::State,
        json!({ "kind": "plugin.draft_submitted", "plugin": name, "files": files.len(), "tools": tool_loaded }),
    );
    Ok(())
}

/// 全部 draft（按名字序，稳定）。
pub fn list() -> Vec<Draft> {
    store()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect()
}

pub fn get(name: &str) -> Option<Draft> {
    store()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned()
}

/// draft 内文件读取（serve_ext 的内存 overlay；rel 为空 = 入口页）。
/// 键必须与 files 里的键完全一致（不做路径拼接 ⇒ 构造上无穿越）。
pub fn read_file(name: &str, rel: &str) -> Option<Vec<u8>> {
    let d = get(name)?;
    let rel = rel.trim().replace('\\', "/");
    let rel = if rel.is_empty() {
        d.manifest.get("entry").and_then(Value::as_str).unwrap_or("index.html").to_string()
    } else {
        rel
    };
    d.files.get(&rel).cloned()
}

/// draft 的入口页文件名（serve_ext 的空 rel 用）。
pub fn entry_file(name: &str) -> Option<String> {
    get(name).map(|d| {
        d.manifest
            .get("entry")
            .and_then(Value::as_str)
            .unwrap_or("index.html")
            .to_string()
    })
}

/// 一键转正：面板文件落 `ui-ext/<name>/`，工具文件落 plugins 目录（manifest.json + 工具文件），
/// 来源改 `user`；随后重扫 WASM 注册表并移除 draft。返回落盘路径列表。
pub fn promote(name: &str) -> Result<Vec<String>, String> {
    let d = get(name).ok_or_else(|| format!("没有名为 {name} 的 draft"))?;
    let app_root = crate::config::app_root();
    let mut written: Vec<String> = Vec::new();

    let mut mf = d.manifest.clone();
    mf["source"] = json!(crate::plugin::contract::ORIGIN_USER);

    // 面板半边：有入口页就落 ui-ext/
    let entry = mf.get("entry").and_then(Value::as_str).unwrap_or("index.html").to_string();
    if d.files.contains_key(&entry) {
        let dir = app_root.join(crate::plugin::ui::DIR).join(name);
        write_all(&dir, &d.files, &mf, &entry, &mut written)
            .map_err(|e| format!("写 ui-ext 失败: {e}"))?;
    }

    // 工具半边：有 tool_file 就落 plugins/<name>/
    if let Some(tf) = &d.tool_file {
        let bytes = d.files.get(tf).ok_or("tool_file 缺失")?;
        let dir = crate::plugin::wasm::plugins_dir()
            .unwrap_or_else(|| app_root.join("plugins"))
            .join(name);
        std::fs::create_dir_all(&dir).map_err(|e| format!("建插件目录失败: {e}"))?;
        let mf_path = dir.join("manifest.json");
        crate::fsx::write_atomic_str(&mf_path, &serde_json::to_string_pretty(&mf).unwrap_or_default())
            .map_err(|e| format!("写 manifest 失败: {e}"))?;
        written.push(mf_path.display().to_string());
        let bin_path = dir.join(tf);
        crate::fsx::write_atomic(&bin_path, bytes).map_err(|e| format!("写工具文件失败: {e}"))?;
        written.push(bin_path.display().to_string());
        crate::plugin::wasm::rescan();
    }

    store().write().unwrap_or_else(|e| e.into_inner()).remove(name);
    try_event(
        EventType::State,
        json!({ "kind": "plugin.draft_promoted", "plugin": name, "paths": written.len() }),
    );
    Ok(written)
}

fn write_all(
    dir: &std::path::Path,
    files: &BTreeMap<String, Vec<u8>>,
    manifest: &Value,
    entry: &str,
    written: &mut Vec<String>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mf_path = dir.join("manifest.json");
    crate::fsx::write_atomic_str(&mf_path, &serde_json::to_string_pretty(manifest).unwrap_or_default())?;
    written.push(mf_path.display().to_string());
    for (rel, bytes) in files {
        let p = dir.join(rel);
        crate::fsx::write_atomic(&p, bytes)?;
        written.push(p.display().to_string());
    }
    let _ = entry;
    Ok(())
}

/// 丢弃一个 draft（用户不要了 / 换会话清理）。返回是否确有删除。
pub fn discard(name: &str) -> bool {
    let removed = store().write().unwrap_or_else(|e| e.into_inner()).remove(name).is_some();
    if removed {
        crate::plugin::wasm::remove_inline(name);
        try_event(EventType::State, json!({ "kind": "plugin.draft_discarded", "plugin": name }));
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ECHO_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (data (i32.const 1024) "{\"tools\":[{\"name\":\"echo\",\"description\":\"echo\",\"schema\":{\"type\":\"object\"}}]}\00")
  (func (export "alloc") (param $n i32) (result i32) (i32.const 4096))
  (func $pack (param $p i32) (param $l i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $l))))
  (func (export "describe") (result i64) (call $pack (i32.const 1024) (i32.const 0)))
  (func (export "run") (param i32 i32 i32 i32) (result i64) (call $pack (i32.const 0) (i32.const 0))))"#;

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn submit_normalizes_and_strips_permissions() {
        let mf = json!({
            "apiVersion": 1,
            "source": "user", // 提交者自称 user 也会被强制改成 model_generated
            "title": "草稿面板",
            "entry": "index.html",
            "permissions": ["workspace.write", "notify"],
        });
        let f = files(&[("index.html", "<html></html>")]);
        submit("draft-a", &mf, &f).unwrap();
        let d = get("draft-a").unwrap();
        assert_eq!(d.manifest["source"], "model_generated", "来源强制 model_generated");
        assert_eq!(d.manifest["permissions"], json!([]), "权限一律清空（最低权限）");
        assert!(d.tool_file.is_none());
        assert!(read_file("draft-a", "").is_some(), "rel 空 = 入口页");
        assert!(read_file("draft-a", "index.html").is_some());
        assert!(read_file("draft-a", "../evil").is_none(), "非键名一律取不到");
        discard("draft-a");
    }

    #[test]
    fn submit_rejects_bad_contract_and_conflicts() {
        let f = files(&[("index.html", "x")]);
        // apiVersion 不匹配 → 拒（与磁盘插件同一把尺子）
        let mf = json!({"apiVersion": 9, "entry": "index.html"});
        assert!(submit("draft-bad", &mf, &f).is_err());
        // 入口页缺失 → 拒
        let mf = json!({"apiVersion": 1, "entry": "index.html"});
        assert!(submit("draft-bad", &mf, &files(&[])).is_err());
        // 非法名字 → 拒
        assert!(submit("../evil", &json!({"apiVersion": 1}), &f).is_err());
    }

    #[test]
    fn tool_draft_compiles_inline_and_promotes_to_disk() {
        let mf = json!({"apiVersion": 1, "title": "草稿计算器", "tool_file": "calc.wat"});
        let f = files(&[("calc.wat", ECHO_WAT)]);
        submit("draft-calc", &mf, &f).unwrap();
        assert!(get("draft-calc").unwrap().tool_loaded, "工具应已编译进引擎");
        assert!(crate::plugin::wasm::has_plugin("draft-calc"));

        let paths = promote("draft-calc").unwrap();
        assert!(paths.iter().any(|p| p.contains("plugins")), "工具半边应落 plugins 目录: {paths:?}");
        assert!(get("draft-calc").is_none(), "转正后 draft 移除");
        assert!(crate::plugin::wasm::has_plugin("draft-calc"), "重扫后仍在注册表");
        // 落盘的 manifest 来源应为 user
        let mf_path = paths.iter().find(|p| p.ends_with("manifest.json")).unwrap();
        let text = std::fs::read_to_string(mf_path).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["source"], "user");
        assert_eq!(v["apiVersion"], 1);
        // 清理落盘产物（测试不污染源码树）
        let dir = std::path::Path::new(mf_path).parent().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
