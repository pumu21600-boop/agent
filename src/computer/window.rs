//! 窗口枚举与聚焦：EnumWindows 清单 + SetForegroundWindow（带 AttachThreadInput
//! 前台锁破解——后台进程直接 SetForegroundWindow 只会闪任务栏，不真的换前台）。

use std::cell::RefCell;

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, ShowWindow, SW_RESTORE,
};
#[cfg(test)]
use windows_sys::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetClassNameW, SendMessageTimeoutW};

use super::input;

thread_local! {
    /// EnumWindows 回调的收集缓冲（回调是 extern "system" fn，拿不到闭包——
    /// 走线程局部）。枚举同步完成，不会跨线程混。
    static ENUM_BUF: RefCell<Vec<(HWND, String)>> = const { RefCell::new(Vec::new()) };
}

/// 一条可见窗口（句柄 + 标题）。系统壳/不可见/无标题的已滤掉。
#[derive(Debug, Clone)]
pub struct WinInfo {
    pub hwnd: isize,
    pub title: String,
}

/// 枚举当前可见窗口（标题非空）。按 z 序（EnumWindows 序）返回。
pub fn list_windows() -> Vec<WinInfo> {
    ENUM_BUF.with(|b| b.borrow_mut().clear());
    unsafe extern "system" fn cb(hwnd: HWND, _l: LPARAM) -> BOOL {
        unsafe {
            if IsWindowVisible(hwnd) == 0 {
                return 1;
            }
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            if n <= 0 {
                return 1;
            }
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            if title.trim().is_empty() {
                return 1;
            }
            ENUM_BUF.with(|b| b.borrow_mut().push((hwnd, title)));
            1
        }
    }
    unsafe {
        EnumWindows(Some(cb), 0);
    }
    ENUM_BUF.with(|b| {
        b.borrow()
            .iter()
            .map(|(h, t)| WinInfo {
                hwnd: *h,
                title: t.clone(),
            })
            .collect()
    })
}

/// 按标题子串找窗口（大小写不敏感；多个命中取第一个=最前）。
pub fn find_window(substr: &str) -> Option<WinInfo> {
    let needle = substr.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    list_windows().into_iter().find(|w| w.title.to_lowercase().contains(&needle))
}

/// 把窗口带到前台：最小化先还原 → AttachThreadInput 接入目标线程的输入队列
/// （绕过前台锁）→ SetForegroundWindow/BringWindowToTop。失败也返回 Ok(false)
/// 由调用方转告（有些壳窗口确实抢不走，比如全屏游戏/安全桌面）。
pub fn focus_window(hwnd: isize) -> Result<bool, String> {
    if hwnd == 0 {
        return Err("窗口句柄为空".into());
    }
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        let target_thread = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
        let my_thread = GetCurrentThreadId();
        let attached = target_thread != 0 && target_thread != my_thread && {
            use windows_sys::Win32::System::Threading::AttachThreadInput;
            AttachThreadInput(my_thread, target_thread, 1) != 0
        };
        let ok = windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd) != 0;
        BringWindowToTop(hwnd);
        if attached {
            use windows_sys::Win32::System::Threading::AttachThreadInput;
            AttachThreadInput(my_thread, target_thread, 0);
        }
        Ok(ok)
    }
}

/// 当前前台窗口标题（诊断/验证用）。
pub fn foreground_title() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == 0 {
            return None;
        }
        let mut buf = [0u16; 512];
        let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        (n > 0).then(|| String::from_utf16_lossy(&buf[..n as usize]))
    }
}

thread_local! {
    /// EnumChildWindows 的收集缓冲（同 ENUM_BUF 的路数）。
    static CHILD_BUF: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
}

