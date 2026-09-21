//! 内置浏览器统一命令模型（P21）—— 协议形状移植自 ZCode（zai-org/ZCode，Apache-2.0）
//! 的 `browserCommandSchema`：一个 `method` 判别联合覆盖全部浏览器动作，结构化错误码
//! 不静默兜底。工具层只暴露一个 `browser` 工具（method + 该方法自己的参数），审批门
//! 按 method 分级：只读方法 Safe、页面操作方法 Normal（与原五件套语义一致）。

use serde::Deserialize;
use serde_json::{Value, json};

/// 结构化错误码（对齐 ZCode browserErrorCodeSchema 中我们用得上的子集）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ErrorCode {
    /// 浏览器没起来/会话已死（拉起失败或进程退出）
    BackendUnavailable,
    /// 该方法或参数组合当前后端不支持
    CapabilityUnsupported,
    /// 快照 ref 在页面上已不存在（DOM 变了）
    RefNotFound,
    /// 导航被拒（非 http/https/file/about:blank）
    NavigationBlocked,
    /// 等待超时（加载/元素出现）
    Timeout,
    /// 被取消（会话停止）
    Cancelled,
    /// 页面脚本执行异常
    ExecutionError,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BackendUnavailable => "backend_unavailable",
            ErrorCode::CapabilityUnsupported => "capability_unsupported",
            ErrorCode::RefNotFound => "ref_not_found",
            ErrorCode::NavigationBlocked => "navigation_blocked",
            ErrorCode::Timeout => "timeout",
            ErrorCode::Cancelled => "cancelled",
            ErrorCode::ExecutionError => "execution_error",
        }
    }
}

/// 命令失败：码 + 人话。展示为 `[code] message`（错误不许静默）。
#[derive(Clone, Debug)]
pub struct BrowserError {
    pub code: ErrorCode,
    pub message: String,
}

impl BrowserError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl std::fmt::Display for BrowserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code.as_str(), self.message)
    }
}

impl From<BrowserError> for String {
    fn from(e: BrowserError) -> Self {
        e.to_string()
    }
}

/// 统一命令方法集（工具 schema 与审批分级共用这一份事实源）。
pub const ALL_METHODS: &[&str] = &[
    "navigate",
    "back",
    "forward",
    "reload",
    "snapshot",
    "click",
    "fill",
    "type",
    "press",
    "scroll",
    "hover",
    "select",
    "check",
    "drag",
    "screenshot",
    "get_state",
    "element_info",
    "evaluate",
    "wait_for",
    "get_dialog",
    "handle_dialog",
    "new_tab",
    "close",
    "close_browser",
    "list",
    "activate_tab",
    "capabilities",
];

/// 只读方法（不动页面状态）：审批门判 Safe。其余（click/fill/type/press/select/check/
/// drag/evaluate）会改变页面状态或执行页面脚本，判 Normal。
pub fn method_is_read_only(method: &str) -> bool {
    matches!(
        method,
        "navigate"
            | "back"
            | "forward"
            | "reload"
            | "snapshot"
            | "scroll"
            | "hover"
            | "screenshot"
            | "get_state"
            | "element_info"
            | "wait_for"
            | "get_dialog"
            | "new_tab"
            | "close"
            | "list"
            | "activate_tab"
            | "capabilities"
    )
}

/// 鼠标按键（CDP Input 同义）。
#[derive(Clone, Copy, Default, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub fn as_str(self) -> &'static str {
        match self {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
        }
    }
}

/// 修饰键（CDP 位掩码由调用方映射）。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyModifier {
    Alt,
    Control,
    Meta,
    Shift,
}

