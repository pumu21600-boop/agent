//! 沙箱后端统一接口（P20 沙箱统一）—— trait 包住既有三档。
//!
//! 三档隔离实现早已建成并经探针矩阵真机验证（Job Objects / 受限 token / AppContainer），
//! 此前由调用方手工拼 SandboxSpec 的 restricted/appcontainer/allow_network 标志位；
//! 现在统一为 [`SandboxTier`] 对象：工具层选档 = 拿一个 trait 对象，不再关心标志位怎么拼。
//!
//! 跨平台留口（bwrap / Seatbelt）：Linux/macOS 档评估过 `ai-sandbox` 0.2.1 crate
//! （MIT、单维护者、Windows 档只是 Restricted Token 与我们自有实现同级）——本机无法
//! 验证 bwrap/Seatbelt 行为，引入即不可测试代码，按窄而刚纪律**不引入**；本 trait 即
//! 插槽，未来在能真机验证的机器上实现 `LinuxBwrapTier` / `MacosSeatbeltTier` 即接入。
//! 评估记录见 docs/ROADMAP.md P20 行。

use super::{proc, probes::{self, ProbeResult}, SandboxChild, SandboxLimits, SandboxOutput, SandboxSpec};

/// 一个沙箱档位 = 一种「怎么把进程关进笼子」的实现。
pub trait SandboxTier: Send + Sync {
    /// 档位 id（config [sandbox].mode 取值口径："unsandboxed" / "restricted" / "appcontainer"）
    fn id(&self) -> &'static str;
    /// 中文标签（UI 明示用）
    fn label(&self) -> &'static str;
    /// 隔离面一句话说明（UI 明示用）
    fn describe(&self) -> String;
    /// 按本档位的隔离方式拉起进程树。spec 里的 restricted/appcontainer/allow_network
    /// 由实现自行校正——调用方不许再手工拼标志位。
    fn spawn(&self, spec: &SandboxSpec) -> anyhow::Result<SandboxChild>;
    /// 本档位的快速探测链（真实拉起进程自证隔离面；慢探针只进 cargo test）。
    fn probe_chain(&self, limits: SandboxLimits) -> Vec<ProbeResult>;

    /// spawn + 等待到退出（或超时团灭）。阻塞调用，调用方包 spawn_blocking。
    fn run(&self, spec: &SandboxSpec) -> anyhow::Result<SandboxOutput> {
        let timeout = spec.limits.timeout_ms;
        self.spawn(spec)?.wait(timeout)
    }
}

/// 本机直跑：完整用户 token + 继承父环境（仅留 Job 超时团灭保险丝）。
pub struct UnsandboxedTier;

/// 受限 token：剥光特权 + 受限 SID 白名单 + 白名单环境 + Job 资源上限（默认档）。
pub struct RestrictedTier;

/// AppContainer：LowBox token + 能力 SID，默认断网；`allow_network` 授予 INTERNET_CLIENT。
pub struct AppContainerTier {
    pub allow_network: bool,
}

impl SandboxTier for UnsandboxedTier {
    fn id(&self) -> &'static str {
        "unsandboxed"
    }
    fn label(&self) -> &'static str {
        "本机直跑（无沙箱）"
    }
    fn describe(&self) -> String {
        "完整用户权限运行，继承父环境（含密钥）；仅保留超时整树击杀保险丝。需 config 显式 allow_unsandboxed=true。".into()
    }
    fn spawn(&self, spec: &SandboxSpec) -> anyhow::Result<SandboxChild> {
        let mut spec = SandboxSpec { restricted: false, appcontainer: false, ..spec.clone() };
        spec.allow_network = false;
        proc::spawn(&spec)
    }
    fn probe_chain(&self, limits: SandboxLimits) -> Vec<ProbeResult> {
        vec![probes::canary_unsandboxed(limits)]
    }
}

impl SandboxTier for RestrictedTier {
    fn id(&self) -> &'static str {
        "restricted"
    }
    fn label(&self) -> &'static str {
        "受限 token 沙箱"
    }
    fn describe(&self) -> String {
        "Job Objects 管进程树（kill-on-close 团灭、内存/进程数上限）+ 受限 token 剥特权 + 白名单环境（密钥进不来）+ 默认断网。".into()
    }
    fn spawn(&self, spec: &SandboxSpec) -> anyhow::Result<SandboxChild> {
        let mut spec = SandboxSpec { restricted: true, appcontainer: false, ..spec.clone() };
        spec.allow_network = false;
        proc::spawn(&spec)
    }
    fn probe_chain(&self, limits: SandboxLimits) -> Vec<ProbeResult> {
        probes::run_fast_suite_for_tier(self, limits)
    }
}

impl SandboxTier for AppContainerTier {
    fn id(&self) -> &'static str {
        "appcontainer"
    }
    fn label(&self) -> &'static str {
        match self.allow_network {
            true => "AppContainer（允许出网）",
            false => "AppContainer（断网）",
        }
    }
    fn describe(&self) -> String {
        format!(
            "LowBox token（SECURITY_CAPABILITIES 启动属性）+ 工作区目录按 ACE 授权；默认无网络出向（{}）。实验特性。",
            if self.allow_network { "已授 INTERNET_CLIENT" } else { "无 INTERNET_CLIENT 能力" }
        )
    }
    fn spawn(&self, spec: &SandboxSpec) -> anyhow::Result<SandboxChild> {
        let spec = SandboxSpec {
            restricted: true,
            appcontainer: true,
            allow_network: self.allow_network,
            ..spec.clone()
        };
        proc::spawn(&spec)
    }
    fn probe_chain(&self, limits: SandboxLimits) -> Vec<ProbeResult> {
        probes::run_fast_suite_for_tier(self, limits)
    }
}
