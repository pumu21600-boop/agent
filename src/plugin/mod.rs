//! 插件系统 —— 总架构 §7
//!
//! MCP client（进程隔离）+ WASM 沙箱 + 双总线。
//! 016（P11）：MCP client stdio 最小闭环已落地（见 mcp.rs）；WASM 与组件总线留 017。

pub mod mcp;
pub mod ui;
pub mod wasm;

/// 工具简述：(工具名, 描述)。MCP 与 WASM 两条桥的下发/UI 共用同一形状。
pub type ToolBrief = (String, String);
