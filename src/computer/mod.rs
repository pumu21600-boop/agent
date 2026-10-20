//! computer 工具：操作电脑（2026-10-20 免打扰模式第 4 轮）。
//!
//! 能力面（一个工具按 method 覆盖，与 browser/ui 同 house style）：
//! - screenshot 截虚拟屏 PNG（落盘工作区 + 内联给模型，模型这才「看得见」）
//! - cursor / screen_size / windows / focus 窗口与坐标
//! - click / move / scroll / drag / type / key 键鼠控制（SendInput）
//!
//! 纪律：
//! - **截图内联进工具结果**（`<<COMPUTER_IMG:base64>>` 标记，exec.rs 剥出升级成
//!   多模态 tool 消息）——没有这条，agent 就是瞎子；落盘的那份给人/面板看。
//! - 图片只进请求上下文，**不进会话落盘**（快照剥离，见 summary.rs）——
//!   否则 session 文件被 base64 撑爆。
//! - 只读方法（screenshot/cursor/screen_size/windows）审批归 Safe；键鼠动作归
//!   Normal，按会话审批档走闸（rules.rs）。
//! - 每一步都经 ui_events 总线推 `computer.action`——聊天区实时条 + 主区面板，
//!   用户随时看得到 agent 在动他的电脑（用户拍板：UI 不落侧边栏）。

mod input;
mod png;
mod screen;
mod window;

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use input::MouseButton;

/// 工具结果里的截图标记（exec.rs 认这个前后缀）。base64 PNG。
pub const IMG_OPEN: &str = "\n<<COMPUTER_IMG:";
pub const IMG_CLOSE: &str = ">>";

/// 内联给模型的截图宽度上限（超过就最近邻降采样——省 token，UI 细节靠落盘那份）。
const INLINE_MAX_WIDTH: u32 = 1440;

/// 截图落盘目录（工作区下）。用户可直接翻看，面板也读这里。
fn shots_dir() -> std::path::PathBuf {
    crate::tools::workspace_root().join("screenshots")
}

/// 最近一次截图（面板/重开后回看用）。
#[derive(Default, Clone)]
pub struct ComputerState {
    pub last_screenshot: Option<String>,
    pub last_size: Option<(u32, u32)>,
    pub actions: Vec<String>,
}

fn state_slot() -> &'static Mutex<ComputerState> {
    static S: OnceLock<Mutex<ComputerState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(ComputerState::default()))
}

pub fn state() -> ComputerState {
    state_slot().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn record_action(line: &str, shot: Option<String>, size: Option<(u32, u32)>) {
    {
        let mut s = state_slot().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = &shot {
            s.last_screenshot = Some(p.clone());
        }
        if size.is_some() {
            s.last_size = size;
        }
        s.actions.insert(0, line.to_string());
        s.actions.truncate(20);
    }
    crate::server::ui_events::emit(json!({
        "kind": "computer.action",
        "action": line,
        "screenshot": shot,
    }));
}

/// 从工具结果里剥出截图 base64（标记从文本中移除）。没有标记返回 None。
pub fn take_image(content: &mut String) -> Option<String> {
    let start = content.find(IMG_OPEN)?;
    let rest = &content[start + IMG_OPEN.len()..];
    let end = rest.find(IMG_CLOSE)?;
    let b64 = rest[..end].to_string();
    content.replace_range(start..start + IMG_OPEN.len() + end + IMG_CLOSE.len(), "");
    content.push('\n');
    Some(b64)
}

/// 方法清单（工具 schema enum 与 rules 分级共用）。
pub const ALL_METHODS: &[&str] = &[
    "screenshot", "cursor", "screen_size", "windows", "focus", "click", "move", "scroll", "drag", "type", "key",
];

/// 只读方法（审批 Safe）。键鼠动作会真的动用户的电脑，归 Normal。
pub fn method_is_read_only(method: &str) -> bool {
    matches!(method, "screenshot" | "cursor" | "screen_size" | "windows")
}

/// 工具入口（tools/mod.rs 分发到这里）。全部 Win32 调用包 spawn_blocking，
/// 不占 tokio worker（截图编码 + 逐键输入都是毫秒级同步活）。
pub async fn execute(args: &str, max_chars: usize) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let method = v.get("method").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if !ALL_METHODS.contains(&method.as_str()) {
        return Err(format!(
            "缺少/未知 method，可用: {}",
            ALL_METHODS.join(" / ")
        ));
    }
    tokio::task::spawn_blocking(move || dispatch(&v, &method, max_chars))
        .await
        .map_err(|e| format!("computer 任务panic: {e}"))?
}

