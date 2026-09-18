//! WASM 插件宿主（施工单 017 + 021，P11/P15；P18 027 契约化）—— 总架构 §7
//!
//! **P18 定位调整**：WASM 不再是一种「插件」，降为插件工具的一种可选实现 / 沙箱轻量档。
//! 用户可见术语只有「插件」；这里的代码是插件工具的执行引擎之一。
//!
//! 插件形态（两种，磁盘自动迁移读入）：
//! 1. 旧形态：`plugins/` 目录下的 .wasm / .wat 文件（wat 由 wasmtime 内置解析，零工具链依赖）；
//! 2. 目录形态：`plugins/<name>/manifest.json` + 同目录一个 .wasm/.wat——manifest 契约
//!    走 `contract.rs`（apiVersion 不匹配拒载），并补 title/description/version/source 元数据。
//!
//! draft（模型自产）插件的工具字节走 `load_inline` 在内存里进同一注册表，与磁盘插件同权
//! 调用、同尺风险。
//!
//! ABI v1（窄而刚，单函数协议）：
//! - 导出 `memory`
//! - 导出 `alloc(size: i32) -> i32`（宿主写输入参数用；插件可忽略）
//! - 导出 `describe() -> i64`（高 32 位=ptr，低 32 位=len；len==0 表示 0 终止字符串）
//!   返回 JSON：{"tools":[{"name","description","schema"}]}
//! - 导出 `run(action_ptr, action_len, payload_ptr, payload_len) -> i64`
//!   action 固定 "call"；payload JSON：{"tool","args"}；返回 JSON：{"text":"..."}（或 {"error"}）
//!
//! **v0 → v1**：v0 把 action 与 payload 写在同一个缓冲区、四个参数传 (ptr,len,ptr,len)，
//! 于是「读第 1/2 个参数」与「读第 3/4 个参数」拿到的都是 payload —— action 形同虚设。
//! v1 用一次 alloc 申请 `action+payload` 连续区（action 在前），四个参数各指其位；
//! 读第 3/4 个参数的插件（含随包 hello.wat，它忽略全部参数）行为不变。
//!
//! 隔离模型：**每次调用新建实例**（无共享状态、无并发问题）；v1 仍不提供任何 host function
//! ——插件是纯计算沙箱，天然无文件/网络边界问题。trap 一律捕获为 Err，绝不连坐宿主。
//!
//! 信任模型：plugins/ 目录文件由用户主动放置（本地可信），工具默认 Normal 档；
//! 未知/加载失败 → Dangerous（与 MCP 防线对齐）。
//!
//! 生命周期（P18 027 定案 5「等完」）：停用 = `set_disabled` 名单 → 工具不再下发
//! （拒新调用）+ `call_tool` 拒之门外；在跑调用是 30s 内自终的独立实例，自然结束即「等完」，
//! 超时击杀兜底——无需任务管理器。

use std::{
    path::{Path, PathBuf},
    sync::{OnceLock, RwLock},
};

use serde_json::{json, Value};
use wasmtime::{Engine, Instance, Module, Store};

use crate::plugin::{contract, ToolBrief};

/// 工具全名前缀（wasm__<plugin>__<tool>）。
pub const PREFIX: &str = "wasm__";
pub const SEP: &str = "__";

fn engine() -> &'static Engine {
    static E: OnceLock<Engine> = OnceLock::new();
    E.get_or_init(Engine::default)
}

/// 已加载插件条目。
#[derive(Debug, Clone)]
pub struct PluginEntry {
    /// 插件名（文件名去扩展名；工具名 wasm__<stem>__<tool>）
    pub stem: String,
    pub file: PathBuf,
    /// (工具名, 描述, schema)
    pub tools: Vec<(String, String, Value)>,
    pub load_error: Option<String>,
    /// P18 契约：来源（built_in / user / model_generated）与元数据
    pub origin: String,
    pub title: String,
    pub description: String,
    pub version: String,
    pub api_version: u32,
    /// draft 插件的内存字节（磁盘插件为 None；call_tool 优先用）
    pub inline: Option<Vec<u8>>,
}

