//! WASM 插件宿主（施工单 017 + 021，P11/P15）—— 总架构 §7「WASM 保深度定制，自带沙箱即边界」
//!
//! 插件形态：`plugins/` 目录下的 .wasm / .wat 文件（wat 由 wasmtime 内置解析，零工具链依赖）。
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

use std::{
    path::{Path, PathBuf},
    sync::{OnceLock, RwLock},
};

use serde_json::{json, Value};
use wasmtime::{Engine, Instance, Module, Store};

use crate::plugin::ToolBrief;

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
}

static REGISTRY: OnceLock<RwLock<Vec<PluginEntry>>> = OnceLock::new();

static PLUGINS_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 启动时设置插件目录（之后 rescan() 复用）。
pub fn set_plugins_dir(dir: PathBuf) {
    let _ = PLUGINS_DIR.set(dir);
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

/// 扫描目录并加载全部插件（幂等：全量重载）。目录不存在=清空。
pub fn scan_and_load(dir: &Path) -> usize {
    let mut out: Vec<PluginEntry> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("wasm") | Some("wat")
                )
            })
            .collect();
        files.sort();
        for f in files {
            let stem = f
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("plugin")
                .to_string();
            let entry = load_plugin(&stem, &f);
            out.push(entry);
        }
    }
    let n = out.len();
    if let Ok(mut reg) = registry().write() {
        *reg = out;
    }
    n
}

/// 加载单个插件：编译 + 实例化 + describe。
fn load_plugin(stem: &str, file: &Path) -> PluginEntry {
    match load_plugin_inner(file) {
        Ok(tools) => PluginEntry { stem: stem.to_string(), file: file.to_path_buf(), tools, load_error: None },
        Err(e) => PluginEntry {
            stem: stem.to_string(),
            file: file.to_path_buf(),
            tools: Vec::new(),
            load_error: Some(e),
        },
    }
}

fn load_plugin_inner(file: &Path) -> Result<Vec<(String, String, Value)>, String> {
    let bytes = std::fs::read(file).map_err(|e| format!("读插件失败: {e}"))?;
    let module = Module::new(engine(), &bytes).map_err(|e| format!("编译失败: {e}"))?;
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
pub async fn call_tool(full_name: &str, args_json: &str) -> Result<String, String> {
    let (stem, tool) = split_full_name(full_name).ok_or("非法 WASM 工具名")?;
    let (module, _entry) = {
        let reg = registry().read().unwrap_or_else(|e| e.into_inner());
        let e = reg.iter().find(|e| e.stem == stem).ok_or_else(|| format!("未知 WASM 插件: {stem}"))?;
        if let Some(err) = &e.load_error {
            return Err(format!("插件加载失败（{stem}）: {err}"));
        }
        (e.file.clone(), e.clone())
    };
    let bytes = std::fs::read(&module).map_err(|e| format!("读插件失败: {e}"))?;
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

/// 已加载插件的 OpenAI function 定义。
pub fn tool_definitions() -> Vec<Value> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    for e in reg.iter() {
        if e.load_error.is_some() {
            continue;
        }
        let stem = &e.stem;
        for (name, desc, schema) in &e.tools {
            out.push(json!({
                "type": "function",
                "function": {
                    "name": format!("{PREFIX}{stem}{SEP}{name}"),
                    "description": if desc.is_empty() { format!("WASM 插件工具（{}）", e.stem) } else { desc.clone() },
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
