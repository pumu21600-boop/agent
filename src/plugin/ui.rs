//! 组件总线 v0 —— UI 插槽（施工单 021，P15）
//!
//! 总架构 §7 的「双总线」后半：工具总线早已统一（内置 + mcp__ + wasm__），组件总线此前
//! 只有设计留口。v0 交付**最小但真能用**的一档——静态 UI 扩展 + 受限数据桥。
//!
//! ## 扩展形态
//!
//! `<root>/ui-ext/<name>/` 目录，内含 `manifest.json` + 静态资源（默认入口 `index.html`）：
//!
//! ```json
//! { "title": "记忆看板", "description": "把记忆库按标签排开", "version": "0.1.0", "entry": "index.html" }
//! ```
//!
//! 两个来源，工作区同名覆盖内置：
//! 1. 应用根 `ui-ext/`（随包携带的内置扩展）；
//! 2. 当前工作区 `<workspace>/ui-ext/`（项目自带扩展）。
//!
//! ## 隔离模型（本模块只负责发现与分发，隔离在前端）
//!
//! - 前端用 `sandbox="allow-scripts"` 的 iframe 承载扩展：**不透明源**，拿不到宿主
//!   DOM / localStorage / cookie，也不能读宿主的 API 响应；
//! - 扩展与宿主的唯一通道是 postMessage 白名单（宿主侧实现，只放只读方法）；
//! - 后端只做两件事：列清单 + 分发**扩展目录内**的静态文件（路径穿越硬拦，
//!   canonicalize 后必须仍在扩展目录下，符号链接也逃不出去）。
//!
//! 信任模型：扩展目录里的文件由用户自己放置（本地可信代码，但仍在 iframe 沙箱里跑）；
//! manifest 读不出来 / 目录不合规 → 列进清单但标 ok=false，界面显式报错，不静默忽略。

use std::path::{Path, PathBuf};

use serde::Serialize;

/// 扩展目录名（应用根与工作区同名）。
pub const DIR: &str = "ui-ext";

/// 一个 UI 扩展的描述（GET /api/ext 直出）。
#[derive(Debug, Clone, Serialize)]
pub struct UiExt {
    pub name: String,
    pub title: String,
    pub description: String,
    pub version: String,
    /// 入口页（相对扩展目录）
    pub entry: String,
    /// "app"（内置，应用根）| "workspace"（当前工作区）
    pub source: String,
    /// 是否覆盖了同名的内置扩展（工作区同名优先；界面显式提示，避免"看起来是内置那个"）
    pub shadowed: bool,
    /// 扩展目录绝对路径（界面提示用）
    pub dir: String,
    /// manifest 是否可用（false 时 error 有原因）
    pub ok: bool,
    pub error: Option<String>,
}

/// 扫描全部扩展（内置 + 当前工作区；工作区同名覆盖内置，顺序稳定）。
pub fn scan() -> Vec<UiExt> {
    let app = crate::config::app_root();
    let ws = crate::tools::workspace_root();
    let mut out: Vec<UiExt> = Vec::new();
    collect(&app.join(DIR), "app", &mut out);
    if ws != app {
        collect(&ws.join(DIR), "workspace", &mut out);
    }
    out
}

/// 扫描单个根目录：同名后者覆盖前者（工作区覆盖内置）。
fn collect(root: &Path, source: &str, out: &mut Vec<UiExt>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let mut dirs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let Some(name) = dir.file_name().and_then(|s| s.to_str()).map(str::to_string) else { continue };
        let mut ext = load_one(&dir, &name, source);
        // 工作区同名者顶掉内置：给幸存的那个打标记（界面提示"覆盖内置"）
        if source == "workspace" && out.iter().any(|e| e.name == name) {
            ext.shadowed = true;
        }
        out.retain(|e| e.name != name);
        out.push(ext);
    }
}