impl PluginEntry {
    fn new(stem: &str, file: PathBuf, tools: Vec<(String, String, Value)>, load_error: Option<String>) -> Self {
        Self {
            stem: stem.to_string(),
            file,
            tools,
            load_error,
            origin: contract::ORIGIN_USER.to_string(),
            title: stem.to_string(),
            description: String::new(),
            version: String::new(),
            api_version: contract::API_VERSION,
            inline: None,
        }
    }
}

static REGISTRY: OnceLock<RwLock<Vec<PluginEntry>>> = OnceLock::new();

static PLUGINS_DIR: OnceLock<PathBuf> = OnceLock::new();

/// P18：停用名单（settings.disabled_extensions 扩面到工具插件）。
/// 停用 = 不下发工具定义（拒新调用）；在跑调用等完（见模块头）。
static DISABLED: OnceLock<RwLock<Vec<String>>> = OnceLock::new();

pub fn set_disabled(list: &[String]) {
    let lock = DISABLED.get_or_init(|| RwLock::new(Vec::new()));
    *lock.write().unwrap_or_else(|e| e.into_inner()) = list.to_vec();
}

pub fn disabled() -> Vec<String> {
    DISABLED
        .get_or_init(|| RwLock::new(Vec::new()))
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn is_disabled(stem: &str) -> bool {
    disabled().iter().any(|d| d == stem)
}

/// 启动时设置插件目录（之后 rescan() 复用）。
pub fn set_plugins_dir(dir: PathBuf) {
    let _ = PLUGINS_DIR.set(dir);
}

/// 插件目录（draft 转正落盘用）。
pub fn plugins_dir() -> Option<PathBuf> {
    PLUGINS_DIR.get().cloned()
}

/// 用已登记的目录重扫（未设置过则空操作）。
pub fn rescan() {
    if let Some(dir) = PLUGINS_DIR.get() {
        scan_and_load(dir);
    }
}

fn registry() -> &'static RwLock<Vec<PluginEntry>> {
    REGISTRY.get_or_init(|| RwLock::new(Vec::new()))
}

/// 注册表里是否有该插件（含 draft inline）。
pub fn has_plugin(stem: &str) -> bool {
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|e| e.stem == stem)
}

/// 扫描目录并加载全部插件（幂等：全量重载）。目录不存在=清空。
/// 兼容两种形态：裸 .wasm/.wat 文件（旧）+ `<name>/manifest.json` 目录（P18 契约形态）。
pub fn scan_and_load(dir: &Path) -> usize {
    let mut out: Vec<PluginEntry> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut files: Vec<PathBuf> = Vec::new();
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                subdirs.push(p);
            } else if matches!(p.extension().and_then(|e| e.to_str()), Some("wasm") | Some("wat")) {
                files.push(p);
            }
        }
        files.sort();
        for f in files {
            let stem = f
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin")
                .to_string();
            out.push(load_plugin(&stem, &f));
        }
        subdirs.sort();
        for d in subdirs {
            if !d.join("manifest.json").is_file() {
                continue;
            }
            let Some(name) = d.file_name().and_then(|s| s.to_str()).map(str::to_string) else { continue };
            out.push(load_dir_plugin(&name, &d));
        }
    }
    out.sort_by(|a, b| a.stem.cmp(&b.stem));
    let n = out.len();
    if let Ok(mut reg) = registry().write() {
        *reg = out;
    }
    n
}

