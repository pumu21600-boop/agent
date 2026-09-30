//! Job Objects —— 施工单 004 / 任务 C
//!
//! 一个沙箱实例 = 一个 Job Object，包住整棵进程树：
//! - KILL_ON_JOB_CLOSE：句柄一关整树必死，孤儿零残留（配合 Drop 双保险）
//! - PROCESS_MEMORY / ACTIVE_PROCESS / PROCESS_TIME 资源上限
//! - UI 限制（JobObjectBasicUIRestrictions）：剪贴板、全局原子、桌面、退出系统、外部句柄
//! - 不设 BREAKAWAY_OK：子进程不许逃逸出 Job

use std::{mem, ptr};

use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, HANDLE},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
            JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
            JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_BASIC_UI_RESTRICTIONS,
            JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_LIMIT_PROCESS_TIME,
            JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS,
            JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_GLOBALATOMS,
            JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
            JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
            SetInformationJobObject, TerminateJobObject,
        },
        Threading::IO_COUNTERS,
    },
};

use anyhow::{anyhow, bail};

use super::SandboxLimits;

/// 击杀整树时给子进程的统一退出码（日志里好认）。
pub(crate) const KILL_EXIT_CODE: u32 = 0x5A4E;

/// Job Object 的 RAII 封装：Drop = TerminateJobObject + CloseHandle。
/// KILL_ON_JOB_CLOSE 兜底保证：就算 Drop 顺序出岔子，句柄关闭也会团灭。
pub(crate) struct JobGuard {
    handle: HANDLE,
}

