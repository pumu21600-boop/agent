//! skill 工具（2026-10-03）：技能的建/改/删/列走正门，复用技能页同一套
//! `skills::save` / `skills::delete`（校验、落盘位置、插件技能保护全在那里面）。
//!
//! 真机实锤：模型被要求「创建一个技能」时，索引里混着的插件自带技能让它以为技能是
//! 磁盘上某处的 SKILL.md，dir 翻遍用户目录直到重复调用保护击杀。本工具 + 索引改口
//! （skills::index_prompt 只列自建技能）一起堵：建技能有了正门，模型不再需要找文件。
//! save 落全局技能根（app_root/skills，2026-10-02 拍板的唯一技能根）；插件自带技能
//! 不可删（skills::delete 已拒，想覆盖走同名 save）。

use serde_json::{Value, json};

pub(crate) async fn run(args: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    match v.get("action").and_then(|x| x.as_str()).unwrap_or("") {
        "list" => {
            let items: Vec<Value> = crate::skills::list()
                .iter()
                .map(|s| {
                    json!({
                        "name": s.name,
                        "description": s.description,
                        "source": s.source, // project | global | plugin
                        "plugin": s.plugin, // 插件自带的归属插件名；自建为空串
                        "refs": s.refs,
                    })
                })
                .collect();
            Ok(json!({
                "skills": items,
                "note": "source=plugin 的是插件自带技能（只读，不能删；想覆盖用 save 写同名自建版）；save 的新技能落全局技能根，下轮对话进系统提示索引"
            })
            .to_string())
        }
        "save" => {
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").trim();
            let description = v.get("description").and_then(|x| x.as_str()).unwrap_or("");
            let content = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
            if name.is_empty() {
                return Err("缺少 name".into());
            }
            if content.trim().is_empty() {
                return Err("缺少 content（技能正文：触发时机 + 步骤/纪律，markdown）".into());
            }
            let dir = crate::skills::save(name, description, content)?;
            Ok(format!("技能「{name}」已保存到 {dir}，下一轮对话起进系统提示索引"))
        }
        "delete" => {
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").trim();
            crate::skills::delete(name).map(|_| format!("技能「{name}」已删除"))
        }
        other => Err(format!("未知 action: {other:?}（list / save / delete）")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn save_list_delete_roundtrip() {
        let r = run(r#"{"action":"save","name":"zz-tool-skill","description":"工具链路测试","content":"步骤一"}"#)
            .await
            .unwrap();
        assert!(r.contains("已保存"), "{r}");
        let r = run(r#"{"action":"list"}"#).await.unwrap();
        assert!(r.contains("zz-tool-skill") && r.contains("\"source\":\"global\""), "{r}");
        // 自建技能立刻进系统提示索引（index 每轮现读盘）
        let idx = crate::skills::index_prompt();
        assert!(idx.contains("- /zz-tool-skill"), "{idx}");
        // 不存在的技能给人话错
        let e = run(r#"{"action":"delete","name":"nope-xyz"}"#).await.unwrap_err();
        assert!(e.contains("未找到"), "{e}");
        let r = run(r#"{"action":"delete","name":"zz-tool-skill"}"#).await.unwrap();
        assert!(r.contains("已删除"), "{r}");
        let idx = crate::skills::index_prompt();
        assert!(!idx.contains("zz-tool-skill"), "删除后应退出索引");
    }

    #[tokio::test]
    async fn save_requires_name_and_content() {
        let e = run(r#"{"action":"save","description":"x","content":"y"}"#).await.unwrap_err();
        assert!(e.contains("name"), "{e}");
        let e = run(r#"{"action":"save","name":"zz-x"}"#).await.unwrap_err();
        assert!(e.contains("content"), "{e}");
        let e = run(r#"{"action":"frobnicate"}"#).await.unwrap_err();
        assert!(e.contains("未知 action"), "{e}");
    }
}