fn dispatch(v: &Value, method: &str, max_chars: usize) -> Result<String, String> {
    match method {
        "screenshot" => cmd_screenshot(v, max_chars),
        "cursor" => {
            let (x, y) = screen::cursor_pos().ok_or("取光标位置失败")?;
            record_action(&format!("查光标位置 ({x},{y})"), None, None);
            Ok(format!("光标位置：({x}, {y})"))
        }
        "screen_size" => {
            let (x, y, w, h) = screen::virtual_screen();
            record_action(&format!("查屏幕尺寸 {w}×{h}"), None, None);
            Ok(format!("虚拟屏 {w}×{h}，左上角 ({x}, {y})（多屏拼接；点击/截图坐标都用这套）"))
        }
        "windows" => {
            let wins = window::list_windows();
            let mut out = format!("可见窗口 {} 个（按前后顺序）：\n", wins.len());
            for (i, w) in wins.iter().take(30).enumerate() {
                out.push_str(&format!("{}. [hwnd {}] {}\n", i + 1, w.hwnd, w.title));
            }
            record_action(&format!("列窗口 {} 个", wins.len()), None, None);
            Ok(crate::context::compress(&out, max_chars))
        }
        "focus" => {
            let title = v.get("title").and_then(Value::as_str).map(str::trim).unwrap_or("");
            let hwnd = v.get("hwnd").and_then(Value::as_i64);
            let (hwnd, label) = if let Some(h) = hwnd {
                (h as isize, format!("hwnd {h}"))
            } else if !title.is_empty() {
                let w = window::find_window(title).ok_or_else(|| format!("找不到标题含「{title}」的窗口"))?;
                (w.hwnd, w.title)
            } else {
                return Err("focus 需要 title 或 hwnd".into());
            };
            let ok = window::focus_window(hwnd)?;
            record_action(&format!("聚焦窗口「{label}」{}", if ok { "" } else { "（系统没让位，可能闪了下任务栏）" }), None, None);
            Ok(if ok {
                format!("已把「{label}」带到前台。{}", window::focus_diagnostics())
            } else {
                format!("SetForegroundWindow 被拒（前台锁）。当前还是：{}", window::focus_diagnostics())
            })
        }
        "click" => {
            let (x, y) = xy(v)?;
            let button = MouseButton::parse(v.get("button").and_then(Value::as_str).unwrap_or("left"))
                .ok_or("button 只能是 left/right/middle")?;
            let clicks = v.get("clicks").and_then(Value::as_u64).unwrap_or(1).clamp(1, 3) as u32;
            input::click(x, y, button, clicks);
            record_action(&format!("{clicks}连击 {button:?} @({x},{y})"), None, None);
            Ok(format!("已在 ({x}, {y}) {button:?} 点击 {clicks} 次"))
        }
        "move" => {
            let (x, y) = xy(v)?;
            input::move_to(x, y);
            record_action(&format!("移动光标 @({x},{y})"), None, None);
            Ok(format!("光标已移到 ({x}, {y})"))
        }
        "scroll" => {
            let (x, y) = xy(v)?;
            let dx = v.get("dx").and_then(Value::as_i64).unwrap_or(0).clamp(-10, 10) as i32;
            let dy = v.get("dy").and_then(Value::as_i64).unwrap_or(0).clamp(-10, 10) as i32;
            input::scroll(x, y, dx, dy);
            record_action(&format!("滚轮 @({x},{y}) dx={dx} dy={dy}"), None, None);
            Ok(format!("已在 ({x}, {y}) 滚动（横 {dx} 竖 {dy} 格）"))
        }
        "drag" => {
            let from = pair(v, "from")?;
            let to = pair(v, "to")?;
            let button = MouseButton::parse(v.get("button").and_then(Value::as_str).unwrap_or("left"))
                .ok_or("button 只能是 left/right/middle")?;
            input::drag(from, to, button);
            record_action(&format!("拖拽 {button:?} {from:?}→{to:?}"), None, None);
            Ok(format!("已从 {from:?} 拖到 {to:?}"))
        }
        "type" => {
            let text = v.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                return Err("type 需要 text".into());
            }
            input::type_text(text);
            let preview: String = text.chars().take(40).collect();
            record_action(&format!("输入 {} 字：{preview}…", text.chars().count()), None, None);
            Ok(format!("已输入 {} 字符。焦点在：{}", text.chars().count(), window::focus_diagnostics()))
        }
        "key" => {
            let combo = v.get("combo").and_then(Value::as_str).unwrap_or("").trim();
            if combo.is_empty() {
                return Err("key 需要 combo（如 \"ctrl+c\" / \"enter\"）".into());
            }
            input::key_combo(combo)?;
            record_action(&format!("按键 {combo}"), None, None);
            Ok(format!("已按 {combo}。焦点在：{}", window::focus_diagnostics()))
        }
        _ => unreachable!(),
    }
}

