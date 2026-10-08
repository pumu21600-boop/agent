//! 钩子（施工单 027）—— 用户不改源码即可在固定挂点插入自定义动作。
//!
//! 承载形式：config.toml `[hooks]` 里配本地命令；事件以 JSON 从 stdin 喂给命令，
//! `before_tool` 读 stdout 的 JSON 决定放行与否，其余挂点只作通知。
//!
//! 语义四条（施工单定案 7，执行不得推翻）：
//! 1. **四个挂点**：before_tool / after_tool / session_end / budget_exceeded；
//! 2. **before_tool 只有否决权、没有放行权**——审批门（rules::needs_approval_for 单点）
//!    语义一字不动，钩子 deny ≠ 审批通过，钩子放行 ≠ 免审批；
//! 3. **超时/失败放行**：挂点命令到点（timeout_ms）或拉不起来，一律视为放行并审计——
//!    坏掉的钩子绝不卡死主循环；
//! 4. **全程 JSONL 审计**：hook.* 事件只增不改（021 纪律）。
//!
//! 信任模型：钩子是用户自己在 config.toml 里写的命令（与 git hooks 同级信任），
//! 不进沙箱、不给模型可见性——模型不知道钩子存在，只看得到被拦后的回喂文本。

use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::HooksConfig;
use crate::observability::{try_event, EventType};

static CONFIG: OnceLock<RwLock<Option<HooksConfig>>> = OnceLock::new();

/// 启动时装入钩子配置（main.rs 调一次；测试可直接覆写）。
pub fn install(cfg: HooksConfig) {
    let lock = CONFIG.get_or_init(|| RwLock::new(None));
    *lock.write().unwrap_or_else(|e| e.into_inner()) = Some(cfg);
}