/// 读一个扩展目录的 manifest，缺文件/字段不合法都记成 ok=false + error。
fn load_one(dir: &Path, name: &str, source: &str) -> UiExt {
    let mut e = UiExt {
        name: name.to_string(),
        title: name.to_string(),
        description: String::new(),
        version: String::new(),
        entry: "index.html".into(),
        source: source.to_string(),
        shadowed: false,
        dir: dir.display().to_string(),
        ok: false,
        error: None,
    };
    if !safe_segment(name) {
        e.error = Some("目录名非法（仅字母数字与 - _ . ，且不能是 . 或 ..）".into());
        return e;
    }
    let mf = dir.join("manifest.json");
    let text = match std::fs::read_to_string(&mf) {
        Ok(t) => t,
        Err(err) => {
            e.error = Some(format!("读 manifest.json 失败: {err}"));
            return e;
        }
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(err) => {
            e.error = Some(format!("manifest.json 不是合法 JSON: {err}"));
            return e;
        }
    };
    if let Some(t) = v.get("title").and_then(|x| x.as_str()) {
        e.title = t.to_string();
    }
    if let Some(d) = v.get("description").and_then(|x| x.as_str()) {
        e.description = d.to_string();
    }
    if let Some(ver) = v.get("version").and_then(|x| x.as_str()) {
        e.version = ver.to_string();
    }
    if let Some(en) = v.get("entry").and_then(|x| x.as_str()) {
        e.entry = en.to_string();
    }
    // 入口必须真实存在且不出目录
    match resolve_in(dir, &e.entry) {
        Ok(p) if p.is_file() => e.ok = true,
        Ok(_) => e.error = Some(format!("入口 {} 不是文件", e.entry)),
        Err(err) => e.error = Some(err),
    }
    e
}

/// 名字片段安全校验（无路径分隔符、无穿越）。
fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
}

/// 相对路径 → 扩展目录内的绝对路径；**路径穿越硬拦**。
///
/// 规则：拒绝绝对路径与含 `..` / `:` 的相对路径；join 后 canonicalize，结果必须仍在
/// （同样 canonicalize 过的）扩展目录之下——`..\..\settings.json` 与符号链接都拦得住。
pub fn resolve_asset(name: &str, rel: &str) -> Result<PathBuf, String> {
    if !safe_segment(name) {
        return Err("扩展名非法".into());
    }
    let dir = find_dir(name).ok_or_else(|| format!("未知扩展: {name}"))?;
    resolve_in(&dir, rel)
}

/// 扩展名 → 目录（工作区优先，与 scan 覆盖规则一致）。
fn find_dir(name: &str) -> Option<PathBuf> {
    let app = crate::config::app_root();
    let ws = crate::tools::workspace_root();
    let cand_ws = ws.join(DIR).join(name);
    if cand_ws.is_dir() {
        return Some(cand_ws);
    }
    let cand_app = app.join(DIR).join(name);
    if cand_app.is_dir() {
        return Some(cand_app);
    }
    None
}

/// 目录内相对路径解析 + 穿越防护（scan 与分发共用）。
fn resolve_in(dir: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim().replace('\\', "/");
    if rel.is_empty() {
        return Err("路径为空".into());
    }
    if rel.starts_with('/') || rel.contains(':') {
        return Err("只允许扩展目录内的相对路径".into());
    }
    let mut p = dir.to_path_buf();
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err("路径不能上溯（..）".into());
        }
        p.push(seg);
    }
    let canon_dir = dir.canonicalize().map_err(|e| format!("扩展目录不可用: {e}"))?;
    let canon = p.canonicalize().map_err(|e| format!("文件不存在: {e}"))?;
    if !canon.starts_with(&canon_dir) {
        return Err("越界：目标不在扩展目录内".into());
    }
    Ok(canon)
}