fn xy(v: &Value) -> Result<(i32, i32), String> {
    let x = v.get("x").and_then(Value::as_i64).ok_or("缺少 x（屏幕坐标）")? as i32;
    let y = v.get("y").and_then(Value::as_i64).ok_or("缺少 y（屏幕坐标）")? as i32;
    Ok((x, y))
}

fn pair(v: &Value, key: &str) -> Result<(i32, i32), String> {
    let a = v.get(key).and_then(Value::as_array).ok_or_else(|| format!("缺少 {key}（[x,y] 数组）"))?;
    if a.len() != 2 {
        return Err(format!("{key} 得是 [x, y] 两元素"));
    }
    Ok((a[0].as_i64().unwrap_or(0) as i32, a[1].as_i64().unwrap_or(0) as i32))
}

fn cmd_screenshot(v: &Value, max_chars: usize) -> Result<String, String> {
    let _ = v; // 预留：region/质量参数（v1 全屏）
    let (bgra, w, h, stride) = screen::capture_bgra()?;
    // 全屏同色 = 锁屏/安全桌面/黑屏——说人话，别让模型对全黑图硬分析
    let solid_hint = if screen::is_solid_color(&bgra) {
        "\n注意：整屏同色（疑似锁屏/安全桌面/黑屏）——先确认用户在场，别对着纯色图做判断。"
    } else {
        ""
    };
    // 全尺寸 PNG：落盘给人/面板看
    let png_full = png::encode_png_from_bgra(&bgra, w, h, stride)?;
    let dir = shots_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("建截图目录失败: {e}"))?;
    let name = format!("computer_{}.png", chrono::Local::now().format("%Y%m%d_%H%M%S_%3f"));
    let rel = format!("screenshots/{name}");
    std::fs::write(dir.join(&name), &png_full).map_err(|e| format!("写截图失败: {e}"))?;
    // 内联版：超限宽先最近邻降采样再编码（token 账；面板看全尺寸那份）
    let (inline_png, iw, ih) = match png::downscale_bgra(&bgra, w, h, stride, INLINE_MAX_WIDTH) {
        Some((nw, nh, raw)) => (png::encode_png_from_bgra(&raw, nw, nh, (nw as usize) * 4)?, nw, nh),
        None => (png_full.clone(), w, h),
    };
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &inline_png);
    record_action(&format!("截图 {w}×{h} → {rel}"), Some(rel.clone()), Some((w, h)));
    // 只压文本部分——标记+base64 原样拼尾部（压缩会掐中段，绝不动它）
    let text = crate::context::compress(
        &format!(
            "已截图 {w}×{h}（内联 {iw}×{ih}，全尺寸存 {rel}，用户可在面板看）。\
             屏幕坐标即点击坐标，直接对看到的元素调 click/type。{solid_hint}"
        ),
        max_chars,
    );
    Ok(format!("{text}{IMG_OPEN}{b64}{IMG_CLOSE}"))
}