/// 找窗口内第一个编辑控件的 (句柄, 屏幕 rect)——鼠标点击定位用（点它左上角
/// = 文本最前面）。仅 opt-in 真机 e2e（mod.rs 的 real_input_tests）用。
#[cfg(test)]
pub fn edit_control_rect(hwnd: isize) -> Option<(isize, (i32, i32, i32, i32))> {
    CHILD_BUF.with(|b| b.borrow_mut().clear());
    unsafe extern "system" fn cb(child: isize, _l: LPARAM) -> BOOL {
        unsafe {
            let mut cls = [0u16; 64];
            let n = GetClassNameW(child, cls.as_mut_ptr(), cls.len() as i32);
            if n > 0 {
                let name = String::from_utf16_lossy(&cls[..n as usize]);
                if name == "Edit" || name == "RichEditD2DPT" {
                    CHILD_BUF.with(|b| b.borrow_mut().push(child));
                }
            }
            1
        }
    }
    unsafe {
        EnumChildWindows(hwnd, Some(cb), 0);
    }
    let children: Vec<isize> = CHILD_BUF.with(|b| b.borrow().clone());
    let child = children.into_iter().next()?;
    unsafe {
        let mut r = windows_sys::Win32::Foundation::RECT { left: 0, top: 0, right: 0, bottom: 0 };
        (windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(child, &mut r) != 0)
            .then_some((child, (r.left, r.top, r.right, r.bottom)))
    }
}

/// 读窗口内**编辑控件的文本**（记事本/输入框类：找 class="Edit" 的子窗口）。
/// 验证键鼠输入落点用——记事本标题只显示文件名，内容在 Edit 子窗口里。
/// 仅 opt-in 真机 e2e 用。
#[cfg(test)]
pub fn edit_control_text(hwnd: isize) -> Option<String> {
    CHILD_BUF.with(|b| b.borrow_mut().clear());
    unsafe extern "system" fn cb(child: isize, _l: LPARAM) -> BOOL {
        unsafe {
            let mut cls = [0u16; 64];
            let n = GetClassNameW(child, cls.as_mut_ptr(), cls.len() as i32);
            if n > 0 {
                let name = String::from_utf16_lossy(&cls[..n as usize]);
                if name == "Edit" || name == "RichEditD2DPT" {
                    CHILD_BUF.with(|b| b.borrow_mut().push(child));
                }
            }
            1
        }
    }
    unsafe {
        EnumChildWindows(hwnd, Some(cb), 0);
    }
    let children: Vec<isize> = CHILD_BUF.with(|b| b.borrow().clone());
    for child in children {
        // 跨进程读文本走 SendMessageTimeoutW + WM_GETTEXT——GetWindowTextW 对别的
        // 进程的窗口经常返空（UIPI/ marshalling 限制，实测记事本就读不到）。
        unsafe {
            let mut buf = [0u16; 4096];
            let mut got: usize = 0;
            let r = SendMessageTimeoutW(
                child,
                0x000D, // WM_GETTEXT
                buf.len(),
                buf.as_mut_ptr() as isize,
                2u32, // SMTO_ABORTIFHUNG（常量被特性门控，用字面量）
                200,
                &mut got,
            );
            if r != 0 && got > 0 {
                let n = got.min(buf.len() - 1);
                return Some(String::from_utf16_lossy(&buf[..n]));
            }
        }
    }
    None
}

/// 键盘输入落点诊断：当前焦点窗口 + 大写锁定/插入等切换键状态（人话一行）。
/// 让模型在「敲了键盘没反应」时先看这儿，而不是盲目重敲。
pub fn focus_diagnostics() -> String {
    let fg = foreground_title().unwrap_or_else(|| "(取不到前台窗口)".into());
    let caps = input::is_key_down(0x14); // VK_CAPITAL
    format!("当前前台窗口：{fg}；大写锁定：{}；光标位置：{:?}", if caps { "开" } else { "关" }, super::screen::cursor_pos())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_windows_returns_titled_windows() {
        // CI/无头环境可能一个都没有——不断言非空，只断言不崩且形状正确
        let wins = list_windows();
        for w in &wins {
            assert!(!w.title.trim().is_empty(), "无标题窗口不该进清单");
        }
    }

    #[test]
    fn find_window_missing_returns_none() {
        assert!(find_window("绝不可能存在的窗口标题 xyz123").is_none());
        assert!(find_window("  ").is_none(), "空子串不瞎找");
    }

    #[test]
    fn focus_null_handle_errors() {
        assert!(focus_window(0).is_err());
    }
}