/// 静态资源 MIME（覆盖扩展常用类型；未知一律 octet-stream）。
pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("txt") | Some("md") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-ui-ext-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn manifest_loaded_and_entry_checked() {
        let root = tmpdir("manifest");
        let d = root.join("panel");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("manifest.json"),
            r#"{"title":"记忆看板","description":"desc","version":"1.2.3","entry":"index.html"}"#,
        )
        .unwrap();
        std::fs::write(d.join("index.html"), "<html></html>").unwrap();
        let e = load_one(&d, "panel", "workspace");
        assert!(e.ok, "应加载成功: {e:?}");
        assert_eq!(e.title, "记忆看板");
        assert_eq!(e.version, "1.2.3");
        assert_eq!(e.source, "workspace");

        // 入口不存在 → 不 ok 且带原因（不静默）
        std::fs::write(d.join("manifest.json"), r#"{"entry":"missing.html"}"#).unwrap();
        let bad = load_one(&d, "panel", "app");
        assert!(!bad.ok);
        assert!(bad.error.is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_ext_shadows_builtin_with_flag_and_test_two_roots() {
        let app = tmpdir("shadow-app");
        let ws = tmpdir("shadow-ws");
        for (root, title) in [(&app, "内置看板"), (&ws, "工作区看板")] {
            let d = root.join("board");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("manifest.json"), format!(r#"{{"title":"{title}"}}"#)).unwrap();
            std::fs::write(d.join("index.html"), "<html></html>").unwrap();
        }
        // 工作区独有 + 覆盖同名两条路径都覆盖到
        let only_ws = ws.join("solo");
        std::fs::create_dir_all(&only_ws).unwrap();
        std::fs::write(only_ws.join("manifest.json"), r#"{"title":"仅工作区"}"#).unwrap();
        std::fs::write(only_ws.join("index.html"), "x").unwrap();

        let mut out: Vec<UiExt> = Vec::new();
        collect(&app, "app", &mut out);
        collect(&ws, "workspace", &mut out);
        assert_eq!(out.len(), 2, "同名只留一份（工作区覆盖内置）");
        let board = out.iter().find(|e| e.name == "board").unwrap();
        assert_eq!(board.title, "工作区看板");
        assert_eq!(board.source, "workspace");
        assert!(board.shadowed, "覆盖内置的扩展应带 shadowed 标记");
        let solo = out.iter().find(|e| e.name == "solo").unwrap();
        assert!(!solo.shadowed, "工作区独有（无同名内置）不该标覆盖");
        assert!(!out.iter().any(|e| e.title == "内置看板"), "被覆盖的内置项应被顶掉");
        let _ = std::fs::remove_dir_all(&app);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn traversal_is_blocked() {
        let root = tmpdir("traversal");
        let d = root.join("panel");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("ok.html"), "hi").unwrap();
        std::fs::write(root.join("secret.txt"), "secret").unwrap();

        // 正常路径可解析
        assert!(resolve_in(&d, "ok.html").is_ok());
        // 上溯 / 绝对路径 / Windows 盘符 全拦
        assert!(resolve_in(&d, "../secret.txt").is_err(), "上溯必须被拦");
        assert!(resolve_in(&d, "..\\secret.txt").is_err(), "反斜杠上溯必须被拦");
        assert!(resolve_in(&d, "/etc/passwd").is_err());
        assert!(resolve_in(&d, "C:/Windows/win.ini").is_err());
        assert!(resolve_in(&d, "sub/../../secret.txt").is_err());
        // 不存在的文件也不放行
        assert!(resolve_in(&d, "nope.html").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn content_types_cover_ui_assets() {
        assert!(content_type(Path::new("a/index.html")).starts_with("text/html"));
        assert!(content_type(Path::new("a/app.js")).starts_with("text/javascript"));
        assert!(content_type(Path::new("a/style.CSS")).starts_with("text/css"));
        assert_eq!(content_type(Path::new("a/data.bin")), "application/octet-stream");
    }

    #[test]
    fn bad_dir_names_rejected() {
        assert!(safe_segment("panel-x"));
        assert!(safe_segment("a.b"));
        assert!(!safe_segment(".."));
        assert!(!safe_segment("a/b"));
        assert!(!safe_segment("a\\b"));
        assert!(!safe_segment(""));
    }
}
