//! 执行沙箱 —— 施工单 004（P3）
//!
//! Job Objects 管进程树生命周期与资源上限（kill-on-close 团灭、内存/进程数/CPU 上限、UI 限制），
//! 受限 token 管权限面（剥光全部特权 + 受限 SID 白名单）。真实隔离、不依赖管理员权限。
//! 同步阻塞实现：P4 工具层接入时用 tokio::task::spawn_blocking 包裹调用。
//!
//! 留口（不实现）：AppContainer / 网络出向限制 / 完整性降级（P12）；交互式输入（P4 视需要）。

mod appcontainer;
pub mod backend;
mod job;
mod proc;
pub mod probes;
mod token;

pub use proc::{register_path_dir, SandboxChild};

/// 沙箱资源上限（config.toml [sandbox]）。
#[derive(Debug, Clone, Copy)]
pub struct SandboxLimits {
    /// 墙钟超时：到点整树击杀
    pub timeout_ms: u32,
    /// 单进程 commit 内存上限（MB）
    pub memory_mb: u32,
    /// 进程树内活跃进程数上限
    pub active_processes: u32,
}

/// 一次受限拉起的完整描述（持有所有权，构造方便）。
/// P20 起派生 Clone：tier.spawn 内部按档位归一标志位时要基于调用方的 spec 出副本。
#[derive(Clone)]
pub struct SandboxSpec {
    /// exe 绝对路径（如 C:\Windows\System32\cmd.exe）
    pub application: std::path::PathBuf,
    /// 完整命令行（首 token 为 application 的带引号路径）
    pub command_line: String,
    /// 工作目录（必须已存在）
    pub working_dir: std::path::PathBuf,
    pub limits: SandboxLimits,
    /// true=权限笼（受限 token + 白名单环境 + 资源/UI 上限）；
    /// false=本机直跑（完整用户 token + 继承父环境，仅留超时团灭保险丝）
    pub restricted: bool,
    /// P12 019：AppContainer 模式（LowBox token + 能力 SID；restricted 被忽略）
    pub appcontainer: bool,
    /// P12 019：AppContainer 网络出向（false=断网；true=授予 INTERNET_CLIENT 能力）
    pub allow_network: bool,
}

/// 一次受限运行的产出。
pub struct SandboxOutput {
    pub exit_code: Option<u32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

impl SandboxOutput {
    /// 正常退出且退出码为 0（P4 工具层判定用）。
    #[allow(dead_code)]
    pub fn ok(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// 系统目录下的 cmd.exe 绝对路径（探针 / P4 shell 工具用）。
pub fn system_cmd() -> std::path::PathBuf {
    proc::system_dir().join("cmd.exe")
}

/// 拉起受限进程树；返回可 wait 的句柄。Drop 即团灭。
/// P4 shell 工具接入；探针与测试已在用（bin 构建不链接测试代码故此处告警）。
#[allow(dead_code)]
pub fn spawn(spec: &SandboxSpec) -> anyhow::Result<SandboxChild> {
    proc::spawn(spec)
}

/// spawn + 等待到退出（或超时团灭）。阻塞调用，见模块注释。
#[allow(dead_code)]
pub fn run(spec: &SandboxSpec) -> anyhow::Result<SandboxOutput> {
    let timeout = spec.limits.timeout_ms;
    proc::spawn(spec)?.wait(timeout)
}
