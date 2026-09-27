//! /api/chat 流式对话 + 请求级参数组装 + 取消（P19 自 server.rs 按域拆出）

use super::*;
use super::gate::ChatGate;
use super::memory::read_system_prompt;
use super::subagent;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaAttachment {
    pub name: String,
    pub mime: String,
    pub data: String,
}

impl MediaAttachment {
    pub fn is_audio(&self) -> bool {
        let n = self.name.to_lowercase();
        self.mime.starts_with("audio/")
            || n.ends_with(".mp3")
            || n.ends_with(".wav")
            || n.ends_with(".m4a")
            || n.ends_with(".ogg")
            || n.ends_with(".flac")
            || n.ends_with(".aac")
            || n.ends_with(".wma")
    }

    pub fn is_video(&self) -> bool {
        let n = self.name.to_lowercase();
        self.mime.starts_with("video/")
            || n.ends_with(".mp4")
            || n.ends_with(".webm")
            || n.ends_with(".mov")
            || n.ends_with(".avi")
            || n.ends_with(".mkv")
            || n.ends_with(".flv")
    }

    pub fn is_image(&self) -> bool {
        let n = self.name.to_lowercase();
        self.mime.starts_with("image/")
            || n.ends_with(".png")
            || n.ends_with(".jpg")
            || n.ends_with(".jpeg")
            || n.ends_with(".webp")
            || n.ends_with(".gif")
            || n.ends_with(".bmp")
            || n.ends_with(".svg")
            || (!self.is_audio() && !self.is_video())
    }

    pub fn audio_format(&self) -> String {
        let n = self.name.to_lowercase();
        if n.ends_with(".wav") || self.mime.contains("wav") {
            "wav".into()
        } else if n.ends_with(".ogg") || self.mime.contains("ogg") {
            "ogg".into()
        } else if n.ends_with(".m4a") || self.mime.contains("m4a") {
            "m4a".into()
        } else if n.ends_with(".flac") || self.mime.contains("flac") {
            "flac".into()
        } else if n.ends_with(".aac") || self.mime.contains("aac") {
            "aac".into()
        } else {
            "mp3".into()
        }
    }

    pub fn raw_base64(&self) -> String {
        if let Some(idx) = self.data.find(";base64,") {
            self.data[idx + 8..].to_string()
        } else if self.data.starts_with("data:") {
            if let Some(idx) = self.data.find(',') {
                self.data[idx + 1..].to_string()
            } else {
                self.data.clone()
            }
        } else {
            self.data.clone()
        }
    }

    pub fn data_url(&self) -> String {
        if self.data.starts_with("data:") {
            self.data.clone()
        } else {
            format!("data:{};base64,{}", self.effective_mime(), self.data)
        }
    }

    pub fn effective_mime(&self) -> String {
        if !self.mime.is_empty() && self.mime != "application/octet-stream" {
            return self.mime.clone();
        }
        let n = self.name.to_lowercase();
        if n.ends_with(".mp3") { "audio/mp3".into() }
        else if n.ends_with(".wav") { "audio/wav".into() }
        else if n.ends_with(".m4a") { "audio/m4a".into() }
        else if n.ends_with(".ogg") { "audio/ogg".into() }
        else if n.ends_with(".mp4") { "video/mp4".into() }
        else if n.ends_with(".webm") { "video/webm".into() }
        else if n.ends_with(".mov") { "video/quicktime".into() }
        else if n.ends_with(".png") { "image/png".into() }
        else if n.ends_with(".jpg") || n.ends_with(".jpeg") { "image/jpeg".into() }
        else if n.ends_with(".webp") { "image/webp".into() }
        else { "application/octet-stream".into() }
    }
}

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// 单轮消息（messages 为空时使用，兼容内置回退页）
    #[serde(default)]
    pub message: String,
    /// P2：完整消息数组（role: user/assistant/tool），非空则优先
    #[serde(default)]
    pub messages: Option<Vec<Value>>,
    /// P8.5 多模态媒体附件（图片、视频、音频）
    #[serde(default)]
    pub attachments: Option<Vec<MediaAttachment>>,
    /// 请求级覆盖：非空优先于已保存设置（页面当前选择即时生效）
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub base_url: String,
    /// 可选的请求级 key 覆盖（仅内存）；为空则用已保存设置
    #[serde(default)]
    pub api_key: String,
    /// 审批档位覆盖（P19 四档）：read-only / sandbox / auto / full-access（旧档名自动映射；缺省用配置默认）
    #[serde(default)]
    pub approval_mode: String,
    /// P6.1 多会话：消息落进哪条会话；缺省自动新建（前端从 SSE `session` 事件拿回 id）
    #[serde(default)]
    pub session_id: String,
    /// 027：同会话正在跑时这条消息怎么处理——false（缺省）= 排队，等这一轮跑完自动发；
    /// true = 立即（引导）：并进正在跑的那一轮，模型下一个决策点就看到（不打断、不另起一轮）
    #[serde(default)]
    pub steer: bool,
    /// P9 B1：Plan 档（true=只读+先出计划；请求级字段，前端随会话持久化到 localStorage）
    #[serde(default)]
    pub plan_mode: bool,
}

/// P8/P9/P8.5：从配置项 + 供应商预设组装请求参数（采样/思考/端点探测线索/工具开关）。
pub(super) fn build_chat_params(
    c: &crate::config::settings::Config,
    preset: Option<&gateway::providers::ProviderPreset>,
) -> gateway::ChatParams {
    let mut p = match preset {
        Some(p) => gateway::ChatParams::from_preset(p),
        None => gateway::ChatParams::default(),
    };
    p.max_tokens = c.max_output_tokens;
    p.temperature = c.temperature;
    p.top_p = c.top_p;
    p.thinking = c.thinking.clone();
    p.base_url = Some(c.effective_base_url());
    // A3：能力标记「工具调用」闭环（施工单 012）
    p.tools_enabled = c.capabilities.tools;
    apply_thinking_style(&mut p, c.thinking_style.as_deref());
    p
}

/// P9：思考参数形态覆盖——中转/自定义渠道端点探测认不出时显式指定（设置面板控件）。
/// B4 思考形态解析顺序（施工单 012 固化）：`Config.thinking_style` 显式覆盖 >
/// 端点探测（compat.rs：官方域名→native 供应商规范 / 本地→local / 其余→openai 兜底）>
/// providers.json 预设 `thinking_format` 兜底（预设值在 compat::resolve 内作为探测输入，
/// 当前仅豆包 zai/混元 qwen 带值且与探测结果一致）。
/// 词表（none/openai/deepseek/zai/qwen/qwen-chat-template）只增不改名；
/// 新形态入词表必须附真实渠道冒烟记录。none=不发任何思考参数。
fn apply_thinking_style(p: &mut gateway::ChatParams, style: Option<&str>) {
    use gateway::compat::ThinkingFormat as TF;
    match style {
        Some("none") => p.thinking_format = Some(TF::None),
        // 旧词表别名（P8.3 设置残留）：thinking_object=GLM 式对象 → zai；
        // reasoning_effort=档位 → openai；enable_thinking（_budget）=布尔 → qwen
        Some("thinking_object") => p.thinking_format = Some(TF::Zai),
        Some("reasoning_effort") => p.thinking_format = Some(TF::Openai),
        Some("enable_thinking" | "enable_thinking_budget") => p.thinking_format = Some(TF::Qwen),
        Some(v) => p.thinking_format = TF::parse(v),
        None => {} // auto/未知：跟端点探测
    }
}

