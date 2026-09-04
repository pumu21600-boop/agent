//! AppContainer 沙箱模式原语（施工单 019，P12）—— P3 受限 token 沙箱的升级路径
//!
//! 机制：LowBox token（由 CreateProcess 的 SECURITY_CAPABILITIES 启动属性自动创建，
//! 见 proc.rs 的 appcontainer 分支）。
//! - 默认**无网络出向**：不授予 INTERNET_CLIENT 能力 SID（S-1-15-3-3）即断网；
//!   `allow_network = true` 时 profile 创建带该能力；
//! - 文件/注册表默认全拒：工作区目录由 icacls 授 AppContainer SID 的 ACE（幂等）；
//! - Job 保险丝照用（超时整树击杀），与 P3 语义一致。
//!
//! 开关：config.toml `[sandbox] mode = "appcontainer"`（默认 "restricted" 向后兼容）；
//! 实验特性，异常时切回 "restricted" 即可。

use std::{ffi::c_void, path::Path, ptr};

use windows_sys::Win32::Foundation::{HANDLE, PSID};
use windows_sys::Win32::Security::SID_AND_ATTRIBUTES;

/// 沙箱 AppContainer profile 名（幂等；能力变更走 recreate）。
/// 坑（真机踩过）：name 含「.」会 E_INVALIDARG（0x80070057），只能字母数字-_。
pub const PROFILE_NAME: &str = "agent_local_sandbox";
/// INTERNET_CLIENT 能力 SID（出网能力）。
pub const INTERNET_CLIENT_SID: &str = "S-1-15-3-3";

#[link(name = "userenv")]
unsafe extern "system" {
    fn CreateAppContainerProfile(
        app_container_name: *const u16,
        display_name: *const u16,
        description: *const u16,
        capabilities: *mut SID_AND_ATTRIBUTES,
        capability_count: u32,
        out_sid: *mut PSID,
    ) -> i32;
    fn DeriveAppContainerSidFromAppContainerName(app_container_name: *const u16, out_sid: *mut PSID) -> i32;
    fn DeleteAppContainerProfile(app_container_name: *const u16) -> i32;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSidToSidW(string_sid: *const u16, out_sid: *mut PSID) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LocalFree(hmem: HANDLE) -> HANDLE;
}

/// HRESULT 成功判定。
fn ok(hr: i32) -> bool {
    hr >= 0
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 确保 AppContainer profile 存在并返回派生 SID。
/// **坑（真机踩过）**：DeriveAppContainerSidFromAppContainerName 是纯数学运算，
/// profile 不存在也返回成功——CreateProcess 会报错误 2。所以这里总是重建 profile
/// （幂等删除+创建；sandbox profile 无持久数据，代价可忽略）。
pub unsafe fn ensure_sid(allow_network: bool) -> Result<SidGuard, String> {
    recreate_profile(allow_network)
}

/// 删除并重建 profile（能力按 allow_network），返回派生 SID。
pub unsafe fn recreate_profile(allow_network: bool) -> Result<SidGuard, String> {
    let name_w = wide(PROFILE_NAME);
    let _ = DeleteAppContainerProfile(name_w.as_ptr());

    // 坑（真机踩过）：capabilities 空数组（count=0 + 非 NULL 指针）会 E_INVALIDARG——
    // 无网络能力时必须传 NULL 数组；有网络能力时传 INTERNET_CLIENT。
    let mut cap_sids: Vec<PSID> = Vec::new();
    if allow_network {
        let s = wide(INTERNET_CLIENT_SID);
        let mut sid: PSID = ptr::null_mut();
        if ConvertStringSidToSidW(s.as_ptr(), &mut sid) != 0 {
            cap_sids.push(sid);
        }
    }
    let mut caps: Vec<SID_AND_ATTRIBUTES> = cap_sids
        .iter()
        .map(|sid| SID_AND_ATTRIBUTES { Sid: *sid, Attributes: 0 })
        .collect();

    let display = wide("Agent Local Sandbox");
    let desc = wide("agent 项目沙箱（LowBox，默认断网）");
    let mut sid: PSID = ptr::null_mut();
    let hr = CreateAppContainerProfile(
        name_w.as_ptr(),
        display.as_ptr(),
        desc.as_ptr(),
        if caps.is_empty() { ptr::null_mut() } else { caps.as_mut_ptr() },
        caps.len() as u32,
        &mut sid,
    );
    if !ok(hr) {
        return Err(format!(
            "CreateAppContainerProfile 失败: hr=0x{hr:08X}（首次创建可能需要管理员权限）"
        ));
    }
    Ok(SidGuard { sid })
}

/// SID 句柄（Drop 时 LocalFree）。
pub struct SidGuard {
    pub sid: PSID,
}
impl Drop for SidGuard {
    fn drop(&mut self) {
        if !self.sid.is_null() {
            unsafe {
                LocalFree(self.sid as HANDLE);
            }
        }
    }
}

/// SID 转字符串（icacls / 日志用）——手写转换，零额外 FFI。
/// SID 内存布局：[Revision u8][SubAuthorityCount u8][IdentifierAuthority 6B 大端][SubAuthority u32 × N 小端]
pub unsafe fn sid_to_string(sid: PSID) -> Option<String> {
    if sid.is_null() {
        return None;
    }
    let p = sid as *const u8;
    if *p != 1 {
        return None; // 仅支持 SID_REVISION 1
    }
    let count = *p.add(1) as usize;
    if count > 15 {
        return None;
    }
    let mut auth: u64 = 0;
    for i in 0..6 {
        auth = (auth << 8) | *p.add(2 + i) as u64;
    }
    let mut s = format!("S-1-{}", auth);
    for i in 0..count {
        let sub = (p.add(8 + i * 4) as *const u32).read_unaligned();
        s.push_str(&format!("-{}", sub));
    }
    Some(s)
}

/// 给目录授 AppContainer SID 的完全控制 ACE（icacls，幂等；递归继承 OI+CI）。
/// 失败仅返回 Err（调用方记日志，不阻断——探针会暴露权限问题）。
pub fn grant_dir_access(dir: &Path, sid_str: &str) -> Result<(), String> {
    let out = std::process::Command::new("icacls")
        .arg(dir)
        .args(["/grant", &format!("*{sid_str}:(OI)(CI)F")])
        .output()
        .map_err(|e| format!("icacls 执行失败: {e}"))?;
    if !out.status.success() {
        return Err(format!("icacls 授权失败: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(())
}
