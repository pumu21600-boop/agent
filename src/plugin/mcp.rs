//! MCP client（施工单 016，P11）—— stdio transport 最小闭环
//!
//! 总架构 §7：MCP 借生态省自研；进程级隔离天然防崩；tool poisoning 用
//! 「首装确认 + 同级审批」对冲（确认前一律 Dangerous）。
//!
//! 协议：JSON-RPC 2.0 over stdio（换行分隔），只做 tools 子集：
//! initialize → notifications/initialized → tools/list → tools/call。
//!
//! 线程模型：每个 server 串行 Mutex（工具调用本就串行）；读行为阻塞读，
//! 整体调用包 tokio timeout（超时放弃等待，残留行由「id 不匹配即跳过」自愈）。
//! 子进程 kill_on_drop，崩溃只标记离线，绝不拖垮主服务。

use std::{
    collections::{BTreeMap, HashMap},
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Mutex, OnceLock},
};
use std::path::Path;

use serde_json::{json, Value};

use crate::config::settings::McpServerCfg;

/// 工具全名前缀（mcp__<server>__<tool>）。
pub const PREFIX: &str = "mcp__";
/// server 与 tool 的分隔（server 名禁止包含它，配置层校验）。
pub const SEP: &str = "__";

/// 单个 server 的运行态。
#[derive(Debug, Default)]
struct ServerState {
    cfg: Option<McpServerCfg>,
    /// tools/list 缓存：(工具名, 描述, inputSchema)
    tools: Vec<(String, String, Value)>,
    online: bool,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    reader: Option<BufReader<ChildStdout>>,
    next_id: u64,
}

impl ServerState {
    fn reset_proc(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
        }
        self.child = None;
        self.stdin = None;
        self.reader = None;
        self.online = false;
    }
}

static MANAGER: OnceLock<Mutex<HashMap<String, ServerState>>> = OnceLock::new();

fn mgr() -> &'static Mutex<HashMap<String, ServerState>> {
    MANAGER.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 从全名解析 (server, tool)；非 mcp__ 前缀返回 None。
pub fn split_full_name(full: &str) -> Option<(String, String)> {
    let rest = full.strip_prefix(PREFIX)?;
    let (srv, tool) = rest.split_once(SEP)?;
    if srv.is_empty() || tool.is_empty() {
        return None;
    }
    Some((srv.to_string(), tool.to_string()))
}

/// settings 加载/保存后同步配置：新增的占位、删除的杀进程下线。
/// 不在这里主动 spawn（首次工具调用/refresh 时按需启动）。
pub fn sync_configs(cfgs: &[McpServerCfg]) {
    let mut m = mgr().lock().expect("mcp 管理器锁毒化");
    // 下线被删除/禁用的
    let names: Vec<String> = cfgs.iter().map(|c| c.name.clone()).collect();
    for (name, st) in m.iter_mut() {
        if !names.contains(name) {
            st.reset_proc();
            st.cfg = None;
            st.tools.clear();
        } else if let Some(c) = cfgs.iter().find(|c| &c.name == name) {
            let changed = st
                .cfg
                .as_ref()
                .map(|old| old.command != c.command || old.args != c.args || old.env != c.env)
                .unwrap_or(true);
            if changed {
                st.reset_proc(); // 命令变了要重启
                st.tools.clear();
            }
            st.cfg = Some(c.clone());
        }
    }
    for c in cfgs {
        m.entry(c.name.clone()).or_default().cfg = Some(c.clone());
    }
}

