//! 键鼠控制：SendInput（user32）。鼠标绝对坐标走 MOUSEEVENTF_VIRTUALDESK
//! （与虚拟屏同系，截图坐标即点击坐标）；文本输入走 KEYEVENTF_UNICODE
//! （绕过键盘布局，中文/emoji/符号都能原样灌）。
//!
//! 全是同步 Win32 调用——由 mod.rs 分发层包 spawn_blocking，不占 tokio worker。

use std::mem::size_of;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY, VK_ACCEPT, VK_ADD, VK_BACK, VK_CAPITAL,
    VK_CONTROL, VK_DECIMAL, VK_DELETE, VK_DIVIDE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F10,
    VK_F11, VK_F12, VK_F2, VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9, VK_HOME, VK_INSERT,
    VK_LEFT, VK_LWIN, VK_MENU, VK_MULTIPLY, VK_NEXT, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4,
    VK_OEM_5, VK_OEM_6, VK_OEM_7, VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS,
    VK_PAUSE, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SNAPSHOT, VK_SPACE,
    VK_TAB, VK_UP,
};

use super::screen;

/// 鼠标按钮。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "left" | "l" | "primary" => Some(Self::Left),
            "right" | "r" | "secondary" => Some(Self::Right),
            "middle" | "m" => Some(Self::Middle),
            _ => None,
        }
    }
    fn flags_down(self) -> u32 {
        match self {
            Self::Left => MOUSEEVENTF_LEFTDOWN,
            Self::Right => MOUSEEVENTF_RIGHTDOWN,
            Self::Middle => MOUSEEVENTF_MIDDLEDOWN,
        }
    }
    fn flags_up(self) -> u32 {
        match self {
            Self::Left => MOUSEEVENTF_LEFTUP,
            Self::Right => MOUSEEVENTF_RIGHTUP,
            Self::Middle => MOUSEEVENTF_MIDDLEUP,
        }
    }
}

/// 屏幕坐标 → SendInput 归一化绝对坐标（0..65535，虚拟桌面）。
fn to_abs(x: i32, y: i32) -> (i32, i32) {
    let (vx, vy, vw, vh) = screen::virtual_screen();
    let nx = ((x - vx) as i64 * 65535 / (vw as i64 - 1).max(1)).clamp(0, 65535) as i32;
    let ny = ((y - vy) as i64 * 65535 / (vh as i64 - 1).max(1)).clamp(0, 65535) as i32;
    (nx, ny)
}

/// 送一批输入事件，返回系统实际接收的条数（0 = 被拒，多为 UIPI/权限问题）。
/// debug 下丢事件会打 stderr——静默丢是「敲了键盘没反应」类灵异案的根。
fn send(inputs: &[INPUT]) -> u32 {
    if inputs.is_empty() {
        return 0;
    }
    let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) };
    #[cfg(debug_assertions)]
    if n != inputs.len() as u32 {
        eprintln!("[computer] SendInput 只收了 {n}/{} 条", inputs.len());
    }
    n
}

