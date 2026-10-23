//! WASM 宿主函数（2026-10-22 当前批次条目 5：ABI v1 增量——只加 imports，不改版本号）。
//!
//! v1 之前插件是纯计算沙箱（零宿主函数，wasm.rs 模块头）；本轮按 manifest 权限声明
//! 有选择地开三个口，能力与场景一一对应：
//! - `host.storage_get / storage_set`：插件命名空间持久 KV——复用面板插件同一份
//!   plugin_data 存储（键名白名单 / DOS 保留名 / 64KB 值上限 / 原子写 / 审计事件
//!   全部继承，面板与工具两半看同一个命名空间）；
//! - `host.fs_read`：读**本任务工作区**内的文本文件（工作区由 call_tool 传入的任务
//!   钉住路径判定，不是全局根——后台任务切换工作区不会串界；canonicalize 防穿越
//!   与 junction，绝对路径拒绝，256KB 上限，非 UTF-8 拒读）；
//! - `host.http_get`：GET 公网（逐跳 SSRF 闸与 web_fetch 同一道、同一个
//!   `PAWCH_AGENT_WEB_ALLOW_PRIVATE` 调试开关；10s 超时，返回体上限 64KB——宿主函数的
//!   结果会整段进模型上下文，不能沿用 web_fetch 的 2MB）。
//!
//! 授予策略（2026-10-24 用户拍板撤闸：「插件即能力」——做联网插件就联网、做
//! 计算插件就计算，能力范围由插件作者决定，不由宿主审）：用户放进 plugins/ 的
//! 插件（目录形态与裸文件）无条件拿到全部宿主函数；唯一例外是模型自产 draft
//! 恒为纯计算沙箱（模型本就有等价的文件/网络工具，draft 不需要也不该自带宿主
//! 口子）。真正的防线不在授予层而在函数本体：工作区判界 / SSRF 逐跳闸 / 大小
//! 上限见各函数注释。manifest `permissions` 数组从此只归面板桥
//! （composer.insert 等），工具侧不读。风险档不变：用户主动放置的插件仍
//! Normal（信任模型见 wasm.rs 模块头）。
//!
//! 返回值约定（四个函数统一）：packed(ptr,len) 指向宿主经插件 `alloc` 写进线性
//! 内存的 UTF-8 字符串——成功 = 数据本身（storage_get 缺键 = `{"missing":true}`，
//! storage_set 成功 = `{"ok":true}`），失败 = `{"error":"人话"}`；插件侧自行判断。
//! 返回 0 = 结果写回失败（插件的 alloc 不按 ABI 工作），视为插件侧缺陷。

use std::path::{Path, PathBuf};

use serde_json::Value;
use wasmtime::Caller;

use crate::storage;

/// 宿主函数权限集：盘上插件 = full（用户放置即信任），模型 draft = default
/// （纯计算沙箱）。2026-10-24 撤权限闸后只剩这两档，不再按 manifest 细分。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostPerms {
    pub storage: bool,
    pub fs_read: bool,
    pub http: bool,
}

impl HostPerms {
    /// 全量授予（用户放置的插件——插件即能力，权限范围由作者决定）。
    pub fn full() -> Self {
        Self { storage: true, fs_read: true, http: true }
    }
}

/// 一次 WASM 实例化/调用的宿主上下文（随 Linker 闭包捕获）。
#[derive(Clone)]
pub struct HostCtx {
    /// 插件名（storage 命名空间按它隔离）
    pub stem: String,
    /// 本任务钉住的工作区（fs_read 判界用它，不是全局根）
    pub ws: PathBuf,
    /// tokio 句柄（http_get 在 blocking 线程上驱动异步闸用；describe 路径拿不到就是 None，
    /// 那条路本来也不该碰网络）
    pub handle: Option<tokio::runtime::Handle>,
}

/// 按权限构建 Linker：盘上插件全量链接；draft 空链接——draft 导入宿主函数在
/// instantiate 时自然失败。
pub fn build_linker(perms: &HostPerms, ctx: &HostCtx) -> wasmtime::Linker<wasmtime::StoreLimits> {
    let mut linker = wasmtime::Linker::new(super::wasm::engine());
    if perms.storage {
        let c = ctx.clone();
        linker
            .func_wrap("host", "storage_get", move |mut caller: Caller<'_, wasmtime::StoreLimits>, kp: i32, kl: i32| -> i64 {
                match read_str(&mut caller, kp, kl).map(|k| storage_get(&c.stem, &k)) {
                    Ok(text) => host_pack(&mut caller, &text),
                    Err(e) => host_pack(&mut caller, &err_json(&e)),
                }
            })
            .expect("注册 storage_get");
        let c = ctx.clone();
        linker
            .func_wrap(
                "host",
                "storage_set",
                move |mut caller: Caller<'_, wasmtime::StoreLimits>, kp: i32, kl: i32, vp: i32, vl: i32| -> i64 {
                    let text = match read_str(&mut caller, kp, kl)
                        .and_then(|k| read_str(&mut caller, vp, vl).map(|v| (k, v)))
                        .map(|(k, v)| storage_set(&c.stem, &k, &v))
                    {
                        Ok(t) => t,
                        Err(e) => err_json(&e),
                    };
                    host_pack(&mut caller, &text)
                },
            )
            .expect("注册 storage_set");
    }
    if perms.fs_read {
        let c = ctx.clone();
        linker
            .func_wrap("host", "fs_read", move |mut caller: Caller<'_, wasmtime::StoreLimits>, pp: i32, pl: i32| -> i64 {
                let text = match read_str(&mut caller, pp, pl) {
                    Ok(rel) => fs_read_ws(&c.ws, &rel),
                    Err(e) => err_json(&e),
                };
                host_pack(&mut caller, &text)
            })
            .expect("注册 fs_read");
    }
    if perms.http {
        let c = ctx.clone();
        linker
            .func_wrap("host", "http_get", move |mut caller: Caller<'_, wasmtime::StoreLimits>, up: i32, ul: i32| -> i64 {
                let text = match read_str(&mut caller, up, ul) {
                    Ok(url) => http_get(&c.handle, &url),
                    Err(e) => err_json(&e),
                };
                host_pack(&mut caller, &text)
            })
            .expect("注册 http_get");
    }
    linker
}