/// P8/P8.5：把配置名（或配置 id）解析成一条可用渠道。解析失败（名字不存在/地址为空）返回 None。
/// B3 角色解析顺序（施工单 012 固化）：settings.roles.<role> > config.toml [roles] > 回落主渠道；
/// 角色值按「配置名或配置 id」解析（本函数），解析失败回落主渠道并 Notice。
pub(super) fn find_channel(s: &Settings, name: &str) -> Option<gateway::Channel> {
    let c = s
        .configs
        .iter()
        .find(|c| c.name == name || c.id == name)?;
    let base_url = c.effective_base_url();
    if base_url.is_empty() {
        return None;
    }
    let model = c.effective_model();
    let preset = gateway::providers::find(&c.provider_id);
    let params = build_chat_params(c, preset);
    // B2 key 回落链（施工单 012）：DPAPI key > DEEPSEEK_API_KEY 环境变量。
    // 角色/降级渠道与主渠道同一把尺子——没单独存 key 的配置靠环境变量兜底
    // （P21 补：此前只有主渠道走这条链，budget/summary/title 渠道会「未设置 key」）
    let key = resolve_key(s, c);
    Some(gateway::Channel::new(&base_url, &model, &key, params))
}

/// B2 key 回落链的角色/降级渠道版：本配置 DPAPI key 优先，缺失回落环境变量。
fn resolve_key(s: &Settings, c: &crate::config::settings::Config) -> String {
    s.config_key(c)
        .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok())
        .unwrap_or_default()
}

/// 审批档位归一化（P19 四档 + 旧档名映射）。返回（生效档, 降级提示）。
/// /api/chat 与无头会话（P22-A）共用——档位语义只有这一处解释。
pub(super) fn normalize_mode(cfg: &crate::config::AgentConfig, requested: &str) -> (String, Option<String>) {
    let norm = |m: &str| -> String {
        match m.trim() {
            "suggest" => "read-only".into(),
            "auto-edit" | "full-auto" => "auto".into(),
            "local" | "full-access" => "full-access".into(),
            other @ ("read-only" | "sandbox" | "auto") => other.to_string(),
            _ => "auto".into(),
        }
    };
    let mut mode = if requested.trim().is_empty() {
        norm(&cfg.default_approval)
    } else {
        norm(requested)
    };
    // full-access 需 config 总开关，没开就降级为 auto 并提示（档位与实际权限保持一致）
    if mode == "full-access" && !cfg.allow_unsandboxed {
        mode = "auto".into();
        return (
            mode,
            Some(
                "完全访问（脱沙箱）未开启：在 config.toml [agent] allow_unsandboxed = true 后重启服务生效，本次已回退为自动档"
                    .to_string(),
            ),
        );
    }
    (mode, None)
}

/// 一轮对话的系统提示：prompts/system.md（每轮实时读盘）+ 双层 AGENTS.md +
/// 项目现状摘要（新会话开场：上一条同项目会话 + 工作区未提交改动）+ 技能索引。
/// /api/chat 与无头会话共用同一套注入，行为对齐。
pub(super) async fn turn_system_prompt(state: &AppState, session_id: &str) -> String {
    let prompt = read_system_prompt(state);
    let agents_md = state
        .memory
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .agents_md();
    let project_brief = {
        let is_new = state
            .session
            .get(session_id)
            .map(|f| f.messages.is_empty())
            .unwrap_or(true);
        if is_new {
            let ws = crate::tools::workspace_root().to_path_buf();
            let stats = tokio::task::spawn_blocking(move || {
                let head = crate::recovery::head_id(&ws).ok().flatten()?;
                crate::recovery::diff_stat(&ws, &head).ok()
            })
            .await
            .ok()
            .flatten();
            state.session.project_brief(session_id, stats).unwrap_or_default()
        } else {
            String::new()
        }
    };
    format!("{prompt}{agents_md}{project_brief}{}", crate::skills::index_prompt())
}

/// 一轮对话的渠道组装结果（主渠道 + 角色渠道 + 降级链 + 预算降级）。
pub(super) struct TurnChannels {
    pub(super) primary: gateway::Channel,
    pub(super) summary: Option<gateway::Channel>,
    pub(super) title: Option<gateway::Channel>,
    pub(super) fallbacks: Vec<gateway::Channel>,
    /// P21：预算降级渠道（[roles] budget；超 token 预算后剩余步骤改走它）
    pub(super) downgrade: Option<gateway::Channel>,
    /// 角色渠道解析失败等提示（转发给调用方展示，不静默）
    pub(super) notices: Vec<String>,
}

