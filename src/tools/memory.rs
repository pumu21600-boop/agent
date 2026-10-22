//! memory 工具：记忆库检索正门（2026-10-08 补 agent 侧通道）+ 自动记忆写入（2026-10-09）。
//!
//! 此前语义记忆只有 UI 一条路：面板（星云视图）调 /api/memory/search，agent 侧
//! 一个能调的工具都没有——用户说「检索一下之前的对话」，模型只能 ui open_memory
//! （单向开面板，自己没有回包）然后 shell dir/findstr 满磁盘考古、最后手解 3MB 的
//! library.json。本工具把同一条数据面接给 agent：search（关键词+语义混合）/
//! recent（最近会话）/ similar（相似会话）/ read（读某条原文）。
//!
//! action=write（施工单第 2 步）：自动记忆的唯一写入口，落 <app_root>/.agent/
//! profile.md，**行级操作**（add/replace/remove/merge），匹配不上报错、绝不整段
//! 重写——用户手改的其它行一概不动（详见 memory/profile.rs 的铁律）。定级「常规
//! 写」：只读档每次先问、自动档静默记、完全访问档照静默（§3.7）。
//!
//! 闸：「记忆库」插件总闸（features.galaxy）——关掉插件则界面入口与 agent 通道
//! （读+写）一起没，与浏览器插件同一套「能力总闸」语义。

use serde_json::Value;

use crate::memory::profile::{Entry, Section};
use serde_json::json;

/// 「记忆库」插件总闸：galaxy 关着时界面入口本就没了，agent 通道一并收回。
/// 读 AppState.settings（生产里与全局 shared_settings 是同一个 Arc，见 main.rs），
/// 不另摸一份全局句柄——测试用隔离 AppState 就能控闸，不必污染进程级状态。
fn galaxy_gate(state: &crate::server::AppState) -> Result<(), String> {
    let on = state
        .settings
        .read()
        .map(|s| s.features.galaxy)
        .unwrap_or(false);
    if on {
        return Ok(());
    }
    Err("「记忆库」插件没开，我查不了记忆。请到 设置→插件 找到「记忆库」一行把开关打开，然后再叫我查。".into())
}

/// RFC3339（UTC）→ 人话相对时间。索引里存的是 UTC，直接展示会和用户差 8 小时，
/// 相对时间没有时区歧义，也不会把「刚刚」说成「昨天」。
fn when(ts: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return ts.chars().take(10).collect();
    };
    let secs = (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds();
    if secs < 60 {
        "刚刚".into()
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86400 {
        format!("{} 小时前", secs / 3600)
    } else if secs < 30 * 86400 {
        format!("{} 天前", secs / 86400)
    } else {
        t.format("%Y-%m-%d").to_string()
    }
}

/// 一条命中的展示行。`score` 为 0（recent）时不显示分数——没有相关性可言。
fn line(i: usize, h: &crate::memory::library::Hit) -> String {
    let score = if h.score > 0.0 {
        format!("[{:.2}] ", h.score)
    } else {
        String::new()
    };
    let ws = if h.workspace.is_empty() {
        "未分组".to_string()
    } else {
        std::path::Path::new(&h.workspace)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| h.workspace.clone())
    };
    format!(
        "{i}. {score}「{}」 · {ws} · {} · {} 条消息\n   {}\n   id: {}",
        h.title,
        when(&h.updated_at),
        h.count,
        h.snippet.replace('\n', " "),
        h.id
    )
}

