//! shell 工具与沙箱命令执行(run_in_sandbox 为 shell / git 两族共用)。

use super::*;
use tokio::task::spawn_blocking;
use crate::sandbox::{self, SandboxLimits, SandboxSpec};

/// shell / git 共用：沙箱（或本机直跑）内跑命令行，格式化输出。
/// P20 沙箱统一：选档交给 SandboxTier 对象（探测链解析出的生效档，含 AC→受限回退），
/// 调用方不再手工拼 restricted/appcontainer 标志位。
pub(crate) async fn run_in_sandbox(
    cfg: &ExecConfig,
    application: PathBuf,
    command_line: String,
    timeout_ms: u32,
) -> Result<String, String> {
    let tier = sandbox::probes::effective_tier(&cfg.sandbox, cfg.unsandboxed);
    let spec = SandboxSpec {
        application,
        command_line,
        working_dir: workspace_root().to_path_buf(),
        limits: SandboxLimits {
            timeout_ms,
            memory_mb: cfg.sandbox.memory_mb,
            active_processes: cfg.sandbox.active_processes,
        },
        restricted: false,
        appcontainer: false,
        allow_network: false,
    };
    let out = spawn_blocking(move || tier.run(&spec))
        .await
        .map_err(|e| format!("沙箱任务失败: {e}"))?
        .map_err(|e| format!("沙箱拉起失败: {e}"))?;
    let mut text = format!("exit={} timed_out={}\n", out.exit_code.unwrap_or(0), out.timed_out);
    if !out.stdout.trim().is_empty() {
        text.push_str("--- stdout ---\n");
        text.push_str(out.stdout.trim_end());
        text.push('\n');
    }
    if !out.stderr.trim().is_empty() {
        text.push_str("--- stderr ---\n");
        text.push_str(out.stderr.trim_end());
        text.push('\n');
    }
    Ok(cap(text.trim_end(), cfg.max_chars))
}

pub(crate) async fn shell(args: &str, cfg: &ExecConfig) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let command = v.get("command").and_then(Value::as_str).ok_or("缺少 command 参数")?;
    if command.trim().is_empty() {
        return Err("command 为空".to_string());
    }
    let timeout_ms = v
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .map(|t| t.clamp(1, 300_000) as u32)
        .unwrap_or(cfg.sandbox.timeout_ms);
    let cmd = sandbox::system_cmd();
    let command_line = format!("\"{}\" /C {command}", cmd.display());
    run_in_sandbox(cfg, cmd, command_line, timeout_ms).await
}