/// 组装一轮对话的全部渠道。请求级覆盖优先（页面当前选择即时生效，不等保存）。
/// /api/chat 与无头会话共用——角色解析顺序（B3）与 key 回落链（B2）只有这一处实现。
pub(super) fn assemble_turn_channels(
    state: &AppState,
    s: &Settings,
    active_cfg: &crate::config::settings::Config,
    req_model: &str,
    req_base_url: &str,
    req_api_key: &str,
) -> TurnChannels {
    let base_url = if !req_base_url.is_empty() {
        req_base_url.to_string()
    } else {
        active_cfg.effective_base_url()
    };
    let model = if !req_model.is_empty() {
        req_model.to_string()
    } else {
        active_cfg.effective_model()
    };
    // B2 key 回落链（施工单 012 固化）：请求显式 key > 激活配置 DPAPI key > DEEPSEEK_API_KEY 环境变量
    let api_key = if !req_api_key.is_empty() {
        req_api_key.to_string()
    } else {
        s.config_key(active_cfg)
            .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok())
            .unwrap_or_default()
    };
    let mut params = build_chat_params(active_cfg, gateway::providers::find(&active_cfg.provider_id));
    params.base_url = Some(base_url.clone());
    let primary = gateway::Channel::new(&base_url, &model, &api_key, params);
    // P8/P8.5 角色映射：优先 settings.roles，未设回落 config.toml [roles]
    let roles = s.roles.as_ref();
    let summary_name = roles
        .and_then(|r| r.summary.as_deref())
        .or_else(|| state.roles_cfg.summary.as_deref());
    let title_name = roles
        .and_then(|r| r.title.as_deref())
        .or_else(|| state.roles_cfg.title.as_deref());
    let summary = summary_name.and_then(|n| find_channel(s, n));
    let title = title_name.and_then(|n| find_channel(s, n));
    let mut notices = Vec::new();
    if let Some(n) = summary_name
        && summary.is_none()
    {
        notices.push(format!(
            "角色「会话摘要」配置的渠道「{n}」解析失败（配置名不存在或地址为空），已回落主渠道代跑"
        ));
    }
    if let Some(n) = title_name
        && title.is_none()
    {
        notices.push(format!(
            "角色「标题生成」配置的渠道「{n}」解析失败（配置名不存在或地址为空），已回落主渠道代跑"
        ));
    }
    // P8/P8.5 降级链：优先 settings.roles.fallbacks，未设回落 [gateway].fallback_configs
    let fallback_names: Vec<String> = roles
        .map(|r| r.fallbacks.clone())
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| state.gateway_cfg.fallback_configs.clone());
    let fallbacks: Vec<gateway::Channel> = fallback_names
        .iter()
        .filter_map(|n| find_channel(s, n))
        .collect();
    // P21：预算降级渠道
    let budget_name = roles
        .and_then(|r| r.budget.as_deref())
        .or_else(|| state.roles_cfg.budget.as_deref());
    let downgrade = budget_name.and_then(|n| find_channel(s, n));
    if let Some(n) = budget_name
        && downgrade.is_none()
    {
        notices.push(format!(
            "角色「预算降级」配置的渠道「{n}」解析失败（配置名不存在或地址为空），超预算后维持主渠道"
        ));
    }
    // P23 思考参数自适应降级登记：该端点此前 400 拒收过思考字段（剥参重试成功），
    // 本轮起明确告知用户，不静默
    if state.gateway.thinking_is_degraded(&base_url, &model) {
        notices.push("该端点此前拒绝了思考参数，已自动关闭思考下发（对话正常进行）".to_string());
    }
    TurnChannels { primary, summary, title, fallbacks, downgrade, notices }
}

/// 检查模型是否具备指定模态的输入能力（对齐 DSH acceptsMedia 门禁规范）
fn accepts_modality(model: &str, kind: &str) -> bool {
    let caps = crate::config::settings::ModelCapabilities::infer_from_model_id(model);
    match kind {
        "audio" => caps.audio,
        "video" => caps.video,
        "image" => caps.vision,
        _ => false,
    }
}

/// A5（施工单 012）：媒体附件落在 <workspace>/.attachments/，若工作区是 git 仓库且
/// .gitignore 未含该目录，自动追加一行 `.attachments/`（只追加，绝不重写用户文件），
/// 防止用户 `git add .` 把二进制附件提交进历史。
fn ensure_attachments_ignored(ws: &std::path::Path) {
    if !ws.join(".git").exists() {
        return;
    }
    let gi = ws.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".attachments/") {
        return;
    }
    let mut add = String::new();
    // 用户文件可能不带结尾换行：直接追加会粘成一行，先补一个换行
    if !existing.is_empty() && !existing.ends_with('\n') {
        add.push('\n');
    }
    add.push_str(".attachments/\n");
    let ok = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&gi)
        .and_then(|mut f| std::io::Write::write_all(&mut f, add.as_bytes()))
        .is_ok();
    if ok {
        // try_event：这条路径会在单测里被直接调用，logger 未初始化时不该 panic
        observability::try_event(
            EventType::State,
            json!({ "kind": "attachments.gitignored", "path": gi.display().to_string() }),
        );
    }
}

