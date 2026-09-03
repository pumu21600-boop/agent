//! 施工单 002（P1）：结构模块化 + 聊天 UI 骨架
//!
//! 模块骨架对齐总架构 §17。核心循环 P2、沙箱 P3、工具 P4 依次后置。

mod config;
mod context;
mod core;
mod gateway;
mod memory;
mod observability;
mod plugin;
mod recovery;
mod sandbox;
mod server;
mod session;
mod tools;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // —— 配置：config.toml（server/logging/gateway 种子）——
    let config = config::load(&manifest.join("config.toml"))?;

    // —— 供应商预设：providers.json 数据驱动 ——
    gateway::providers::init(&manifest.join("providers.json"))?;

    // —— 日志地基：单会话 session_id，进程重启即换，无任何残留 ——
    let session_id = uuid::Uuid::new_v4().to_string();
    let log_dir = if Path::new(&config.logging.dir).is_absolute() {
        PathBuf::from(&config.logging.dir)
    } else {
        manifest.join(&config.logging.dir)
    };
    observability::init(&log_dir, &config.logging.file, session_id.clone())?;
    observability::debug(&format!(
        "logger ready: {}",
        log_dir.join(&config.logging.file).display()
    ));

    // —— 设置：settings.json 持久化；首次用 config.toml 的 gateway 节作种子 ——
    let user_settings = config::settings::Settings::load(&config.gateway.base_url, &config.gateway.model);
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
    // P8 本地限流：[gateway] max_rpm>0 时打开（全渠道共用节流器）
    gw.set_max_rpm(config.gateway.max_rpm);
    if config.gateway.max_rpm > 0 {
        observability::debug(&format!("[gateway] 本地限流已开启: {} rpm", config.gateway.max_rpm));
    }
    let gateway = Arc::new(gw);
    let cancel = Arc::new(core::cancel::CancelFlag::new());
    let prompt_path = manifest.join("prompts").join("system.md");
    let system_prompt = std::fs::read_to_string(&prompt_path).unwrap_or_else(|_| {
        observability::debug(&format!(
            "警告: 读 {} 失败，使用兜底提示词",
            prompt_path.display()
        ));
        "用中文简洁回答。".to_string()
    });
    let approval_default = config.agent.default_approval.clone();
    // P8.2 工作区：settings 里记了就用它（存在才生效），否则工程根
    let ws = match user_settings.workspace.as_deref() {
        Some(p) => {
            let path = std::path::PathBuf::from(p);
            if path.is_dir() {
                tools::set_workspace_root(&path);
                path
            } else {
                manifest.clone()
            }
        }
        None => manifest.clone(),
    };
    // P7 记忆层：双层存储 + index 登记 + AGENTS.md 模板（开箱即用）；工作区切换时热重开
    let mem_cfg = std::sync::Arc::new(config.memory.clone());
    let memory = std::sync::Arc::new(std::sync::RwLock::new(std::sync::Arc::new(
        memory::Memory::open(&ws, &mem_cfg),
    )));
    memory.read().unwrap().ensure_agents_template(&ws);
    let state = server::AppState {
        gateway,
        cancel,
        settings: Arc::new(std::sync::RwLock::new(user_settings)),
        agent_cfg: Arc::new(config.agent),
        sandbox_cfg: Arc::new(config.sandbox),
        context_cfg: Arc::new(config.context),
        recovery_cfg: Arc::new(config.recovery),
        system_prompt: Arc::new(system_prompt),
        session: Arc::new(session::SessionManager::new(&log_dir)),
        memory,
        mem_cfg,
        approvals: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        // P9 A3：挂起审批的元信息 + 会话内已授权范围——均仅内存，重启即失效
        approval_meta: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        approved_scopes: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        approval_mode: Arc::new(std::sync::RwLock::new(approval_default)),
        // P8 角色映射与网关级配置（降级链/限流参数）
        roles_cfg: Arc::new(config.roles.clone()),
        gateway_cfg: Arc::new(config.gateway.clone()),
    };
    let app = server::router(state, manifest.join("web").join("dist"));

    let addr = format!("{}:{}", config.server.host, config.server.port);
    observability::debug(&format!("listening on http://{addr}"));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

