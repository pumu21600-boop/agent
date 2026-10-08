//! 施工单 002：结构模块化 + 聊天 UI 骨架
//!
//! 模块骨架对齐总架构 §17。核心循环、沙箱、工具等依次后置。

mod browser;
mod config;
mod context;
mod core;
mod doctext;
mod fsx;
mod gateway;
mod hooks;
mod jobguard;
mod mcp;
mod memory;
mod observability;
mod plugin;
mod recovery;
mod sandbox;
mod schedule;
mod server;
mod service;
mod session;
mod skills;
mod storage;
mod tools;
mod usage;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let manifest = config::app_root();

    // —— 配置：config.toml（server/logging/gateway 种子）——
    let config = config::load(&manifest.join("config.toml"))?;

    // —— 服务自管理（前置）：start/stop/status/--install/双击智能启动——
    // 接管后直接返回，不跑服务；--minimized 后台子进程与无参服务进程继续往下走。
    let argv: Vec<String> = std::env::args().collect();
    if service::handle(&argv, &config.server.host, config.server.port) {
        return Ok(());
    }

    // —— 供应商预设：providers.json 数据驱动 ——
    gateway::providers::init(&manifest.join("providers.json"))?;

    // —— 日志地基：单会话 session_id，进程重启即换，无任何残留 ——
    let session_id = uuid::Uuid::new_v4().to_string();
    let log_dir = if Path::new(&config.logging.dir).is_absolute() {
        PathBuf::from(&config.logging.dir)
    } else {
        manifest.join(&config.logging.dir)
    };
    observability::init_full(
        &log_dir,
        &config.logging.file,
        session_id.clone(),
        config.logging.max_mb.saturating_mul(1024 * 1024),
        config.logging.keep_archives,
    )?;
    observability::debug(&format!(
        "logger ready: {}",
        log_dir.join(&config.logging.file).display()
    ));

    // —— 设置：settings.json 持久化；首次用 config.toml 的 gateway 节作种子 ——
    let user_settings = config::settings::Settings::load(&config.gateway.base_url, &config.gateway.model);
    crate::mcp::sync_configs(&user_settings.mcp_servers); // 启动同步 MCP server 配置
    // 预连启用中的 MCP server（2026-10-04 用户拍板「装了就装了」）：异步拉起不阻塞启动
    // （npx 首发装包要几十秒），失败静默保持离线、工具调用路径会再试。
    for c in user_settings.mcp_servers.iter().filter(|c| c.enabled) {
        let name = c.name.clone();
        std::thread::spawn(move || {
            let _ = crate::mcp::ensure_online_by_name(&name);
        });
    }
    crate::plugin::wasm::set_plugins_dir(manifest.join("plugins"));
    crate::plugin::wasm::scan_and_load(&manifest.join("plugins")); //  017：启动扫描 WASM 插件
    crate::plugin::wasm::set_disabled(&user_settings.disabled_plugins); //  027：停用名单贯通工具插件
    crate::plugin::ui::set_disabled(&user_settings.disabled_plugins); // 生态轮：同名单贯通插件技能根
    crate::skills::set_disabled(&user_settings.disabled_skills); // 2026-10-02：技能启停名单贯通（index_prompt/composer 过滤）
    crate::plugin::trust::install(&user_settings.trusted_plugins); // 红线3：第三方插件验签（pin digest）
    crate::hooks::install(config.hooks.clone()); //  027：钩子四挂点（config.toml [hooks]）
    // 成本台账：价格表来自 settings.json（由「用量价格」插件的价格设置页维护），
    // 初始化台账时扫一遍装入累计花费（费用已冻结在记录里，直接求和），最后装花费硬闸。
    usage::install_prices(&user_settings.cost.prices);
    usage::init(&log_dir.join("usage"));
    usage::set_cap(config.cost.spend_cap_usd);
    if config.cost.spend_cap_usd > 0.0 {
        observability::debug(&format!(
            "[cost] 全局花费上限已开启: ${:.2}（累计达到即拒绝新模型请求）",
            config.cost.spend_cap_usd
        ));
    }
    observability::event(
        observability::EventType::State,
        serde_json::json!({
            "kind": "startup",
            "session_id": session_id,
            "provider_id": user_settings.provider_id(),
            "base_url": user_settings.effective_base_url(),
            "model": user_settings.effective_model(),
            "host": config.server.host,
            "port": config.server.port,
        }),
    );

    let mut gw = gateway::Gateway::new();
    // 本地限流：[gateway] max_rpm>0 时打开（全渠道共用节流器）
    gw.set_max_rpm(config.gateway.max_rpm);
    if config.gateway.max_rpm > 0 {
        observability::debug(&format!("[gateway] 本地限流已开启: {} rpm", config.gateway.max_rpm));
    }
    let gateway = Arc::new(gw);
    // D2：会话级取消注册表（每轮任务按 session_id 登记自己的取消标志）
    let cancel = Arc::new(core::cancel::CancelRegistry::new());
    // 025：运行中追加消息队列（同会话再发消息 → 并入当前这一轮，不拒收）
    let steer = Arc::new(core::steer::SteerQueue::new());
    let prompt_path = manifest.join("prompts").join("system.md");
    let system_prompt = std::fs::read_to_string(&prompt_path).unwrap_or_else(|_| {
        observability::debug(&format!(
            "警告: 读 {} 失败，使用兜底提示词",
            prompt_path.display()
        ));
        "用中文简洁回答。".to_string()
    });
    let approval_default = config.agent.default_approval.clone();
    // 桌面壳 webview 控制通道（壳连进来后，内置浏览器搬进壳的 child webview；
    // 没有壳时 start_listener 空转，浏览器照旧走无头模式）
    browser::shell::start_listener();
    // 开箱即用：启动时预定位 git.exe（发现链含 PATH/注册表/标准位置/捆绑目录），
    // 并把 git 相关目录登记进沙箱 PATH——shell 工具里按名调 git 也即刻可用
    if let Err(e) = tools::git_exe() {
        observability::debug(&format!("[git] {e}（git 工具将不可用，其余功能不受影响）"));
    }
    // 工作区：settings 里记了就用它（存在才生效），否则工程根
    let ws = match user_settings.workspace.as_deref() {
        Some(p) => {
            let path = std::path::PathBuf::from(p);
            if path.is_dir() {
                tools::set_workspace_root(&path);
                path
            } else {
                // 2026-10-08 修：目录不在（移动盘/卸载/误删）时原来静默降级到 app_root
                // ——fs/shell 工具会静静落到 exe 同级目录（可能往安装目录里写文件），
                // 界面还显示原工作区路径，用户毫无感知。HTTP 端点同场景是硬报错，
                // 两边口径不一致。降级可以（否则起不来），但必须留痕 + 界面说人话。
                observability::event(
                    observability::EventType::State,
                    serde_json::json!({
                        "kind": "workspace.fallback",
                        "missing": p,
                        "fallback": manifest.display().to_string(),
                        "note": "settings 里的工作区目录不存在，本次降级到应用根目录",
                    }),
                );
                eprintln!(
                    "[workspace] settings 里的工作区不存在（{p}），本次降级到应用根目录：{}",
                    manifest.display()
                );
                manifest.clone()
            }
        }
        None => manifest.clone(),
    };
    // 记忆层：双层存储 + index 登记 + AGENTS.md 模板（开箱即用）；工作区切换时热重开
    let mem_cfg = std::sync::Arc::new(config.memory.clone());
    let memory = std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(
        memory::Memory::open(&ws, &mem_cfg),
    )));
    memory.read().unwrap_or_else(|e| e.into_inner()).ensure_agents_template(&ws);
    let state = server::AppState {
        gateway,
        cancel,
        steer,
        settings: {
            // 2026-10-03 agent 自改设置轮：与 settings 工具共用同一份 Arc（工具层不持 AppState）
            let shared = Arc::new(std::sync::RwLock::new(user_settings));
            config::settings::install_shared_settings(shared.clone());
            shared
        },
        // 2026-10-08：settings 写互斥（读-改-写整条链路持锁，见字段注释）
        settings_write: Arc::new(std::sync::Mutex::new(())),
        agent_cfg: Arc::new(config.agent),
        sandbox_cfg: Arc::new(config.sandbox),
        context_cfg: Arc::new(config.context),
        recovery_cfg: Arc::new(config.recovery),
        system_prompt: Arc::new(system_prompt),
        prompt_path: Arc::new(prompt_path),
        session: Arc::new(session::SessionManager::new(&log_dir)),
        // 记忆库索引落在 <log_dir>/memory/library.json（派生缓存）
        log_dir: Arc::new(log_dir.clone()),
        memory,
        mem_cfg,
        approvals: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        // A3：挂起审批的元信息 + 会话内已授权范围——均仅内存，重启即失效
        approval_meta: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        approved_scopes: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        approval_mode: Arc::new(std::sync::RwLock::new(approval_default)),
        // M1：每会话审批档覆盖（并发会话各自独立）
        session_approval: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        // 角色映射与网关级配置（降级链/限流参数）
        roles_cfg: Arc::new(config.roles.clone()),
        gateway_cfg: Arc::new(config.gateway.clone()),
        // 子代理并行度（并发上限/单父任务上限）
        subagent_cfg: Arc::new(config.subagent.clone()),
    };
    // 2026-10-03 agent 自改设置轮：工具层（settings/schedule 等运维动作）经共享句柄触达 AppState
    server::install_shared_state(state.clone());
    let app = server::router(state.clone(), manifest.join("web").join("dist"));
    // 2026-10-05 外部改动监听：agent/用户直接改 settings.json（不走端点）也即时生效
    server::settings::start_external_reload(&state);
    // 定时任务：2s 心跳到到唤醒无头会话（触发最多延迟 2s；每跳只读一次设置）
    schedule::start(state);

    let addr = format!("{}:{}", config.server.host, config.server.port);
    observability::debug(&format!("listening on http://{addr}"));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

