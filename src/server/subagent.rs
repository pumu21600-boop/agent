//! 子代理派发：SubagentRunner 的实现（并发度 / 取消级联 / 成本归属都在这层）
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

use std::collections::{HashMap, VecDeque};
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

/// 派发历史（UI「子代理」面板的数据源；有界环形，新→旧）。
/// 为什么不在前端攒：SSE 事件只推给当时开着的页面，重开/刷新就没了；
/// 跑完的派发要能回看（状态/耗时/token/跳子会话），必须服务端着账。
fn history_slot() -> &'static Mutex<VecDeque<SubagentRecord>> {
    static HISTORY: OnceLock<Mutex<VecDeque<SubagentRecord>>> = OnceLock::new();
    HISTORY.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// 历史容量（条）。子代理是高频操作（一轮可以派好几个），200 条够回看最近若干轮。
const HISTORY_CAP: usize = 200;

/// 一条派发记录（status: queued / running / done / cancelled / timeout / error）。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SubagentRecord {
    pub dispatch: String,
    pub parent: String,
    pub child: String,
    pub label: String,
    pub model: String,
    pub task_chars: usize,
    pub status: String,
    pub tokens: u64,
    pub duration_ms: u128,
    /// 派发时刻（Unix ms）；finished_at 同理，未结束为 None
    pub started_at: u64,
    pub finished_at: Option<u64>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 推一条 UI 事件（子代理生命周期 → 前端实时条；无人订阅即丢弃，等历史接口兜底）。
/// 审计事件（observability）照旧另发，两路各司其职。
fn ui_push(v: Value) {
    crate::server::ui_events::emit(v);
}