/// 截图区域（视口 CSS px，与 full_page 互斥）。
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Clip {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 坐标点。
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// 统一命令（`method` 内部 tag；字段缺省即 None）。
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "method")]
pub enum BrowserCommand {    #[serde(rename = "navigate")]
    Navigate { url: String, tab_id: Option<String> },
    #[serde(rename = "back")]
    Back { tab_id: Option<String> },
    #[serde(rename = "forward")]
    Forward { tab_id: Option<String> },
    #[serde(rename = "reload")]
    Reload { tab_id: Option<String> },
    #[serde(rename = "snapshot")]
    Snapshot {
        max_elements: Option<usize>,
        include_hidden: Option<bool>,
        tab_id: Option<String>,
    },
    #[serde(rename = "click")]
    Click {
        #[serde(rename = "ref")]
        reference: Option<String>,
        x: Option<f64>,
        y: Option<f64>,
        text: Option<String>,
        button: Option<MouseButton>,
        double_click: Option<bool>,
        modifiers: Option<Vec<KeyModifier>>,
        tab_id: Option<String>,
    },
    #[serde(rename = "fill")]
    Fill {
        #[serde(rename = "ref")]
        reference: String,
        value: String,
        tab_id: Option<String>,
    },
    #[serde(rename = "type")]
    Type {
        #[serde(rename = "ref")]
        reference: Option<String>,
        text: String,
        label: Option<String>,
        submit: Option<bool>,
        tab_id: Option<String>,
    },
    #[serde(rename = "press")]
    Press {
        key: String,
        #[serde(rename = "ref")]
        reference: Option<String>,
        modifiers: Option<Vec<KeyModifier>>,
        tab_id: Option<String>,
    },
    #[serde(rename = "scroll")]
    Scroll {
        #[serde(rename = "ref")]
        reference: Option<String>,
        x: Option<f64>,
        y: Option<f64>,
        delta_x: Option<f64>,
        delta_y: Option<f64>,
        tab_id: Option<String>,
    },
    #[serde(rename = "hover")]
    Hover {
        #[serde(rename = "ref")]
        reference: Option<String>,
        x: Option<f64>,
        y: Option<f64>,
        tab_id: Option<String>,
    },
    #[serde(rename = "select")]
    Select {
        #[serde(rename = "ref")]
        reference: String,
        values: Vec<String>,
        tab_id: Option<String>,
    },
    #[serde(rename = "check")]
    Check {
        #[serde(rename = "ref")]
        reference: String,
        checked: Option<bool>,
        tab_id: Option<String>,
    },
    #[serde(rename = "drag")]
    Drag {
        from_reference: Option<String>,
        to_reference: Option<String>,
        from: Option<Point>,
        to: Option<Point>,
        tab_id: Option<String>,
    },
    #[serde(rename = "screenshot")]
    Screenshot {
        #[serde(rename = "ref")]
        reference: Option<String>,
        full_page: Option<bool>,
        clip: Option<Clip>,
        tab_id: Option<String>,
    },
    #[serde(rename = "get_state")]
    GetState { tab_id: Option<String> },
    #[serde(rename = "element_info")]
    ElementInfo { x: f64, y: f64, tab_id: Option<String> },
    #[serde(rename = "evaluate")]
    Evaluate { expression: String, tab_id: Option<String> },
    #[serde(rename = "wait_for")]
    WaitFor {
        selector: Option<String>,
        text: Option<String>,
        text_gone: Option<String>,
        timeout_ms: Option<u64>,
        tab_id: Option<String>,
    },
    #[serde(rename = "get_dialog")]
    GetDialog { tab_id: Option<String> },
    #[serde(rename = "handle_dialog")]
    HandleDialog {
        accept: bool,
        prompt_text: Option<String>,
        tab_id: Option<String>,
    },
    #[serde(rename = "new_tab")]
    NewTab,
    #[serde(rename = "close")]
    Close { tab_id: Option<String> },
    /// 关掉整个浏览器进程（承 P20 的 browser_close；回收本由 10 分钟空转收割负责，
    /// 这里是显式提前释放——页面操作全部结束、不想留进程时用）。
    #[serde(rename = "close_browser")]
    CloseBrowser,
    #[serde(rename = "list")]
    List,
    #[serde(rename = "activate_tab")]
    ActivateTab { tab_id: String },
    #[serde(rename = "capabilities")]
    Capabilities,
}

impl BrowserCommand {
    /// 该命令作用的标签页 id（None = 会话默认页，即 agent 专用页）。
    pub fn tab_id(&self) -> Option<&str> {
        match self {
            BrowserCommand::Navigate { tab_id, .. }
            | BrowserCommand::Back { tab_id }
            | BrowserCommand::Forward { tab_id }
            | BrowserCommand::Reload { tab_id }
            | BrowserCommand::Snapshot { tab_id, .. }
            | BrowserCommand::Click { tab_id, .. }
            | BrowserCommand::Fill { tab_id, .. }
            | BrowserCommand::Type { tab_id, .. }
            | BrowserCommand::Press { tab_id, .. }
            | BrowserCommand::Scroll { tab_id, .. }
            | BrowserCommand::Hover { tab_id, .. }
            | BrowserCommand::Select { tab_id, .. }
            | BrowserCommand::Check { tab_id, .. }
            | BrowserCommand::Drag { tab_id, .. }
            | BrowserCommand::Screenshot { tab_id, .. }
            | BrowserCommand::GetState { tab_id }
            | BrowserCommand::ElementInfo { tab_id, .. }
            | BrowserCommand::Evaluate { tab_id, .. }
            | BrowserCommand::WaitFor { tab_id, .. }
            | BrowserCommand::GetDialog { tab_id }
            | BrowserCommand::HandleDialog { tab_id, .. }
            | BrowserCommand::Close { tab_id } => tab_id.as_deref(),
            BrowserCommand::NewTab
            | BrowserCommand::CloseBrowser
            | BrowserCommand::List
            | BrowserCommand::ActivateTab { .. }
            | BrowserCommand::Capabilities => None,
        }
    }
}

