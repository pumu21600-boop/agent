//! 施工单 001：日志地基 + 网关冒烟 + 取消按钮
//! 后续扩展：供应商预设 + 设置持久化（settings.json，key 走 DPAPI）。

mod cancel;
mod config;
mod gateway;
mod logging;
mod providers;
mod server;
mod settings;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // —— 配置：config.toml（server/logging/gateway 种子）——
    let config = config::load(&manifest.join("config.toml"))?;

    // —— 日志地基：单会话 session_id，进程重启即换，无任何残留 ——
    let session_id = uuid::Uuid::new_v4().to_string();
    let log_dir = if Path::new(&config.logging.dir).is_absolute() {
        PathBuf::from(&config.logging.dir)
    } else {
        manifest.join(&config.logging.dir)
    };
    logging::init(&log_dir, &config.logging.file, session_id.clone())?;
    logging::debug(&format!(
        "logger ready: {}",
        log_dir.join(&config.logging.file).display()
    ));

    // —— 设置：settings.json 持久化；首次用 config.toml 的 gateway 节作种子 ——
    let user_settings = settings::Settings::load(&config.gateway.base_url, &config.gateway.model);
    logging::event(
        logging::EventType::State,
        serde_json::json!({
            "kind": "startup",
            "session_id": session_id,
            "provider_id": user_settings.provider_id,
            "base_url": user_settings.effective_base_url(),
            "model": user_settings.effective_model(),
            "host": config.server.host,
            "port": config.server.port,
        }),
    );

    let gateway = Arc::new(gateway::Gateway::new());
    let cancel = Arc::new(cancel::CancelFlag::new());
    let state = server::AppState {
        gateway,
        cancel,
        settings: Arc::new(std::sync::RwLock::new(user_settings)),
    };
    let app = server::router(state, manifest.join("frontend").join("dist"));

    let addr = format!("{}:{}", config.server.host, config.server.port);
    logging::debug(&format!("listening on http://{addr}"));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
