//! P22-C 子代理派发：SubagentRunner 的实现（并发度 / 取消级联 / 成本归属都在这层）
//!
//! 对齐 docs/subagent-draft.md：
//! - 子代理 = 独立会话（无头会话跑整轮），主会话「一会话一轮」不变量不碰；
//! - 并发度：全局 `[subagent] max_concurrent` + 单父任务 `per_parent`，满员等待
//!   （轮询式 FIFO 近似：200ms 粒度，对分钟级任务无感）；
//! - 取消：父任务被取消 → 级联取消它的全部活跃子代理；
//! - 成本：子代理的模型调用在台账里记 `parent_session` = 父任务 id（按父任务聚合），
//!   且 token 增量并进父任务的预算计数（防拆子代理绕过 budget_tokens）；
//! - 上下文隔离：子代理只看到派发指令，看不到父会话历史；审批门用无头策略
//!   （默认拒绝，派发时可显式放行）。
//! - v1 不支持嵌套派发（子代理不能再派子代理）——防递归烧额度。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::*;
use crate::core::subagent::SubagentRunner;
use crate::server::chat::find_channel;

/// 全局活跃派发表：dispatch_id → 记录（并发度核算 + 取消级联 + 嵌套深度都查它）。
fn running_slot() -> &'static Mutex<HashMap<String, Dispatch>> {
    static RUNNING: OnceLock<Mutex<HashMap<String, Dispatch>>> = OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 会话 → 派发深度（根会话不在表里 = 0；子代理会话 = 1）。仅内存，重启即失。
fn depth_slot() -> &'static Mutex<HashMap<String, u32>> {
    static DEPTH: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    DEPTH.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 父任务 → 子代理已耗 token 累计（预算并入用水印记账）。
fn tokens_slot() -> &'static Mutex<HashMap<String, u64>> {
    static TOKENS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    TOKENS.get_or_init(|| Mutex::new(HashMap::new()))
}

struct Dispatch {
    parent: String,
    child: String,
    registry: Arc<crate::core::cancel::CancelRegistry>,
}

/// 子代理派发器（每请求一个，挂进 LoopChannels::subagents）。
pub(super) struct SubagentDispatcher {
    pub state: AppState,
    pub limits: crate::config::SubagentConfig,
}

#[derive(Debug, Deserialize)]
struct SubagentArgs {
    task: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    approval_mode: String,
    #[serde(default)]
    on_approval: String,
    #[serde(default)]
    timeout_secs: u64,
}

/// 某父任务当前活跃的子代理数。
fn running_for(parent: &str) -> usize {
    running_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .filter(|d| d.parent == parent)
        .count()
}

fn running_total() -> usize {
    running_slot().lock().unwrap_or_else(|e| e.into_inner()).len()
}

fn depth_of(session: &str) -> u32 {
    depth_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session)
        .copied()
        .unwrap_or(0)
}

/// 父任务被取消 → 取消它的全部活跃子代理。返回取消数。
pub fn cancel_children(parent: &str) -> usize {
    let children: Vec<Dispatch> = {
        let mut running = running_slot().lock().unwrap_or_else(|e| e.into_inner());
        let hit: Vec<(String, Dispatch)> = running
            .iter()
            .filter(|(_, d)| d.parent == parent)
            .map(|(k, d)| {
                (
                    k.clone(),
                    Dispatch {
                        parent: d.parent.clone(),
                        child: d.child.clone(),
                        registry: d.registry.clone(),
                    },
                )
            })
            .collect();
        for (k, _) in &hit {
            running.remove(k);
        }
        hit.into_iter().map(|(_, d)| d).collect()
    };
    let mut n = 0;
    for d in &children {
        d.registry.cancel(&d.child);
        n += 1;
    }
    if n > 0 {
        // try_event：单测会直跑这条路径，logger 未初始化时不该 panic（生产环境必已初始化）
        observability::try_event(
            EventType::State,
            json!({ "kind": "subagent.cascade_cancel", "parent": parent, "count": n }),
        );
    }
    n
}

/// 子代理为父任务累计的 token（预算并入用；调用方持水印只取增量）。
pub fn subagent_tokens(parent: &str) -> u64 {
    tokens_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(parent)
        .copied()
        .unwrap_or(0)
}

fn add_tokens(parent: &str, n: u64) {
    if n == 0 {
        return;
    }
    let mut t = tokens_slot().lock().unwrap_or_else(|e| e.into_inner());
    *t.entry(parent.to_string()).or_insert(0) += n;
}

impl SubagentRunner for SubagentDispatcher {
    fn run(
        &self,
        parent_session: &str,
        args_json: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + '_>> {
        let parent = parent_session.to_string();
        let args = args_json.to_string();
        Box::pin(async move { self.dispatch(&parent, &args).await })
    }
}

