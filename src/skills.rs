//! 传统 Skill（SKILL.md）机制：目录约定 + frontmatter 解析 + 索引注入 + 读写。
//! 目录：`<workspace>/skills/`（项目级）与 `~/.agent/skills/`（全局），每个技能一个文件夹，内含 SKILL.md。
//! SKILL.md 支持 `---` 围栏 frontmatter，字段 name / description；缺省 name 用文件夹名。
//! 注入策略：system 提示只注入「名字 + 一句话描述」的索引，模型按需用 read_skill 工具拉全文。
//! P25 加固（对齐 ZCode skills 扫描）：①符号链接三级拒绝（技能目录 / SKILL.md / references 内文件）——
//! 链接是文件级逃逸面（SKILL.md → 外部任意文件），拒绝即无逃逸，不判定指向何处；
//! ②references/ 渐进披露——大技能的参考文档不放全文，索引里报篇数，模型按需 read_skill(name, ref) 拉单篇。

use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// "project"（工作区 skills/）| "global"（~/.agent/skills/）| "plugin"（插件自带）
    pub source: String,
    pub dir: String,
    pub content: String,
    /// P25：references/ 下的参考文件名（渐进披露；read_skill 带 ref 参数按需拉全文）
    pub refs: Vec<String>,
    /// 生态轮：贡献这个技能的插件名（非插件技能为空串）
    pub plugin: String,
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

/// 是否链接类入口（符号链接 / junction 都算）。三级拒绝策略见模块注释。
/// Windows 上 junction 不带 symlink 语义但带 FILE_ATTRIBUTE_REPARSE_POINT(0x400)，
/// 且普通用户权限就能建——只查 is_symlink 会漏掉它，必须按 reparse point 判。
fn is_symlink(p: &Path) -> bool {
    let Ok(m) = fs::symlink_metadata(p) else {
        return false;
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        m.file_attributes() & 0x400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    }
    #[cfg(not(windows))]
    {
        m.file_type().is_symlink()
    }
}

/// references/ 下的参考文件名（一层、仅文件、按名排序；链接一律跳过）。
fn list_refs(dir: &Path) -> Vec<String> {
    let Ok(rd) = fs::read_dir(dir.join("references")) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file() && !is_symlink(&e.path()))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    out.sort();
    out
}

