//! Agent 桌面壳（P13，施工单 020）—— 窗口 + 托盘 + 服务保活
//!
//! 设计（窄而刚 v0）：
//! - WebView 直接加载 http://127.0.0.1:8321（前端零改动，agent.exe 是唯一真源）；
//! - 启动时探测 8321，未起则 spawn `<repo>/target/release/agent.exe`（start.bat 内置化）；
//! - 托盘：左键显示/聚焦窗口，右键菜单（打开主窗口 / 退出壳）；
//! - 壳退出**不杀**服务（服务独立生命周期）；
//! - agent.exe 路径按开发机布局解析（desktop/ 的上两级 target/release）；
//!   未来打包分发时改为同级目录探测（留口）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use tauri::Manager;

const SERVICE_URL: &str = "127.0.0.1:8321";

/// 服务 exe 路径：desktop/ 的上两级 target\release\agent.exe（开发机布局）。
fn service_exe() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|repo| repo.join("target").join("release").join("agent.exe"))
        .unwrap_or_else(|| PathBuf::from("target/release/agent.exe"))
}

fn service_alive() -> bool {
    std::net::TcpStream::connect(SERVICE_URL).is_ok()
}

/// 探测服务，未起则后台拉起（CREATE_NO_WINDOW，不随壳退出）。
fn ensure_service() -> Option<u32> {
    if service_alive() {
        println!("[desktop] 服务已在运行");
        return None;
    }
    let exe = service_exe();
    if !exe.exists() {
        eprintln!(
            "[desktop] 服务未运行且未找到 {}（先在项目根 cargo build --release）",
            exe.display()
        );
        return None;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        match std::process::Command::new(&exe)
            .current_dir(exe.parent().map(|p| p.parent().unwrap_or(p)).unwrap_or(Path::new(".")))
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn()
        {
            Ok(child) => {
                println!("[desktop] 已拉起服务 pid={}", child.id());
                Some(child.id())
            }
            Err(e) => {
                eprintln!("[desktop] 拉起服务失败: {e}");
                None
            }
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

use std::path::Path;

fn main() {
    // 服务保活先于窗口（窗口加载 8321，服务没起会白屏）
    ensure_service();

    tauri::Builder::default()
        .setup(|app| {
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

            let open = MenuItem::with_id(app, "open", "打开主窗口", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出壳（服务保持运行）", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;

            let _tray = TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().expect("缺窗口图标").clone())
                .tooltip("Agent（127.0.0.1:8321）")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| match ev.id.as_ref() {
                    "open" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = ev
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                })
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Agent 桌面壳启动失败");
}