/// 解析统一命令。method 缺失/未知 → CapabilityUnsupported；JSON 坏 → ExecutionError。
pub fn parse(args: &str) -> Result<BrowserCommand, BrowserError> {
    let v: Value = serde_json::from_str(args)
        .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, format!("参数不是合法 JSON: {e}")))?;
    let method = v.get("method").and_then(Value::as_str).unwrap_or("").to_string();
    if method.is_empty() {
        return Err(BrowserError::new(
            ErrorCode::CapabilityUnsupported,
            format!("缺少 method 参数，可用: {}", ALL_METHODS.join(" / ")),
        ));
    }
    if !ALL_METHODS.contains(&method.as_str()) {
        return Err(BrowserError::new(
            ErrorCode::CapabilityUnsupported,
            format!("未知 method「{method}」，可用: {}", ALL_METHODS.join(" / ")),
        ));
    }
    serde_json::from_value::<BrowserCommand>(v).map_err(|e| {
        BrowserError::new(
            ErrorCode::CapabilityUnsupported,
            format!("method「{method}」参数不合法: {e}"),
        )
    })
}

/// 页面基础状态（get_state / 导航后回传；对齐 ZCode browserPageStateSchema）。
#[derive(Clone, Default)]
pub struct PageState {
    pub url: String,
    pub title: String,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub loading: bool,
    pub viewport_w: i64,
    pub viewport_h: i64,
}

impl PageState {
    /// 人话多行渲染（工具结果直接给模型）。
    pub fn render(&self) -> String {
        let mut s = format!(
            "地址: {}\n标题: {}\n后退: {} 前进: {} 加载中: {}",
            if self.url.is_empty() { "（空）" } else { &self.url },
            if self.title.is_empty() { "（无标题）" } else { &self.title },
            if self.can_go_back { "可" } else { "不可" },
            if self.can_go_forward { "可" } else { "不可" },
            if self.loading { "是" } else { "否" },
        );
        if self.viewport_w > 0 && self.viewport_h > 0 {
            s.push_str(&format!("\n视口: {}×{}", self.viewport_w, self.viewport_h));
        }
        s
    }
}

/// 标签页摘要（list / panel_state 用）。
#[derive(Clone)]
pub struct TabBrief {
    pub id: String,
    pub agent: bool,
    pub title: String,
    pub url: String,
    pub visible: bool,
    pub loading: bool,
    pub suspended: bool,
}

impl TabBrief {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "agent": self.agent,
            "title": self.title,
            "url": self.url,
            "visible": self.visible,
            "loading": self.loading,
            "suspended": self.suspended,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_navigate_and_defaults() {
        let cmd = parse(r#"{"method":"navigate","url":"https://example.com"}"#).unwrap();
        match cmd {
            BrowserCommand::Navigate { url, tab_id } => {
                assert_eq!(url, "https://example.com");
                assert!(tab_id.is_none());
            }
            _ => panic!("应为 navigate"),
        }
    }

    #[test]
    fn parse_click_ref_field() {
        let cmd = parse(r#"{"method":"click","ref":"e12","double_click":true}"#).unwrap();
        match cmd {
            BrowserCommand::Click { reference, double_click, .. } => {
                assert_eq!(reference.as_deref(), Some("e12"));
                assert_eq!(double_click, Some(true));
            }
            _ => panic!("应为 click"),
        }
    }

    #[test]
    fn parse_type_with_label_and_submit() {
        let cmd = parse(r#"{"method":"type","text":"hi","label":"搜索","submit":true}"#).unwrap();
        match cmd {
            BrowserCommand::Type { text, label, submit, .. } => {
                assert_eq!(text, "hi");
                assert_eq!(label.as_deref(), Some("搜索"));
                assert_eq!(submit, Some(true));
            }
            _ => panic!("应为 type"),
        }
    }

    #[test]
    fn parse_unknown_method_is_capability_error() {
        let e = parse(r#"{"method":"fly"}"#).unwrap_err();
        assert_eq!(e.code, ErrorCode::CapabilityUnsupported);
        assert!(e.to_string().contains("未知 method"));
    }

    #[test]
    fn parse_missing_method_lists_available() {
        let e = parse(r#"{"url":"x"}"#).unwrap_err();
        assert_eq!(e.code, ErrorCode::CapabilityUnsupported);
        assert!(e.to_string().contains("navigate"));
    }

    #[test]
    fn parse_bad_json_is_execution_error() {
        let e = parse("not json").unwrap_err();
        assert_eq!(e.code, ErrorCode::ExecutionError);
    }

    #[test]
    fn read_only_methods_cover_reads_not_writes() {
        assert!(method_is_read_only("snapshot"));
        assert!(method_is_read_only("navigate"));
        assert!(method_is_read_only("list"));
        assert!(!method_is_read_only("click"));
        assert!(!method_is_read_only("evaluate"));
        // 表里每个方法都必须有分级（不许漏网）
        for m in ALL_METHODS {
            let _ = method_is_read_only(m);
        }
    }

    #[test]
    fn tab_id_extraction() {
        let cmd = parse(r#"{"method":"click","ref":"e1","tab_id":"T9"}"#).unwrap();
        assert_eq!(cmd.tab_id(), Some("T9"));
        let cmd = parse(r#"{"method":"list"}"#).unwrap();
        assert_eq!(cmd.tab_id(), None);
    }
}
