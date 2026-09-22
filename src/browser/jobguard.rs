//! 无头浏览器子进程的生命周期保险丝（P20.4）—— Job Object + KILL_ON_JOB_CLOSE。
//!
//! 为什么需要：服务进程被**硬杀**（`agent.exe stop` 走 taskkill /F、崩溃、重启）时，
//! `Child::kill()` 那套优雅路径根本来不及跑，无头 Edge 会作为孤儿继续活着、占着临时
//! profile——真机踩过：用户机器上留了一个杀不掉的提权孤儿 Edge。把浏览器子进程收进
//! 只带 kill-on-close 的 Job 后，服务进程一死（句柄全关），整棵树跟着团灭。
//!
//! 与沙箱的 `sandbox::job::JobGuard` 是两回事：那个是"权限笼"（限内存/进程数/UI），
//! 这个只要生命周期保险丝，不碰任何资源上限。

use std::ptr;
use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, HANDLE},
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, SetInformationJobObject, TerminateJobObject,
    },
};

/// 无头浏览器子进程的 Job（句柄开着 = 保险丝在；Drop/服务死亡 = 整树击杀）。
pub(crate) struct ChildJob {
    handle: HANDLE,
}

impl ChildJob {
    /// 建 Job（失败返回 None——调用方照旧拉起浏览器，只是没保险丝）。
    pub(crate) fn new() -> Option<Self> {
        unsafe {
            let handle = CreateJobObjectW(ptr::null(), ptr::null());
            if handle == 0 {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation = JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..std::mem::zeroed()
            };
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                crate::observability::debug(&format!(
                    "[browser] Job Object 设置失败: {}（无头浏览器将无生命周期保险丝）",
                    GetLastError()
                ));
                CloseHandle(handle);
                return None;
            }
            Some(Self { handle })
        }
    }

    /// 把子进程收进 Job（spawn 之后立刻调用）。
    pub(crate) fn assign(&self, process: HANDLE) {
        unsafe {
            if AssignProcessToJobObject(self.handle, process) == 0 {
                crate::observability::debug(&format!(
                    "[browser] 浏览器子进程入 Job 失败: {}（杀服务时可能留孤儿）",
                    GetLastError()
                ));
            }
        }
    }
}

impl Drop for ChildJob {
    fn drop(&mut self) {
        unsafe {
            TerminateJobObject(self.handle, 0x5A4E);
            CloseHandle(self.handle);
        }
    }
}