/// 单个 SKILL.md → Skill（name 缺省用文件夹名，正文去掉 frontmatter）。
/// 符号链接三级拒绝：技能目录是链接、或 SKILL.md 是链接 → 整个技能不加载。
fn load_skill(dir: &Path, source: &str) -> Option<Skill> {
    if !dir.is_dir() || is_symlink(dir) {
        return None;
    }
    let md_path = dir.join("SKILL.md");
    if is_symlink(&md_path) {
        return None;
    }
    let content = fs::read_to_string(&md_path).ok()?;
    let folder = dir.file_name()?.to_string_lossy().to_string();
    // 正文不单独用：下面连 frontmatter 一起原样存（read_skill 给模型全文）
    let (fm_name, fm_desc, _body) = parse_frontmatter(content.as_str());
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
        refs: list_refs(dir),
        plugin: String::new(),
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
    merge_plugin_skills(&mut out, &crate::plugin::ui::skill_roots());
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 生态轮：把插件自带的技能并入清单。同名规则：项目/全局技能已在前 → 覆盖插件
/// （用户自建同名技能压过插件版，与「项目覆盖全局」同哲学）；插件之间先到先得
/// （工作区插件先于内置插件，各自按目录名排序，见 ui::skill_roots）。
fn merge_plugin_skills(out: &mut Vec<Skill>, roots: &[(PathBuf, String)]) {
    for (root, plugin) in roots {
        let Ok(rd) = fs::read_dir(root) else {
            continue;
        };
        let mut dirs: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        dirs.sort();
        for d in dirs {
            if let Some(mut s) = load_skill(&d, "plugin") {
                s.plugin = plugin.clone();
                if !out.iter().any(|x| x.name == s.name) {
                    out.push(s);
                }
            }
        }
    }
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

/// 生态轮：模型侧读取——正文前带技能目录绝对路径与资产清单。插件技能的脚本资产
/// （scripts/ 等）要用绝对路径跑，模型只有从这里才知道技能落在磁盘何处；列出一层
/// 文件名，省掉模型去 read_file 猜路径（猜相对路径必然 os error 2——那相对的是工作区）。
/// 技能页编辑器仍走 read（原文）。
pub fn read_for_agent(name: &str) -> Result<String, String> {
    let listed = list().into_iter().find(|s| s.name == name.trim());
    let dir = listed.as_ref().map(|s| s.dir.clone());
    let content = read(name)?;
    Ok(match dir {
        Some(d) => {
            let mut assets = fs::read_dir(&d)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.path().file_name() != Some(std::ffi::OsStr::new("SKILL.md")))
                        .map(|e| {
                            let is_dir = e.path().is_dir();
                            let n = e.file_name().to_string_lossy().to_string();
                            if is_dir { format!("{n}/") } else { n }
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            assets.sort();
            let hint = if assets.is_empty() {
                "（本技能无独立资产文件）".to_string()
            } else {
                format!("（资产：{}）", assets.join("、"))
            };
            format!(
                "【技能目录】{d}\n（技能内命令一律用此绝对路径开头；read_file/shell 的相对路径是相对工作区的，不能用来读技能文件）\n{hint}\n\n{content}"
            )
        }
        None => content,
    })
}

/// P25：读技能的单篇参考资料（references/<ref>）。ref 名同样过 valid_name，
/// 落点文件是链接一律拒（逃逸防护与 SKILL.md 同级）。
pub fn read_ref(name: &str, ref_name: &str) -> Result<String, String> {
    valid_name(name)?;
    valid_name(ref_name)?;
    let dir = find_dir(name).ok_or_else(|| format!("未找到技能「{name}」"))?;
    let path = dir.join("references").join(ref_name);
    if is_symlink(&path) {
        return Err("参考资料是符号链接，已拒绝读取（防逃逸）".into());
    }
    fs::read_to_string(&path).map_err(|_| {
        format!(
            "技能「{name}」没有参考资料「{ref_name}」：可用的见索引里的篇数，或 read_skill(\"{name}\") 看 SKILL.md 里的引用说明"
        )
    })
}

/// 技能目录定位（frontmatter name 优先，文件夹名兜底）。
fn find_dir(name: &str) -> Option<PathBuf> {
    if let Some(s) = list().into_iter().find(|s| s.name == name) {
        return Some(PathBuf::from(s.dir));
    }
    for (root, _) in roots() {
        let dir = root.join(name);
        if load_skill(&dir, "project").is_some() {
            return Some(dir);
        }
    }
    None
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
    crate::fsx::write_atomic_str(&dir.join("SKILL.md"), &full).map_err(|e| format!("写入失败: {e}"))?;
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

/// 插件技能不许删（目录在插件里，删了等于毁插件文件）；想覆盖走同名新建。
/// 返回 Some(人话错) = 该拒。
fn plugin_delete_error(s: &Skill) -> Option<String> {
    if s.source != "plugin" {
        return None;
    }
    let from = if s.plugin.is_empty() {
        String::new()
    } else {
        format!("（插件 {}）", s.plugin)
    };
    Some(format!(
        "「{}」是插件{}自带的技能，不能在这里删除；想覆盖它就在技能页新建一个同名技能",
        s.name, from
    ))
}

/// 删除技能目录（frontmatter name 优先匹配，兼容文件夹名；项目级/全局均可删）。
pub fn delete(name: &str) -> Result<(), String> {
    valid_name(name)?;
    if let Some(s) = list().into_iter().find(|s| s.name == name) {
        if let Some(err) = plugin_delete_error(&s) {
            return Err(err);
        }
        let dir = PathBuf::from(&s.dir);
        // 符号链接目录不删：remove_dir_all 对链接的行为平台相关，可能伤及链接目标
        if is_symlink(&dir) {
            return Err("技能目录是符号链接，已拒绝删除（防逃逸）".into());
        }
        fs::remove_dir_all(&dir).map_err(|e| format!("删除失败: {e}"))?;
        return Ok(());
    }
    for (root, _) in roots() {
        let dir = root.join(name);
        if dir.is_dir() && !is_symlink(&dir) {
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
        // P25：references 渐进披露——索引只报篇数，模型按需 read_skill(name, ref) 拉单篇
        let refs_hint = if sk.refs.is_empty() {
            String::new()
        } else {
            format!("（含 {} 篇参考资料，按需 read_skill 带 ref 读）", sk.refs.len())
        };
        // 生态轮：插件自带技能标注来源插件，模型与用户都能看出它不是工作区自建的
        let from = if sk.plugin.is_empty() {
            String::new()
        } else {
            format!("（插件 {}）", sk.plugin)
        };
        s.push_str(&format!("- ${}{}: {}{}\n", sk.name, from, desc, refs_hint));
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

    /// P25：references 渐进披露——list 报篇数、read_ref 按名读单篇、穿越名被拒。
    #[test]
    fn refs_listing_and_read_ref() {
        let name = "zz-test-skill-ref";
        save(name, "带参考资料的技能", "步骤一").expect("save 失败");
        let dir = crate::tools::workspace_root().join("skills").join(name);
        fs::create_dir_all(dir.join("references")).unwrap();
        fs::write(dir.join("references/r1.md"), "参考正文A").unwrap();
        let listed = list().into_iter().find(|s| s.name == name).expect("应列出");
        assert_eq!(listed.refs, vec!["r1.md".to_string()], "references 文件应进清单");
        assert_eq!(read_ref(name, "r1.md").unwrap(), "参考正文A");
        assert!(read_ref(name, "../SKILL.md").is_err(), "穿越名必须拒");
        assert!(read_ref(name, "nope.md").is_err(), "不存在的参考资料给人话错");
        let idx = index_prompt();
        assert!(idx.contains("含 1 篇参考资料"), "索引应报篇数: {idx}");
        delete(name).expect("清理失败");
    }

    /// P25：链接类入口逃逸防护——用 junction（普通权限可建）指向外部带 SKILL.md 的目录，
    /// 必须整体拒载。顺带覆盖符号链接（无权限造时 junction 这条也够证明 reparse 判定生效）。
    #[test]
    fn linked_skill_dir_rejected() {
        let outside = std::env::temp_dir().join(format!("agent-skill-out-{}", std::process::id()));
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("SKILL.md"), "---\nname: evil\ndescription: 外部\n---\n外部内容").unwrap();
        let link = crate::tools::workspace_root().join("skills").join("zz-link-skill");
        let _ = fs::remove_dir_all(&link);
        // junction：普通用户权限可建（symlink_dir 在这台机器上要特权，造不了）
        let made = {
            use std::os::windows::process::CommandExt;
            std::process::Command::new("cmd")
                .args([
                    "/C",
                    "mklink",
                    "/J",
                    &link.to_string_lossy(),
                    &outside.to_string_lossy(),
                ])
                .creation_flags(0x0800_0000)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if made {
            assert!(is_symlink(&link), "junction 应按链接类入口识别");
            assert!(load_skill(&link, "project").is_none(), "链接技能目录必须拒载");
            assert!(!list().iter().any(|s| s.name == "zz-link-skill"), "链接技能不该进索引");
            let _ = fs::remove_dir(&link); // 删 junction 本身（不伤目标）
        } else {
            eprintln!("跳过链接目录测试（mklink /J 失败）");
        }
        let _ = fs::remove_dir_all(&outside);
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

    /// 生态轮：插件技能并入清单——同名时项目/全局压过插件版，插件名落 Skill.plugin。
    #[test]
    fn plugin_skills_merge_with_precedence() {
        let tmp = std::env::temp_dir().join(format!("agent-skill-plug-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let sd = tmp.join("skills");
        fs::create_dir_all(sd.join("docx")).unwrap();
        fs::write(
            sd.join("docx").join("SKILL.md"),
            "---\nname: docx\ndescription: 插件版文档技能\n---\n插件正文",
        )
        .unwrap();
        fs::create_dir_all(sd.join("shared")).unwrap();
        fs::write(sd.join("shared").join("SKILL.md"), "---\nname: shared\ndescription: 插件同名\n---\n插件正文").unwrap();

        // 清单里已有一个同名项目级技能 → 插件版不得顶掉它
        let mut out = vec![Skill {
            name: "shared".into(),
            description: "项目版".into(),
            source: "project".into(),
            dir: "nowhere".into(),
            content: "项目正文".into(),
            refs: vec![],
            plugin: String::new(),
        }];
        merge_plugin_skills(&mut out, &[(sd, "documents".to_string())]);
        let docx = out.iter().find(|s| s.name == "docx").expect("插件技能应并入");
        assert_eq!(docx.source, "plugin");
        assert_eq!(docx.plugin, "documents");
        assert!(docx.content.contains("插件正文"));
        let shared = out.iter().find(|s| s.name == "shared").unwrap();
        assert_eq!(shared.source, "project", "项目级同名技能必须压过插件版");
        assert_eq!(shared.content, "项目正文");
        let _ = fs::remove_dir_all(&tmp);
    }

    /// 生态轮：插件技能删除被拒（给出覆盖的路），项目级技能照常可删。
    #[test]
    fn plugin_skill_delete_guarded() {
        let mk = |name: &str, source: &str, plugin: &str| Skill {
            name: name.into(),
            description: String::new(),
            source: source.into(),
            dir: "nowhere".into(),
            content: String::new(),
            refs: vec![],
            plugin: plugin.into(),
        };
        let err = plugin_delete_error(&mk("docx", "plugin", "documents")).unwrap();
        assert!(err.contains("documents") && err.contains("不能在这里删除") && err.contains("同名技能"), "{err}");
        assert!(plugin_delete_error(&mk("docx", "plugin", "")).is_some(), "缺插件名也要拒");
        assert!(plugin_delete_error(&mk("x", "project", "")).is_none(), "项目级照常可删");
    }

    /// 生态轮：read_for_agent 头部带技能目录（插件技能的脚本要用绝对路径跑）。
    #[test]
    fn read_for_agent_includes_dir() {
        let name = "zz-test-skill-dir";
        save(name, "验证目录头", "步骤一").expect("save 失败");
        let text = read_for_agent(name).expect("read_for_agent 应成功");
        assert!(text.starts_with("【技能目录】"), "头部要有目录标注: {text}");
        assert!(text.contains("SKILL.md") || text.contains("skills"), "目录应为真实路径: {text}");
        assert!(text.contains("步骤一"));
        assert!(text.ends_with(&read(name).unwrap()), "read 仍返回原文（头部之外不改正文）");
        delete(name).expect("清理失败");
    }
}