fn mouse_input(dx: i32, dy: i32, flags: u32, data: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 移动光标到屏幕坐标（绝对）。
pub fn move_to(x: i32, y: i32) {
    let (nx, ny) = to_abs(x, y);
    let _ = send(&[mouse_input(
        nx,
        ny,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        0,
    )]);
}

/// 在屏幕坐标点击（移动 + 按下 + 抬起；双击传 clicks=2）。
pub fn click(x: i32, y: i32, button: MouseButton, clicks: u32) {
    move_to(x, y);
    for _ in 0..clicks.max(1) {
        let _ = send(&[mouse_input(0, 0, button.flags_down(), 0)]);
        std::thread::sleep(std::time::Duration::from_millis(12));
        let _ = send(&[mouse_input(0, 0, button.flags_up(), 0)]);
        if clicks > 1 {
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
    }
}

/// 滚轮：dy 正=上滚（一格 120），dx 正=右滚。
pub fn scroll(x: i32, y: i32, dx: i32, dy: i32) {
    move_to(x, y);
    if dy != 0 {
        let _ = send(&[mouse_input(0, 0, MOUSEEVENTF_WHEEL, (dy * 120) as u32)]);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    if dx != 0 {
        let _ = send(&[mouse_input(0, 0, MOUSEEVENTF_WHEEL | MOUSEEVENTF_HWHEEL, (dx * 120) as u32)]);
    }
}

/// 拖拽：from 按下 → 分段移动到 to（应用按 mousemove 轨迹判定拖拽，一步跳过去
/// 有些目标不认）→ 抬起。
pub fn drag(from: (i32, i32), to: (i32, i32), button: MouseButton) {
    move_to(from.0, from.1);
    let _ = send(&[mouse_input(0, 0, button.flags_down(), 0)]);
    std::thread::sleep(std::time::Duration::from_millis(50));
    let steps = 12;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let x = (from.0 as f64 + (to.0 - from.0) as f64 * t).round() as i32;
        let y = (from.1 as f64 + (to.1 - from.1) as f64 * t).round() as i32;
        move_to(x, y);
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
    let _ = send(&[mouse_input(0, 0, button.flags_up(), 0)]);
}

/// 文本输入：UTF-16 码元逐个灌（KEYEVENTF_UNICODE，surrogate 对原样发）。
pub fn type_text(text: &str) {
    for unit in text.encode_utf16() {
        let down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let up = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let _ = send(&[down]);
        let _ = send(&[up]);
        std::thread::sleep(std::time::Duration::from_millis(4));
    }
}

fn key_input(vk: VIRTUAL_KEY, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 键名 → 虚拟键码。认不出的返回 None（调用方报错列可用键）。
pub fn vk_from_name(name: &str) -> Option<VIRTUAL_KEY> {
    let n = name.trim().to_lowercase();
    Some(match n.as_str() {
        "enter" | "return" => VK_RETURN,
        "tab" => VK_TAB,
        "escape" | "esc" => VK_ESCAPE,
        "space" | "spacebar" => VK_SPACE,
        "backspace" | "back" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "insert" | "ins" => VK_INSERT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" => VK_PRIOR,
        "pagedown" | "pgdn" => VK_NEXT,
        "up" | "arrowup" => VK_UP,
        "down" | "arrowdown" => VK_DOWN,
        "left" | "arrowleft" => VK_LEFT,
        "right" | "arrowright" => VK_RIGHT,
        "shift" => VK_SHIFT,
        "ctrl" | "control" => VK_CONTROL,
        "alt" | "menu" => VK_MENU,
        "win" | "cmd" | "super" => VK_LWIN,
        "capslock" => VK_CAPITAL,
        "printscreen" | "prtsc" => VK_SNAPSHOT,
        "pause" => VK_PAUSE,
        "-" => VK_OEM_MINUS,
        "=" => VK_OEM_PLUS,
        "[" => VK_OEM_4,
        "]" => VK_OEM_6,
        "\\" => VK_OEM_5,
        ";" => VK_OEM_1,
        "'" => VK_OEM_7,
        "," => VK_OEM_COMMA,
        "." => VK_OEM_PERIOD,
        "/" => VK_OEM_2,
        "`" => VK_OEM_3,
        "+" => VK_ADD,
        "*" => VK_MULTIPLY,
        "num/" => VK_DIVIDE,
        "num." => VK_DECIMAL,
        "numenter" => VK_ACCEPT,
        _ => {
            // 单字符：a-z / 0-9 直取 VK 码（ASCII 一致）；F1-F12 走前缀解析
            if n.len() == 1 {
                let c = n.chars().next().unwrap();
                return c.is_ascii_alphanumeric().then(|| c.to_ascii_uppercase() as VIRTUAL_KEY);
            }
            if let Some(num) = n.strip_prefix('f')
                && let Ok(k) = num.parse::<u16>()
            {
                return Some(match k {
                    1 => VK_F1,
                    2 => VK_F2,
                    3 => VK_F3,
                    4 => VK_F4,
                    5 => VK_F5,
                    6 => VK_F6,
                    7 => VK_F7,
                    8 => VK_F8,
                    9 => VK_F9,
                    10 => VK_F10,
                    11 => VK_F11,
                    12 => VK_F12,
                    _ => return None,
                });
            }
            return None;
        }
    })
}

/// 解析组合键为虚拟键码序列（纯函数，不发任何键）。
/// 单测只测它——**绝不**在单测里调 key_combo：那会真的向系统发全局按键。
pub fn parse_combo(combo: &str) -> Result<Vec<VIRTUAL_KEY>, String> {
    let parts: Vec<&str> = combo.split('+').map(str::trim).filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Err("组合键为空".into());
    }
    let mut vks = Vec::with_capacity(parts.len());
    for p in &parts {
        let vk = vk_from_name(p).ok_or_else(|| format!("不认识的键「{p}」"))?;
        vks.push(vk);
    }
    Ok(vks)
}

/// 组合键："ctrl+c" / "ctrl+shift+t" / "alt+f4"。按住修饰 → 敲末键 → 逆序抬起。
/// 单个键（无 +）也走这里。
///
/// **真的会向系统发全局按键**（作用在当时的焦点窗口上）。生产路径只有
/// computer 工具；单测一律用 [parse_combo]，别调它——2026-10-21 实锤：
/// 单测里 `key_combo("alt+f4")` 在桌面无焦点窗口时直接弹出了系统的
/// 「关闭 Windows」对话框（用户屏幕上突然出现关机确认窗）。
pub fn key_combo(combo: &str) -> Result<(), String> {
    let vks = parse_combo(combo)?;
    for &vk in &vks {
        let _ = send(&[key_input(vk, 0)]);
    }
    std::thread::sleep(std::time::Duration::from_millis(15));
    let _ = send(&[key_input(*vks.last().unwrap(), KEYEVENTF_KEYUP)]);
    std::thread::sleep(std::time::Duration::from_millis(15));
    for &vk in vks[..vks.len() - 1].iter().rev() {
        let _ = send(&[key_input(vk, KEYEVENTF_KEYUP)]);
    }
    Ok(())
}

/// 某键此刻是否按下（诊断用：验证组合键真的落下了）。
pub fn is_key_down(vk: i32) -> bool {
    unsafe { (GetAsyncKeyState(vk) as u32 & 0x8000) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_parsing() {
        assert_eq!(MouseButton::parse("LEFT"), Some(MouseButton::Left));
        assert_eq!(MouseButton::parse("Right"), Some(MouseButton::Right));
        assert_eq!(MouseButton::parse("middle"), Some(MouseButton::Middle));
        assert_eq!(MouseButton::parse("x"), None);
    }

    #[test]
    fn key_name_lookup() {
        assert_eq!(vk_from_name("enter"), Some(VK_RETURN));
        assert_eq!(vk_from_name("ESC"), Some(VK_ESCAPE));
        assert_eq!(vk_from_name("ctrl"), Some(VK_CONTROL));
        assert_eq!(vk_from_name("a"), Some(0x41)); // 'A'
        assert_eq!(vk_from_name("5"), Some(0x35)); // '5'
        assert_eq!(vk_from_name("f4"), Some(VK_F4));
        assert_eq!(vk_from_name("-"), Some(VK_OEM_MINUS));
        assert_eq!(vk_from_name("不认识的键"), None);
    }

    /// 组合键解析覆盖生产入口用到的写法。**只测解析，不发键**——
    /// 真发键的单测会打到用户当前桌面（alt+f4 在无焦点窗口时弹「关闭 Windows」，
    /// 2026-10-21 实锤）。真正发键的通路由 AGENT_COMPUTER_E2E 那条记事本 e2e 覆盖。
    #[test]
    fn combo_requires_known_keys() {
        assert_eq!(parse_combo("ctrl+c").unwrap(), vec![VK_CONTROL, 0x43]);
        assert_eq!(parse_combo("alt+f4").unwrap(), vec![VK_MENU, VK_F4]);
        assert_eq!(parse_combo("end").unwrap(), vec![VK_END]);
        assert_eq!(parse_combo("ctrl+shift+t").unwrap(), vec![VK_CONTROL, VK_SHIFT, 0x54]);
        assert_eq!(parse_combo(" ctrl + c ").unwrap(), vec![VK_CONTROL, 0x43], "空白宽容");
        assert!(parse_combo("ctrl+不懂").is_err());
        assert!(parse_combo("").is_err());
        assert!(parse_combo("   ").is_err());
    }

    #[test]
    fn abs_mapping_stays_in_range() {
        // 任意（含虚拟屏外）坐标都必须落在 0..=65535
        let (vx, vy, vw, vh) = screen::virtual_screen();
        for (x, y) in [(vx, vy), (vx + vw as i32 - 1, vy + vh as i32 - 1), (-9999, -9999), (99999, 99999)] {
            let (nx, ny) = to_abs(x, y);
            assert!((0..=65535).contains(&nx) && (0..=65535).contains(&ny), "({x},{y}) → ({nx},{ny}) 越界");
        }
    }
}