/// 目录形态插件：manifest（契约校验 + 元数据）+ 目录内**恰好一个** .wasm/.wat 工具文件。
fn load_dir_plugin(name: &str, dir: &Path) -> PluginEntry {
    let mut entry = PluginEntry::new(name, dir.to_path_buf(), Vec::new(), None);
    let mf_text = match std::fs::read_to_string(dir.join("manifest.json")) {
        Ok(t) => t,
        Err(e) => {
            entry.load_error = Some(format!("读 manifest.json 失败: {e}"));
            return entry;
        }
    };
    let mf: Value = match serde_json::from_str(&mf_text) {
        Ok(v) => v,
        Err(e) => {
            entry.load_error = Some(format!("manifest.json 不是合法 JSON: {e}"));
            return entry;
        }
    };
    if let Err(e) = contract::check_api_version(&mf) {
        entry.load_error = Some(e); // apiVersion 不匹配 → 拒载（UI 会显示「请升级」）
        return entry;
    }
    entry.origin = contract::origin_of(&mf, "app");
    if let Some(t) = mf.get("title").and_then(Value::as_str) {
        entry.title = t.to_string();
    }
    if let Some(d) = mf.get("description").and_then(Value::as_str) {
        entry.description = d.to_string();
    }
    if let Some(v) = mf.get("version").and_then(Value::as_str) {
        entry.version = v.to_string();
    }
    // 目录内只允许一个工具文件（多个 = 不知道加载哪个，显式报错不静默）
    let mut bins: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("wasm") | Some("wat")))
            .collect(),
        Err(e) => {
            entry.load_error = Some(format!("读目录失败: {e}"));
            return entry;
        }
    };
    bins.sort();
    match bins.len() {
        0 => {
            entry.load_error = Some("目录里没有 .wasm/.wat 工具文件".into());
            entry
        }
        1 => {
            let f = &bins[0];
            match compile_tools(&std::fs::read(f).unwrap_or_default()) {
                Ok(tools) => {
                    entry.tools = tools;
                    entry.file = f.clone();
                    entry
                }
                Err(e) => {
                    entry.load_error = Some(e);
                    entry
                }
            }
        }
        _ => {
            entry.load_error = Some("目录里有多个 .wasm/.wat，只允许一个工具文件".into());
            entry
        }
    }
}

/// 加载单个插件：编译 + 实例化 + describe。
fn load_plugin(stem: &str, file: &Path) -> PluginEntry {
    match std::fs::read(file).map_err(|e| format!("读插件失败: {e}")).and_then(|b| compile_tools(&b)) {
        Ok(tools) => PluginEntry::new(stem, file.to_path_buf(), tools, None),
        Err(e) => PluginEntry::new(stem, file.to_path_buf(), Vec::new(), Some(e)),
    }
}

/// 编译 + describe 拿工具清单（scan / inline / call 共用的入口）。
fn compile_tools(bytes: &[u8]) -> Result<Vec<(String, String, Value)>, String> {
    let module = Module::new(engine(), bytes).map_err(|e| format!("编译失败: {e}"))?;
    let result = with_instance(&module, |store, inst| {
        let mem = inst
            .get_export(&mut *store, "memory")
            .and_then(|e| e.into_memory())
            .ok_or("缺少 memory 导出")?;
        // describe() -> i64
        let describe = inst
            .get_typed_func::<(), i64>(&mut *store, "describe")
            .map_err(|e| format!("缺少 describe 导出: {e}"))?;
        let packed = describe.call(&mut *store, ()).map_err(|e| format!("describe trap: {e}"))?;
        let (ptr, len) = unpack(packed);
        let text = read_cstr(&mem, store, ptr, len)?;
        // 解析工具清单
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("describe JSON 不合法: {e}"))?;
        let tools = v
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|t| {
                let name = t.get("name").and_then(Value::as_str)?.to_string();
                let desc = t.get("description").and_then(Value::as_str).unwrap_or("").to_string();
                let schema = t.get("schema").cloned().unwrap_or_else(|| json!({"type":"object"}));
                Some((name, desc, schema))
            })
            .collect::<Vec<_>>();
        Ok(tools)
    })?;
    if result.is_empty() {
        return Err("describe 未声明任何工具".into());
    }
    Ok(result)
}

/// draft 插件：把内存字节编译进注册表（失败返回 Err，调用方整体拒绝）。
pub fn load_inline(stem: &str, bytes: &[u8]) -> Result<(), String> {
    let tools = compile_tools(bytes)?;
    let mut entry = PluginEntry::new(stem, PathBuf::new(), tools, None);
    entry.origin = contract::ORIGIN_MODEL.to_string();
    entry.inline = Some(bytes.to_vec());
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    // 调用方已查重；这里再 retain 一次保证幂等
    reg.retain(|e| e.stem != stem);
    reg.push(entry);
    Ok(())
}

