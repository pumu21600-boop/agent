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
            JobObjectExtendedLimitInformation, JOBOBJECT_BASIC_LIMIT_INFORMATION,
            JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
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