#[cfg(test)]
mod real_input_tests {
    //! 真实键鼠 e2e（默认忽略，AGENT_COMPUTER_E2E=1 才跑）。
    //! 靶子是记事本：自己拉起来、自己聚焦、自己输入、自己收。**绝不碰用户前台**——
    //! 没有 AGENT_COMPUTER_E2E 环境变量时整个测试跳过（CI/别人机器上不误触）。
    use super::*;

    #[test]
    #[ignore = "真实键鼠：AGENT_COMPUTER_E2E=1 才跑（会动你的桌面）"]
    fn type_into_notepad_lands() {
        if std::env::var("AGENT_COMPUTER_E2E").is_err() {
            return;
        }
        // 靶子记事本：自己拉、自己收（Child 句柄 kill，不留进程）
        let mut child = std::process::Command::new("notepad.exe")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdin(std::process::Stdio::null())
            .spawn()
            .expect("拉起记事本");
        std::thread::sleep(std::time::Duration::from_millis(2000));
        let w = window::find_window("记事本")
            .or_else(|| window::find_window("Notepad"))
            .expect("找到记事本窗口");
        assert!(window::focus_window(w.hwnd).is_ok(), "聚焦调用不报错");
        std::thread::sleep(std::time::Duration::from_millis(600));
        assert!(
            window::foreground_title().is_some_and(|t| t.contains("记事本") || t.contains("Notepad")),
            "前台应是记事本"
        );
        // 1) unicode 输入落地（中文/符号同路径，UTF-16 码元直灌）
        let marker = format!("AGENT_E2E_{}", std::process::id());
        input::type_text(&marker);
        std::thread::sleep(std::time::Duration::from_millis(400));
        let c1 = window::edit_control_text(w.hwnd).unwrap_or_default();
        assert!(c1.contains(&marker), "unicode 输入应落进编辑框，实际：{c1:?}");
        // 2) 鼠标点击定位：点编辑控件左上角内侧，光标离开行尾
        let Some((_eh, (el, et, _, _))) = window::edit_control_rect(w.hwnd) else {
            child.kill().ok();
            panic!("找不到记事本的 Edit 子控件");
        };
        input::click(el + 12, et + 8, input::MouseButton::Left, 1);
        std::thread::sleep(std::time::Duration::from_millis(300));
        input::type_text("X");
        std::thread::sleep(std::time::Duration::from_millis(400));
        let c2 = window::edit_control_text(w.hwnd).unwrap_or_default();
        assert_ne!(c2, format!("{marker}X"), "鼠标点击应把光标从行尾移走（实际：{c2:?}）");
        // 点击把光标挪到了文本中段（marker 被 X 断开=没落在行尾，正是鼠标定位的证据）
        assert!(c2.contains('X') && c2.len() == marker.len() + 1, "X 应插在文本中：{c2:?}");
        // 3) 组合键 end：光标到行尾，Y 落在最后
        input::key_combo("end").expect("end 键");
        std::thread::sleep(std::time::Duration::from_millis(200));
        input::type_text("Y");
        std::thread::sleep(std::time::Duration::from_millis(400));
        let c3 = window::edit_control_text(w.hwnd).unwrap_or_default();
        assert!(c3.ends_with('Y'), "end 组合键应把光标移行尾（实际：{c3:?}");
        // 4) 截图通路（顺路验真实桌面能出图）
        let (bgra, sw, sh, stride) = screen::capture_bgra().expect("截图成功");
        assert!(sw > 0 && sh > 0);
        let png = png::encode_png_from_bgra(&bgra, sw, sh, stride).expect("PNG 编码成功");
        assert!(png.starts_with(&[0x89u8, b'P', b'N', b'G']), "PNG 签名");
        assert!(png.len() > 10_000, "截图应有实际内容（{} 字节）", png.len());
        // 5) 收拾：关窗不保存（alt+f4 → 对话框按 N）；句柄 kill 兜底
        let _ = input::key_combo("alt+f4");
        std::thread::sleep(std::time::Duration::from_millis(400));
        let _ = input::key_combo("n");
        std::thread::sleep(std::time::Duration::from_millis(400));
        child.kill().ok();
        child.wait().ok();
    }
}