/// draft 丢弃/转正失败时的回收。
pub fn remove_inline(stem: &str) {
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    reg.retain(|e| !(e.stem == stem && e.inline.is_some()));
}

/// 实例化并执行（每次调用全新 Store/Instance：无状态、无并发、崩不连坐）。
fn with_instance<T>(
    module: &Module,
    f: impl FnOnce(&mut wasmtime::Store<()>, &Instance) -> Result<T, String>,
) -> Result<T, String> {
    let mut store = Store::new(engine(), ());
    let linker = wasmtime::Linker::new(engine()); // v0：零 host function = 纯计算沙箱
    let instance = linker
        .instantiate(&mut store, module)
        .map_err(|e| format!("实例化失败: {e}"))?;
    f(&mut store, &instance)
}

/// 调用插件工具（tools::execute 对 wasm__ 前缀分发到这里）。
/// 停用的插件在此拒之门外（拒新调用）；在跑调用等完（独立实例 30s 自终）。
pub async fn call_tool(full_name: &str, args_json: &str) -> Result<String, String> {
    let (stem, tool) = split_full_name(full_name).ok_or("非法插件工具名")?;
    if is_disabled(&stem) {
        return Err(format!("插件已停用（{stem}）：不接受新调用，可在设置里重新启用"));
    }
    let bytes = {
        let reg = registry().read().unwrap_or_else(|e| e.into_inner());
        let e = reg.iter().find(|e| e.stem == stem).ok_or_else(|| format!("未知插件: {stem}"))?;
        if let Some(err) = &e.load_error {
            return Err(format!("插件加载失败（{stem}）: {err}"));
        }
        match &e.inline {
            Some(b) => b.clone(),
            None => std::fs::read(&e.file).map_err(|e| format!("读插件失败: {e}"))?,
        }
    };
    let module = Module::new(engine(), &bytes).map_err(|e| format!("编译失败: {e}"))?;

    let args: Value = if args_json.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(args_json).map_err(|e| format!("参数不是合法 JSON: {e}"))?
    };
    let payload = json!({ "tool": tool, "args": args }).to_string();

    // 阻塞执行（WASM 纯计算）丢 blocking 池 + 超时 30s
    let fut = tokio::task::spawn_blocking(move || {
        with_instance(&module, |store, inst| {
            let mem = inst
                .get_export(&mut *store, "memory")
                .and_then(|e| e.into_memory())
                .ok_or("缺少 memory 导出")?;
            let alloc = inst
                .get_typed_func::<i32, i32>(&mut *store, "alloc")
                .map_err(|e| format!("缺少 alloc 导出: {e}"))?;
            let run = inst
                .get_typed_func::<(i32, i32, i32, i32), i64>(&mut *store, "run")
                .map_err(|e| format!("缺少 run 导出: {e}"))?;
            // ABI v1：一次 alloc 申请 action+payload 连续区，四个参数各指其位
            let (a_off, a_len, p_off, p_len, buf) = abi_layout("call", &payload);
            let ptr = alloc
                .call(&mut *store, buf.len() as i32)
                .map_err(|e| format!("alloc trap: {e}"))?;
            pipe_write(&mem, &mut *store, ptr, &buf).map_err(|e| format!("写 action/payload: {e}"))?;
            let packed = run
                .call(
                    &mut *store,
                    (
                        ptr + a_off as i32,
                        a_len as i32,
                        ptr + p_off as i32,
                        p_len as i32,
                    ),
                )
                .map_err(|e| format!("run trap: {e}"))?;
            let (rptr, rlen) = unpack(packed);
            let text = read_cstr(&mem, store, rptr, rlen)?;
            let v: Value = serde_json::from_str(&text).map_err(|e| format!("run JSON 不合法: {e}"))?;
            if let Some(err) = v.get("error").and_then(Value::as_str) {
                return Err(format!("WASM 工具报告失败: {err}"));
            }
            Ok(v.get("text")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
                .unwrap_or(text))
        })
    });
    match tokio::time::timeout(std::time::Duration::from_secs(30), fut).await {
        Ok(r) => r.map_err(|e| format!("WASM 调用任务失败: {e}"))?,
        Err(_) => Err("WASM 调用超时（30s）".into()),
    }
}