/// 流式对话：base_url/model/key 全部来自已保存设置（请求级 api_key 可覆盖）。
pub(super) async fn chat(State(state): State<AppState>, Json(req): Json<ChatRequest>) -> Response {
    let message = req.message.trim().to_string();
    let req_model = req.model.trim().to_string();
    let req_base_url = req.base_url.trim().to_string();
    let req_api_key = req.api_key.trim().to_string();
    let attachments = req.attachments.unwrap_or_default();
    let history: Vec<Value> = match &req.messages {
        Some(m) if !m.is_empty() => m.clone(),
        _ => {
            if message.is_empty() {
                vec![]
            } else {
                vec![json!({ "role": "user", "content": message })]
            }
        }
    };
    let (tx, rx) = mpsc::unbounded_channel::<Result<Event, Infallible>>();
    let gateway = state.gateway.clone();
    let cancel_registry = state.cancel.clone();
    let steer_queue = state.steer.clone();
    let s = state.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    let active_cfg = s.active_or_default().clone();
    let cfg = (*state.agent_cfg).clone();
    let sandbox_cfg = (*state.sandbox_cfg).clone();
    // 上下文预算：当前模型设了上下文长度就用它（覆盖全局 [context].budget_tokens）
    let ctx_cfg = {
        let mut c = (*state.context_cfg).clone();
        if let Some(cl) = active_cfg.context_length {
            c.budget_tokens = cl;
        }
        c
    };
    // P7 AGENTS.md 双层注入（全局在前、项目在后）；P8.2 记忆层随工作区热重开
    let approvals = state.approvals.clone();
    let session = state.session.clone();
    let mem = state.memory.read().unwrap_or_else(|e| e.into_inner()).clone();
    let rec_cfg = (*state.recovery_cfg).clone();
    // P6.1 多会话：请求带了合法 id 就用，没带就新建（前端从首个 SSE 事件拿 id）
    let session_id = {
        let sid = req.session_id.trim().to_string();
        let valid = !sid.is_empty()
            && sid.len() <= 64
            && sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        if valid { sid } else { session.new_id() }
    };
    // 025：同一会话已在跑一轮 → **不拒收**：文本消息进追加队列，运行中的那一轮在下一个
    // 决策点就会看到（原轮不打断、不另起一轮，会话文件保持单写者）。判定与登记仍在同一把
    // 锁内（try_begin），并发请求不会双双通过检查；登记好的标志随任务移交 RAII 守卫
    // CancelRun，任务结束（含提前 return / panic）自动注销。
    // 例外：带附件的追加暂不支持（媒体流水线在轮首一次性构建），明确拒绝而不是悄悄丢附件。
    // M2：这里的 history 在「接管」分支里会被换成会话快照 + 队列消息，故为可变绑定。
    let mut history = history;
    let mut run_flag = state.cancel.try_begin(&session_id);
    if run_flag.is_none() {
        if !attachments.is_empty() {
            observability::event(
                EventType::State,
                json!({ "kind": "chat.rejected", "reason": "session_busy_attachments", "session": session_id }),
            );
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!("会话 {session_id} 正在运行中：追加消息暂不支持带附件，等这一轮跑完再发")
                })),
            )
                .into_response();
        }
        let text = if !message.is_empty() {
            message.clone()
        } else {
            history
                .iter()
                .rev()
                .find(|m| m["role"] == "user")
                .and_then(|m| m["content"].as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        if text.is_empty() {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": "消息为空" }))).into_response();
        }
        // 防线（2026-09-27 前端重发风暴教训）：单会话排队上限。正常永远到不了——
        // 每条消息客户端只发一次；堆到上限说明客户端在循环重发，拒收并给人话，
        // 别让队列被灌到几千条后一次性排进模型上下文。
        if state.steer.pending(&session_id) >= 32 {
            observability::event(
                EventType::State,
                json!({ "kind": "chat.queue_full", "session": session_id, "pending": state.steer.pending(&session_id) }),
            );
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({
                    "error": format!("会话 {session_id} 的排队消息已堆到上限（32 条）：客户端可能在循环重发。请点「停止」清空队列后再发。")
                })),
            )
                .into_response();
        }
        // 027：缺省 = 排队（这一轮跑完自动接着发）；steer=true = 立即（引导当前这一轮）
        let immediate = req.steer;
        let pending = state.steer.push(&session_id, text, immediate);
        observability::event(
            EventType::State,
            json!({
                "kind": "chat.steered",
                "session": session_id,
                "mode": if immediate { "immediate" } else { "queue" },
                "pending": pending,
            }),
        );
        let (stx, srx) = mpsc::unbounded_channel::<Result<Event, Infallible>>();
        let notice = if immediate {
            "已并入正在运行的一轮：模型下一个决策点就会看到（引导当前回答，不打断）".to_string()
        } else {
            "已排队：当前这一轮结束后自动发送（想马上引导当前回答，点这条消息上的「立即发送」）"
                .to_string()
        };
        let flag = if immediate { "steered" } else { "queued" };
        // M2 收口：入队的这一刻，上一轮可能刚好收尾（它的收尾 drain 已经跑过）——
        // 那样这条就没人取了。再抢一次占位：抢到 = 确实已结束 → 本次请求直接把队列
        // 里的消息跑掉（不让消息悬在队列里等下一次活动）；抢不到 = 真的还在跑，照常回执。
        if let Some(f) = state.cancel.try_begin(&session_id) {
            let extra = state.steer.drain_all(&session_id);
            let snap = session.get(&session_id).map(|s| s.messages).unwrap_or_default();
            let mut merged = if snap.is_empty() { history.clone() } else { snap };
            for t in &extra {
                merged.push(json!({ "role": "user", "content": t }));
            }
            observability::event(
                EventType::State,
                json!({ "kind": "chat.takeover", "session": session_id, "count": extra.len() }),
            );
            history = merged;
            run_flag = Some(f);
        } else {
            let _ = stx.send(Ok(json_event(json!({ "session": session_id.clone() }))));
            let _ = stx.send(Ok(json_event(json!({ flag: true, "notice": notice }))));
            let _ = stx.send(Ok(json_event(json!({ "done": true }))));
            return Sse::new(UnboundedReceiverStream::new(srx)).into_response();
        }
    }
    let run_flag = run_flag.expect("占位或接管必得其一");
    // P7 滚动摘要：从会话文件带出旧摘要继续压
    let prior_summary = session.get(&session_id).and_then(|f| f.summary);
    // P22-C 子代理派发器：主对话任务才挂（子代理自己的无头轮不挂——v1 不支持嵌套）。
    // 在 spawn 前构建：dispatcher 持 AppState 克隆，任务里随 LoopChannels 带进核心循环。
    let subagents: Option<Arc<dyn crate::core::subagent::SubagentRunner>> =
        Some(Arc::new(subagent::SubagentDispatcher {
            state: state.clone(),
            limits: (*state.subagent_cfg).clone(),
        }));
    // P22-C：子代理 token 并入本任务预算的水印（只计自己没计过的增量）
    let subagent_seen = std::sync::atomic::AtomicU64::new(subagent::subagent_tokens(&session_id));
    // 审批档位归一化（P19 四档）：旧档名映射保留兼容 → 写入共享档位供 gate 活读。
    // full-access 需 config 总开关，没开就降级为 auto 并告知前端（档位与实际权限保持一致）。
    let allow_unsandboxed = cfg.allow_unsandboxed;
    let (mode, mode_notice) = normalize_mode(&cfg, &req.approval_mode);
    if let Some(n) = mode_notice {
        let _ = tx.send(Ok(json_event(json!({ "notice": n }))));
    }
    // M1：档位写进**本会话**（不再写全局默认，否则并发多会话会互相覆盖——
    // 后到者生效可能让另一个会话以比预期更宽松的档位执行）
    if let Ok(mut m) = state.session_approval.write() {
        m.insert(session_id.clone(), mode.clone());
    }

    tokio::spawn(async move {
        let send = |payload: Value| {
            let _ = tx.send(Ok(json_event(payload)));
        };
        // D2：本轮取消标志（会话级）——新会话不再复位别人的标志，停止只停本会话。
        // N1：标志已在进入 handler 时占位登记（同会话并发在门口就 409）。
        // 守卫在任务结束（含提前 return / panic）时注销登记，不留「永远在跑」的僵尸会话。
        // M2：run/cancel 可重绑——收尾时会先注销占位、再取队列续跑一轮
        let mut run = CancelRun::from_flag(cancel_registry.clone(), &session_id, run_flag);
        let mut cancel = run.flag.clone();
        // P7 摘要槽：Summary 事件写入，Snapshot 落盘时随会话持久化
        let summary_slot: Arc<std::sync::Mutex<Option<String>>> =
            Arc::new(std::sync::Mutex::new(prior_summary.clone()));
        // 025：本任务最新一次会话快照（消息全量，不含 system）——迟到的追加消息
        // 要接着再跑一轮时，用它当上下文，而不是重新问客户端要历史。
        let last_snapshot: Arc<std::sync::Mutex<Vec<Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        if history.is_empty() {
            send(json!({ "error": "消息为空" }));
            return;
        }
        // P6.1：先把会话 id 告诉前端（新建时它还不知道）
        send(json!({ "session": session_id }));

        // 系统约定 + 双层 AGENTS.md + 项目现状摘要 + 技能索引（与无头会话同一组装路径）
        let system_prompt = turn_system_prompt(&state, &session_id).await;

        // P8.5 多模态媒体理解流水线：感知媒体附件与视觉/视频/音频解析能力
        let mut history = history;
        // 025：续跑一轮时摘要可能被上一轮更新（见收尾处的重新赋值）
        let mut prior_summary = prior_summary;
        if !attachments.is_empty() {
            let roles = s.roles.as_ref();
            let mut native_attachments: Vec<&MediaAttachment> = Vec::new();
            let mut forward_attachments: Vec<&MediaAttachment> = Vec::new();

            for att in &attachments {
                let is_vid = att.is_video();
                let is_aud = att.is_audio();
                let is_img = att.is_image();

                let supports_natively = (is_img && active_cfg.capabilities.vision)
                    || (is_vid && active_cfg.capabilities.video)
                    || (is_aud && active_cfg.capabilities.audio);

                if supports_natively {
                    native_attachments.push(att);
                } else {
                    forward_attachments.push(att);
                }
            }

            // 1. 对于主模型原生支持的媒体类型，挂载原生多模态结构
            if !native_attachments.is_empty()
                && let Some(last_msg) = history.iter_mut().rev().find(|m| m["role"] == "user") {
                    let mut parts = Vec::new();
                    if let Some(txt) = last_msg["content"].as_str() {
                        parts.push(json!({ "type": "text", "text": txt }));
                    }
                    for att in native_attachments {
                        if att.is_audio() {
                            parts.push(json!({
                                "type": "input_audio",
                                "input_audio": {
                                    "data": att.raw_base64(),
                                    "format": att.audio_format()
                                }
                            }));
                        } else {
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": { "url": att.data_url() }
                            }));
                        }
                    }
                    last_msg["content"] = json!(parts);
                }

            // 2. 对于主模型不支持的媒体类型，走专职转接与确定性文本投影（对齐 DSH media-understanding 规范）
            if !forward_attachments.is_empty() {
                let mut projections = Vec::new();

                for att in forward_attachments {
                    let is_vid = att.is_video();
                    let is_aud = att.is_audio();
                    let kind = if is_vid { "video" } else if is_aud { "audio" } else { "image" };
                    let kind_cn = if is_vid { "视频" } else if is_aud { "音频" } else { "图片" };
                    let success_label = if is_vid { "Video understanding" } else if is_aud { "Audio understanding" } else { "Image understanding" };

                    // 持久化附件到工作区 .attachments/ 便于主模型与工具查阅
                    let ws = crate::tools::workspace_root();
                    let att_dir = ws.join(".attachments");
                    let _ = std::fs::create_dir_all(&att_dir);
                    // A5：附件目录自动进工作区 git 排除（幂等，只追加）
                    ensure_attachments_ignored(&ws);
                    let safe_name = std::path::Path::new(&att.name)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("attachment");
                    let target_file = att_dir.join(safe_name);
                    use base64::Engine;
                    let clean = att.raw_base64().trim().replace(['\r', '\n', ' '], "");
                    let unpadded = clean.trim_end_matches('=');
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(&clean)
                        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(unpadded))
                        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(unpadded));
                    if let Ok(bytes) = decoded {
                        let _ = std::fs::write(&target_file, bytes);
                    }

                    // 1) 专属路由解析（对齐 DSH）：严格隔离，仅使用用户在 roles 中针对该模态显式配置的渠道，严禁自动跨渠道降级
                    let role_target = if is_vid {
                        roles.and_then(|r| r.video.as_deref())
                    } else if is_aud {
                        roles.and_then(|r| r.audio.as_deref())
                    } else {
                        roles.and_then(|r| r.vision.as_deref())
                    };

                    let mut candidates: Vec<gateway::Channel> = Vec::new();
                    if let Some(n) = role_target.filter(|n| !n.trim().is_empty())
                        && let Some(ch) = find_channel(&s, n) {
                            candidates.push(ch);
                        }

                    // 2) 若该模态未配置路由或无可用候选：对齐 DSH 替换为确定性占位文本（deterministic placeholder），绝不发起注定失败的请求
                    if candidates.is_empty() {
                        let placeholder = format!(
                            "[{kind} omitted because no {kind}-understanding model is configured; 附件「{}」已跳过并保存至工作区 .attachments/{}。当前未在“设置 → 角色分工”中配置{}理解专家 ({})，请在设置中指定具备{}能力的多模态模型（如 Gemini、Whisper、Qwen-Audio 等），或直接粘贴文字描述]",
                            att.name,
                            safe_name,
                            kind_cn,
                            if is_aud { "Audio" } else if is_vid { "Video" } else { "Vision" },
                            kind_cn
                        );
                        projections.push(placeholder);
                        continue;
                    }

                    // 3) 能力预检：以专家配置里用户勾选的能力标记为准（勾了「图片解析」就放行，
                    //    不再看模型名里有没有 vision 等关键字）；找不到对应配置时才退回按名推导
                    let first_ch = &candidates[0];
                    let first_provider = first_ch.params.provider_id.as_deref().unwrap_or("openai_compat");
                    let first_model = &first_ch.model;

                    let gate_ok = match role_target
                        .and_then(|id| s.configs.iter().find(|c| c.id == id || c.name == id))
                    {
                        Some(expert) => match kind {
                            "audio" => expert.capabilities.audio,
                            "video" => expert.capabilities.video,
                            "image" => expert.capabilities.vision,
                            _ => false,
                        },
                        None => accepts_modality(first_model, kind),
                    };
                    if !gate_ok {
                        let text = format!(
                            "[{} not run: 专家配置「{}/{}」未勾选{}解析能力。该{}未展示给主模型。请在「设置 → 模型配置」选中该专家配置并勾选「{}解析」（或在「角色分工」把{}理解专家换成具备该能力的模型）]",
                            success_label, first_provider, first_model, kind_cn, kind_cn, kind_cn, kind_cn
                        );
                        projections.push(text);
                        continue;
                    }

                    let candidate_prompts: Vec<Value> = if is_aud {
                        vec![
                            // 格式 1（标准 OpenAI input_audio：媒体块在前，提示词在后）
                            json!([
                                {
                                    "role": "user",
                                    "content": [
                                        {
                                            "type": "input_audio",
                                            "input_audio": {
                                                "data": att.raw_base64(),
                                                "format": att.audio_format()
                                            }
                                        },
                                        {
                                            "type": "text",
                                            "text": "Transcribe and describe this audio in detail. Cover spoken content and notable non-speech sounds in order. 请仔细倾听并详细转录该音频内容：逐字转录全部语音对话内容（ASR），提取说话人语调与关键要点，并记录环境背景声音。"
                                        }
                                    ]
                                }
                            ]),
                            // 格式 2（备选 audio_url）
                            json!([
                                {
                                    "role": "user",
                                    "content": [
                                        {
                                            "type": "audio_url",
                                            "audio_url": { "url": att.data_url() }
                                        },
                                        {
                                            "type": "text",
                                            "text": "Transcribe and describe this audio in detail. Cover spoken content and notable non-speech sounds in order."
                                        }
                                    ]
                                }
                            ]),
                        ]
                    } else if is_vid {
                        vec![
                            json!([
                                {
                                    "role": "user",
                                    "content": [
                                        {
                                            "type": "video_url",
                                            "video_url": { "url": att.data_url() }
                                        },
                                        {
                                            "type": "text",
                                            "text": "Describe this video in detail: what happens across it, including visual content and any speech or captions, in chronological order. 请按时间顺序详细描述该视频：视频中发生了什么，包括画面视觉内容、任何语音旁白或字幕。"
                                        }
                                    ]
                                }
                            ]),
                            json!([
                                {
                                    "role": "user",
                                    "content": [
                                        {
                                            "type": "image_url",
                                            "image_url": { "url": att.data_url() }
                                        },
                                        {
                                            "type": "text",
                                            "text": "Describe this video in detail: what happens across it, including visual content and any speech or captions, in chronological order."
                                        }
                                    ]
                                }
                            ]),
                        ]
                    } else {
                        vec![
                            json!([
                                {
                                    "role": "user",
                                    "content": [
                                        {
                                            "type": "image_url",
                                            "image_url": { "url": att.data_url() }
                                        },
                                        {
                                            "type": "text",
                                            "text": "Describe this image in detail. Cover every element that would matter to someone who cannot see it, in the order a reader would need. 请详细观察并深度解析该图片内容：涵盖所有对看不见该图片的人至关重要的元素，逐字提取画面中的所有可见文本、代码片段、图表与数值数据。"
                                        }
                                    ]
                                }
                            ])
                        ]
                    };

                    let mut desc_result = None;
                    let mut last_route = (first_provider.to_string(), first_model.clone());
                    let mut last_error_msg = String::new();

                    for vch in &candidates {
                        let provider = vch.params.provider_id.as_deref().unwrap_or("openai_compat");
                        last_route = (provider.to_string(), vch.model.clone());

                        let mut params = vch.params.clone();
                        // DSH 规范：上限 1500 tokens，清空思考参数
                        params.max_tokens = Some(params.max_tokens.unwrap_or(1500).min(1500));
                        params.thinking = None;

                        let mut call_err = None;
                        for prompt_variant in &candidate_prompts {
                            // P21：媒体旁路调用记台账（purpose=media，归属本会话）
                            let media_ctx = gateway::UsageCtx::aux(&session_id, "media");
                            match gateway.complete(&media_ctx, &vch.base_url, &vch.model, &vch.api_key, prompt_variant, &params).await {
                                Ok(text) if !text.trim().is_empty() => {
                                    let is_refusal = text.contains("没有能力听音频")
                                        || text.contains("无法听音频")
                                        || text.contains("无法识别音频")
                                        || text.contains("无法处理音频")
                                        || text.contains("无法解析音频")
                                        || text.contains("我是纯文本")
                                        || text.contains("不支持音频")
                                        || text.contains("未附带音频")
                                        || text.contains("没看到音频")
                                        || text.contains("don't have the ability to listen")
                                        || text.contains("cannot process audio")
                                        || text.contains("text-based AI assistant")
                                        || text.contains("no audio file was attached")
                                        || text.contains("don't see any audio file")
                                        || text.contains("looks like no audio");
                                    if is_refusal {
                                        call_err = Some(format!("上游网关或模型声明未收到/无法解析该音频: {}", text.trim()));
                                    } else {
                                        desc_result = Some(text.trim().to_string());
                                        break;
                                    }
                                }
                                Ok(_) => {
                                    call_err = Some("模型返回空内容".to_string());
                                }
                                Err(e) => {
                                    observability::event(
                                        EventType::Error,
                                        json!({ "kind": "media.error", "modality": kind, "model": vch.model, "url": vch.base_url, "error": format!("{e:#}") }),
                                    );
                                    call_err = Some(format!("{e}"));
                                }
                            }
                        }

                        if desc_result.is_some() {
                            break;
                        }
                        if let Some(err) = call_err {
                            last_error_msg = err;
                        }
                    }

                    // 4) 投影格式化（DSH 规范）
                    let substituted_text = if let Some(text) = desc_result {
                        format!("[{} ({}/{}):\n{}]", success_label, last_route.0, last_route.1, text)
                    } else {
                        format!("[{} failed ({}/{}): {}]", success_label, last_route.0, last_route.1, last_error_msg)
                    };
                    projections.push(substituted_text);
                }

                // 5) 历史投影（projectMessages）：直接替换用户消息内容，媒体字节不传给会话模型
                if !projections.is_empty()
                    && let Some(last_msg) = history.iter_mut().rev().find(|m| m["role"] == "user") {
                        let orig = match &last_msg["content"] {
                            Value::String(s) => s.clone(),
                            Value::Array(arr) => {
                                arr.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")
                            }
                            _ => "".to_string(),
                        };
                        let combined = if orig.trim().is_empty() {
                            projections.join("\n\n")
                        } else {
                            format!("{}\n\n{}", projections.join("\n\n"), orig.trim())
                        };
                        last_msg["content"] = json!(combined);
                    }
            }
        }
        // 请求级覆盖 + 角色/降级渠道组装（与无头会话同一路径；提示见 notices）
        let ch = assemble_turn_channels(&state, &s, &active_cfg, &req_model, &req_base_url, &req_api_key);
        for n in &ch.notices {
            send(json!({ "notice": n }));
        }
        // P18 钩子（027）：单任务累计 token（预算通知用；[hooks] 未配置时零开销）
        // P21：同一个 Arc 标志同时驱动「剩余步骤改走降级渠道」（核心循环每步读它）
        let task_tokens = std::sync::atomic::AtomicU64::new(0);
        let budget_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        loop {
            let outcome = agent::run_task(
                &gateway,
                // 续跑一轮时这些值复用（Channel 是克隆廉价的连接参数快照）
                &agent::LoopChannels {
                    primary: ch.primary.clone(),
                    summary: ch.summary.clone(),
                    title: ch.title.clone(),
                    fallbacks: ch.fallbacks.clone(),
                    // P21：预算降级渠道 + 共享超预算标志（见上方 budget_fired）
                    downgrade: ch.downgrade.clone(),
                    budget_hit: budget_fired.clone(),
                    // P22-C：子代理派发（None 的任务不许派发）
                    subagents: subagents.clone(),
                },
                // 提示词保持最小：prompts/system.md（"用中文简洁回答"）+ AGENTS.md 仓库事实 + 技能索引（无技能时不占上下文）。
                // 不注入模型名/身份/角色——用户明确要求，模型自造身份属可接受代价（坑 13）
                &system_prompt,
                &history,
                &cancel,
                &cfg,
                &sandbox_cfg,
                &ctx_cfg,
                &rec_cfg,
                &mem,
                prior_summary.clone(),
                req.plan_mode,
                &ChatGate {
                    mode: state.approval_mode.clone(),
                    session_modes: state.session_approval.clone(),
                    allow_unsandboxed,
                    unsandboxed_approval: cfg.unsandboxed_approval,
                    pending: approvals.clone(),
                    // P9 A3：会话内同类允许的作用域与记忆存储
                    session_id: session_id.clone(),
                    meta: state.approval_meta.clone(),
                    approved: state.approved_scopes.clone(),
                plan_mode: req.plan_mode,
                tx: tx.clone(),
                cancel: cancel.clone(),
                steer: steer_queue.clone(),
            },
                |ev| match ev {
                    AgentEvent::Content(c) => send(json!({ "content": c })),
                    AgentEvent::Reasoning(r) => send(json!({ "reasoning": r })),
                    AgentEvent::ToolCall { name, args } => {
                        send(json!({ "tool_call": { "name": name, "args": args } }))
                    }
                    AgentEvent::ToolResult { name, ok, preview } => {
                        send(json!({ "tool_result": { "name": name, "ok": ok, "preview": preview } }))
                    }
                    AgentEvent::StepLimit(n) => send(json!({ "step_limit": n })),
                    AgentEvent::ContextUsage { tokens, budget, breakdown } => send(json!({
                        "context_usage": {
                            "tokens": tokens,
                            "budget": budget,
                            "breakdown": {
                                "messages": breakdown.messages,
                                "tools": breakdown.tools,
                                "system": breakdown.system,
                                "other": breakdown.other,
                            },
                        }
                    })),
                    AgentEvent::ApiUsage { prompt_tokens, cached_tokens } => {
                        // P18 钩子（027）：单任务累计 token 预算——超 [hooks] budget_tokens
                        // 触发一次 budget_exceeded 通知挂点（降级策略本身留 P21 成本台账）
                        // P22-C：子代理的花销并进本任务预算（水印增量，防拆子代理绕过预算）
                        let sub_total = subagent::subagent_tokens(&session_id);
                        let sub_fresh =
                            sub_total.saturating_sub(subagent_seen.swap(sub_total, std::sync::atomic::Ordering::Relaxed));
                        let total = task_tokens
                            .fetch_add(prompt_tokens + sub_fresh, std::sync::atomic::Ordering::Relaxed)
                            + prompt_tokens
                            + sub_fresh;
                        let budget = crate::hooks::budget_tokens();
                        if crate::hooks::budget_exceeded(total, budget)
                            && !budget_fired.swap(true, std::sync::atomic::Ordering::Relaxed)
                        {
                            crate::hooks::notify_budget_exceeded(&session_id, total, budget);
                        }
                        send(json!({ "api_usage": { "prompt_tokens": prompt_tokens, "cached_tokens": cached_tokens } }))
                    }
                    // P6 会话落盘：核心循环每关键点推全量消息（不含 system），写盘即续命
                    AgentEvent::Snapshot { messages } => {
                        *last_snapshot.lock().unwrap_or_else(|e| e.into_inner()) = messages.clone();
                        let cur_summary = summary_slot.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        match session.save(&session_id, &messages, cur_summary.as_deref()) {
                            Ok(()) => observability::event(
                                EventType::State,
                                json!({ "kind": "session.saved", "session": session_id, "messages": messages.len() }),
                            ),
                            Err(e) => observability::event(
                                EventType::Error,
                                json!({ "message": format!("会话落盘失败: {e}") }),
                            ),
                        }
                    }
                    // P7 滚动摘要：存槽里，下一次 Snapshot 落盘时随会话一起写 summary 字段
                    AgentEvent::Summary(s) => {
                        *summary_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(s);
                        send(json!({ "summary": true }))
                    }
                    // P6.1 改动统计条：撤销按钮恢复到任务起点基线
                    AgentEvent::Checkpoint { id, files_changed, additions, deletions } => {
                        send(json!({ "checkpoint": { "id": id, "files": files_changed, "additions": additions, "deletions": deletions } }))
                    }
                    // P8 TITLE 角色：标题已生成 → 改会话文件并转前端
                    AgentEvent::Title(t) => {
                        match session.rename(&session_id, &t) {
                            Ok(()) => send(json!({ "title": t })),
                            Err(e) => observability::event(
                                EventType::Error,
                                json!({ "message": format!("会话改名失败: {e}") }),
                            ),
                        }
                    }
                    // P9 B3：Plan 档结构化计划 → 前端计划卡（SSE 增字段；JSONL 由 agent 层记 plan.proposed）
                    AgentEvent::PlanProposal { goal, steps, files } => {
                        send(json!({ "plan_proposal": { "goal": goal, "steps": steps, "files": files } }))
                    }
                    AgentEvent::StepReset { content, reasoning } => {
                        send(json!({ "reset": { "content": content, "reasoning": reasoning } }))
                    }
                    // 025：运行中追加的用户消息已并入本轮 → 前端补用户气泡
                    AgentEvent::Steer { messages } => {
                        send(json!({ "steer": { "messages": messages } }))
                    }
                    AgentEvent::Notice(n) => send(json!({ "notice": n }))
                },
            )
            .await;
        match outcome {
            Ok(LoopOutcome::Done) => {}
            Ok(LoopOutcome::Cancelled) => {
                send(json!({ "cancelled": true }));
                // 用户点了停止：排队的消息一并作废（「停」= 这一轮和还没轮上的话都别跑了）。
                // 先注销占位再清队列（M2 同一顺序）：此后到达的消息会走正常路径自己跑，不会悬空。
                drop(run);
                steer_queue.drain_all(&session_id);
                break;
            }
            Err(e) => {
                // 出错也继续走下面的收尾：已排队的追加消息是用户发出去的话，不能因上一轮
                // 失败被悄悄吞掉（若下一轮同样失败，会再报一次错，语义仍然清楚）
                observability::event(EventType::Error, json!({ "message": e.to_string() }));
                send(json!({ "error": e.to_string() }));
            }
        }
        // M2 收尾顺序：**先注销占位、再取队列**。顺序反过来的话，「入队后没人取」的窗口
        // 就会出现（消息在收尾 drain 之后到达 → 队列里没人消费）。先注销后，这一瞬之后
        // 到达的消息会走正常路径自己起一轮（而不是排队等人取）。
        drop(run);
        // 025/027：一轮跑完后还剩的排队消息（缺省模式，或最后一个决策点之后才到的）→
        // 用最后一轮快照当上下文，紧接着再跑一轮（同一个 SSE 流）：「进了队列的消息一定被处理」。
        let extra = steer_queue.drain_all(&session_id);
        if extra.is_empty() {
            send(json!({ "done": true }));
            break;
        }
        // 还有排队的消息 → 抢回占位自己续跑；抢不到说明已有别的请求起来了，
        // 把它们放回队列（那一轮的收尾会取走——入队与收尾的顺序保证不会悬空）。
        let Some(f) = cancel_registry.try_begin(&session_id) else {
            for t in extra {
                steer_queue.push(&session_id, t, false);
            }
            send(json!({ "done": true }));
            break;
        };
        run = CancelRun::from_flag(cancel_registry.clone(), &session_id, f);
        cancel = run.flag.clone();
        let mut next: Vec<Value> = last_snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if next.is_empty() {
            // 理论上到不了（每轮至少落一次快照）；兜底退回本轮历史，别把整轮丢掉
            next = history.clone();
        }
        observability::event(
            EventType::State,
            json!({ "kind": "steer", "phase": "followup", "session": session_id, "count": extra.len() }),
        );
        let turn_msgs: Vec<Value> =
            extra.iter().map(|t| json!({ "role": "user", "content": t })).collect();
        // 通知前端：这些排队消息现在开始跑了（气泡从「排队中」转成正式消息、开新回答气泡）
        send(json!({ "turn": { "messages": turn_msgs } }));
        next.extend(turn_msgs);
        history = next;
        // 滚动摘要跟着上一轮走（跑的过程中 Summary 事件更新过槽）
        prior_summary = summary_slot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        }
        // P18 钩子（027）：任务结束挂点（Done / Cancelled / 出错都算结束；含续跑轮）
        crate::hooks::session_end(&session_id).await;
    });

    Sse::new(UnboundedReceiverStream::new(rx)).into_response()
}