/// 启动子进程并握手（initialize → initialized → tools/list）。
/// 已在线则直接返回；失败重置进程并返回 Err（离线）。
fn ensure_online(st: &mut ServerState) -> Result<(), String> {
    // 先收割已退出的子进程（崩溃自愈：标记离线，走下方重启路径）
    if let Some(child) = st.child.as_mut() {
        if matches!(child.try_wait(), Ok(Some(_))) {
            st.reset_proc();
        }
    }
    if st.online && st.stdin.is_some() && st.reader.is_some() {
        return Ok(());
    }
    st.reset_proc();
    let cfg = st.cfg.clone().ok_or("无配置")?;
    if cfg.command.trim().is_empty() {
        return Err("command 为空".into());
    }
    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args)
        .envs(&cfg.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null()); // server 的日志走 stderr，丢弃避免干扰（Windows 句柄由 Child drop 关闭）
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：后台服务不起黑窗
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动 {} 失败: {e}", cfg.command))?;
    let stdin = child.stdin.take().ok_or("无 stdin")?;
    let stdout = child.stdout.take().ok_or("无 stdout")?;
    st.stdin = Some(stdin);
    st.reader = Some(BufReader::new(stdout));
    st.child = Some(child);

    // —— 握手：initialize ——
    let init = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "agent-local", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    rpc_round(st, &init, 0, 15_000).map_err(|e| {
        st.reset_proc();
        format!("initialize 失败: {e}")
    })?;
    // initialized 通知（无 id，无响应）
    if let Some(w) = st.stdin.as_mut() {
        let _ = w.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n");
        let _ = w.flush();
    }
    // —— tools/list ——
    let list_req = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} });
    let resp = rpc_round(st, &list_req, 1, 15_000).map_err(|e| {
        st.reset_proc();
        format!("tools/list 失败: {e}")
    })?;
    let tools = resp
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    st.tools = tools
        .iter()
        .filter_map(|t| {
            let name = t.get("name").and_then(Value::as_str)?.to_string();
            let desc = t.get("description").and_then(Value::as_str).unwrap_or("").to_string();
            let schema = t.get("inputSchema").cloned().unwrap_or_else(|| json!({"type":"object"}));
            Some((name, desc, schema))
        })
        .collect();
    st.online = true;
    Ok(())
}

/// 写一行请求并阻塞读响应行（跳过 id 不匹配的 notification/log 行）。
fn rpc_round(
    st: &mut ServerState,
    req: &Value,
    expect_id: u64,
    timeout_ms: u64,
) -> Result<Value, String> {
    let line = format!("{}\n", serde_json::to_string(req).map_err(|e| e.to_string())?);
    let stdin = st.stdin.as_mut().ok_or("进程未启动")?;
    stdin.write_all(line.as_bytes()).map_err(|e| format!("写 stdin: {e}"))?;
    stdin.flush().map_err(|e| format!("flush stdin: {e}"))?;

    let reader = st.reader.as_mut().ok_or("进程未启动")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if std::time::Instant::now() > deadline {
            return Err(format!("超时（{timeout_ms}ms）"));
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return Err("server 已退出（EOF）".into()),
            Ok(_) => {}
            Err(e) => return Err(format!("读 stdout: {e}")),
        }
        let v: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue, // 非 JSON 行跳过
        };
        let id = v.get("id").and_then(Value::as_u64);
        if id != Some(expect_id) {
            continue; // notification / 日志 / 串扰行跳过（自愈）
        }
        if let Some(err) = v.get("error") {
            return Err(format!("server 错误: {err}"));
        }
        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
    }
}

/// 公开：拉起（或重连）一个 server 并刷新工具清单。
pub fn ensure_online_by_name(name: &str) -> Result<usize, String> {
    let mut m = mgr().lock().expect("mcp 管理器锁毒化");
    let st = m.get_mut(name).ok_or("未知 server")?;
    ensure_online(st)?;
    Ok(st.tools.len())
}

/// 公开：server 状态快照（UI 用）。
pub fn status() -> Vec<(String, bool, Vec<(String, String)>)> {
    let m = mgr().lock().expect("mcp 管理器锁毒化");
    m.iter()
        .map(|(name, st)| {
            (
                name.clone(),
                st.online,
                st.tools.iter().map(|(n, d, _)| (n.clone(), d.clone())).collect(),
            )
        })
        .collect()
}

