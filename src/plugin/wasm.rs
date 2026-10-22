//! WASM 插件宿主（施工单 017 / 021 / 027）—— 总架构 §7
//!
//! ** 定位调整**：WASM 不再是一种「插件」，降为插件工具的一种可选实现 / 沙箱轻量档。
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
//!   action 固定 "call"；payload JSON：{"tool","args"}；返回首选 JSON：{"text":"..."}
//!   （或 {"error"}）；v1.1 起解析不出 JSON 或无 text/error 字段时按纯文本透传
//!   （宿主函数结果直转的插件——http_get 响应体、fs_read 文件内容——无需自己拼 JSON）
//!
//! **v0 → v1**：v0 把 action 与 payload 写在同一个缓冲区、四个参数传 (ptr,len,ptr,len)，
//! 于是「读第 1/2 个参数」与「读第 3/4 个参数」拿到的都是 payload —— action 形同虚设。
//! v1 用一次 alloc 申请 `action+payload` 连续区（action 在前），四个参数各指其位；
//! 读第 3/4 个参数的旧插件行为不变。
//!
//! 隔离模型：**每次调用新建实例**（无共享状态、无并发问题）。宿主函数按 manifest
//! 权限声明有选择地链接（2026-10-22 当前批次条目 5，见 wasm_host.rs：storage /
//! fs_read / http 三个口子）——未声明权限的插件、裸 .wasm/.wat 与模型自产 draft
//! 仍是纯计算沙箱。trap 一律捕获为 Err，绝不连坐宿主。
//!
//! 信任模型：plugins/ 目录文件由用户主动放置（本地可信），工具默认 Normal 档；
//! 未知/加载失败 → Dangerous（与 MCP 防线对齐）。
//!
//! 生命周期（027 定案 5「等完」）：停用 = `set_disabled` 名单 → 工具不再下发
//! （拒新调用）+ `call_tool` 拒之门外；在跑调用是 30s 内自终的独立实例，自然结束即「等完」，
//! 超时击杀兜底——无需任务管理器。

use std::{
    path::{Path, PathBuf},
    sync::{OnceLock, RwLock},
};

use serde_json::{json, Value};
use wasmtime::{Engine, Instance, Module, Store};

use crate::plugin::wasm_host::{self, HostCtx, HostPerms};
use crate::plugin::{contract, ToolBrief};

/// 工具全名前缀（wasm__<plugin>__<tool>）。
pub const PREFIX: &str = "wasm__";
pub const SEP: &str = "__";

/// 插件实例资源闸（2026-10-08 修）：此前 Store 无任何资源上限——插件导出超大
/// memory 时 read_cstr 会跟着分配；死循环只能等 30s 墙钟超时，而 spawn_blocking
/// 的任务超时后仍在跑，反复调用可耗尽 blocking 池。两道闸：
/// ① StoreLimits：memory 64MB / 1 个实例 / 1 个 memory / 表 1 万元素，超限的
///    grow 按 spec 返回 -1（不 trap 不崩，实测）；
/// ② 并发信号量：同时最多 4 个 WASM 调用占 blocking 池（见 call_tool）。
///
/// **为什么不设燃料 / epoch 中断**（实测结论，2026-10-08，勿「顺手修好」）：
/// wasmtime 24.0.13 在本机（Windows）上，`consume_fuel` 耗尽与 `epoch_interruption`
/// 触发都走 libcall 陷阱路径，会以 "panic in a function that cannot unwind" 直接
/// 炸掉整个进程（debug/release 均复现；内联陷阱如 unreachable 正常）。开燃料
/// =死循环插件从「空转 30s」升级为「炸宿主」，比不加更糟，故弃用。死循环的兜底
/// 是 30s 墙钟超时 + 信号量封顶（最坏 4 条 blocking 线程被占，每条 30s 自终）。
pub(crate) const WASM_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// 0 终止返回值最多扫 1MB（len==0 的老路径；v1 ABI 都带显式长度）。
const WASM_CSTR_SCAN_MAX: usize = 1024 * 1024;
/// 同时执行的 WASM 调用上限（纯计算沙箱，4 并发对合法插件绰绰有余；
/// 封顶则恶意插件最坏也只能占住 4 条 blocking 线程，每条 30s 自终）。
const WASM_MAX_CONCURRENT: usize = 4;