/// 插件 memory 写入辅助。
fn pipe_write(mem: &wasmtime::Memory, store: &mut wasmtime::Store<()>, ptr: i32, bytes: &[u8]) -> Result<(), String> {
    mem.write(store, ptr as usize, bytes).map_err(|e| format!("memory.write: {e}"))
}

/// 读插件内存字符串：len>0 按长度；len==0 按 0 终止扫描。
fn read_cstr(mem: &wasmtime::Memory, store: &wasmtime::Store<()>, ptr: i32, len: i32) -> Result<String, String> {
    let (mut buf, start) = if len > 0 {
        (vec![0u8; len as usize], ptr as usize)
    } else {
        // 0 终止：从 ptr 读到页尾，找 0
        let size = mem.size(store) as usize * 65536;
        let start = ptr as usize;
        (vec![0u8; size.saturating_sub(start)], start)
    };
    mem.read(store, start, &mut buf)
        .map_err(|e| format!("memory.read: {e}"))?;
    let end = if len > 0 { buf.len() } else { buf.iter().position(|&b| b == 0).unwrap_or(buf.len()) };
    String::from_utf8(buf[..end].to_vec()).map_err(|e| format!("非 UTF-8: {e}"))
}

/// ABI v1 缓冲区布局：`action` 在前、`payload` 紧随其后，返回
/// (action_off, action_len, payload_off, payload_len, 连续区字节)。
/// 纯函数（可单测）：宿主只 alloc 一次，两次写内存合成一次写完。
fn abi_layout(action: &str, payload: &str) -> (usize, usize, usize, usize, Vec<u8>) {
    let a = action.as_bytes();
    let p = payload.as_bytes();
    let mut buf = Vec::with_capacity(a.len() + p.len());
    buf.extend_from_slice(a);
    buf.extend_from_slice(p);
    (0, a.len(), a.len(), p.len(), buf)
}

fn unpack(packed: i64) -> (i32, i32) {
    let ptr = (packed >> 32) as u32 as i32;
    let len = (packed & 0xFFFF_FFFF) as u32 as i32;
    (ptr, len)
}

/// 解析全名；非 wasm__ 前缀返回 None。
pub fn split_full_name(full: &str) -> Option<(String, String)> {
    let rest = full.strip_prefix(PREFIX)?;
    let (stem, tool) = rest.split_once(SEP)?;
    if stem.is_empty() || tool.is_empty() {
        return None;
    }
    Some((stem.to_string(), tool.to_string()))
}

/// 审批档判定（rules.rs wasm__ 分支）：本地可信目录默认 Normal；未知 None → rules 落 Dangerous。
pub fn risk_for(full_name: &str) -> Option<u8> {
    let (stem, _tool) = split_full_name(full_name)?;
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    let e = reg.iter().find(|e| e.stem == stem)?;
    if e.load_error.is_some() {
        return Some(2); // 加载失败的插件不可静默执行
    }
    Some(1)
}

/// 已加载插件的 OpenAI function 定义（停用的不下发 = 拒新调用的第一道）。
pub fn tool_definitions() -> Vec<Value> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    let dis = disabled();
    let mut out = Vec::new();
    for e in reg.iter() {
        if e.load_error.is_some() || dis.iter().any(|d| d == &e.stem) {
            continue;
        }
        let stem = &e.stem;
        for (name, desc, schema) in &e.tools {
            out.push(json!({
                "type": "function",
                "function": {
                    "name": format!("{PREFIX}{stem}{SEP}{name}"),
                    "description": if desc.is_empty() { format!("插件工具（{}）", e.stem) } else { desc.clone() },
                    "parameters": schema,
                }
            }));
        }
    }
    out
}
/// UI 状态快照：(插件名, 加载成功, 工具清单, 加载错误)。
pub fn status() -> Vec<(String, bool, Vec<ToolBrief>, Option<String>)> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    reg.iter()
        .map(|e| {
            (
                e.stem.clone(),
                e.load_error.is_none(),
                e.tools.iter().map(|(n, d, _)| (n.clone(), d.clone())).collect(),
                e.load_error.clone(),
            )
        })
        .collect()
}