fn workspace_filter(v: &Value) -> Option<String> {
    v.get("workspace")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn keep(h: &crate::memory::library::Hit, ws: &Option<String>) -> bool {
    match ws {
        Some(w) => {
            let w = w.to_lowercase();
            h.workspace.to_lowercase().contains(&w) || h.project.to_lowercase().contains(&w)
        }
        None => true,
    }
}

pub(crate) async fn run(args: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("");
    let state = crate::server::shared_state().ok_or("记忆库子系统尚未初始化")?;
    galaxy_gate(&state)?;

    match action {
        // 关键词 + 语义混合检索（先有界同步，索引不新鲜会漏掉最近的会话）
        "search" => {
            let q = v
                .get("query")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if q.is_empty() {
                return Err("缺少 query（要搜的关键词，或用户的自然语言描述）".into());
            }
            let ws = workspace_filter(&v);
            let (hits, meta) = crate::server::memory::search_hits(&state, &q, 30).await;
            let hits: Vec<_> = hits.into_iter().filter(|h| keep(h, &ws)).collect();
            let mode = meta.get("mode").and_then(|x| x.as_str()).unwrap_or("");
            let model = meta.get("model").and_then(|x| x.as_str()).unwrap_or("");
            let total = meta.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
            let mut out = format!(
                "记忆库检索「{q}」：命中 {} 条（索引共 {total} 条会话，{mode}{model}）",
                hits.len(),
                model = if model.is_empty() { String::new() } else { format!(" · {model}") }
            );
            if let Some(n) = meta.get("notice").and_then(|x| x.as_str()) {
                out.push_str(&format!("\n注意：{n}"));
            }
            if let Some(w) = &ws {
                out.push_str(&format!("\n已按工作区过滤：{w}"));
            }
            if hits.is_empty() {
                out.push_str("\n没有匹配的会话。可以换关键词再搜，或 action=recent 看最近的会话。");
                return Ok(out);
            }
            for (i, h) in hits.iter().enumerate() {
                out.push_str(&format!("\n\n{}", line(i + 1, h)));
            }
            out.push_str("\n\n要看某条原文：action=read 配上面的 id；要在界面打开给用户看：session method=open 配 id。");
            Ok(out)
        }
        // 最近会话（用户说「看看你记得什么」「最近在聊什么」）
        "recent" => {
            let ws = workspace_filter(&v);
            let limit = v.get("limit").and_then(|x| x.as_u64()).unwrap_or(10) as usize;
            // recent 不走检索，但索引仍可能旧了（同步只在开面板/定时时跑）——先补一次
            let _ = crate::server::memory::sync_index(&state, false).await;
            let hits: Vec<_> = crate::server::memory::recent_hits(&state, limit * 3)
                .into_iter()
                .filter(|h| keep(h, &ws))
                .take(limit)
                .collect();
            let mut out = format!("记忆库最近 {} 条会话：", hits.len());
            if let Some(w) = &ws {
                out.push_str(&format!("（已按工作区过滤：{w}）"));
            }
            if hits.is_empty() {
                out.push_str("\n索引里还没有会话。");
                return Ok(out);
            }
            for (i, h) in hits.iter().enumerate() {
                out.push_str(&format!("\n\n{}", line(i + 1, h)));
            }
            out.push_str("\n\n按内容找某件事：action=search 配关键词。");
            Ok(out)
        }
        // 相似会话（以某条会话自己的向量当查询，不调外部渠道）
        "similar" => {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
            if id.is_empty() {
                return Err("缺少 id（要哪条会话的相似会话）".into());
            }
            let hits = crate::server::memory::similar_hits(&state, &id, 20);
            if hits.is_empty() {
                return Ok(format!("没有找到与 {id} 相近的会话（或该 id 不在索引里）。"));
            }
            let mut out = format!("与 {id} 相似的会话 {} 条：", hits.len());
            for (i, h) in hits.iter().enumerate() {
                out.push_str(&format!("\n\n{}", line(i + 1, h)));
            }
            Ok(out)
        }
        // 读某条会话的原文（命中后要真答「上次聊到哪」就得读内容）
        "read" => {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
            if id.is_empty() {
                return Err("缺少 id（先 action=search / recent 拿到会话 id）".into());
            }
            let Some(f) = state.session.get(&id) else {
                return Err(format!("会话 {id} 不存在或读不出（id 要从 search/recent 的结果里拿，别自己编）"));
            };
            let (msgs, omitted) = crate::session::transcript(&f, 16_000, 2_500);
            if msgs.is_empty() {
                return Ok(format!("会话「{}」（id: {id}）里没有可读的对话正文。", f.title));
            }
            let mut out = format!(
                "会话「{}」（{} · {} · {} 条消息 · id: {}）原文：",
                f.title,
                when(&f.updated_at),
                if f.workspace.is_empty() {
                    "未分组".to_string()
                } else {
                    f.workspace.clone()
                },
                // 会话总消息数，不是本次保留数——少了就是省略，omitted 单独说
                f.messages.len(),
                id
            );
            for (role, text) in &msgs {
                let who = if role == "user" { "用户" } else { "我" };
                out.push_str(&format!("\n\n{who}：{text}"));
            }
            if omitted {
                out.push_str("\n\n（这条会话更长，早期内容已省略；要精准定位就告我关键词，我改用 search 搜。）");
            }
            Ok(out)
        }
        // 写入自动记忆（profile.md 行级操作；§3.3 安全设计 + §3.7 通知与审批）
        "write" => {
            let op = v.get("op").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
            let section = match v.get("section").and_then(|x| x.as_str()).unwrap_or("").trim() {
                "prefs" => Section::Prefs,
                "lessons" => Section::Lessons,
                other => {
                    return Err(format!(
                        "section 须为 prefs（偏好区）或 lessons（教训区），收到 {other:?}"
                    ));
                }
            };
            let mem = state.memory.read().unwrap_or_else(|e| e.into_inner()).clone();
            if !mem.enabled {
                return Err("记忆层没开（设置→记忆里把开关打开），我写不了自动记忆。".into());
            }
            // 全程持写互斥：两个会话同时记记忆不许丢更新
            let _guard = mem.profile_write_guard();
            let mut profile = mem.profile();
            // 硬顶前置闸（§6.2）：到顶就拒绝继续写，三选一交给用户。remove 是收缩，放行
            if !matches!(op.as_str(), "remove") && profile.chars() >= mem.profile_max_chars() {
                return Err(mem.profile_cap_notice(profile.chars(), true));
            }
            let today = chrono::Local::now().date_naive().format("%Y-%m-%d").to_string();
            let src = if mem.name.is_empty() { "工作区".to_string() } else { mem.name.clone() };
            let field = |key: &str| {
                v.get(key)
                    .and_then(|x| x.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            };
            // 隐私闸（§7）：含敏感信息的候选直接丢弃不记——先判断再决定记不记。
            // merge 不动正文（只 hits+1），不过检
            if matches!(op.as_str(), "add" | "replace")
                && let Some(t) = field("text")
                && let Some(why) = crate::memory::profile::looks_sensitive(&t)
            {
                return Err(format!(
                    "这条像含敏感信息（{why}），没记。把具体值去掉再叫我记；\
                     确实要留的你自己到 设置→记忆→自动记忆 里手动加。"
                ));
            }
            let result = match op.as_str() {
                // 加一条：T1 偏好一次就够；T2 首记教训落「观察中」
                "add" => {
                    let Some(text) = field("text") else {
                        return Err("缺少 text（要记的条目，一句话 ≤40 字，别原句照搬）".into());
                    };
                    let status_hint = if section.has_status() {
                        "（观察中，第二次踩坑再转正）"
                    } else {
                        ""
                    };
                    profile
                        .add(section, Entry::new(&text, &src, &today, section))
                        .map(|()| format!("已记入{}{status_hint}：{text}\n（跟用户说一句「记住了：…」就好，别展开。）", section.label()))
                }
                // 换一条：只改正文，元数据原样保留
                "replace" => {
                    let (Some(m), Some(text)) = (field("match"), field("text")) else {
                        return Err("replace 要配 match（现有条目原文，一字不差）和 text（新文）".into());
                    };
                    profile
                        .replace(section, &m, &text)
                        .map(|()| format!("已更新{}条目：{text}\n（跟用户说一句已更新就好，别展开。）", section.label()))
                }
                // 删一条：用户说「忘了这条」就走它
                "remove" => {
                    let Some(m) = field("match") else {
                        return Err("remove 要配 match（要删的条目原文，一字不差）".into());
                    };
                    profile
                        .remove(section, &m)
                        .map(|()| format!("已从{}删掉：{m}\n（跟用户说一句已删掉就好，别展开。）", section.label()))
                }
                // 合并：写入前先搜、搜到就合并——hits+1；confirm=true 且教训区观察中时转正
                "merge" => {
                    let Some(m) = field("match") else {
                        return Err("merge 要配 match（搜到的那条原文，一字不差）".into());
                    };
                    let confirm = v.get("confirm").and_then(|x| x.as_bool()).unwrap_or(false);
                    profile.merge_hit(section, &m, confirm).map(|()| {
                        let turned = section.has_status() && confirm;
                        format!(
                            "已合并{}里的重复条目（命中 +1{}）\n（跟用户说一句「记住了：…」就好，别展开。）",
                            section.label(),
                            if turned { "，观察中→已确认" } else { "" }
                        )
                    })
                }
                other => {
                    return Err(format!(
                        "未知 op: {other:?}（add 配 section+text；replace/remove/merge 配 section+match；replace 还要 text，merge 可配 confirm）"
                    ));
                }
            };
            let msg = match result {
                Ok(m) => m,
                Err(e) => {
                    // 匹配不上：把该区现有条目回带给模型按原文重试——不硬盖、不偷偷加回来
                    let cur: Vec<String> = profile
                        .entries()
                        .iter()
                        .filter(|(s, _)| *s == section)
                        .map(|(_, e)| format!("- {}", e.text))
                        .collect();
                    if cur.is_empty() {
                        return Err(format!("{e}（当前{}还是空的）", section.label()));
                    }
                    return Err(format!(
                        "{e}\n当前{}有这些条目，match 要跟其中一条一字不差：\n{}",
                        section.label(),
                        cur.join("\n")
                    ));
                }
            };
            // 写后顺带扫一遍观察中过期（§3.6：memory.write 是两个同步触发点之一），
            // 静默清、events.log 留痕
            for text in profile.prune_stale(chrono::Local::now().date_naive()) {
                mem.record(
                    "memory.prune",
                    json!({ "section": "lessons", "text": text, "reason": "观察中且首次记录超 30 天" }),
                );
            }
            // 上限一把尺子（§6.2）：过预警线（80%）就在回包里提醒 + 三选项由用户选
            let chars = profile.chars();
            let mut out = msg;
            if chars > crate::memory::profile::warn_line(mem.profile_max_chars()) {
                out.push_str(&format!("\n{}", mem.profile_cap_notice(chars, chars > mem.profile_max_chars())));
            }
            Ok(out)
        }
        other => Err(format!(
            "未知 action: {other:?}（search 配 query / recent 可配 limit+workspace / similar 配 id / read 配 id / write 配 op+section）"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_formats_relative_and_falls_back() {
        // 坏输入不能 panic：退回日期前缀
        assert_eq!(when("不是时间"), "不是时间");
        assert_eq!(when(""), "");
        // 一小时前的时间串 → 「1 小时前」
        let one_hour_ago = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        assert_eq!(when(&one_hour_ago), "1 小时前");
        // 很早的日期 → YYYY-MM-DD
        assert_eq!(when("2020-01-02T03:04:05Z"), "2020-01-02");
    }

    #[test]
    fn line_hides_zero_score_and_keeps_id() {
        let h = crate::memory::library::Hit {
            id: "abc-123".into(),
            score: 0.0,
            vector: 0.0,
            lexical: 0.0,
            snippet: "第一行\n第二行".into(),
            title: "测试会话".into(),
            workspace: r"C:\Users\x\Desktop\我的项目".into(),
            project: "我的项目".into(),
            created_at: "2026-10-01T00:00:00Z".into(),
            updated_at: "2026-10-01T00:00:00Z".into(),
            count: 7,
        };
        let s = line(1, &h);
        assert!(!s.contains("[0.00]"), "recent 的 0 分不该显示: {s}");
        assert!(s.contains("「测试会话」"), "{s}");
        assert!(s.contains("我的项目"), "工作区只显示末段目录名: {s}");
        assert!(s.contains("7 条消息"), "{s}");
        assert!(s.contains("id: abc-123"), "{s}");
        // snippet 里的换行要压掉，否则一行变多行破坏编号列表
        assert!(!s.contains("第一行\n第二行"), "snippet 换行未压平: {s}");
    }

    #[test]
    fn workspace_filter_matches_path_or_project() {
        let mk = |ws: &str, proj: &str| crate::memory::library::Hit {
            id: "i".into(),
            score: 1.0,
            vector: 0.0,
            lexical: 0.0,
            snippet: String::new(),
            title: "t".into(),
            workspace: ws.into(),
            project: proj.into(),
            created_at: String::new(),
            updated_at: String::new(),
            count: 0,
        };
        let a = mk(r"C:\Users\x\Desktop\agent-copy", "agent-copy");
        assert!(keep(&a, &Some("agent-copy".into())));
        assert!(keep(&a, &Some("AGENT-COPY".into())), "大小写不敏感");
        assert!(!keep(&a, &Some("别的项目".into())));
        assert!(keep(&a, &None), "不过滤时全留");
    }

    // —— 数据面端到端：真会话 → 有界同步 → 混合检索 → recent/similar ——
    // 不装进程级共享句柄，AppState 本地构造，与其它测试无全局状态纠缠。
    // mem_on：记忆层开关（write 用例要开，检索数据面用例无所谓）。
    fn isolated_state(log_dir: &std::path::Path, mem_on: bool) -> crate::server::AppState {
        use crate::config::{AgentConfig, ContextConfig, GatewayConfig, MemoryConfig, RecoveryConfig, RolesConfig, SandboxConfig, SubagentConfig};
        use std::sync::{Arc, Mutex, RwLock};
        let ws = log_dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        crate::server::AppState {
            gateway: Arc::new(crate::gateway::Gateway::new()),
            cancel: Arc::new(crate::core::cancel::CancelRegistry::new()),
            steer: Arc::new(crate::core::steer::SteerQueue::new()),
            settings: Arc::new(RwLock::new(crate::config::settings::Settings::default())),
            settings_write: Arc::new(Mutex::new(())),
            agent_cfg: Arc::new(AgentConfig::default()),
            sandbox_cfg: Arc::new(SandboxConfig::default()),
            context_cfg: Arc::new(ContextConfig::default()),
            recovery_cfg: Arc::new(RecoveryConfig::default()),
            system_prompt: Arc::new(String::new()),
            prompt_path: Arc::new(log_dir.join("system.md")),
            session: Arc::new(crate::session::SessionManager::new(log_dir)),
            log_dir: Arc::new(log_dir.to_path_buf()),
            memory: Arc::new(RwLock::new(Arc::new(crate::memory::Memory::open_at(
                &ws,
                &MemoryConfig { enabled: mem_on, persist_sensitive: false, profile_max_chars: 2500 },
                crate::memory::GlobalDirs {
                    install: log_dir.join("global"),
                    user: log_dir.join("user"),
                    install_legacy: log_dir.join("global-legacy"),
                    user_legacy: log_dir.join("user-legacy"),
                },
            )))),
            mem_cfg: Arc::new(MemoryConfig { enabled: mem_on, persist_sensitive: false, profile_max_chars: 2500 }),
            approvals: Arc::new(Mutex::new(std::collections::HashMap::new())),
            approval_meta: Arc::new(Mutex::new(std::collections::HashMap::new())),
            approved_scopes: Arc::new(Mutex::new(std::collections::HashMap::new())),
            approval_mode: Arc::new(RwLock::new("auto".into())),
            session_approval: Arc::new(RwLock::new(std::collections::HashMap::new())),
            roles_cfg: Arc::new(RolesConfig::default()),
            gateway_cfg: Arc::new(GatewayConfig::default()),
            subagent_cfg: Arc::new(SubagentConfig::default()),
        }
    }

    #[tokio::test]
    async fn data_plane_syncs_then_searches_and_reads() {
        let dir = std::env::temp_dir().join(format!("agent-mem-tool-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = isolated_state(&dir, false);
        let sm = &state.session;
        let a = sm.new_id();
        sm.save(&a, &[serde_json::json!({"role":"user","content":"插件删除能力要不要做？上次说等拍板"})], Some("任务目标：插件删除缺口")).unwrap();
        let b = sm.new_id();
        sm.save(&b, &[serde_json::json!({"role":"user","content":"星云背景换个紫色配色"})], None).unwrap();

        // search：语义+词法混合，命中应带身份字段（曾经只有 id，用户问「哪条」就卡住）
        let (hits, meta) = crate::server::memory::search_hits(&state, "插件删除", 10).await;
        assert!(!hits.is_empty(), "应命中插件删除会话: {meta}");
        assert_eq!(hits[0].id, a);
        assert_eq!(hits[0].title, "插件删除能力要不要做？上次说等拍板", "标题取首条用户消息: {:?}", hits[0]);
        assert!(!hits[0].workspace.is_empty(), "要能说清哪个工作区");
        assert_eq!(meta["total"], 2, "同步后索引应含两条会话");

        // 无关查询：宁缺毋滥
        let (none, _) = crate::server::memory::search_hits(&state, "zzzqqq 不存在的词元组合", 10).await;
        assert!(none.is_empty(), "无关查询不该命中: {:?}", none.iter().map(|h| &h.id).collect::<Vec<_>>());

        // recent：时间倒序，不带分数
        let rec = crate::server::memory::recent_hits(&state, 10);
        assert_eq!(rec.len(), 2);
        assert!(rec.iter().all(|h| h.score == 0.0));

        // similar：同主题会话互相靠近（a/b 主题不同，similar(a) 不该把 b 顶到最前）
        let sim = crate::server::memory::similar_hits(&state, &a, 5);
        assert!(sim.iter().all(|h| h.id != a), "不含自己");

        // read：命中后读原文，转写要含双方消息
        let f = sm.get(&a).unwrap();
        let (msgs, _omitted) = crate::session::transcript(&f, 16_000, 2_500);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].0, "user");
        assert!(msgs[0].1.contains("插件删除"), "{:?}", msgs[0]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— 工具全链路：经 tools::execute 分发，验闸 + 参数校验 + 输出给人话 ——
    // 只装进程级 shared_state（本测试可控）；闸读 AppState.settings，不碰全局
    // shared_settings——那个一旦被装就会改掉别的测试指望的「未安装」态。
    #[tokio::test]
    async fn execute_dispatches_and_gate_blocks_when_plugin_off() {
        use crate::config::SandboxConfig;
        if crate::server::shared_state().is_some() {
            eprintln!("[skip] shared_state 已被其它测试安装，本用例无法控制全局状态");
            return;
        }
        let dir = std::env::temp_dir().join(format!("agent-mem-exec-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        // 记忆层开着：write 用例要能落盘（隔离 install 根 = dir/global）
        let state = isolated_state(&dir, true);
        let a = state.session.new_id();
        state
            .session
            .save(&a, &[serde_json::json!({"role":"user","content":"把记忆库接到 agent 工具上"})], None)
            .unwrap();
        crate::server::install_shared_state(state.clone());

        let cfg = crate::tools::ExecConfig {
            sandbox: SandboxConfig::default(),
            max_chars: 8000,
            unsandboxed: false,
            cpu_percent: 0,
            fs_unconfined: false,
            workspace: dir.join("ws"),
        };
        let call = async |args: &str| crate::tools::execute(&cfg, "memory", args).await;

        // 正常检索：输出要带标题、id 和下一步指引
        let out = call(r#"{"action":"search","query":"记忆库"}"#).await.unwrap();
        assert!(out.contains("记忆库检索"), "{out}");
        assert!(out.contains("把记忆库接到 agent 工具上"), "要带会话标题: {out}");
        assert!(out.contains(&format!("id: {a}")), "要带 id 供 read/open: {out}");
        assert!(out.contains("action=read"), "要告诉模型怎么读原文: {out}");

        // recent
        let out = call(r#"{"action":"recent","limit":5}"#).await.unwrap();
        assert!(out.contains("记忆库最近"), "{out}");

        // read：读原文
        let read_args = format!(r#"{{"action":"read","id":"{a}"}}"#);
        let out = call(&read_args).await.unwrap();
        assert!(out.contains("原文") && out.contains("把记忆库接到 agent 工具上"), "{out}");

        // 参数校验：缺 query 给人话，不是 panic
        let err = call(r#"{"action":"search"}"#).await.unwrap_err();
        assert!(err.contains("query"), "{err}");
        // read 一个不存在的 id：说清「别自己编」
        let err = call(r#"{"action":"read","id":"no-such-id"}"#).await.unwrap_err();
        assert!(err.contains("不存在"), "{err}");
        // 未知 action
        let err = call(r#"{"action":"fly"}"#).await.unwrap_err();
        assert!(err.contains("未知 action"), "{err}");

        // —— write：自动记忆行级写入全链路（落隔离 install 根的 profile.md）——
        // add：T1 偏好一次就够；教训区首记默认「观察中」
        let out = call(r#"{"action":"write","op":"add","section":"prefs","text":"以后回答先给结论"}"#).await.unwrap();
        assert!(out.contains("已记入偏好区") && out.contains("以后回答先给结论"), "{out}");
        let out = call(r#"{"action":"write","op":"add","section":"lessons","text":"cargo test 失败先看前 50 行"}"#).await.unwrap();
        assert!(out.contains("观察中"), "教训区首记默认观察中: {out}");
        // merge + confirm：第二次踩坑 → 观察中→已确认，hits+1
        let out = call(r#"{"action":"write","op":"merge","section":"lessons","match":"cargo test 失败先看前 50 行","confirm":true}"#).await.unwrap();
        assert!(out.contains("已确认"), "{out}");
        // replace：只改正文，元数据不动
        let out = call(r#"{"action":"write","op":"replace","section":"prefs","match":"以后回答先给结论","text":"回答先给结论"}"#).await.unwrap();
        assert!(out.contains("已更新偏好区"), "{out}");
        // 匹配不上：报错 + 回带该区现有条目，不硬盖
        let err = call(r#"{"action":"write","op":"remove","section":"prefs","match":"不存在的条目"}"#).await.unwrap_err();
        assert!(err.contains("这条不在了"), "{err}");
        assert!(err.contains("回答先给结论"), "要回带现有条目供按原文重试: {err}");
        // remove
        let out = call(r#"{"action":"write","op":"remove","section":"prefs","match":"回答先给结论"}"#).await.unwrap();
        assert!(out.contains("已从偏好区删掉"), "{out}");
        // 落盘核验：行级内容、hits、状态、src（来源工作区名）
        let raw = std::fs::read_to_string(dir.join("global").join("profile.md")).unwrap();
        assert!(raw.contains("## 偏好区"), "{raw}");
        assert!(!raw.contains("回答先给结论"), "删掉的条目不在文件里: {raw}");
        assert!(raw.contains("- cargo test 失败先看前 50 行 <!-- src: ws · "), "{raw}");
        assert!(raw.contains("hits: 2 · 已确认"), "{raw}");
        // 缺 section / 缺 text：给人话
        let err = call(r#"{"action":"write","op":"add","text":"x"}"#).await.unwrap_err();
        assert!(err.contains("section"), "{err}");
        let err = call(r#"{"action":"write","op":"add","section":"prefs"}"#).await.unwrap_err();
        assert!(err.contains("text"), "{err}");

        // —— 上限（§6.2）：硬顶拒绝 + 三选一；预警线只提醒；remove 是收缩永远放行 ——
        let fat = format!(
            "## 偏好区\n{}",
            "- 灌水条目填充填充填充填充填充填充填充填充填充 <!-- src: ws · 2026-10-09 · hits: 1 -->\n"
                .repeat(60)
        );
        std::fs::write(dir.join("global").join("profile.md"), &fat).unwrap();
        let err = call(r#"{"action":"write","op":"add","section":"prefs","text":"再写一条"}"#).await.unwrap_err();
        assert!(err.contains("到顶") && err.contains("这次没写"), "{err}");
        for opt in ["压缩旧条目", "归档到文件", "提高上限"] {
            assert!(err.contains(opt), "硬顶也要给全三个选项: {err}");
        }
        // remove 收缩，到顶也放行
        let out = call(r#"{"action":"write","op":"remove","section":"prefs","match":"灌水条目填充填充填充填充填充填充填充填充填充"}"#).await.unwrap();
        assert!(out.contains("已从偏好区删掉"), "{out}");
        // 预警线（>80% 但没到顶）：写成功 + 回包带三选项提醒
        let mid = format!(
            "## 偏好区\n{}",
            "- 预警探针条目填充填充填充填充填充填充填充填充 <!-- src: ws · 2026-10-09 · hits: 1 -->\n"
                .repeat(30)
        );
        std::fs::write(dir.join("global").join("profile.md"), &mid).unwrap();
        let out = call(r#"{"action":"write","op":"add","section":"prefs","text":"预警线探针"}"#).await.unwrap();
        assert!(out.contains("已记入偏好区"), "{out}");
        assert!(out.contains("预警线") && out.contains("压缩旧条目"), "过线要提醒+三选项: {out}");

        // 隐私闸（§7）：含敏感信息的候选直接丢弃不记，文件不变
        let before = std::fs::read_to_string(dir.join("global").join("profile.md")).unwrap();
        let err = call(r#"{"action":"write","op":"add","section":"prefs","text":"密钥是 sk-abc1234567890 记下来"}"#).await.unwrap_err();
        assert!(err.contains("敏感信息") && err.contains("没记"), "{err}");
        let err = call(r#"{"action":"write","op":"replace","section":"prefs","match":"预警线探针","text":"手机号 13812345678"}"#).await.unwrap_err();
        assert!(err.contains("敏感信息"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.join("global").join("profile.md")).unwrap(),
            before,
            "隐私闸拒了就不落盘"
        );

        // 闸：关掉「记忆库」插件 → 拒 + 指路（与浏览器插件同一套能力总闸语义；写通道一并收回）
        state.settings.write().unwrap().features.galaxy = false;
        let err = call(r#"{"action":"recent"}"#).await.unwrap_err();
        assert!(err.contains("记忆库") && err.contains("设置→插件"), "{err}");
        let err = call(r#"{"action":"write","op":"add","section":"prefs","text":"x"}"#).await.unwrap_err();
        assert!(err.contains("记忆库") && err.contains("设置→插件"), "写通道也归插件总闸: {err}");
        state.settings.write().unwrap().features.galaxy = true;

        let _ = std::fs::remove_dir_all(&dir);
    }
}