// —— 宿主函数本体（纯逻辑，全部可脱离 wasm 单测）——

/// storage_get：读插件命名空间。面板插件写过的值（任意 JSON）字符串化读回。
fn storage_get(stem: &str, key: &str) -> String {
    match storage::plugin_read_at(&storage::plugin_root(), stem, key) {
        Ok(Some(Value::String(s))) => s,
        Ok(Some(other)) => other.to_string(),
        Ok(None) => "{\"missing\":true}".to_string(),
        Err(e) => err_json(&e),
    }
}

/// storage_set：写插件命名空间（值上限/键名白名单由 storage 层强制）。
fn storage_set(stem: &str, key: &str, val: &str) -> String {
    match storage::plugin_write_at(&storage::plugin_root(), stem, key, &Value::String(val.to_string())) {
        Ok(_) => "{\"ok\":true}".to_string(),
        Err(e) => err_json(&e),
    }
}

/// fs_read 可读上限（字节）：宿主函数结果整段进模型上下文，收得比 web_fetch 紧。
const HOST_FS_MAX_BYTES: usize = 256 * 1024;

/// fs_read：读工作区内文本文件。判界用 canonicalize 后的包含关系——绝对路径、
/// `..` 穿越出工作区、junction 指向工作区外、二进制（非 UTF-8）一律拒。
fn fs_read_ws(ws: &Path, rel: &str) -> String {
    let rel_path = Path::new(rel);
    if rel.trim().is_empty() {
        return err_json("路径为空");
    }
    if rel_path.is_absolute() {
        return err_json("只接受工作区内的相对路径（绝对路径拒绝）");
    }
    if rel.contains('\0') {
        return err_json("路径含非法字符");
    }
    let Ok(canon_ws) = ws.canonicalize() else {
        return err_json(&format!("工作区目录不存在: {}", ws.display()));
    };
    let target = canon_ws.join(rel_path);
    let Ok(canon_target) = target.canonicalize() else {
        return err_json(&format!("文件不存在或不可访问: {rel}"));
    };
    if !canon_target.starts_with(&canon_ws) {
        return err_json(&format!("路径 {rel} 越出工作区，已拒绝"));
    }
    if canon_target.is_dir() {
        return err_json(&format!("{rel} 是目录不是文件"));
    }
    let Ok(mut f) = std::fs::File::open(&canon_target) else {
        return err_json(&format!("打开失败: {rel}"));
    };
    use std::io::Read;
    let mut buf = vec![0u8; HOST_FS_MAX_BYTES + 1];
    let mut total = 0usize;
    loop {
        match f.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                if total > HOST_FS_MAX_BYTES {
                    return err_json(&format!(
                        "文件超过 {}KB 上限（fs_read 只读小文本）",
                        HOST_FS_MAX_BYTES / 1024
                    ));
                }
            }
            Err(e) => return err_json(&format!("读取失败: {e}")),
        }
    }
    match String::from_utf8(buf[..total].to_vec()) {
        Ok(s) => s,
        Err(_) => err_json("文件不是 UTF-8 文本（二进制文件不走 fs_read）"),
    }
}

/// http_get 返回体上限（字节）。
const HOST_HTTP_MAX_BYTES: usize = 64 * 1024;