/// P18 二合一（027）：工具插件以 UiExt 同构形状进统一清单（/api/ext 一份列表看全部插件）。
/// 纯工具插件 `entry` 为空（前端不渲染「打开」），能力以 `tools` 清单呈现。
pub fn scan_plugins(disabled: &[String]) -> Vec<crate::plugin::ui::UiExt> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    reg.iter()
        .map(|e| {
            let dis = disabled.iter().any(|d| d == &e.stem);
            let inline = e.inline.is_some();
            crate::plugin::ui::UiExt {
                name: e.stem.clone(),
                title: if e.title.is_empty() { e.stem.clone() } else { e.title.clone() },
                description: if e.description.is_empty() && !e.tools.is_empty() {
                    format!("工具插件 · {} 个工具", e.tools.len())
                } else {
                    e.description.clone()
                },
                version: e.version.clone(),
                entry: String::new(),
                source: if inline { "draft".into() } else { "app".into() },
                origin: e.origin.clone(),
                draft: inline,
                api_version: e.api_version,
                shadowed: false,
                dir: if inline {
                    "（内存草稿 · 转正前不入盘，重启即失）".into()
                } else {
                    e.file
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| e.file.display().to_string())
                },
                ok: e.load_error.is_none(),
                error: e.load_error.clone(),
                enabled: !dis,
                ui: Vec::new(),
                permissions: Vec::new(),
                tools: e.tools.iter().map(|(n, d, _)| (n.clone(), d.clone())).collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 沾注册表的测试全部串行（全局静态会被 scan_and_load 整表替换）。
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::plugin::REGISTRY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 测试插件：describe 声明 echo 工具；run 把 payload（第 3/4 个参数指到的那段）
    /// 逐字节抄到 8192 再按显式长度返回——不掺任何业务逻辑，专门用来验证 ABI 契约。
    const ECHO_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (data (i32.const 1024) "{\"tools\":[{\"name\":\"echo\",\"description\":\"echo payload\",\"schema\":{\"type\":\"object\",\"properties\":{\"x\":{\"type\":\"integer\"}}}}]}\00")
  (func (export "alloc") (param $n i32) (result i32) (i32.const 4096))
  (func $pack (param $p i32) (param $l i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $l))))
  (func (export "describe") (result i64)
    (call $pack (i32.const 1024) (i32.const 0)))
  (func (export "run") (param $a1 i32) (param $a2 i32) (param $p1 i32) (param $p2 i32) (result i64)
    (local $i i32)
    (block $done
      (loop $copy
        (br_if $done (i32.ge_u (local.get $i) (local.get $p2)))
        (i32.store8 (i32.add (i32.const 8192) (local.get $i))
                    (i32.load8_u (i32.add (local.get $p1) (local.get $i))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $copy)))
    (call $pack (i32.const 8192) (local.get $p2))))