impl JobGuard {
    /// restricted=true：全套资源+UI 上限（权限笼）。
    /// restricted=false（本机直跑）：只留 kill-on-close 生命周期保险丝，
    /// 不限内存/进程数/CPU——那是"权限笼"的一部分，脱笼时不带。
    pub(crate) fn new(limits: &SandboxLimits, restricted: bool) -> anyhow::Result<Self> {
        unsafe {
            let handle = CreateJobObjectW(ptr::null(), ptr::null());
            if handle == 0 {
                bail!("CreateJobObjectW 失败: {}", GetLastError());
            }
            if restricted {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
                info.BasicLimitInformation = JOBOBJECT_BASIC_LIMIT_INFORMATION {
                    // CPU 时间兜底：单进程用户态 CPU 到墙钟超时值即杀（防吃满核的 runaway）
                    PerProcessUserTimeLimit: (limits.timeout_ms as i64) * 10_000, // 100ns 单位
                    PerJobUserTimeLimit: 0,
                    LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
                        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                        | JOB_OBJECT_LIMIT_PROCESS_TIME,
                    MinimumWorkingSetSize: 0,
                    MaximumWorkingSetSize: 0,
                    ActiveProcessLimit: limits.active_processes,
                    Affinity: 0,
                    PriorityClass: 0,
                    SchedulingClass: 0,
                };
                info.IoInfo = IO_COUNTERS {
                    ReadOperationCount: 0,
                    WriteOperationCount: 0,
                    OtherOperationCount: 0,
                    ReadTransferCount: 0,
                    WriteTransferCount: 0,
                    OtherTransferCount: 0,
                };
                info.ProcessMemoryLimit = limits.memory_mb as usize * 1024 * 1024;
                if SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    CloseHandle(handle);
                    bail!("SetInformationJobObject(ExtendedLimit) 失败: {}", GetLastError());
                }
                // CPU 硬上限（可选，cpu_percent>0 才设）：整树压到单核的 N%——
                // 防超时窗口内吃满一颗核拖卡宿主；默认 0 = 不限（构建/装依赖类活儿不限速）
                if limits.cpu_percent > 0 {
                    let mut rate: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = mem::zeroed();
                    rate.ControlFlags = JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
                    rate.Anonymous.CpuRate = limits.cpu_percent.min(100) * 100; // 单位 = 百分比×100
                    if SetInformationJobObject(
                        handle,
                        JobObjectCpuRateControlInformation,
                        &rate as *const _ as *const _,
                        mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                    ) == 0
                    {
                        CloseHandle(handle);
                        bail!("SetInformationJobObject(CpuRate) 失败: {}", GetLastError());
                    }
                }
                // UI 限制是独立的信息类，分开设
                let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
                    UIRestrictionsClass: JOB_OBJECT_UILIMIT_READCLIPBOARD
                        | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                        | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                        | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                        | JOB_OBJECT_UILIMIT_GLOBALATOMS
                        | JOB_OBJECT_UILIMIT_DESKTOP
                        | JOB_OBJECT_UILIMIT_EXITWINDOWS
                        | JOB_OBJECT_UILIMIT_HANDLES,
                };
                if SetInformationJobObject(
                    handle,
                    JobObjectBasicUIRestrictions,
                    &ui as *const _ as *const _,
                    mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
                ) == 0
                {
                    CloseHandle(handle);
                    bail!("SetInformationJobObject(UIRestrictions) 失败: {}", GetLastError());
                }
            } else {
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    CloseHandle(handle);
                    bail!("SetInformationJobObject(ExtendedLimit) 失败: {}", GetLastError());
                }
            }
            Ok(Self { handle })
        }
    }

    /// 把已挂起进程收进 Job（spawn 时 CREATE_SUSPENDED 后、ResumeThread 前调用）。
    pub(crate) fn assign(&self, process: HANDLE) -> anyhow::Result<()> {
        unsafe {
            if AssignProcessToJobObject(self.handle, process) == 0 {
                return Err(anyhow!("AssignProcessToJobObject 失败: {}", GetLastError()));
            }
        }
        Ok(())
    }

    pub(crate) fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.handle, KILL_EXIT_CODE);
        }
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        self.terminate();
        unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::System::JobObjects::{
        JOB_OBJECT_UILIMIT_READCLIPBOARD, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
        JobObjectBasicUIRestrictions, QueryInformationJobObject,
    };

    use super::*;

    fn limits() -> SandboxLimits {
        SandboxLimits { timeout_ms: 5_000, memory_mb: 256, active_processes: 4, cpu_percent: 0 }
    }

    fn read_limits(job: &JobGuard) -> JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        unsafe {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
            let mut ret = 0u32;
            let ok = QueryInformationJobObject(
                job.handle,
                JobObjectExtendedLimitInformation,
                &mut info as *mut _ as *mut _,
                mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                &mut ret,
            );
            assert_ne!(ok, 0, "QueryInformationJobObject 失败: {}", GetLastError());
            info
        }
    }

    fn read_cpu_rate(job: &JobGuard) -> JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
        unsafe {
            let mut info: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = mem::zeroed();
            let mut ret = 0u32;
            let ok = QueryInformationJobObject(
                job.handle,
                JobObjectCpuRateControlInformation,
                &mut info as *mut _ as *mut _,
                mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                &mut ret,
            );
            assert_ne!(ok, 0, "QueryInformationJobObject(CpuRate) 失败: {}", GetLastError());
            info
        }
    }

    /// 权限笼：不只「设置调用没报错」，而是**回读内核里的实际值**——
    /// SetInformationJobObject 静默失败（或 flags 拼错）这类事故只有回读才拦得住。
    #[test]
    fn restricted_job_limits_are_really_applied() {
        let l = limits();
        let job = JobGuard::new(&l, true).expect("建受限 Job 应成功");
        let info = read_limits(&job);
        let f = info.BasicLimitInformation.LimitFlags;
        for (flag, name) in [
            (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, "KILL_ON_JOB_CLOSE"),
            (JOB_OBJECT_LIMIT_PROCESS_MEMORY, "PROCESS_MEMORY"),
            (JOB_OBJECT_LIMIT_ACTIVE_PROCESS, "ACTIVE_PROCESS"),
            (JOB_OBJECT_LIMIT_PROCESS_TIME, "PROCESS_TIME"),
        ] {
            assert_ne!(f & flag, 0, "{name} 未落进 Job（flags={f:#x}）");
        }
        assert_eq!(info.ProcessMemoryLimit, 256 * 1024 * 1024, "内存上限字节数要精确");
        assert_eq!(info.BasicLimitInformation.ActiveProcessLimit, 4);
        // CPU 兜底 = 墙钟超时（100ns 单位）；漏了这条就会有无超时的吃核 runaway
        assert_eq!(info.BasicLimitInformation.PerProcessUserTimeLimit, 5_000 * 10_000);

        // UI 限制是独立信息类，单独回读
        unsafe {
            let mut ui: JOBOBJECT_BASIC_UI_RESTRICTIONS = mem::zeroed();
            let mut ret = 0u32;
            let ok = QueryInformationJobObject(
                job.handle,
                JobObjectBasicUIRestrictions,
                &mut ui as *mut _ as *mut _,
                mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
                &mut ret,
            );
            assert_ne!(ok, 0, "回读 UI 限制失败: {}", GetLastError());
            for flag in [
                JOB_OBJECT_UILIMIT_READCLIPBOARD,
                JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
                JOB_OBJECT_UILIMIT_HANDLES,
                JOB_OBJECT_UILIMIT_EXITWINDOWS,
            ] {
                assert_ne!(ui.UIRestrictionsClass & flag, 0, "UI 限制缺位（{:#x}）", ui.UIRestrictionsClass);
            }
        }
    }

    /// CPU 硬上限回读：cpu_percent=50 必须以 ENABLE|HARD_CAP + MaxRate=50 落进内核；
    /// 默认 0 = 完全不设 CpuRate 信息类（保持不限速语义）。
    #[test]
    fn cpu_rate_control_is_really_applied() {
        let l = SandboxLimits { cpu_percent: 50, ..limits() };
        let job = JobGuard::new(&l, true).expect("建带 CPU 上限的 Job 应成功");
        let rate = read_cpu_rate(&job);
        assert_ne!(rate.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, 0, "CpuRate 未启用");
        assert_ne!(rate.ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, 0, "CpuRate 非硬顶");
        let cpu = unsafe { rate.Anonymous.CpuRate };
        assert_eq!(cpu, 5_000, "CpuRate = 百分比×100（50% → 5000）");

        let off = JobGuard::new(&limits(), true).expect("建默认 Job 应成功");
        assert_eq!(read_cpu_rate(&off).ControlFlags & JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, 0, "默认不得设 CpuRate");
    }

    /// 本机直跑：只留生命周期保险丝，不带资源笼（内存/进程数上限都不设）。
    #[test]
    fn unrestricted_job_keeps_only_kill_on_close() {
        let job = JobGuard::new(&limits(), false).expect("建直跑 Job 应成功");
        let info = read_limits(&job);
        assert_eq!(
            info.BasicLimitInformation.LimitFlags,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            "脱笼模式不该带资源上限"
        );
        assert_eq!(info.ProcessMemoryLimit, 0);
        assert_eq!(info.BasicLimitInformation.ActiveProcessLimit, 0);
    }
}