/// http_get：GET 公网。SSRF 逐跳闸复用 web_fetch 同一道（含
/// `PAWCH_AGENT_WEB_ALLOW_PRIVATE=1` 本地调试开关）；10s 总超时——WASM 调用整层
/// 30s 墙钟，网络段必须留得出余量。
fn http_get(handle: &Option<tokio::runtime::Handle>, url: &str) -> String {
    let Some(handle) = handle else {
        return err_json("宿主没有可用的异步运行时句柄（此路径不该发网络请求）");
    };
    let url = url.to_string();
    handle.block_on(async {
        let client = crate::tools::web_client()?;
        // 闸与发送共用 web_fetch 的逐跳 SSRF 防线（重定向目标也判内网段）
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::tools::send_with_ssrf_guard(&client, &url),
        )
        .await
        .map_err(|_| "请求超时（10s）".to_string())??;
        let status = resp.status();
        // 流式读体（带上限，超限即断——不整段拉进内存）
        let mut body: Vec<u8> = Vec::new();
        let mut resp = resp;
        while body.len() < HOST_HTTP_MAX_BYTES {
            match resp.chunk().await {
                Ok(Some(chunk)) => body.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(e) => return Err(format!("读响应体失败: {e}")),
            }
        }
        let mut text = String::from_utf8_lossy(&body).to_string();
        if text.len() > HOST_HTTP_MAX_BYTES {
            text.truncate(HOST_HTTP_MAX_BYTES);
        }
        Ok(format!("HTTP {status}\n{text}"))
    })
    .unwrap_or_else(|e: String| err_json(&e))
}

// —— 编组辅助 ——

/// 从插件线性内存读 (ptr,len) UTF-8 字符串（长度不可信：先钳到 memory 上限）。
fn read_str(caller: &mut Caller<'_, wasmtime::StoreLimits>, ptr: i32, len: i32) -> Result<String, String> {
    let mem = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or("插件未导出 memory")?;
    let len = len as usize;
    if len > super::wasm::WASM_MEMORY_LIMIT {
        return Err(format!("参数长度 {len} 超过 memory 上限，拒读"));
    }
    let mut buf = vec![0u8; len];
    mem.read(&mut *caller, ptr as usize, &mut buf).map_err(|e| format!("读插件内存失败: {e}"))?;
    String::from_utf8(buf).map_err(|e| format!("参数不是 UTF-8: {e}"))
}

/// 宿主结果写回：调插件的 alloc 拿缓冲 → 写入 → packed(ptr,len)。
/// 任何一步失败（alloc 缺失/trap/写越界）返回 0 = 「结果未能写回」（插件侧缺陷）。
fn host_pack(caller: &mut Caller<'_, wasmtime::StoreLimits>, text: &str) -> i64 {
    let bytes = text.as_bytes();
    let alloc = caller
        .get_export("alloc")
        .and_then(|e| e.into_func())
        .and_then(|f| f.typed::<i32, i32>(&mut *caller).ok());
    let Some(alloc) = alloc else { return 0 };
    let Ok(ptr) = alloc.call(&mut *caller, bytes.len() as i32) else { return 0 };
    let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else { return 0 };
    if mem.write(&mut *caller, ptr as usize, bytes).is_err() {
        return 0;
    }
    ((ptr as u32 as i64) << 32) | (bytes.len() as u32 as i64)
}

/// 统一错误串：{"error":"人话"}（插件侧按此约定判断）。
fn err_json(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perms_full_and_default_are_the_only_two_modes() {
        let full = HostPerms::full();
        assert!(full.storage && full.fs_read && full.http, "盘上插件=全量");
        let none = HostPerms::default();
        assert!(!none.storage && !none.fs_read && !none.http, "draft=纯沙箱");
    }

    #[test]
    fn fs_read_containment_and_caps() {
        let ws = std::env::temp_dir().join(format!("agent-host-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("hello.txt"), "你好，工作区").unwrap();
        std::fs::write(ws.join("bin.bin"), [0u8, 159, 146, 150]).unwrap();

        assert_eq!(fs_read_ws(&ws, "hello.txt"), "你好，工作区");
        assert_eq!(fs_read_ws(&ws, "sub/../hello.txt"), "你好，工作区", "canonicalize 后未出界=放行");
        // 工作区外真放一个文件：目标存在时必须死在越界判定（而不是「不存在」）
        let outside = ws.parent().unwrap().join(format!("agent-host-fs-escape-{}.txt", std::process::id()));
        std::fs::write(&outside, "外面的秘密").unwrap();
        let esc = fs_read_ws(&ws, "../agent-host-fs-escape-marker.txt");
        let esc2 = fs_read_ws(&ws, &format!("../{}", outside.file_name().unwrap().to_str().unwrap()));
        let _ = std::fs::remove_file(&outside);
        assert!(
            esc.contains("不存在") || esc.contains("越出工作区"),
            "工作区外的目标必须被拒: {esc}"
        );
        assert!(esc2.contains("越出工作区"), "已存在的外部文件穿越必须拒: {esc2}");
        let abs = fs_read_ws(&ws, "C:\\Windows\\win.ini");
        assert!(abs.contains("相对路径"), "绝对路径必须拒: {abs}");
        let bin = fs_read_ws(&ws, "bin.bin");
        assert!(bin.contains("UTF-8"), "二进制必须拒: {bin}");
        let missing = fs_read_ws(&ws, "nope.txt");
        assert!(missing.contains("不存在"), "缺文件说人话: {missing}");
        assert!(fs_read_ws(&ws, "sub").contains("目录"));

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn err_json_is_valid_json_with_message() {
        let s = err_json("带 \"引号\" 的消息");
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error"], "带 \"引号\" 的消息");
    }
}
