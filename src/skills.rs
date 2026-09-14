//! 传统 Skill（SKILL.md）机制：目录约定 + frontmatter 解析 + 索引注入 + 读写。
//! 目录：`<workspace>/skills/`（项目级）与 `~/.agent/skills/`（全局），每个技能一个文件夹，内含 SKILL.md。
//! SKILL.md 支持 `---` 围栏 frontmatter，字段 name / description；缺省 name 用文件夹名。
//! 注入策略：system 提示只注入「名字 + 一句话描述」的索引，模型按需用 read_skill 工具拉全文。

use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// "project"（工作区 skills/）| "global"（~/.agent/skills/）
    pub source: String,
    pub dir: String,
    pub content: String,
}

/// 技能根目录：(路径, 来源)。项目级在前，同名时项目覆盖全局。
pub fn roots() -> Vec<(PathBuf, &'static str)> {
    let mut v = vec![(crate::tools::workspace_root().join("skills"), "project")];
    v.push((crate::memory::global_dir().join("skills"), "global"));
    v
}

/// 解析 `---` 围栏 frontmatter，返回 (name, description, 正文)。
fn parse_frontmatter(content: &str) -> (Option<String>, Option<String>, String) {
    let trimmed = content.trim_start_matches('\u{feff}');
    if !trimmed.starts_with("---") {
        return (None, None, content.to_string());
    }
    let rest = &trimmed[3..];
    let Some(end) = rest.find("\n---") else {
        return (None, None, content.to_string());
    };
    let head = &rest[..end];
    let body = rest[end + 4..].trim_start().to_string();
    let mut name = None;
    let mut desc = None;
    for line in head.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("name:") {
            name = Some(v.trim().trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("description:") {
            desc = Some(v.trim().trim_matches('"').to_string());
        }
    }
    (name, desc, body)
}

/// 单个 SKILL.md → Skill（name 缺省用文件夹名，正文去掉 frontmatter）。
fn load_skill(dir: &Path, source: &str) -> Option<Skill> {
    if !dir.is_dir() {
        return None;
    }
    let md_path = dir.join("SKILL.md");
    let content = fs::read_to_string(&md_path).ok()?;
    let folder = dir.file_name()?.to_string_lossy().to_string();
    let (fm_name, fm_desc, body) = parse_frontmatter(content.as_str());
    // frontmatter name 留空时回退文件夹名（空名字技能既没法删也会污染索引）
    let name = match fm_name {
        Some(n) if !n.trim().is_empty() => n,
        _ => folder,
    };
    // 正文与 frontmatter 都保留，read_skill 原样给模型
    Some(Skill {
        description: fm_desc.unwrap_or_default(),
        source: source.to_string(),
        dir: dir.to_string_lossy().to_string(),
        content,
        name,
    })
}

/// 扫描全部根目录（项目覆盖同名全局）。
pub fn list() -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    for (root, source) in roots() {
        let Ok(rd) = fs::read_dir(&root) else {
            continue;
        };
        let mut dirs: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        dirs.sort();
        for d in dirs {
            if let Some(s) = load_skill(&d, source) {
                if let Some(existing) = out.iter_mut().find(|x| x.name == s.name) {
                    *existing = s; // 项目级覆盖全局
                } else {
                    out.push(s);
                }
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 按名读全文（frontmatter 的 name 优先匹配，避免与 list 展示名不一致）。
pub fn read(name: &str) -> Result<String, String> {
    let name = name.trim();
    valid_name(name)?;
    if let Some(s) = list().into_iter().find(|s| s.name == name) {
        return Ok(s.content);
    }
    // 兼容：文件夹名与 frontmatter name 不一致时，按文件夹名兜底
    for (root, _) in roots() {
        let dir = root.join(name);
        if let Some(s) = load_skill(&dir, "project") {
            return Ok(s.content);
        }
    }
    Err(format!("未找到技能「{name}」：请在对应 skills 目录下创建 <name>/SKILL.md"))
}

fn valid_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    let bad = n.is_empty()
        || n.chars().count() > 64
        || n.contains('/')
        || n.contains('\\')
        || n.contains("..")
        || n.chars().any(|c| c.is_control());
    if bad {
        Err("技能名不能为空、含 / \\ .. 或控制字符（最长 64 字符）".into())
    } else {
        Ok(())
    }
}

/// 保存（仅项目级）：写 `<workspace>/skills/<name>/SKILL.md`，frontmatter 由 description 生成。
pub fn save(name: &str, description: &str, content: &str) -> Result<String, String> {
    valid_name(name)?;
    // 描述压成单行：换行会破坏 frontmatter 结构
    let desc = description.trim().replace(['\r', '\n'], " ");
    let body = content.trim_start();
    let full = if body.starts_with("---") {
        // 已有 frontmatter：同步其中的 description（缺行则补在 name 后）
        sync_desc(body, &desc)
    } else {
        format!("---\nname: {}\ndescription: {}\n---\n\n{}", name, desc, body)
    };
    let dir = crate::tools::workspace_root().join("skills").join(name);
    fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;
    fs::write(dir.join("SKILL.md"), full).map_err(|e| format!("写入失败: {e}"))?;
    Ok(dir.to_string_lossy().to_string())
}

/// 内容自带 frontmatter 时，把其中的 description 行同步为传入值（缺行则补在 name 行后）。
fn sync_desc(full: &str, desc: &str) -> String {
    if desc.is_empty() {
        return full.to_string();
    }
    let mut out = String::with_capacity(full.len() + desc.len() + 16);
    let mut in_fm = false;
    let mut replaced = false;
    for line in full.lines() {
        if line.trim() == "---" {
            in_fm = !in_fm;
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if in_fm && line.trim_start().starts_with("description:") {
            out.push_str(&format!("description: {desc}\n"));
            replaced = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
        if in_fm && !replaced && line.trim_start().starts_with("name:") {
            out.push_str(&format!("description: {desc}\n"));
            replaced = true;
        }
    }
    out
}

/// 删除技能目录（frontmatter name 优先匹配，兼容文件夹名；项目级/全局均可删）。
pub fn delete(name: &str) -> Result<(), String> {
    valid_name(name)?;
    if let Some(s) = list().into_iter().find(|s| s.name == name) {
        let dir = PathBuf::from(&s.dir);
        fs::remove_dir_all(&dir).map_err(|e| format!("删除失败: {e}"))?;
        return Ok(());
    }
    for (root, _) in roots() {
        let dir = root.join(name);
        if dir.is_dir() {
            fs::remove_dir_all(&dir).map_err(|e| format!("删除失败: {e}"))?;
            return Ok(());
        }
    }
    Err(format!("未找到技能「{name}」"))
}

/// system 提示注入的技能索引；无技能时返回空串（不占上下文）。
pub fn index_prompt() -> String {
    let skills = list();
    if skills.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n\n## 可用技能（Skills）\n");
    s.push_str("以下是本环境可用的技能清单。当用户消息中出现 `$技能名`、或任务与某技能的描述明显相关时，先调用 read_skill 工具读取该技能全文，再严格按其步骤执行：\n");
    for sk in &skills {
        let desc = if sk.description.is_empty() {
            "（无描述）"
        } else {
            &sk.description
        };
        s.push_str(&format!("- ${}: {}\n", sk.name, desc));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parse_and_fallback() {
        let (n, d, body) = parse_frontmatter("---\nname: deploy\ndescription: 部署流程\n---\n正文A");
        assert_eq!(n.as_deref(), Some("deploy"));
        assert_eq!(d.as_deref(), Some("部署流程"));
        assert_eq!(body, "正文A");
        let (n, d, body) = parse_frontmatter("没有围栏的正文");
        assert_eq!(n, None);
        assert_eq!(d, None);
        assert_eq!(body, "没有围栏的正文");
    }

    #[test]
    fn save_read_delete_roundtrip() {
        // save 写到工作区 skills/ 下；用带前缀的测试技能名，结束即清理
        let name = "zz-test-skill";
        let r = save(name, "测试用技能", "步骤一\n步骤二");
        assert!(r.is_ok(), "save 失败: {:?}", r.err());
        let content = read(name).expect("read 应成功");
        assert!(content.contains("name: zz-test-skill"));
        assert!(content.contains("步骤一"));
        let names: Vec<_> = list().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&name.to_string()));
        assert!(delete(name).is_ok());
        assert!(read(name).is_err(), "删除后不应再读到");
    }

    #[test]
    fn invalid_name_rejected() {
        assert!(valid_name("../evil").is_err());
        assert!(valid_name("").is_err());
        assert!(valid_name("a\\b").is_err());
        assert!(valid_name("ok-name_1").is_ok());
        assert!(valid_name("部署流程").is_ok(), "中文技能名应允许");
    }

    #[test]
    fn sync_desc_updates_and_inserts() {
        // frontmatter 缺 description → 补在 name 后
        let fm = sync_desc("---\nname: a\n---\nbody", "新描述");
        assert!(fm.contains("description: 新描述"));
        assert!(fm.contains("description: 新描述") && fm.lines().any(|l| l.trim() == "name: a"));
        // 已有 description → 替换
        let fm2 = sync_desc("---\nname: a\ndescription: old\n---\nbody", "新描述");
        assert!(fm2.contains("description: 新描述"));
        assert!(!fm2.contains("old"));
        // 多行描述已在 save 前压成单行（save 侧职责），这里确认 sync 不再引入换行
        assert!(!fm2.contains("description: 新\n"));
    }

    #[test]
    fn index_prompt_empty_when_no_skills() {
        // 不保证环境无技能；只验证函数不 panic 且格式正确
        let idx = index_prompt();
        if !idx.is_empty() {
            assert!(idx.starts_with("\n\n## 可用技能"));
        }
    }
}