/// 本轮任务的取消标志守卫（D2）：登记即持标志，Drop 即注销。
/// 任务正常结束、提前 return 或 panic 都会走到 Drop，注册表不留僵尸登记。
/// 无头会话（P22-A）复用同一把守卫——「会话在跑」的登记随任务生命周期。
pub(super) struct CancelRun {
    registry: Arc<CancelRegistry>,
    session: String,
    flag: Arc<CancelFlag>,
}

impl CancelRun {
    /// 接管一个**已登记**的标志（登记在 handler 入口由 `try_begin` 完成，
    /// 这样「拒并发」与「占位」是同一个原子动作）。
    pub(super) fn from_flag(registry: Arc<CancelRegistry>, session_id: &str, flag: Arc<CancelFlag>) -> Self {
        Self { registry, session: session_id.to_string(), flag }
    }
}

impl Drop for CancelRun {
    fn drop(&mut self) {
        self.registry.end(&self.session, &self.flag);
    }
}

#[derive(Debug, Deserialize)]
pub struct CancelQuery {
    /// 目标会话（可空：不带 = 停全部在跑任务，命令行客户端与内置页的兼容语义）
    #[serde(default)]
    pub session: String,
}

/// POST /cancel[?session=<id>] —— 会话级停止：只停指定会话当前这一轮；
/// 不带 session 时停全部在跑任务。
pub(super) async fn cancel(State(state): State<AppState>, Query(q): Query<CancelQuery>) -> Json<Value> {
    let session = q.session.trim();
    if session.is_empty() {
        let count = state.cancel.cancel_all();
        observability::event(
            EventType::State,
            json!({ "kind": "cancel", "scope": "all", "count": count }),
        );
        Json(json!({ "cancelled": true, "scope": "all", "count": count }))
    } else {
        let hit = state.cancel.cancel(session);
        // P22-C：父任务被停 → 级联停它的全部活跃子代理（子代理跑在独立会话，标志互不相通）
        let cascaded = subagent::cancel_children(session);
        observability::event(
            EventType::State,
            json!({ "kind": "cancel", "scope": "session", "session": session, "active": hit, "subagents_cancelled": cascaded }),
        );
        Json(json!({ "cancelled": true, "scope": "session", "session": session, "active": hit, "subagents_cancelled": cascaded }))
    }
}