/// 近期派发历史（新→旧；UI 面板拉全量自己按 parent 过滤）。
pub(crate) fn recent_dispatches() -> Vec<SubagentRecord> {
    history_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// 按 dispatch id 取消一个活跃子代理（UI「取消」按钮）。返回是否找到并取消。
pub(crate) fn cancel_dispatch(dispatch_id: &str) -> bool {
    let hit = {
        let mut running = running_slot().lock().unwrap_or_else(|e| e.into_inner());
        let removed = running.remove(dispatch_id);
        // 同步历史状态：排队/执行中的记录翻成 cancelled
        if removed.is_some() {
            let mut hist = history_slot().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(rec) = hist.iter_mut().find(|r| r.dispatch == dispatch_id) {
                rec.status = "cancelled".into();
                rec.finished_at = Some(now_ms());
            }
        }
        removed
    };
    let Some(d) = hit else { return false };
    d.registry.cancel(&d.child);
    ui_push(json!({
        "kind": "subagent.cancelled",
        "dispatch": dispatch_id,
        "parent": d.parent,
        "child": d.child,
    }));
    true
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

/// 子代理审批档钳制：只准更严，不许借子代理升权（返回 生效档, 降级原因）。
///
/// 白名单式档位排序——认不出的名字返回 `None` 按「非法」处理（跟随父档 + 事件）。
/// 早先这里 `_ => 1` 把任何拼错/编造的档位（如 "yolo"）当成 auto 放行，模型传错
/// 字得不到任何反馈（2026-10-20 修，与 on_approval 的处理方式对齐）。
fn clamp_child_mode(parent_mode: &str, requested: &str) -> (String, Option<&'static str>) {
    let rank = |m: &str| -> Option<u8> {
        match m {
            // 0 = 只读侧（sandbox/suggest 是 2026-10-03 并三档前的旧名）
            "read-only" | "sandbox" | "suggest" => Some(0),
            "auto" | "auto-edit" | "full-auto" => Some(1),
            "full-access" | "local" => Some(2),
            _ => None,
        }
    };
    // 父档为空/认不出时按 auto 计——config 默认档即 auto
    let parent_rank = rank(parent_mode).unwrap_or(1);
    let requested = requested.trim();
    if requested.is_empty() {
        return (parent_mode.to_string(), None);
    }
    match rank(requested) {
        Some(r) if r > parent_rank => (parent_mode.to_string(), Some("above_parent")),
        Some(_) => (requested.to_string(), None),
        None => (parent_mode.to_string(), Some("unknown_mode")),
    }
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

/// 历史入队（新→旧，超容 pop_back）。
fn history_push(rec: SubagentRecord) {
    let mut h = history_slot().lock().unwrap_or_else(|e| e.into_inner());
    h.push_front(rec);
    while h.len() > HISTORY_CAP {
        h.pop_back();
    }
}

/// 按 dispatch id 更新历史记录（queued→running→终态 全生命周期同一段）。
fn history_update(dispatch: &str, f: impl FnOnce(&mut SubagentRecord)) {
    let mut h = history_slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(rec) = h.iter_mut().find(|r| r.dispatch == dispatch) {
        f(rec);
    }
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
        // 派发 id 提前发号：排队/spawned/done/取消全生命周期共用（前端实时条按它聚合）
        let dispatch_id = uuid::Uuid::new_v4().simple().to_string();
        let label = if args.label.trim().is_empty() {
            task.chars().take(20).collect::<String>()
        } else {
            args.label.trim().to_string()
        };
        let task_preview = task.chars().take(80).collect::<String>();
        // 并发度：全局上限 + 单父任务上限，满员等待（轮询式 FIFO 近似，200ms 粒度）。
        // 等待期间父任务被取消 → 直接返回，不派发。
        // 排队即上屏：历史记 queued + UI 推一条——用户看得到「在排队」，不是干等。
        let mut queued_pushed = false;
        loop {
            if self.state.cancel.is_set(parent) {
                if queued_pushed {
                    history_update(&dispatch_id, |r| {
                        r.status = "cancelled".into();
                        r.finished_at = Some(now_ms());
                    });
                    ui_push(json!({ "kind": "subagent.cancelled", "dispatch": dispatch_id, "parent": parent }));
                }
                return "派发失败：父任务已取消，未派发。".to_string();
            }
            if running_total() < self.limits.max_concurrent
                && running_for(parent) < self.limits.per_parent.max(1)
            {
                break;
            }
            if !queued_pushed {
                queued_pushed = true;
                history_push(SubagentRecord {
                    dispatch: dispatch_id.clone(),
                    parent: parent.to_string(),
                    child: String::new(),
                    label: label.clone(),
                    model: String::new(),
                    task_chars: task.chars().count(),
                    status: "queued".into(),
                    tokens: 0,
                    duration_ms: 0,
                    started_at: now_ms(),
                    finished_at: None,
                });
                ui_push(json!({
                    "kind": "subagent.queued",
                    "dispatch": dispatch_id,
                    "parent": parent,
                    "label": label,
                    "task": task_preview,
                }));
            }
            observability::event(
                EventType::State,
                json!({ "kind": "subagent.queued", "parent": parent, "running": running_total(), "per_parent": running_for(parent) }),
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        // 没排过队（容量当即就绪）：记录还没入队，这里补一条（queued 态），
        // 下面的 spawned 更新会把它翻成 running——两条路径汇到同一段记录
        if !queued_pushed {
            history_push(SubagentRecord {
                dispatch: dispatch_id.clone(),
                parent: parent.to_string(),
                child: String::new(),
                label: label.clone(),
                model: String::new(),
                task_chars: task.chars().count(),
                status: "queued".into(),
                tokens: 0,
                duration_ms: 0,
                started_at: now_ms(),
                finished_at: None,
            });
        }

        // 子会话：独立会话文件（工作区继承——全局 workspace_root 本来就是共享的）
        let child = self.state.session.new_id();
        // 先算深度再上锁：depth_of 自己也要锁 depth_slot，持锁调用会自锁死（std Mutex 不可重入）
        let child_depth = depth_of(parent) + 1;
        depth_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(child.clone(), child_depth);
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

        // 历史：queued → running（补上 child/model）；UI：spawned（实时条出「执行中」行）
        history_update(&dispatch_id, |r| {
            r.child = child.clone();
            r.model = model.clone();
            r.status = "running".into();
        });
        ui_push(json!({
            "kind": "subagent.spawned",
            "dispatch": dispatch_id,
            "parent": parent,
            "child": child,
            "model": model,
            "label": label,
            "task": task_preview,
        }));
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
        // 白名单式档位排序：认不出的名字返回 None，调用方按「非法」处理。
        // 早先这里 `_ => 1` 把任何拼错/编造的档位（如 "yolo"）当成 auto 放行，
        // 模型传错字得不到任何反馈（2026-10-20 修，与下方 on_approval 的行为对齐）。
        let requested_mode = args.approval_mode.trim();
        let (child_mode, mode_clamp) = clamp_child_mode(&parent_mode, requested_mode);
        if mode_clamp.is_some() || (!args.on_approval.trim().is_empty() && !args.on_approval.eq_ignore_ascii_case("deny")) {
            observability::event(
                EventType::State,
                json!({
                    "kind": "subagent.approval_clamped",
                    "parent": parent,
                    "reason": mode_clamp,
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

        // 收尾：注销派发 + 成本并入父任务 + 历史终结态 + UI/审计事件
        running_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&dispatch_id);
        let (status_kind, tokens, ms) = match &result {
            Ok(r) => (r.status.clone(), r.prompt_tokens, r.duration_ms),
            Err(_) => ("error".to_string(), 0, 0),
        };
        add_tokens(parent, tokens);
        history_update(&dispatch_id, |r| {
            r.status = status_kind.clone();
            r.tokens = tokens;
            r.duration_ms = ms;
            r.finished_at = Some(now_ms());
        });
        ui_push(json!({
            "kind": "subagent.done",
            "dispatch": dispatch_id,
            "parent": parent,
            "child": child,
            "label": label,
            "status": status_kind,
            "tokens": tokens,
            "duration_ms": ms,
        }));
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

/// GET /api/subagents — 近期派发历史（新→旧；UI 面板按 parent 会话过滤自己那份）。
pub(super) async fn list_subagents() -> Json<Value> {
    Json(json!({ "dispatches": recent_dispatches() }))
}

#[derive(Deserialize)]
pub(super) struct CancelSubagentReq {
    dispatch: String,
}

/// POST /api/subagents/cancel — 取消一个活跃子代理（UI「取消」按钮；
/// 级联取消整组的场景走父任务停止键，这里是单发精确取消）。
pub(super) async fn cancel_subagent(Json(req): Json<CancelSubagentReq>) -> Json<Value> {
    let ok = cancel_dispatch(req.dispatch.trim());
    Json(json!({ "ok": ok }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 沾共享静态（running/history/tokens/depth 都是进程级）的测试全部串行——
    /// 并行会互相清场/插队，断言时灵时不灵（本轮实锤：容量测试被并行测试的
    /// clear() 打出一个偏差）。范式同 plugin::REGISTRY_TEST_LOCK。
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 清场（本文件静态跨用例共享）。
    fn clear_all() {
        running_slot().lock().unwrap_or_else(|e| e.into_inner()).clear();
        history_slot().lock().unwrap_or_else(|e| e.into_inner()).clear();
        tokens_slot().lock().unwrap_or_else(|e| e.into_inner()).clear();
        depth_slot().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    /// 并发度核算：全局上限 + 单父任务上限都是硬闸；0 = 不许派发。
    #[test]
    fn concurrency_accounting() {
        let _g = lock();
        // 清场（本文件静态跨用例共享）
        clear_all();
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
        clear_all();
    }

    /// 审批档钳制：只准更严；非法档位跟随父档并给出原因（不再静默当 auto）。
    #[test]
    fn approval_mode_clamped_or_rejected() {
        // 缺省 → 跟随父档，无降级
        assert_eq!(clamp_child_mode("auto", ""), ("auto".into(), None));
        assert_eq!(clamp_child_mode("read-only", "  "), ("read-only".into(), None));
        // 合法且不高于父档 → 原样生效
        assert_eq!(clamp_child_mode("auto", "read-only"), ("read-only".into(), None));
        assert_eq!(clamp_child_mode("full-access", "auto"), ("auto".into(), None));
        assert_eq!(clamp_child_mode("auto", "auto"), ("auto".into(), None));
        // 旧名照旧认（2026-10-03 并三档前的值）
        assert_eq!(clamp_child_mode("auto", "suggest"), ("suggest".into(), None));
        // 高于父档 → 压回父档，原因 above_parent
        assert_eq!(clamp_child_mode("auto", "full-access"), ("auto".into(), Some("above_parent")));
        assert_eq!(clamp_child_mode("read-only", "auto"), ("read-only".into(), Some("above_parent")));
        // 非法档位：不猜、不放行，跟随父档，原因 unknown_mode（旧实现静默当 auto）
        assert_eq!(clamp_child_mode("read-only", "yolo"), ("read-only".into(), Some("unknown_mode")));
        assert_eq!(clamp_child_mode("auto", "Auto"), ("auto".into(), Some("unknown_mode")), "大小写不对也是非法，不猜");
        // 父档为空（config 默认 auto）→ 按 auto 计
        assert_eq!(clamp_child_mode("", "read-only"), ("read-only".into(), None));
        assert_eq!(clamp_child_mode("", "full-access"), ("".into(), Some("above_parent")));
    }

    /// token 并入：水印式增量（调用方只认自己没计过的那部分）。
    #[test]
    fn token_attribution_accumulates() {
        let _g = lock();
        clear_all();
        assert_eq!(subagent_tokens("p"), 0);
        add_tokens("p", 100);
        add_tokens("p", 0); // 0 不入账
        add_tokens("p", 50);
        assert_eq!(subagent_tokens("p"), 150);
        assert_eq!(subagent_tokens("q"), 0, "父任务之间互不串");
        clear_all();
    }

    /// 嵌套深度：根=0，子代理=1；深度表决定「子代理不能再派」。
    #[test]
    fn depth_guard() {
        let _g = lock();
        clear_all();
        assert_eq!(depth_of("root"), 0);
        // 先算再锁：depth_of 也要锁同一把锁，持锁调用会自锁死
        let child_depth = depth_of("root") + 1;
        depth_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert("child".into(), child_depth);
        assert_eq!(depth_of("child"), 1);
        clear_all();
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

    /// 历史生命周期：queued → running → 终态，同一段记录就地更新（不堆多条）。
    #[test]
    fn history_lifecycle_updates_in_place() {
        let _g = lock();
        clear_all();
        history_push(SubagentRecord {
            dispatch: "d1".into(),
            parent: "p1".into(),
            child: String::new(),
            label: "调研".into(),
            model: String::new(),
            task_chars: 10,
            status: "queued".into(),
            tokens: 0,
            duration_ms: 0,
            started_at: 1000,
            finished_at: None,
        });
        history_update("d1", |r| {
            r.child = "c1".into();
            r.model = "m1".into();
            r.status = "running".into();
        });
        history_update("d1", |r| {
            r.status = "done".into();
            r.tokens = 4321;
            r.duration_ms = 1500;
            r.finished_at = Some(2500);
        });
        // 更新不存在的 dispatch：静默无操作（不 panic、不插队）
        history_update("nope", |r| r.status = "x".into());
        let all = recent_dispatches();
        assert_eq!(all.len(), 1, "全生命周期只应有一段记录");
        let r = &all[0];
        assert_eq!(r.child, "c1");
        assert_eq!(r.model, "m1");
        assert_eq!(r.status, "done");
        assert_eq!(r.tokens, 4321);
        assert_eq!(r.duration_ms, 1500);
        assert_eq!(r.finished_at, Some(2500));
        clear_all();
    }

    /// 容量边界：超容丢最旧，新的永远在队首。
    #[test]
    fn history_cap_drops_oldest() {
        let _g = lock();
        clear_all();
        for i in 0..HISTORY_CAP + 5 {
            history_push(SubagentRecord {
                dispatch: format!("d{i}"),
                parent: "p".into(),
                child: format!("c{i}"),
                label: format!("L{i}"),
                model: String::new(),
                task_chars: 0,
                status: "done".into(),
                tokens: 0,
                duration_ms: 0,
                started_at: i as u64,
                finished_at: Some(i as u64),
            });
        }
        let all = recent_dispatches();
        assert_eq!(all.len(), HISTORY_CAP, "超容必须截到容量");
        assert_eq!(all[0].dispatch, format!("d{}", HISTORY_CAP + 4), "队首是最新");
        assert_eq!(all[HISTORY_CAP - 1].dispatch, "d5", "队尾是第 6 新（前 5 条被丢）");
        clear_all();
    }

    /// 取消：命中活跃派发（子会话取消标志置位 + 历史翻 cancelled + UI 推一条）；
    /// 未命中（不存在/已结束）返回 false，不误伤。
    #[test]
    fn cancel_dispatch_precise() {
        let _g = lock();
        clear_all();
        clear_all();
        let reg = Arc::new(crate::core::cancel::CancelRegistry::new());
        let _f = reg.try_begin("child-1").expect("占位");
        running_slot().lock().unwrap_or_else(|e| e.into_inner()).insert(
            "disp-1".into(),
            Dispatch { parent: "p1".into(), child: "child-1".into(), registry: reg.clone() },
        );
        history_push(SubagentRecord {
            dispatch: "disp-1".into(),
            parent: "p1".into(),
            child: "child-1".into(),
            label: "L".into(),
            model: String::new(),
            task_chars: 0,
            status: "running".into(),
            tokens: 0,
            duration_ms: 0,
            started_at: 1,
            finished_at: None,
        });
        // UI 总线：取消要推得到（无人订阅时丢弃，这里订一条验真推了）
        let mut rx = crate::server::ui_events::test_subscribe();
        assert!(cancel_dispatch("disp-1"), "活跃派发应取消成功");
        assert!(reg.is_set("child-1"), "子会话取消标志必须置位");
        let rec = recent_dispatches().into_iter().find(|r| r.dispatch == "disp-1").expect("历史在");
        assert_eq!(rec.status, "cancelled");
        assert!(rec.finished_at.is_some(), "取消也要落终态时刻");
        let ev = rx.try_recv().expect("UI 事件应推得到");
        assert_eq!(ev["kind"], "subagent.cancelled");
        assert_eq!(ev["dispatch"], "disp-1");
        assert!(!cancel_dispatch("disp-1"), "重复取消返回 false");
        assert!(!cancel_dispatch("disp-404"), "不存在的 dispatch 返回 false");
        assert!(!reg.is_set("other"), "不相干的会话不许被误伤");
        clear_all();
        clear_all();
    }
}
