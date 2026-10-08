use super::*;

/// //从配置项 + 供应商预设组装请求参数（采样/思考/端点探测线索/工具开关）。
pub(crate) fn build_chat_params(
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

/// 思考参数形态覆盖——中转/自定义渠道端点探测认不出时显式指定（设置面板控件）。
/// B4 思考形态解析顺序（施工单 012 固化）：`Config.thinking_style` 显式覆盖 >
/// 端点探测（compat.rs：官方域名→native 供应商规范 / 本地→local / 其余→openai 兜底）>
/// providers.json 预设 `thinking_format` 兜底（预设值在 compat::resolve 内作为探测输入，
/// 当前仅豆包 zai/混元 qwen 带值且与探测结果一致）。
/// 词表（none/openai/deepseek/zai/qwen/qwen-chat-template）只增不改名；
/// 新形态入词表必须附真实渠道冒烟记录。none=不发任何思考参数。
pub(crate) fn apply_thinking_style(p: &mut gateway::ChatParams, style: Option<&str>) {
    use gateway::compat::ThinkingFormat as TF;
    match style {
        Some("none") => p.thinking_format = Some(TF::None),
        // 旧词表别名（设置残留）：thinking_object=GLM 式对象 → zai；
        // reasoning_effort=档位 → openai；enable_thinking（_budget）=布尔 → qwen
        Some("thinking_object") => p.thinking_format = Some(TF::Zai),
        Some("reasoning_effort") => p.thinking_format = Some(TF::Openai),
        Some("enable_thinking" | "enable_thinking_budget") => p.thinking_format = Some(TF::Qwen),
        Some(v) => p.thinking_format = TF::parse(v),
        None => {} // auto/未知：跟端点探测
    }
}

/// /把配置名（或配置 id）解析成一条可用渠道。解析失败（名字不存在/地址为空）返回 None。
/// B3 角色解析顺序（施工单 012 固化）：settings.roles.<role> > config.toml [roles] > 回落主渠道；
/// 角色值按「配置名或配置 id」解析（本函数），解析失败回落主渠道并 Notice。
pub(crate) fn find_channel(s: &Settings, name: &str) -> Option<gateway::Channel> {
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
    // （补：此前只有主渠道走这条链，budget/summary/title 渠道会「未设置 key」）
    let key = resolve_key(s, c);
    Some(gateway::Channel::new(&base_url, &model, &key, params))
}

/// B2 key 回落链的角色/降级渠道版：本配置 DPAPI key 优先，缺失回落环境变量。
pub(crate) fn resolve_key(s: &Settings, c: &crate::config::settings::Config) -> String {
    s.config_key(c)
        .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok())
        .unwrap_or_default()
}