pub(crate) fn engine() -> &'static Engine {
    static E: OnceLock<Engine> = OnceLock::new();
    E.get_or_init(Engine::default)
}

/// WASM 调用并发闸（进程级）：拿不到许可的调用在 async 侧排队等待，
/// 不占 blocking 池线程（等待期间父任务取消也能立即返回，见 call_tool）。
fn wasm_semaphore() -> &'static tokio::sync::Semaphore {
    static S: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    S.get_or_init(|| tokio::sync::Semaphore::new(WASM_MAX_CONCURRENT))
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
    /// 契约：来源（built_in / user / model_generated）与元数据
    pub origin: String,
    pub title: String,
    pub description: String,
    pub version: String,
    pub api_version: u32,
    /// draft 插件的内存字节（磁盘插件为 None；call_tool 优先用）
    pub inline: Option<Vec<u8>>,
    /// 宿主函数权限（manifest permissions 声明 → 链接器授予；缺省全无 = 纯沙箱）
    pub perms: HostPerms,
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
            perms: HostPerms::default(),
        }
    }
}

static REGISTRY: OnceLock<RwLock<Vec<PluginEntry>>> = OnceLock::new();

static PLUGINS_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 停用名单（settings.disabled_plugins 扩面到工具插件）。
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
/// 兼容两种形态：裸 .wasm/.wat 文件（旧）+ `<name>/manifest.json` 目录（契约形态）。
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
    // 红线3：本注册表只扫**应用根** plugins/（main.rs 启动时传入，随包内置位），
    // 位置即 app——与 ui::scan 对同一目录的判定保持一致（否则同一插件面板免验签、
    // 工具半边要 pin）。workspace/plugins 的目录形态不过这里（ui::scan 管面板与技能）。
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
    // 宿主函数权限（条目 5）：声明过闸——未知权限名拒载（防拼错静默失效），
    // 链接器只授予声明过的函数（导入未声明的宿主函数 = 实例化失败）
    match HostPerms::from_manifest(&mf) {
        Ok(p) => entry.perms = p,
        Err(e) => {
            entry.load_error = Some(e);
            return entry;
        }
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
        // 合并后一个插件一个目录：纯面板插件（manifest 只声明面板、没有工具
        // 半边）合法——工具清单留空不算加载错误，「打开」半边由 ui 扫描负责。
        0 => entry,
        1 => {
            let f = &bins[0];
            match compile_tools(&std::fs::read(f).unwrap_or_default(), &entry.perms, name) {
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

/// 加载单个插件：编译 + 实例化 + describe。裸文件无 manifest → 零宿主权限。
fn load_plugin(stem: &str, file: &Path) -> PluginEntry {
    let none = HostPerms::default();
    match std::fs::read(file).map_err(|e| format!("读插件失败: {e}")).and_then(|b| compile_tools(&b, &none, stem)) {
        Ok(tools) => PluginEntry::new(stem, file.to_path_buf(), tools, None),
        Err(e) => PluginEntry::new(stem, file.to_path_buf(), Vec::new(), Some(e)),
    }
}

/// 编译 + describe 拿工具清单（scan / inline / call 共用的入口）。
/// describe 阶段的宿主上下文给占位值（清单阶段不该碰 fs/http——碰了只会拿到
/// 占位错误串，不影响 describe 返回工具清单；storage 命名空间按 stem 正确隔离）。
fn compile_tools(bytes: &[u8], perms: &HostPerms, stem: &str) -> Result<Vec<(String, String, Value)>, String> {
    let module = Module::new(engine(), bytes).map_err(|e| format!("编译失败: {e}"))?;
    let ctx = HostCtx {
        stem: stem.to_string(),
        ws: crate::tools::workspace_root().to_path_buf(),
        handle: tokio::runtime::Handle::try_current().ok(),
    };
    let result = with_instance(&module, perms, &ctx, |store, inst| {
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
/// draft 是模型自产——恒为零宿主权限纯沙箱（导入宿主函数会在 describe 实例化时
/// 失败，等于整体拒绝，提示模型去掉 import）。
pub fn load_inline(stem: &str, bytes: &[u8]) -> Result<(), String> {
    let none = HostPerms::default();
    let tools = compile_tools(bytes, &none, stem)?;
    let mut entry = PluginEntry::new(stem, PathBuf::new(), tools, None);
    entry.origin = contract::ORIGIN_MODEL.to_string();
    entry.inline = Some(bytes.to_vec());
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    // 调用方已查重；这里再 retain 一次保证幂等
    reg.retain(|e| e.stem != stem);
    reg.push(entry);
    // 保持注册表 stem 名字序（tool_definitions 也排，这里双保险）——
    // 乱序会让 tools 定义数组变形，打翻 Anthropic 的 system→tools 缓存前缀
    reg.sort_by(|a, b| a.stem.cmp(&b.stem));
    Ok(())
}

/// draft 丢弃/转正失败时的回收。
pub fn remove_inline(stem: &str) {
    let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
    reg.retain(|e| !(e.stem == stem && e.inline.is_some()));
}

/// 实例化并执行（每次调用全新 Store/Instance：无状态、无并发、崩不连坐）。
/// 资源闸：StoreLimits 钉死内存/实例数（燃料不可用，见常量区注释）。
/// 宿主函数按 perms 授予（wasm_host::build_linker）：未声明的宿主导入在此实例化失败。
fn with_instance<T>(
    module: &Module,
    perms: &HostPerms,
    ctx: &HostCtx,
    f: impl FnOnce(&mut wasmtime::Store<wasmtime::StoreLimits>, &Instance) -> Result<T, String>,
) -> Result<T, String> {
    let limits = wasmtime::StoreLimitsBuilder::new()
        .instances(1)
        .memories(1)
        .table_elements(10_000)
        .memory_size(WASM_MEMORY_LIMIT)
        .build();
    let mut store = Store::new(engine(), limits);
    store.limiter(|l| l); // StoreLimits: ResourceLimiter——超限的 grow/实例化直接失败
    let linker = wasm_host::build_linker(perms, ctx);
    let instance = linker
        .instantiate(&mut store, module)
        .map_err(|e| {
            let s = e.to_string();
            if s.contains("host::") {
                format!(
                    "实例化失败（宿主函数未授权）: {s}——插件导入的宿主函数必须先在 manifest.json 的 permissions 里声明"
                )
            } else {
                format!("实例化失败: {s}")
            }
        })?;
    f(&mut store, &instance)
}

/// 调用插件工具（tools::execute 对 wasm__ 前缀分发到这里）。
/// 停用的插件在此拒之门外（拒新调用）；在跑调用等完（独立实例 30s 自终）。
/// `ws_root` = 本任务钉住的工作区（fs_read 宿主函数判界用它——后台任务运行中
/// 用户切了全局工作区不会串界，与判级基准同一条纪律）。
pub async fn call_tool(full_name: &str, args_json: &str, ws_root: &Path) -> Result<String, String> {
    let (stem, tool) = split_full_name(full_name).ok_or("非法插件工具名")?;
    if is_disabled(&stem) {
        return Err(format!("插件已停用（{stem}）：不接受新调用，可在设置里重新启用"));
    }
    let (bytes, perms) = {
        let reg = registry().read().unwrap_or_else(|e| e.into_inner());
        let e = reg.iter().find(|e| e.stem == stem).ok_or_else(|| format!("未知插件: {stem}"))?;
        if let Some(err) = &e.load_error {
            return Err(format!("插件加载失败（{stem}）: {err}"));
        }
        let bytes = match &e.inline {
            Some(b) => b.clone(),
            None => std::fs::read(&e.file).map_err(|e| format!("读插件失败: {e}"))?,
        };
        (bytes, e.perms)
    };
    let module = Module::new(engine(), &bytes).map_err(|e| format!("编译失败: {e}"))?;
    // 异步上下文里先抓运行时句柄（http 宿主函数在 blocking 线程上驱动网络闸用；
    // 纯计算/storage/fs 路径不碰它，拿不到也不影响）
    let ctx = HostCtx {
        stem: stem.clone(),
        ws: ws_root.to_path_buf(),
        handle: tokio::runtime::Handle::try_current().ok(),
    };

    let args: Value = if args_json.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(args_json).map_err(|e| format!("参数不是合法 JSON: {e}"))?
    };
    let payload = json!({ "tool": tool, "args": args }).to_string();

    // 并发闸：最多 WASM_MAX_CONCURRENT 个调用同时占 blocking 池（恶意插件写死
    // 循环时最坏也只拖住 4 条线程，每条 30s 自终）。许可在 async 侧等，不占线程。
    let _permit = wasm_semaphore()
        .acquire()
        .await
        .map_err(|e| format!("WASM 并发闸已关闭: {e}"))?;
    // 阻塞执行（WASM 纯计算）丢 blocking 池 + 超时 30s
    let fut = tokio::task::spawn_blocking(move || {
        with_instance(&module, &perms, &ctx, |store, inst| {
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
            // v1.1 透传语义：run 返回首选 JSON {"text":"..."} / {"error":"..."}；
            // 解析不出 JSON（宿主函数结果直转的常见形态，如 http_get 的响应体）
            // 或 JSON 里没有 text/error 字段时，按纯文本原样返回。
            let v: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => return Ok(text),
            };
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
fn pipe_write<T>(mem: &wasmtime::Memory, store: &mut wasmtime::Store<T>, ptr: i32, bytes: &[u8]) -> Result<(), String> {
    mem.write(store, ptr as usize, bytes).map_err(|e| format!("memory.write: {e}"))
}

/// 读插件内存字符串：len>0 按长度；len==0 按 0 终止扫描。
fn read_cstr<T>(mem: &wasmtime::Memory, store: &wasmtime::Store<T>, ptr: i32, len: i32) -> Result<String, String> {
    let (mut buf, start) = if len > 0 {
        // 2026-10-08 修：len 来自插件 unpack 的返回值（不可信）——i32::MAX 会先
        // vec![0u8; 2GB] 把宿主 OOM/abort，mem.read 随后才报错。插件 memory 上限
        // 64MB，合法返回值不可能超过它；超了按恶意/劣质插件拒。
        let len = len as usize;
        if len > WASM_MEMORY_LIMIT {
            return Err(format!("插件返回的长度 {len} 超过 memory 上限（{WASM_MEMORY_LIMIT}），已拒读"));
        }
        (vec![0u8; len], ptr as usize)
    } else {
        // 0 终止：从 ptr 读到页尾，找 0（扫描窗封顶 1MB——插件导出 64MB memory
        // 时不再一次性分配整个内存，2026-10-08 修）
        let size = mem.size(store) as usize * 65536;
        let start = ptr as usize;
        let want = size.saturating_sub(start).min(WASM_CSTR_SCAN_MAX);
        (vec![0u8; want], start)
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
/// **顺序确定**（缓存前缀地基，同 MCP 侧）：注册表顺序可能被 load_inline 追加 /
/// rescan 局部替换打乱，而 tools 数组是 Anthropic 协议里 system 之后、messages
/// 之前的稳定区——顺序一变整个前缀作废、缓存全丢。下发前按 stem 名字序排一遍。
pub fn tool_definitions() -> Vec<Value> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    let dis = disabled();
    let mut entries: Vec<&PluginEntry> = reg.iter().collect();
    entries.sort_by(|a, b| a.stem.cmp(&b.stem));
    let mut out = Vec::new();
    for e in entries {
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

/// 目录合并：工具半边转成统一清单（与 ui::scan 同目录同形态，由 ui::scan_all
/// 合成一份）。纯面板的目录在这里 tools 为空、**不报错**（起合法形态）；
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
                skills: Vec::new(), // 工具插件不声明技能（技能走 ui::skill_roots 扫描目录形态）
                settings_entry: String::new(), // 工具插件没有设置页
                feature: String::new(), // 工具插件不绑功能开关（面板插件才绑）
                popup: false, // 工具插件没有面板页
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
            .block_on(call_tool("wasm__echo__echo", r#"{"x":1}"#, &std::env::temp_dir()))
            .expect("插件调用应成功");
        // payload 里的键序由 serde_json 决定，按值断言（同时证明 action 没混进来：
        // 若两段共用指针，这里会回吐 "call" 而解析失败）
        let got: Value = serde_json::from_str(&out).expect("回吐的应是 JSON");
        assert_eq!(got["tool"], "echo");
        assert_eq!(got["args"]["x"], 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 契约：目录形态（manifest + 单工具文件）加载、apiVersion 拒载、停用过滤、
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
        let err = rt.block_on(call_tool("wasm__calc__echo", "{}", &std::env::temp_dir())).unwrap_err();
        assert!(err.contains("已停用"), "{err}");
        // 3) 恢复后可调
        set_disabled(&[]);
        let out = rt.block_on(call_tool("wasm__calc__echo", r#"{"x":2}"#, &std::env::temp_dir())).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["args"]["x"], 2);

        // 4) draft inline：内存字节进同一注册表，调用同权
        load_inline("draft-echo", ECHO_WAT.as_bytes()).unwrap();
        assert!(has_plugin("draft-echo"));
        let out = rt.block_on(call_tool("wasm__draft-echo__echo", r#"{"y":3}"#, &std::env::temp_dir())).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["args"]["y"], 3);
        remove_inline("draft-echo");
        assert!(!has_plugin("draft-echo"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 工具定义顺序必须确定（缓存前缀地基，与 MCP 侧同规）：注册表被直接塞进
    /// 乱序条目（真实路径 scan_and_load 虽排序，但 inline/draft 装载与 rescan
    /// 都可能打乱），下发的定义仍必须按 stem 名字序——顺序一变，Anthropic 协议里
    /// system→tools 的整个稳定区前缀作废、缓存全丢。
    #[test]
    fn tool_definitions_sorted_by_stem() {
        let _g = lock();
        set_disabled(&[]);
        let mk = |stem: &str| {
            PluginEntry::new(
                stem,
                std::env::temp_dir().join(format!("{stem}.wasm")),
                vec![("echo".to_string(), "d".to_string(), json!({"type":"object"}))],
                None,
            )
        };
        {
            let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
            reg.clear();
            // 故意乱序 + 一个加载失败的（必须被过滤，排序不能把过滤规则绕掉）
            reg.push(mk("zeta"));
            reg.push(mk("alpha"));
            reg.push(mk("mid"));
            let mut broken = mk("broken");
            broken.load_error = Some("炸了".into());
            reg.push(broken);
        }
        let defs = tool_definitions();
        let names: Vec<&str> = defs
            .iter()
            .map(|d| d.pointer("/function/name").and_then(Value::as_str).unwrap_or(""))
            .collect();
        assert_eq!(
            names,
            vec!["wasm__alpha__echo", "wasm__mid__echo", "wasm__zeta__echo"],
            "按插件名序下发，加载失败的被过滤"
        );
        {
            let mut reg = registry().write().unwrap_or_else(|e| e.into_inner());
            reg.clear();
        }
    }
    /// （不 trap 不崩——实测；燃料/epoch 在本平台会炸进程，见常量区注释故不设）。
    /// 直接走 with_instance 验证限额接线，不经插件注册表。
    #[test]
    fn memory_limit_denies_growth_past_64mb() {
        let wat = "(module (memory (export \"memory\") 1) \
             (func (export \"grow\") (param $n i32) (result i32) (memory.grow (local.get $n))) \
             (func (export \"size\") (result i32) (memory.size)))";
        let module = Module::new(engine(), wat).unwrap();
        let ctx = HostCtx { stem: String::new(), ws: std::env::temp_dir(), handle: None };
        with_instance(&module, &HostPerms::default(), &ctx, |store, inst| {
            let grow = inst
                .get_typed_func::<i32, i32>(&mut *store, "grow")
                .map_err(|e| e.to_string())?;
            let size = inst
                .get_typed_func::<(), i32>(&mut *store, "size")
                .map_err(|e| e.to_string())?;
            // 限额内：成功，返回旧大小（页）
            let old = grow.call(&mut *store, 100).map_err(|e| e.to_string())?;
            assert!(old >= 0, "限额内 grow 应成功: {old}");
            // 超上限（再要 10 万页 = 640MB）：按 spec 返回 -1，不 trap
            let denied = grow.call(&mut *store, 100_000).map_err(|e| e.to_string())?;
            assert_eq!(denied, -1, "超 64MB 的 grow 必须被拒（返回 -1）");
            // memory 实际没涨上去
            let pages = size.call(&mut *store, ()).map_err(|e| e.to_string())?;
            assert!(pages < 1000, "memory 页数应停在限额内: {pages}");
            Ok(())
        })
        .unwrap();
    }

    /// 条目 5（宿主函数）：① 权限闸——导入宿主函数但 manifest 未声明 → 拒载且
    /// 错误点名「宿主函数未授权」；② draft（模型自产）恒为零权限，导入宿主函数
    /// 直接整体拒绝；③ 三个随包示例插件（wasm-rate/wasm-readfile/wasm-notepad）
    /// 从仓库 plugins/ 真装载真调用——notepad 跨两次调用（两个独立实例）读回
    /// 同一值证明持久化落在宿主侧，readfile 按传入工作区读文件。
    #[test]
    fn host_permission_gate_and_shipped_examples() {
        let _g = lock();
        // ① 权限闸：manifest 不声明 permissions + wat 导入 host.storage_get → 拒载
        let dir = std::env::temp_dir().join(format!("agent-wasm-hostgate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gate = dir.join("gate");
        std::fs::create_dir_all(&gate).unwrap();
        std::fs::write(gate.join("manifest.json"), r#"{"apiVersion":1,"title":"闸"}"#).unwrap();
        std::fs::write(
            gate.join("t.wat"),
            r#"(module (import "host" "storage_get" (func $g (param i32 i32) (result i64)))
                (memory (export "memory") 1)
                (func (export "alloc") (param $n i32) (result i32) (i32.const 4096))
                (func (export "describe") (result i64) (i64.const 0))
                (func (export "run") (param i32 i32 i32 i32) (result i64) (call $g (i32.const 0) (i32.const 0))))"#,
        )
        .unwrap();
        scan_and_load(&dir);
        {
            let reg = registry().read().unwrap_or_else(|e| e.into_inner());
            let err = reg.iter().find(|e| e.stem == "gate").unwrap().load_error.clone().unwrap();
            assert!(err.contains("宿主函数未授权"), "权限闸错误要说人话: {err}");
        }
        // ② draft 拒宿主导入：同款 wat 走 load_inline（无 manifest = 零权限）
        assert!(load_inline("draft-gate", std::fs::read(gate.join("t.wat")).unwrap().as_slice()).is_err());
        let _ = std::fs::remove_dir_all(&dir);

        // ③ 三个随包示例：仓库 plugins/ 真装载
        let plugins = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins");
        scan_and_load(&plugins);
        {
            let reg = registry().read().unwrap_or_else(|e| e.into_inner());
            for (stem, expect_tools) in [
                ("wasm-rate", vec!["rate"]),
                ("wasm-readfile", vec!["read"]),
                ("wasm-notepad", vec!["save", "load"]),
            ] {
                let e = reg.iter().find(|e| e.stem == stem)
                    .unwrap_or_else(|| panic!("示例插件 {stem} 应在注册表"));
                assert!(e.load_error.is_none(), "{stem} 加载失败: {:?}", e.load_error);
                let names: Vec<&str> = e.tools.iter().map(|(n, _, _)| n.as_str()).collect();
                assert_eq!(names, expect_tools, "{stem} 工具清单");
            }
            // 回归锁：面板插件（board 等带面板 permissions 的目录）在工具注册表里
            // 不得因 permissions 数组被宿主权限解析误判而背上 load_error
            // （宽容解析修复前的严格版会把 composer.insert 当未知权限拒载）
            if let Some(board) = reg.iter().find(|e| e.stem == "board") {
                assert!(board.load_error.is_none(), "面板插件不得被宿主权限解析误伤: {:?}", board.load_error);
            }
        }

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        // notepad：save 写 → 跨实例 load 读回（持久化在宿主侧）
        let saved = rt
            .block_on(call_tool("wasm__wasm-notepad__save", r#"{"text":"你好 Pawch，宿主函数真跑通"}"#, &std::env::temp_dir()))
            .expect("save 应成功");
        assert_eq!(saved, "你好 Pawch，宿主函数真跑通", "save 读回保存值（纯文本透传）");
        let loaded = rt
            .block_on(call_tool("wasm__wasm-notepad__load", "{}", &std::env::temp_dir()))
            .expect("load 应成功");
        assert_eq!(loaded, "你好 Pawch，宿主函数真跑通", "跨实例读回 = 宿主侧持久化");

        // readfile：按传入的工作区读文件 + 越界拒绝
        let ws = std::env::temp_dir().join(format!("agent-wasm-rf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("note.txt"), "工作区里的文本").unwrap();
        let out = rt
            .block_on(call_tool("wasm__wasm-readfile__read", r#"{"path":"note.txt"}"#, &ws))
            .expect("readfile 应成功");
        assert_eq!(out, "工作区里的文本");
        let esc = rt
            .block_on(call_tool("wasm__wasm-readfile__read", r#"{"path":"../../secret.txt"}"#, &ws))
            .expect_err("越界路径应报错");
        assert!(esc.contains("越出工作区") || esc.contains("不存在"), "{esc}");
        let _ = std::fs::remove_dir_all(&ws);

        // 清理宿主侧存储（plugin_data/wasm-notepad——app_root 在测试态回落仓库根）
        let _ = std::fs::remove_dir_all(crate::storage::plugin_root().join("wasm-notepad"));
    }

    /// 并发闸不泄滞：超过 WASM_MAX_CONCURRENT 的并发调用应全部正常完成
    /// （许可 acquire/释放配对；漏释放会在这里挂死）。
    ///
    /// 注册表是进程级静态：`scan_and_load` 整表替换，别的测试跑一次就把 echo 清掉，
    /// 于是「未知插件: echo」。所以本测试全程持锁（装载 → 并发调用 → 收尾），
    /// `MutexGuard` 不是 Send 不能跨 await，锁交给常驻同步线程持着，主线程只在
    /// await 里等它发话。
    #[tokio::test]
    async fn concurrent_calls_all_complete_without_deadlock() {
        let dir = std::env::temp_dir().join(format!("agent-wasm-conc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("echo.wat"), ECHO_WAT).unwrap();

        // 持锁线程：拿锁 → 装载 → 等收尾信号。装载结果经 channel 回报给主线程。
        let dir2 = dir.clone();
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<usize>();
        let holder = std::thread::spawn(move || {
            let _g = crate::plugin::REGISTRY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let n = scan_and_load(&dir2);
            let _ = ready_tx.send(n);
            let _ = hold_rx.recv();
        });
        assert_eq!(ready_rx.recv().expect("持锁线程应回报装载结果"), 1, "应装载 1 个插件");

        let mut handles = Vec::new();
        for _ in 0..WASM_MAX_CONCURRENT * 3 {
            handles.push(tokio::spawn(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    call_tool("wasm__echo__echo", r#"{"x":1}"#, &std::env::temp_dir()),
                )
                .await
            }));
        }
        let mut all_ok = true;
        for h in handles {
            let r = h.await.unwrap().expect("并发调用超时（许可泄漏？）");
            if r.is_err() {
                all_ok = false;
                eprintln!("并发调用失败: {:?}", r.err());
            }
        }
        let _ = hold_tx.send(());
        holder.join().expect("持锁线程异常");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(all_ok, "并发调用应全部成功");
    }
}