"#;

    #[test]
    fn abi_layout_separates_action_and_payload() {
        let (a_off, a_len, p_off, p_len, buf) = abi_layout("call", "{\"a\":1}");
        assert_eq!((a_off, a_len), (0, 4));
        assert_eq!((p_off, p_len), (4, 7), "payload 紧跟 action 之后，两段不重叠");
        assert_eq!(&buf[a_off..a_off + a_len], b"call");
        assert_eq!(&buf[p_off..p_off + p_len], b"{\"a\":1}");
        assert_eq!(buf.len(), a_len + p_len);
    }

    /// 端到端（真 wasmtime 实例）：临时目录写 wat → scan_and_load → call_tool。
    /// 插件回吐 payload，宿主必须拿回完整 {"tool","args"}——一次覆盖 describe 解析、
    /// 工具登记、alloc/写内存/参数布局、run 调用、结果回读全链路。
    #[test]
    fn wat_plugin_end_to_end_round_trips_payload() {
        let _g = lock();
        let dir = std::env::temp_dir().join(format!("agent-wasm-abi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("echo.wat"), ECHO_WAT).unwrap();

        let n = scan_and_load(&dir);
        assert_eq!(n, 1, "应扫描到 1 个插件");
        let entry = registry().read().unwrap_or_else(|e| e.into_inner()).first().cloned().expect("注册表应有 echo");
        assert!(entry.load_error.is_none(), "加载不应失败: {:?}", entry.load_error);
        assert_eq!(entry.tools.len(), 1);
        assert_eq!(entry.tools[0].0, "echo");

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let out = rt
            .block_on(call_tool("wasm__echo__echo", r#"{"x":1}"#))
            .expect("插件调用应成功");
        // payload 里的键序由 serde_json 决定，按值断言（同时证明 action 没混进来：
        // 若两段共用指针，这里会回吐 "call" 而解析失败）
        let got: Value = serde_json::from_str(&out).expect("回吐的应是 JSON");
        assert_eq!(got["tool"], "echo");
        assert_eq!(got["args"]["x"], 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P18 契约：目录形态（manifest + 单工具文件）加载、apiVersion 拒载、停用过滤、
    /// draft inline 加载与调用——一次覆盖 027 的四个新行为。
    #[test]
    fn contract_dir_form_disabled_filter_and_inline() {
        let _g = lock();
        let dir = std::env::temp_dir().join(format!("agent-wasm-contract-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 1) 目录形态 + manifest：来源/标题从清单来；工具可调用
        let sub = dir.join("calc");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("manifest.json"),
            r#"{"apiVersion":1,"title":"计算器","description":"demo","version":"2.0.0","source":"user"}"#,
        )
        .unwrap();
        std::fs::write(sub.join("calc.wat"), ECHO_WAT).unwrap();
        // 同目录再放一个 .wat → 应报错（只允许一个工具文件）
        let bad = dir.join("bad-multi");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("manifest.json"), r#"{"apiVersion":1}"#).unwrap();
        std::fs::write(bad.join("a.wat"), ECHO_WAT).unwrap();
        std::fs::write(bad.join("b.wat"), ECHO_WAT).unwrap();
        // apiVersion 不匹配 → 拒载 + 提示升级
        let future = dir.join("future");
        std::fs::create_dir_all(&future).unwrap();
        std::fs::write(future.join("manifest.json"), r#"{"apiVersion":2}"#).unwrap();
        std::fs::write(future.join("f.wat"), ECHO_WAT).unwrap();

        let n = scan_and_load(&dir);
        assert_eq!(n, 3, "目录形态 + 两个坏目录都应进注册表（坏的带 load_error）");
        let reg = registry().read().unwrap_or_else(|e| e.into_inner());
        let calc = reg.iter().find(|e| e.stem == "calc").unwrap();
        assert!(calc.load_error.is_none(), "{:?}", calc.load_error);
        assert_eq!(calc.title, "计算器");
        assert_eq!(calc.origin, contract::ORIGIN_USER);
        assert_eq!(calc.api_version, 1);
        assert!(reg.iter().find(|e| e.stem == "bad-multi").unwrap().load_error.is_some());
        let fut = reg.iter().find(|e| e.stem == "future").unwrap();
        assert!(fut.load_error.as_deref().unwrap().contains("升级"), "拒载要提示升级");
        drop(reg);

        // 2) 停用过滤：不下发 + 拒新调用（在跑调用等完由实例自终保证）
        set_disabled(&["calc".into()]);
        assert!(
            tool_definitions().iter().all(|d| !d["function"]["name"].as_str().unwrap().starts_with("wasm__calc__")),
            "停用插件的工具不应下发"
        );
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let err = rt.block_on(call_tool("wasm__calc__echo", "{}")).unwrap_err();
        assert!(err.contains("已停用"), "{err}");
        // 3) 恢复后可调
        set_disabled(&[]);
        let out = rt.block_on(call_tool("wasm__calc__echo", r#"{"x":2}"#)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["args"]["x"], 2);

        // 4) draft inline：内存字节进同一注册表，调用同权
        load_inline("draft-echo", ECHO_WAT.as_bytes()).unwrap();
        assert!(has_plugin("draft-echo"));
        let out = rt.block_on(call_tool("wasm__draft-echo__echo", r#"{"y":3}"#)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["args"]["y"], 3);
        remove_inline("draft-echo");
        assert!(!has_plugin("draft-echo"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
