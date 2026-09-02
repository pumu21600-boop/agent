//! 受限 token —— 施工单 004 / 任务 B
//!
//! `CreateRestrictedToken`：DISABLE_MAX_PRIVILEGE 剥光全部特权 + 受限 SID 白名单
//! （当前用户 / Everyone / BUILTIN\Users / Authenticated Users）。
//! 受限 token 的访问检查 = 正常 SID 要过 + 受限 SID 也要过，等于给 ACL 再套一层白名单：
//! 工作目录（用户 SID 授权）与系统目录（Users 授权）可达；管理员面、他人文件不可达。
//! 不降完整性等级：低 IL 会挡住 P4 shell 在工作目录内写文件（工作目录对象默认中 IL）。
//! AppContainer / 完整性策略留 P12。全程不需要管理员权限。

use std::{ffi::c_void, ptr};

use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, HANDLE, PSID},
    Security::{
        CopySid, CreateRestrictedToken, CreateWellKnownSid, GetLengthSid, GetTokenInformation,
        DISABLE_MAX_PRIVILEGE, SE_PRIVILEGE_ENABLED, SID_AND_ATTRIBUTES, TOKEN_ASSIGN_PRIMARY,
        TOKEN_DUPLICATE, TOKEN_GROUPS, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER, TokenGroups,
        TokenPrivileges, TokenUser, WinAuthenticatedUserSid, WinBuiltinUsersSid, WinWorldSid,
        LookupPrivilegeNameW,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

use anyhow::{anyhow, bail};

/// SECURITY_MAX_SID_SIZE 在 windows-sys 0.52 未导出，SDK 头文件定值 68。
const SECURITY_MAX_SID_SIZE: u32 = 68;
/// SE_GROUP_LOGON_ID（windows-sys 0.52 放在 SystemServices 且为 i32，这里按 SDK 值本地定义）。
const SE_GROUP_LOGON_ID: u32 = 0xC000_0000;

/// 受限 token 的 RAII 句柄；spawn 结束后即弃。
#[derive(Debug)]
pub(crate) struct TokenHandle(HANDLE);

impl TokenHandle {
    pub(crate) fn handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for TokenHandle {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// 构造受限 token：特权全剥 + 受限 SID 白名单（见模块注释）。
pub(crate) fn create_restricted_token() -> anyhow::Result<TokenHandle> {
    unsafe {
        let mut base: HANDLE = 0;
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY,
            &mut base,
        ) == 0
        {
            return Err(anyhow!("OpenProcessToken 失败: {}", GetLastError()));
        }
        let _base_guard = HandleCloser(base);

        // 受限 SID：三个内置组 + 当前用户 + 登录会话 SID，存进独立缓冲保证指针在调用期间有效
        let mut sids: Vec<Vec<u8>> = Vec::new();
        for kind in [WinWorldSid, WinAuthenticatedUserSid, WinBuiltinUsersSid] {
            let mut buf = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
            let mut size = SECURITY_MAX_SID_SIZE;
            if CreateWellKnownSid(kind, ptr::null_mut(), buf.as_mut_ptr() as PSID, &mut size) == 0 {
                bail!("CreateWellKnownSid 失败: {}", GetLastError());
            }
            sids.push(buf);
        }
        let user_sid = current_user_sid(base)?;
        sids.push(user_sid);
        // 窗口站/桌面的 ACL 按登录会话 SID 授权：受限列表缺了它，.NET 类进程
        // （powershell 等）起手就 0xC0000142 DLL 初始化失败。必须带上。
        if let Some(logon) = logon_sid(base)? {
            sids.push(logon);
        }

        let attrs: Vec<SID_AND_ATTRIBUTES> = sids
            .iter()
            .map(|b| SID_AND_ATTRIBUTES { Sid: b.as_ptr() as PSID, Attributes: 0 })
            .collect();

        let mut restricted: HANDLE = 0;
        if CreateRestrictedToken(
            base,
            DISABLE_MAX_PRIVILEGE,
            0,
            ptr::null(),
            0,
            ptr::null(),
            attrs.len() as u32,
            attrs.as_ptr(),
            &mut restricted,
        ) == 0
        {
            return Err(anyhow!("CreateRestrictedToken 失败: {}", GetLastError()));
        }
        Ok(TokenHandle(restricted))
    }
}

/// 当前进程 token 里的用户 SID（拷贝到自有缓冲）。
fn current_user_sid(token: HANDLE) -> anyhow::Result<Vec<u8>> {
    unsafe {
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut len);
        if len == 0 {
            bail!("GetTokenInformation(TokenUser) 取长度失败: {}", GetLastError());
        }
        let buf = vec![0u8; len as usize];
        if GetTokenInformation(token, TokenUser, buf.as_ptr() as *mut c_void, len, &mut len) == 0 {
            bail!("GetTokenInformation(TokenUser) 失败: {}", GetLastError());
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let sid_len = GetLengthSid(user.User.Sid);
        let mut sid = vec![0u8; sid_len as usize];
        if CopySid(sid_len, sid.as_mut_ptr() as PSID, user.User.Sid) == 0 {
            bail!("CopySid 失败: {}", GetLastError());
        }
        Ok(sid)
    }
}

/// token 里的登录会话 SID（SE_GROUP_LOGON_ID 标记的那个）。
fn logon_sid(token: HANDLE) -> anyhow::Result<Option<Vec<u8>>> {
    unsafe {
        let mut len = 0u32;
        GetTokenInformation(token, TokenGroups, ptr::null_mut(), 0, &mut len);
        if len == 0 {
            bail!("GetTokenInformation(TokenGroups) 取长度失败: {}", GetLastError());
        }
        let buf = vec![0u8; len as usize];
        if GetTokenInformation(token, TokenGroups, buf.as_ptr() as *mut c_void, len, &mut len) == 0 {
            bail!("GetTokenInformation(TokenGroups) 失败: {}", GetLastError());
        }
        let groups = &*(buf.as_ptr() as *const TOKEN_GROUPS);
        for g in std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) {
            if g.Attributes & SE_GROUP_LOGON_ID != 0 {
                let sid_len = GetLengthSid(g.Sid);
                let mut sid = vec![0u8; sid_len as usize];
                if CopySid(sid_len, sid.as_mut_ptr() as PSID, g.Sid) == 0 {
                    bail!("CopySid(logon) 失败: {}", GetLastError());
                }
                return Ok(Some(sid));
            }
        }
        Ok(None)
    }
}

struct HandleCloser(HANDLE);
impl Drop for HandleCloser {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// 探针用：当前进程的未受限 token（对照组）。
pub(crate) fn base_token() -> anyhow::Result<TokenHandle> {
    unsafe {
        let mut base: HANDLE = 0;
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut base) == 0 {
            return Err(anyhow!("OpenProcessToken 失败: {}", GetLastError()));
        }
        Ok(TokenHandle(base))
    }
}

/// token 里启用状态（SE_PRIVILEGE_ENABLED）的特权名列表。供探针做差分断言：
/// DISABLE_MAX_PRIVILEGE 的语义是把特权置为禁用；其中 SeChangeNotifyPrivilege
/// 是系统固定保留（CreateRestrictedToken 既有行为，遍历检查绕过本身无害），
/// 按名字白名单判定而不是数个数。
pub(crate) fn enabled_privilege_names(token: HANDLE) -> anyhow::Result<Vec<String>> {
    unsafe {
        let mut len = 0u32;
        GetTokenInformation(token, TokenPrivileges, ptr::null_mut(), 0, &mut len);
        if len == 0 {
            bail!("GetTokenInformation(TokenPrivileges) 取长度失败: {}", GetLastError());
        }
        let buf = vec![0u8; len as usize];
        if GetTokenInformation(token, TokenPrivileges, buf.as_ptr() as *mut c_void, len, &mut len) == 0 {
            bail!("GetTokenInformation(TokenPrivileges) 失败: {}", GetLastError());
        }
        let privs = &*(buf.as_ptr() as *const TOKEN_PRIVILEGES);
        let list =
            std::slice::from_raw_parts(privs.Privileges.as_ptr(), privs.PrivilegeCount as usize);
        let mut names = Vec::new();
        for p in list.iter().filter(|p| p.Attributes & SE_PRIVILEGE_ENABLED != 0) {
            let mut cap = 0u32;
            LookupPrivilegeNameW(ptr::null(), &p.Luid, ptr::null_mut(), &mut cap);
            if cap == 0 {
                continue; // 拿不到名字的特权罕见，跳过（名字查不到本身就说明不是常驻启用特权）
            }
            let mut out = vec![0u16; cap as usize];
            if LookupPrivilegeNameW(ptr::null(), &p.Luid, out.as_mut_ptr(), &mut cap) != 0 {
                names.push(String::from_utf16_lossy(&out[..cap as usize]).to_string());
            }
        }
        Ok(names)
    }
}
