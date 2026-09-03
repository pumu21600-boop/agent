//! 插件系统 —— 总架构 §7
//!
//! MCP client（进程隔离）+ WASM 沙箱 + 双总线。
//! 016（P11）：MCP client stdio 最小闭环已落地（见 mcp.rs）；WASM 与组件总线留 017。

pub mod mcp;