/// GET /api/cancel[?session=<id>] —— 带 session 查该会话；不带查「是否有在跑会话被取消」。
pub(super) async fn cancel_status(State(state): State<AppState>, Query(q): Query<CancelQuery>) -> Json<Value> {
    let session = q.session.trim();
    if session.is_empty() {
        Json(json!({
            "cancelled": state.cancel.any_cancelled(),
            "active": state.cancel.active_count(),
        }))
    } else {
        Json(json!({
            "cancelled": state.cancel.is_set(session),
            "running": state.cancel.is_active(session),
        }))
    }
}

// —— P10-A（施工单 014）：trace 时间线 ——

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_save_and_decode() {
        let att = MediaAttachment {
            name: "test.mp3".into(),
            mime: "audio/mp3".into(),
            data: "data:audio/mp3;base64,//uQxAAAAAAAAAAAAAAAAAAAAAA==".into(),
        };
        assert_eq!(att.raw_base64(), "//uQxAAAAAAAAAAAAAAAAAAAAAA==");
        use base64::Engine;
        let clean = att.raw_base64().trim().replace(['\r', '\n', ' '], "");
        let unpadded = clean.trim_end_matches('=');
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&clean)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(unpadded))
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(unpadded))
            .unwrap();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn attachments_dir_appended_to_gitignore() {
        // A5（施工单 012）：附件目录自动进工作区 git 排除（只追加，不重写用户文件）
        let dir = std::env::temp_dir().join(format!("agent-a5-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let gi = dir.join(".gitignore");
        std::fs::write(&gi, "target/\nlogs/").unwrap(); // 故意不带结尾换行
        ensure_attachments_ignored(&dir);
        assert_eq!(
            std::fs::read_to_string(&gi).unwrap(),
            "target/\nlogs/\n.attachments/\n",
            "只追加一行并补齐换行"
        );
        // 幂等：再跑一次不重复追加
        ensure_attachments_ignored(&dir);
        let text = std::fs::read_to_string(&gi).unwrap();
        assert_eq!(text.matches(".attachments/").count(), 1, "不得重复追加");
        // 非 git 仓库不动
        let dir2 = std::env::temp_dir().join(format!("agent-a5-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir2).unwrap();
        ensure_attachments_ignored(&dir2);
        assert!(!dir2.join(".gitignore").exists(), "非 git 仓库不落 .gitignore");
    }

    /// P21：角色/降级渠道的 key 回落链（B2）——没单独存 key 的配置靠环境变量兜底，
    /// 否则 budget/summary/title 渠道会一律「API key 未设置」。
    #[test]
    fn resolve_key_falls_back_to_env() {
        use crate::config::settings::{Config, Settings};
        let s = Settings::default();
        let mut c = Config {
            id: "cfg-test".into(),
            name: "无 key 配置".into(),
            provider_id: "deepseek".into(),
            base_url: "http://127.0.0.1:9399".into(),
            model: "m".into(),
            models: Vec::new(),
            key_cipher: None,
            context_length: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            thinking: None,
            thinking_style: None,
            capabilities: Default::default(),
            capabilities_user_set: false,
        };
        // 无 DPAPI key、无环境变量 → 空
        unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
        assert!(resolve_key(&s, &c).is_empty());
        // 环境变量兜底
        unsafe { std::env::set_var("DEEPSEEK_API_KEY", "sk-env-fallback") };
        assert_eq!(resolve_key(&s, &c), "sk-env-fallback");
        // DPAPI key 优先（配了 key 时环境变量只是兜底）
        c.set_key("sk-dpapi").expect("测试配置应能存 key");
        assert_eq!(resolve_key(&s, &c), "sk-dpapi");
        unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
    }
}