impl SubagentDispatcher {
    async fn dispatch(&self, parent: &str, args_json: &str) -> String {
        let args: SubagentArgs = match serde_json::from_str(args_json) {
            Ok(a) => a,
            Err(e) => return format!("派发失败：参数不是合法 JSON（{e}）。请按工具说明重发。"),
        };
        let task = args.task.trim().to_string();
        if task.is_empty() {
            return "派发失败：task 不能为空。".to_string();
        }
        // v1 不支持嵌套：父任务自己就是子代理时拒绝
        if depth_of(parent) >= 1 {
            observability::event(
                EventType::State,
                json!({ "kind": "subagent.rejected", "parent": parent, "reason": "nested" }),
            );
            return "派发失败：子代理不能再派子代理（v1 不支持嵌套）。请把任务合并成一个子代理，或自己直接做。".to_string();
        }
        if self.limits.max_concurrent == 0 {
            return "派发失败：子代理并行未启用（config.toml [subagent] max_concurrent = 0）。".to_string();
        }
        // 并发度：全局上限 + 单父任务上限，满员等待（轮询式 FIFO 近似，200ms 粒度）。
        // 等待期间父任务被取消 → 直接返回，不派发。
        loop {
            if self.state.cancel.is_set(parent) {
                return "派发失败：父任务已取消，未派发。".to_string();
            }
            if running_total() < self.limits.max_concurrent
                && running_for(parent) < self.limits.per_parent.max(1)
            {
                break;
            }
            observability::event(
                EventType::State,
                json!({ "kind": "subagent.queued", "parent": parent, "running": running_total(), "per_parent": running_for(parent) }),
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        // 子会话：独立会话文件（工作区继承——全局 workspace_root 本来就是共享的）
        let child = self.state.session.new_id();
        // 先算深度再上锁：depth_of 自己也要锁 depth_slot，持锁调用会自锁死（std Mutex 不可重入）
        let child_depth = depth_of(parent) + 1;
        depth_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(child.clone(), child_depth);
        let dispatch_id = uuid::Uuid::new_v4().simple().to_string();
        running_slot().lock().unwrap_or_else(|e| e.into_inner()).insert(
            dispatch_id.clone(),
            Dispatch { parent: parent.to_string(), child: child.clone(), registry: self.state.cancel.clone() },
        );

        // 模型解析（草案第 5 节）：派发级 model 覆盖 > roles.subagent（B3 链）> 主渠道
        let (mut model, mut base_url, mut api_key) = (String::new(), String::new(), String::new());
        if !args.model.trim().is_empty() {
            // 请求级覆盖：只换模型，渠道仍走激活配置（与 /api/chat 请求级覆盖同义）
            model = args.model.trim().to_string();
        } else {
            let s = self.state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
            // features.roles=false（「功能桥接」关）= 跳过 B3 链，子代理直接走主渠道
            let role_name = if s.features.roles {
                s.roles
                    .as_ref()
                    .and_then(|r| r.subagent.as_deref())
                    .or_else(|| self.state.roles_cfg.subagent.as_deref())
            } else {
                None
            };
            if let Some(name) = role_name
                && let Some(ch) = find_channel(&s, name)
            {
                model = ch.model.clone();
                base_url = ch.base_url.clone();
                api_key = ch.api_key.clone();
            }
            // 未配置席位：三项留空 → 无头会话用激活配置（主渠道）
        }

        let label = if args.label.trim().is_empty() {
            task.chars().take(20).collect::<String>()
        } else {
            args.label.trim().to_string()
        };
        observability::event(
            EventType::State,
            json!({
                "kind": "subagent.spawned",
                "parent": parent,
                "child": child,
                "model": model,
                "label": label,
                "task_chars": task.chars().count(),
            }),
        );

        // 派发指令 = 子代理的唯一上文（看不到父会话历史——草案第 2 节）
        let message = format!(
            "【子代理任务{}】{task}\n\n（你是被主任务派发的子代理：独立完成上面的任务，用中文给出最终结论。你看不到主任务的对话历史，任务描述要自带全部必要信息。需要审批的操作默认按拒绝处理——请以只读方式完成，或明确在结论里说明「需要写权限」让主任务自己动手。）",
            if args.label.trim().is_empty() { String::new() } else { format!("：{label}") }
        );

        // 审批策略钳制（2026-10-08 修）：无人值守会话没有 UI 来回答审批。模型自选
        // on_approval=approve = 自己批准自己（连 undo_checkpoint 红线都被 auto_approve
        // 吸收）；approval_mode 传 full-access 同理是自抬权。策略：
        // ① on_approval 一律 deny（子代理没有用户在场，放行高危必须用户明示）；
        // ② 档位不得高于父会话当前档——只允许更严，不允许借子代理升权。
        let parent_mode = self
            .state
            .session_approval
            .read()
            .ok()
            .and_then(|m| m.get(parent).cloned())
            .or_else(|| self.state.approval_mode.read().ok().map(|m| m.clone()))
            .unwrap_or_default();
        let mode_rank = |m: &str| match m {
            "read-only" | "suggest" | "sandbox" => 0,
            "full-access" | "local" | "full-auto" => 2,
            _ => 1, // auto / auto-edit / 空（走 config 默认档）
        };
        let requested_mode = args.approval_mode.trim();
        let child_mode = if requested_mode.is_empty() || mode_rank(requested_mode) > mode_rank(&parent_mode) {
            parent_mode.clone()
        } else {
            requested_mode.to_string()
        };
        if !args.on_approval.trim().is_empty() && !args.on_approval.eq_ignore_ascii_case("deny") {
            observability::event(
                EventType::State,
                json!({
                    "kind": "subagent.approval_clamped",
                    "parent": parent,
                    "requested_on_approval": args.on_approval,
                    "requested_mode": requested_mode,
                    "effective_mode": child_mode,
                }),
            );
        }

        let result = crate::server::run_headless_inner(
            self.state.clone(),
            crate::server::HeadlessRequest {
                message,
                session_id: child.clone(),
                model,
                base_url,
                api_key,
                approval_mode: child_mode,
                plan_mode: false,
                on_approval: "deny".to_string(),
                timeout_secs: args.timeout_secs,
                budget_tokens: 0,
                config_id: String::new(),
                purpose: "subagent".into(),
                parent_session: parent.to_string(),
                workspace: String::new(),
            },
        )
        .await;

        // 收尾：注销派发 + 成本并入父任务 + 审计
        running_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&dispatch_id);
        let (status_kind, tokens, ms) = match &result {
            Ok(r) => (r.status.clone(), r.prompt_tokens, r.duration_ms),
            Err(_) => ("error".to_string(), 0, 0),
        };
        add_tokens(parent, tokens);
        observability::event(
            EventType::State,
            json!({
                "kind": "subagent.done",
                "parent": parent,
                "child": child,
                "status": status_kind,
                "tokens": tokens,
                "duration_ms": ms,
            }),
        );
        match result {
            Ok(r) => format!(
                "[子代理结果·{}] 状态 {} · 用时 {:.1}s · 输入 token {}\n{}",
                label,
                r.status,
                r.duration_ms as f64 / 1000.0,
                r.prompt_tokens,
                if r.answer.trim().is_empty() { "（子代理没有产出文字）" } else { r.answer.trim() }
            ),
            Err((_c, m)) => format!("[子代理失败] {m}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 并发度核算：全局上限 + 单父任务上限都是硬闸；0 = 不许派发。
    #[test]
    fn concurrency_accounting() {
        // 清场（本文件静态跨用例共享）
        running_slot().lock().unwrap_or_else(|e| e.into_inner()).clear();
        let reg = Arc::new(crate::core::cancel::CancelRegistry::new());
        // 三个会话都「在跑」：注册表里有标志，取消才落得下（try_begin 即登记）
        for sid in ["c1", "c2", "c3"] {
            let f = reg.try_begin(sid).expect("新会话应能占位");
            assert!(!f.is_set());
        }
        let mut running = running_slot().lock().unwrap_or_else(|e| e.into_inner());
        running.insert("d1".into(), Dispatch { parent: "p1".into(), child: "c1".into(), registry: reg.clone() });
        running.insert("d2".into(), Dispatch { parent: "p1".into(), child: "c2".into(), registry: reg.clone() });
        running.insert("d3".into(), Dispatch { parent: "p2".into(), child: "c3".into(), registry: reg.clone() });
        drop(running);
        assert_eq!(running_total(), 3);
        assert_eq!(running_for("p1"), 2);
        assert_eq!(running_for("p2"), 1);
        assert_eq!(running_for("p3"), 0);
        // 取消级联：只取消 p1 的两个子代理
        assert_eq!(cancel_children("p1"), 2);
        assert_eq!(running_for("p1"), 0);
        assert_eq!(running_for("p2"), 1, "别的父任务的子代理不受影响");
        assert!(reg.is_set("c1") && reg.is_set("c2") && !reg.is_set("c3"));
        running_slot().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// token 并入：水印式增量（调用方只认自己没计过的那部分）。
    #[test]
    fn token_attribution_accumulates() {
        tokens_slot().lock().unwrap_or_else(|e| e.into_inner()).clear();
        assert_eq!(subagent_tokens("p"), 0);
        add_tokens("p", 100);
        add_tokens("p", 0); // 0 不入账
        add_tokens("p", 50);
        assert_eq!(subagent_tokens("p"), 150);
        assert_eq!(subagent_tokens("q"), 0, "父任务之间互不串");
        tokens_slot().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 嵌套深度：根=0，子代理=1；深度表决定「子代理不能再派」。
    #[test]
    fn depth_guard() {
        depth_slot().lock().unwrap_or_else(|e| e.into_inner()).clear();
        assert_eq!(depth_of("root"), 0);
        // 先算再锁：depth_of 也要锁同一把锁，持锁调用会自锁死
        let child_depth = depth_of("root") + 1;
        depth_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("child".into(), child_depth);
        assert_eq!(depth_of("child"), 1);
        depth_slot().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 参数解析：task 必填，其余全缺省。
    #[test]
    fn args_defaults() {
        let a: SubagentArgs = serde_json::from_str(r#"{"task":"调研"}"#).unwrap();
        assert_eq!(a.task, "调研");
        assert!(a.model.is_empty());
        assert!(a.on_approval.is_empty());
        assert_eq!(a.timeout_secs, 0);
        assert!(serde_json::from_str::<SubagentArgs>(r#"{"label":"x"}"#).is_err(), "缺 task 必须报错");
    }
}