fn cfg() -> Option<HooksConfig> {
    let lock = CONFIG.get_or_init(|| RwLock::new(None));
    lock.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 事件参数截断（write_file 这类大参数只给钩子头部，避免塞爆 stdin）。
fn trunc(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// before_tool 挂点：Err(理由) = 拦下该工具调用（回喂模型）；Ok = 放行。
/// 未配置 / 超时 / 命令失败 / 输出不合法 → 一律放行（语义第 3 条）。
pub async fn before_tool(session: &str, tool: &str, args: &str) -> Result<(), String> {
    let Some(cfg) = cfg() else { return Ok(()) };
    if cfg.before_tool.trim().is_empty() {
        return Ok(());
    }
    let event = json!({
        "hook": "before_tool",
        "session": session,
        "tool": tool,
        "arguments": trunc(args, 2000),
    });
    let outcome = match run_hook_cmd(&cfg.before_tool, &event.to_string(), cfg.timeout_ms).await {
        Ok(Some(stdout)) => parse_decision(&stdout),
        Ok(None) => {
            try_event(EventType::State, json!({ "kind": "hook.before_tool", "tool": tool, "decision": "timeout_pass", "timeout_ms": cfg.timeout_ms }));
            Ok(())
        }
        Err(e) => {
            try_event(EventType::State, json!({ "kind": "hook.before_tool", "tool": tool, "decision": "spawn_error_pass", "error": e }));
            Ok(())
        }
    };
    match &outcome {
        Err(reason) => try_event(
            EventType::State,
            json!({ "kind": "hook.before_tool", "tool": tool, "decision": "deny", "reason": reason }),
        ),
        Ok(()) => try_event(
            EventType::State,
            json!({ "kind": "hook.before_tool", "tool": tool, "decision": "allow" }),
        ),
    }
    outcome
}

/// after_tool 通知：fire-and-forget（不阻塞核心循环；内部自带限时）。
pub fn notify_after_tool(session: &str, tool: &str, ok: bool) {
    fire_and_forget("after_tool", &cfg().map(|c| c.after_tool).unwrap_or_default(), json!({
        "hook": "after_tool",
        "session": session,
        "tool": tool,
        "ok": ok,
    }));
}

/// session_end 挂点：任务结束（Done 或 Cancelled 都算）时调用。
pub async fn session_end(session: &str) {
    let Some(cfg) = cfg() else { return };
    if cfg.session_end.trim().is_empty() {
        return;
    }
    let event = json!({ "hook": "session_end", "session": session });
    let outcome = run_hook_cmd(&cfg.session_end, &event.to_string(), cfg.timeout_ms).await;
    try_event(
        EventType::State,
        json!({ "kind": "hook.session_end", "session": session, "ok": matches!(outcome, Ok(Some(_))) }),
    );
}

/// budget_exceeded 通知：单任务累计 token 超过 [hooks] budget_tokens 时由 server 触发
/// （每次任务最多一次；降级策略本身留成本台账，这里只做通知挂点）。
pub fn notify_budget_exceeded(session: &str, tokens: u64, budget: u64) {
    fire_and_forget(
        "budget_exceeded",
        &cfg().map(|c| c.budget_exceeded).unwrap_or_default(),
        json!({ "hook": "budget_exceeded", "session": session, "tokens": tokens, "budget": budget }),
    );
}

/// 预算是否超限（纯函数便于单测；budget=0 = 不启用）。
pub fn budget_exceeded(total_tokens: u64, budget: u64) -> bool {
    budget > 0 && total_tokens > budget
}

/// 预算阈值（未装配置 = 0 = 不启用）。
pub fn budget_tokens() -> u64 {
    cfg().map(|c| c.budget_tokens).unwrap_or(0)
}

fn fire_and_forget(hook: &str, cmd: &str, event: Value) {
    if cmd.trim().is_empty() {
        return;
    }
    let cmd = cmd.to_string();
    let kind = format!("hook.{hook}");
    let timeout_ms = cfg().map(|c| c.timeout_ms).unwrap_or(5000);
    tokio::spawn(async move {
        let outcome = run_hook_cmd(&cmd, &event.to_string(), timeout_ms).await;
        try_event(
            EventType::State,
            json!({ "kind": kind, "ok": matches!(outcome, Ok(Some(_))) }),
        );
    });
}

/// 跑一个挂点命令：事件 JSON 从 stdin 进，stdout 收回。
/// Ok(Some(stdout)) = 正常退出；Ok(None) = 超时（已随 kill_on_drop 击杀）；Err = 拉起失败。
async fn run_hook_cmd(cmd: &str, event_json: &str, timeout_ms: u64) -> Result<Option<String>, String> {
    use tokio::io::AsyncWriteExt;
    use std::process::Stdio;

    // 本项目只对准 Windows：钩子命令一律走 cmd /C（非 Windows 的 sh -c 分支已撤）
    let mut command = {
        let mut c = tokio::process::Command::new("cmd");
        c.args(["/C", cmd]);
        c
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true); // 超时后 future 被 drop → 子进程被杀，不留孤儿

    let mut child = command.spawn().map_err(|e| format!("拉起钩子命令失败: {e}"))?;
    // 生命周期保险丝（2026-10-08 修）：kill_on_drop 只杀直子进程（cmd.exe），钩子
    // 派生的孙进程（start /b、node 再 spawn）在超时后照样活着占资源/端口——模块头
    // 宣称的「不留孤儿」原先并不成立。挂 Job Object 整树击杀（与 browser/mcp 同一套
    // 保险丝），job 守到本函数返回（含超时路径的 drop）。
    let job = crate::jobguard::ChildJob::new();
    if let Some(j) = job.as_ref()
        && let Some(pid) = child.id()
    {
        j.assign_pid(pid);
    }
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(event_json.as_bytes()).await;
        let _ = si.shutdown().await; // 关 stdin：让命令的 EOF 语义成立
    }
    let dur = Duration::from_millis(timeout_ms.max(1));
    match tokio::time::timeout(dur, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(Some(String::from_utf8_lossy(&out.stdout).to_string())),
        Ok(Err(e)) => Err(format!("钩子命令执行失败: {e}")),
        Err(_) => Ok(None), // 超时 → 放行
    }
}

/// 解析 before_tool 的 stdout 决定：空 = 放行；`{"decision":"deny","reason":"…"}` = 拦下；
/// 其余（含非 JSON 输出）= 放行（钩子输出不合法不升级成故障）。
fn parse_decision(stdout: &str) -> Result<(), String> {
    let s = stdout.trim();
    if s.is_empty() {
        return Ok(());
    }
    let Ok(v) = serde_json::from_str::<Value>(s) else {
        return Ok(()); // 输出不是 JSON：忽略，放行
    };
    if v.get("decision").and_then(Value::as_str) == Some("deny") {
        let reason = v
            .get("reason")
            .and_then(Value::as_str)
            .filter(|r| !r.trim().is_empty())
            .unwrap_or("钩子未说明理由")
            .to_string();
        return Err(reason);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 钩子配置挂全局静态：本文件测试互相串行 + 结束恢复空配置，避免泄漏给其它测试。
    /// 用 tokio Mutex（guard 要跨 await 持有；current-thread 测试串行语义不变）。
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn set(cfg: HooksConfig) {
        install(cfg);
    }

    fn base() -> HooksConfig {
        HooksConfig { timeout_ms: 3000, ..Default::default() }
    }

    /// 在临时目录写一个 .cmd 脚本，回显固定 JSON（Windows 测试钩子）。
    fn script(tag: &str, body: &str) -> String {
        let dir = std::env::temp_dir().join(format!("agent-hooks-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join(format!("{tag}.cmd"));
        std::fs::write(&p, body).unwrap();
        p.display().to_string()
    }

    #[tokio::test]
    async fn before_tool_full_matrix() {
        let _g = TEST_LOCK.lock().await;
        // 1) 未配置 → 放行
        set(base());
        assert!(before_tool("s1", "shell", "{}").await.is_ok());

        // 2) deny：脚本回吐 deny JSON → Err(理由)
        let deny = script("deny", "@echo {\"decision\":\"deny\",\"reason\":\"禁跑 cargo\"}\r\n");
        set(HooksConfig { before_tool: deny, ..base() });
        let err = before_tool("s1", "shell", "{}").await.unwrap_err();
        assert_eq!(err, "禁跑 cargo");

        // 3) 空输出 / 非 JSON 输出 → 放行
        let empty = script("empty", "@exit 0\r\n");
        set(HooksConfig { before_tool: empty, ..base() });
        assert!(before_tool("s1", "shell", "{}").await.is_ok());
        let noisy = script("noisy", "@echo 我不是JSON\r\n");
        set(HooksConfig { before_tool: noisy, ..base() });
        assert!(before_tool("s1", "shell", "{}").await.is_ok());

        // 4) 超时 → 放行（kill_on_drop，不留孤儿）
        let slow = script("slow", "@ping -n 5 127.0.0.1 >nul\r\n");
        set(HooksConfig { before_tool: slow, timeout_ms: 300, ..base() });
        assert!(before_tool("s1", "shell", "{}").await.is_ok());

        // 5) 拉不起来的命令 → 放行
        set(HooksConfig { before_tool: "definitely-not-exist-027.cmd".into(), ..base() });
        assert!(before_tool("s1", "shell", "{}").await.is_ok());

        // 清场：恢复空配置（防其它测试撞上本文件的临时钩子）
        set(HooksConfig::default());
    }

    #[tokio::test]
    async fn session_end_and_decision_parsing() {
        let _g = TEST_LOCK.lock().await;
        set(base());
        session_end("s1").await; // 未配置 = no-op，不炸不卡
        // parse_decision 纯函数矩阵
        assert!(parse_decision("").is_ok());
        assert!(parse_decision("  \n").is_ok());
        assert!(parse_decision("hello").is_ok());
        assert!(parse_decision(r#"{"decision":"allow"}"#).is_ok());
        assert!(parse_decision(r#"{"decision":"deny"}"#).unwrap_err() == "钩子未说明理由");
        assert!(parse_decision(r#"{"decision":"deny","reason":"x"}"#).unwrap_err() == "x");
    }

    #[test]
    fn budget_threshold() {
        assert!(!budget_exceeded(100, 0), "budget=0 = 不启用");
        assert!(!budget_exceeded(100, 100), "恰好等于不算超");
        assert!(budget_exceeded(101, 100));
    }
}