/// 审批档位归一化（四档 + 旧档名映射）。返回（生效档, 降级提示）。
/// /api/chat 与无头会话共用——档位语义只有这一处解释。
pub(crate) fn normalize_mode(cfg: &crate::config::AgentConfig, requested: &str) -> (String, Option<String>) {
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
pub(crate) async fn turn_system_prompt(state: &AppState, session_id: &str) -> String {
    let prompt = read_system_prompt(state);
    let agents_md = state
        .memory
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .agents_md();
    // 工作区目录常驻系统提示：用户说「工作区/项目」都指这里，模型据此拼绝对路径，
    // 不再满磁盘 dir 猜位置（真机踩过：shell 工具描述写「项目根」，模型把用户说的
    // 工作区理解成了应用仓库，白搜 6 轮目录才找对文件）
    //
    // 2026-10-05 再补一句「改名 = 改建显示名」：用户说「把工作区名改成 X」时，模型
    // 因为没有现成入口，去 dir 桌面、翻 .agent、读应用源码找接口，白花十几步（真机
    // 实锤）。入口本来就在 settings 工具里，这里点名，别让模型再当侦探。
    let workspace_hint = format!(
        "\n\n【工作区】当前工作区目录：{}\nread_file/write_file/shell 的相对路径、用户提到的工作区文件，都以这个目录为根。\n\
         用户说「把工作区/项目的名字改成 X」= 改它的**显示名**（侧栏那一行字），不是改磁盘目录名：\
         直接调 settings 工具 action=alias 配 name（path 省略=当前工作区；name 留空=恢复文件夹名）。\
         别去动目录名，也别翻应用源码找接口。\n",
        crate::tools::workspace_root().display()
    );
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
    format!(
        "{prompt}{agents_md}{workspace_hint}{project_brief}{}{}{}",
        crate::skills::index_prompt(),
        crate::plugin::ui::index_prompt(),
        selfop_policy(state),
    )
}

/// 自我管理与探索纪律（2026-10-03 用户拍板，与 core/rules 的红线/闸门同一套语义）：
/// 真机实锤过翻车——用户关着「中枢」插件让改审批档位，agent 不知道这属于自身修改、
/// 更不知道审批档位根本没有它的通道，于是 dir/findstr 翻十六步撞上限被砍。这块就堵
/// 这个：先认领请求类型，再查闸/认红线，没有合法通道就直接说，不许自己造路绕。
fn selfop_policy(state: &AppState) -> String {
    let self_on = state.settings.read().map(|s| s.features.self_mod).unwrap_or(false);
    format!(
        "\n\n## 自我管理与探索纪律\n\
        - 改自身（设置/渠道/插件技能启停/定时任务/会话/侧栏面板/边栏/主题/壁纸/终端/设置弹窗）\
         由「中枢」插件管着：它现在{state}。没开时只回一句引导（去 设置→插件 打开「中枢」），\
         不要翻文件系统找替代路径、不要试绕；开了才动手，红线照旧先问。\n\
        - 红线（agent 没有任何通道可改，说了也做不到）：审批档位（read-only只读/auto自动/\
         full-access完全访问三档，用户手动在输入框上方的档位菜单里切；config.toml 的 \
         default_approval 是它的持久形态，改它必问）、API key（key 永不经模型之手）。\
         其余配置（渠道指向/MCP/config.toml/价格表/插件信任）2026-10-04 起全部放开：\
         走 settings/plugin 工具正常改，按审批档位走，改坏了可改回。\n\
        - 探索纪律：翻目录/search 找东西，同一目标两三次找不到就停下来汇报已知情况，\
         禁止反复 dir/findstr 扫全盘；本环境没有的能力直接说没有，列用户可走的路。\n\
        - 步数：本轮没有步数上限，也没有「相同调用自动终止」这类变相限制（2026-10-05 用户拍板：都是变相步数限制，一律不要）——失控只有取消键和花费硬闸两道缰绳管着；\
         长任务按「做实事」分批推进，别把一步拆成十步、别为验证已确认的事反复调工具。\n",
        state = if self_on {
            "【开着】——按当前审批档位正常放行"
        } else {
            "【关着】——任何改自身动作先一句引导用户去开，不许绕路"
        },
    )
}

/// 一轮对话的渠道组装结果（主渠道 + 角色渠道 + 降级链 + 预算降级）。
pub(crate) struct TurnChannels {
    pub(crate) primary: gateway::Channel,
    pub(crate) summary: Option<gateway::Channel>,
    pub(crate) title: Option<gateway::Channel>,
    pub(crate) fallbacks: Vec<gateway::Channel>,
    /// 预算降级渠道（[roles] budget；超 token 预算后剩余步骤改走它）
    pub(crate) downgrade: Option<gateway::Channel>,
    /// 角色渠道解析失败等提示（转发给调用方展示，不静默）
    pub(crate) notices: Vec<String>,
}

/// 组装一轮对话的全部渠道。请求级覆盖优先（页面当前选择即时生效，不等保存）。
/// /api/chat 与无头会话共用——角色解析顺序（B3）与 key 回落链（B2）只有这一处实现。
pub(crate) fn assemble_turn_channels(
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
    // / 角色映射：优先 settings.roles，未设回落 config.toml [roles]；
    // features.roles=false（插件页「功能桥接」关）= 整条路由链跳过，全部走主渠道/默认退化
    let roles = if s.features.roles { s.roles.as_ref() } else { None };
    let cfg_roles = if s.features.roles { Some(&state.roles_cfg) } else { None };
    let summary_name = roles
        .and_then(|r| r.summary.as_deref())
        .or_else(|| cfg_roles.and_then(|c| c.summary.as_deref()));
    let title_name = roles
        .and_then(|r| r.title.as_deref())
        .or_else(|| cfg_roles.and_then(|c| c.title.as_deref()));
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
    // / 降级链：优先 settings.roles.fallbacks，未设回落 [gateway].fallback_configs
    let fallback_names: Vec<String> = roles
        .map(|r| r.fallbacks.clone())
        .filter(|f| !f.is_empty())
        .unwrap_or_else(|| state.gateway_cfg.fallback_configs.clone());
    let fallbacks: Vec<gateway::Channel> = fallback_names
        .iter()
        .filter_map(|n| find_channel(s, n))
        .collect();
    // 预算降级渠道
    let budget_name = roles
        .and_then(|r| r.budget.as_deref())
        .or_else(|| cfg_roles.and_then(|c| c.budget.as_deref()));
    let downgrade = budget_name.and_then(|n| find_channel(s, n));
    if let Some(n) = budget_name
        && downgrade.is_none()
    {
        notices.push(format!(
            "角色「预算降级」配置的渠道「{n}」解析失败（配置名不存在或地址为空），超预算后维持主渠道"
        ));
    }
    // 思考参数自适应降级登记：该端点此前 400 拒收过思考字段（剥参重试成功），
    // 本轮起明确告知用户，不静默
    if state.gateway.thinking_is_degraded(&base_url, &model) {
        notices.push("该端点此前拒绝了思考参数，已自动关闭思考下发（对话正常进行）".to_string());
    }
    TurnChannels { primary, summary, title, fallbacks, downgrade, notices }
}

/// 检查模型是否具备指定模态的输入能力：只认用户手动勾选的能力标记，
/// 不再按模型名智能推导（聚合渠道上名字与真实能力对不上）。
/// 此前 accept_modality 的按名推导兜底已删：专家配置解析失败时按「未勾选」处理，
/// A5（施工单 012）：媒体附件落在 <workspace>/.attachments/，若工作区是 git 仓库且
/// .gitignore 未含该目录，自动追加一行 `.attachments/`（只追加，绝不重写用户文件），
/// 防止用户 `git add .` 把二进制附件提交进历史。
pub(crate) fn ensure_attachments_ignored(ws: &std::path::Path) {
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