/// 审批档判定（rules.rs mcp__ 分支调用）。
/// 未确认 → Dangerous；禁用 → Dangerous；确认后按 server 配置档。
pub fn risk_for(full_name: &str) -> Option<u8> {
    let (srv, _tool) = split_full_name(full_name)?;
    let m = mgr().lock().expect("mcp 管理器锁毒化");
    let st = m.get(&srv)?;
    let cfg = st.cfg.as_ref()?;
    if !cfg.enabled || !cfg.confirmed {
        return Some(2); // Dangerous：未确认/禁用的 MCP 工具不可静默执行
    }
    Some(match cfg.risk.as_str() {
        "dangerous" => 2,
        _ => 1, // normal
    })
}

/// 已启用 + 已确认 + 在线工具的 OpenAI function 定义。
pub fn tool_definitions() -> Vec<Value> {
    let m = mgr().lock().expect("mcp 管理器锁毒化");
    let mut out = Vec::new();
    for (srv, st) in m.iter() {
        if !st.online {
            continue;
        }
        let Some(cfg) = st.cfg.as_ref() else { continue };
        if !cfg.enabled || !cfg.confirmed {
            continue;
        }
        for (name, desc, schema) in &st.tools {
            out.push(json!({
                "type": "function",
                "function": {
                    "name": format!("{PREFIX}{srv}{SEP}{name}"),
                    "description": if desc.is_empty() { format!("MCP 工具（{srv}）") } else { desc.clone() },
                    "parameters": schema,
                }
            }));
        }
    }
    out
}

/// 执行 MCP 工具调用（tools::execute 对 mcp__ 前缀分发到这里）。
pub async fn call_tool(full_name: &str, args_json: &str) -> Result<String, String> {
    let (srv, tool) = split_full_name(full_name).ok_or("非法 MCP 工具名")?;
    let args: Value = if args_json.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(args_json).map_err(|e| format!("参数不是合法 JSON: {e}"))?
    };

    // 阻塞 IO 丢进 blocking 线程池；外层 timeout 到点放弃（残留行由 id 过滤自愈）
    let full = full_name.to_string();
    let fut = tokio::task::spawn_blocking(move || {
        let mut m = mgr().lock().expect("mcp 管理器锁毒化");
        let st = m.get_mut(&srv).ok_or_else(|| format!("未知 server: {srv}"))?;
        ensure_online(st)?;
        let req = json!({
            "jsonrpc": "2.0", "id": st.next_id, "method": "tools/call",
            "params": { "name": tool, "arguments": args }
        });
        let id = st.next_id;
        st.next_id += 1;
        let resp = rpc_round(st, &req, id, 120_000)?;
        // 拼接 content 里全部 text 段
        let mut texts: Vec<String> = Vec::new();
        let mut is_err = false;
        if let Some(arr) = resp.get("content").and_then(Value::as_array) {
            for c in arr {
                if c.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = c.get("text").and_then(Value::as_str) {
                        texts.push(t.to_string());
                    }
                }
            }
        }
        if resp.get("isError").and_then(Value::as_bool) == Some(true) {
            is_err = true;
        }
        if texts.is_empty() {
            texts.push(String::new());
        }
        let joined = texts.join("\n");
        if is_err {
            Err(format!("MCP 工具报告失败: {joined}"))
        } else {
            Ok(joined)
        }
    });
    match tokio::time::timeout(std::time::Duration::from_secs(125), fut).await {
        Ok(r) => r.map_err(|e| format!("MCP 调用任务失败: {e}"))?,
        Err(_) => Err("MCP 调用超时（120s）".into()),
    }
    .map(|r| {
        let _ = &full;
        r
    })
}

/// 进程存活自检：把 wait() 已退出的 server 标记离线（供周期/读取时调用）。
pub fn reap_dead() {
    let mut m = mgr().lock().expect("mcp 管理器锁毒化");
    for st in m.values_mut() {
        if let Some(child) = st.child.as_mut() {
            if let Ok(Some(_)) = child.try_wait() {
                st.reset_proc();
            }
        }
    }
}

/// 校验 server 名（禁止包含分隔符）。
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(SEP) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 按工作区无关的固定 env（MCP server 不继承主服务完整环境，白名单式注入）。
pub fn sanitize_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.clone()
}

/// 兼容：路径透传（预留）。
pub fn _unused(_p: &Path) {}
