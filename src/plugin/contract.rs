//! 插件契约 v1（施工单 027，P18）—— 二合一后的统一约定。
//!
//! 2026-09-19 定案：**扩展与 WASM 插件二合一为「插件」**，一个插件 = 一个目录 + 一份
//! manifest.json，清单里只声明「有什么」（面板 / 工具 / 权限），不贴类型标签——一个插件
//! 可以同时有面板和工具。对外只留四个词：**Skill / MCP / 插件 / 沙箱**；「扩展」「WASM
//! 插件」作为术语退役（MCP 是独立一层，不并入插件：公开标准协议、兼容责任在外）。
//!
//! 契约纪律（6.3 省事方案）：
//! - **只增不删**：解析器忽略未知字段，加字段天然向后兼容；
//! - **apiVersion 不匹配即拒载并提示升级**，不做 shim；README 写明 v0.x 不保证兼容；
//! - **source（来源）三值**：`built_in`（随包）/ `user`（用户安装）/ `model_generated`
//!   （模型自产，走 draft 隔离区）。报告原枚举（user/mcp/wasm/model_generated）作废——
//!   MCP 是一层不是一种来源，WASM 是一种实现不是一种来源。

use serde_json::Value;

/// 当前契约版本。清单缺失该字段按 v1 读入（老目录自动迁移）；写了但不等于 1 → 拒载。
pub const API_VERSION: u32 = 1;

/// 来源三值。
pub const ORIGIN_BUILT_IN: &str = "built_in";
pub const ORIGIN_USER: &str = "user";
pub const ORIGIN_MODEL: &str = "model_generated";

/// 校验清单 apiVersion：缺失/Null → v1（兼容旧清单）；非数字 → 报错；
/// 不等于当前版本 → 报「请升级」并拒载（不做 shim）。
pub fn check_api_version(manifest: &Value) -> Result<u32, String> {
    match manifest.get("apiVersion") {
        None | Some(Value::Null) => Ok(API_VERSION),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| "apiVersion 必须是正整数".to_string())?;
            if n == API_VERSION as u64 {
                Ok(API_VERSION)
            } else {
                Err(format!(
                    "插件 API 版本 v{n} 不受支持（当前宿主支持 v{API_VERSION}）：请升级宿主或插件后重试"
                ))
            }
        }
    }
}

/// 解析清单声明的来源；未声明/不合法时按所在位置取默认。
/// **P21 红线3 收口**：只有应用根内置位（`app`）默认 built_in，其余位置（工作区 plugins、
/// plugins/ 自装目录）一律默认 user——「不声明 source」不能成为绕过第三方验签的后门。
pub fn origin_of(manifest: &Value, location: &str) -> String {
    match manifest.get("source").and_then(Value::as_str) {
        Some(s @ (ORIGIN_BUILT_IN | ORIGIN_USER | ORIGIN_MODEL)) => s.to_string(),
        _ if location == "app" => ORIGIN_BUILT_IN.to_string(),
        _ => ORIGIN_USER.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn api_version_matrix() {
        // 缺字段 = 旧清单，按 v1 读入（老目录自动迁移，不丢能力）
        assert_eq!(check_api_version(&json!({})).unwrap(), 1);
        assert_eq!(check_api_version(&json!({"apiVersion": null})).unwrap(), 1);
        assert_eq!(check_api_version(&json!({"apiVersion": 1})).unwrap(), 1);
        // 不匹配 → 拒载 + 提示升级（不做 shim）
        let err = check_api_version(&json!({"apiVersion": 2})).unwrap_err();
        assert!(err.contains("v2"), "{err}");
        assert!(err.contains("升级"), "{err}");
        assert!(check_api_version(&json!({"apiVersion": "1"})).is_err(), "必须是数字");
    }

    #[test]
    fn origin_defaults_and_overrides() {
        // 未声明：按位置取默认
        assert_eq!(origin_of(&json!({}), "app"), ORIGIN_BUILT_IN);
        assert_eq!(origin_of(&json!({}), "workspace"), ORIGIN_USER);
        // 声明了合法值：以声明为准
        assert_eq!(origin_of(&json!({"source": "user"}), "app"), ORIGIN_USER);
        assert_eq!(origin_of(&json!({"source": "model_generated"}), "app"), ORIGIN_MODEL);
        // 非法值回落位置默认
        assert_eq!(origin_of(&json!({"source": "mcp"}), "app"), ORIGIN_BUILT_IN);
    }
}
