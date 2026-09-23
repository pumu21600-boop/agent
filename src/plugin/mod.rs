//! 插件系统 —— 总架构 §7
//!
//! P18 施工单 027 起为**二合一模型**：扩展与 WASM 插件统一为「插件」（一个目录 + 一份
//! manifest.json，只声明能力、不贴类型标签）；MCP 保持独立层（公开标准协议，兼容责任
//! 在外）。对外四词：Skill / MCP / 插件 / 沙箱。契约约定见 contract.rs。

pub mod contract;
pub mod draft;
pub mod mcp;
pub mod trust;
pub mod ui;
pub mod wasm;

/// 工具简述：(工具名, 描述)。MCP 与 WASM 两条桥的下发/UI 共用同一形状。
pub type ToolBrief = (String, String);

/// 测试专用：wasm 注册表与 draft 都挂全局静态，多个测试并发碰会互相清场。
/// 沾注册表的测试一律先锁这把锁（跨文件共用，故放模块根）。
#[cfg(test)]
pub static REGISTRY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
