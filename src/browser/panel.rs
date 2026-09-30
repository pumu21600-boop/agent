use super::*;

/// 面板默认视口尺寸（launch 的 --window-size 一致；前端按比例换算点击坐标）。
pub const VIEWPORT_W: i64 = 1280;
pub const VIEWPORT_H: i64 = 900;

pub(crate) fn check_panel_url(url: &str) -> Result<(), BrowserError> {
    check_url(url)
}

/// 面板状态：浏览器是否在跑 + 全部标签页（含可见/加载/挂起）+ 可见页弹窗。
pub async fn panel_state() -> Value {
    let mut guard = SESSION.lock().await;
    let Some(s) = guard.as_ref() else {
        return json!({ "running": false, "tabs": [], "dialog": null, "mode": "headless" });
    };
    let mode = mode_name(s.mode);
    if !browser_alive(s.port).await {
        // 端口没了（壳关了/浏览器进程死了）：会话整体作废——下次动作会自动重建
        log("browser.stale", json!({}));
        if let Some(dead) = guard.take() {
            kill_child(&dead);
        }
        return json!({ "running": false, "tabs": [], "dialog": null, "mode": mode });
    }
    // 端口活着也要摘僵尸 tab：壳重启后 9223 复生，但旧连接的 actor 早退了
    let _ = reap_dead_tabs(&mut guard).await;
    let Some(s) = guard.as_ref() else {
        return json!({ "running": false, "tabs": [], "dialog": null, "mode": "headless" });
    };
    let mut briefs = s.tab_briefs().await;
    let visible = s.visible();
    // loading / dialog 只对可见页细算（其余页不值得花 CDP 往返）
    let mut dialog = Value::Null;
    if let Some(vid) = &visible {
        let tab = s
            .tabs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|t| &t.id == vid)
            .cloned();
        if let Some(tab) = tab {
            // 弹窗开着时先报弹窗（页面脚本阻塞，readyState 求值会白等超时）
            if let Some(d) = tab.dialog.lock().await.clone() {
                dialog = json!({ "kind": d.kind, "message": d.message, "defaultPrompt": d.default_prompt });
            } else if let Ok(ready) = tab.cdp.evaluate_in_isolated_world(ISOLATED_WORLD, "document.readyState").await
                && let Some(st) = ready.as_str()
            {
                for b in briefs.iter_mut() {
                    if b.id == tab.id {
                        b.loading = st != "complete";
                    }
                }
            }
        }
    }
    json!({
        "running": true,
        "mode": mode_name(s.mode),
        "port": s.port,
        "tabs": briefs.iter().map(TabBrief::to_json).collect::<Vec<_>>(),
        "dialog": dialog,
        // agent 工具最近一次碰浏览器的时间戳（0 = 没有过）——前端据此自动唤起面板
        "agentTouched": s.agent_touched_ms.load(Ordering::Relaxed),
        "agentTab": s.agent_tab_now(),
    })
}

/// 宿主模式名（透给前端：shell = 壳里托管真 webview，headless = 截图帧流）。
pub(crate) fn mode_name(mode: HostMode) -> &'static str {
    match mode {
        HostMode::Shell => "shell",
        HostMode::Headless => "headless",
    }
}

/// 面板开新标签页（可选直接导航）。`native` = 调用方就是桌面壳里的那个客户端
/// （见 BrowserSession::panel_showing 的注释）——只有它能置「面板在看」。
pub async fn panel_new(url: Option<String>, native: bool) -> Result<Value, String> {
    let url = url.unwrap_or_else(|| "about:blank".into());
    if url != "about:blank" {
        check_panel_url(&url).map_err(|e| e.to_string())?;
    }
    let session = ensure_session().await?;
    session.touch();
    let tab = create_tab(&session, &url, false).await?;
    session.set_visible(&tab.id);
    if native {
        session.set_shell_tab(&tab.id);
        session.set_showing(true); // 面板在建页 = 人在看
    }
    sweep_session(&session).await;
    Ok(json!({ "id": tab.id }))
}

/// 面板导航（地址栏回车）。`native` 见 `panel_viewport`：只有壳里那个客户端的导航
/// 该把壳里的 webview 显示出来。
pub async fn panel_navigate(id: &str, url: &str, native: bool) -> Result<Value, String> {
    check_panel_url(url).map_err(|e| e.to_string())?;
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    if native {
        session.set_shell_tab(id);
        session.set_showing(true);
    }
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    match navigate_tab(&tab, url).await {
        Ok(_) => {
            sweep_session(&session).await;
            // 落点检查：面板用户手输的 URL 停在错误页时，红字给出人话 + 代理诊断
            if let Some(msg) = chrome_error_note(&tab).await {
                return Err(msg);
            }
            Ok(json!({ "ok": true }))
        }
        Err(e) => {
            if e.contains("连接已断开") {
                close_internal("dead").await;
                return Err(format!("导航失败: {}", friendly_backend_error(&e)));
            }
            // 失败也先查落点（CDP 超时时 Chromium 往往还在连接，错误页稍后才落地）
            let msg = chrome_error_note_wait(&tab, 12_000)
                .await
                .unwrap_or_else(|| friendly_backend_error(&e));
            Err(format!("导航失败: {msg}"))
        }
    }
}

/// 面板截图：PNG 字节（旧路径保留；帧流不可用时回落）。
pub async fn panel_screenshot(id: &str) -> Result<Vec<u8>, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    // 弹窗开着时页面脚本阻塞，readyState 求值会白等超时——直接跳过等待
    let dialog_open = tab.dialog.lock().await.is_some();
    for _ in 0..4 {
        if dialog_open {
            break;
        }
        let st = tab
            .cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": "document.readyState", "returnByValue": true }),
            )
            .await
            .ok()
            .and_then(|v| v.get("result").and_then(|r| r.get("value")).and_then(Value::as_str).map(str::to_string));
        match st.as_deref() {
            Some("loading") => tokio::time::sleep(Duration::from_millis(200)).await,
            _ => break,
        }
    }
    let res = tab.cdp.call("Page.captureScreenshot", json!({ "format": "png" })).await?;
    let data = res
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| "截图结果缺 data".to_string())?;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("截图 base64 解码失败: {e}"))
}

/// 面板帧（P21）：screencast 最新 JPEG（内存直出，零 CDP 往返）；
/// 没有帧（screencast 刚起步/失败）时回落 captureScreenshot（PNG）。
/// P22：轮询即「有客户端要帧」的信号，驱动 screencast 门控（壳里看真页面不编码）。
/// `native` = 调用方就是壳里那个客户端：轮询帧对它来说是"壳里看真页面"，本来就不该
/// 因此把壳的 webview 显示出来（显示只归壳面板自己的 activate/viewport 管）。
pub async fn panel_frame(id: &str, native: bool) -> Result<(Vec<u8>, &'static str), String> {
    let session = current_session().await?;
    session.touch();
    session.last_frame_poll_ms.store(now_ms(), Ordering::Relaxed);
    session.set_visible(id);
    if native {
        session.set_shell_tab(id);
        session.set_showing(true); // 轮询帧 = 有客户端在面板里看
    }
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    sweep_session(&session).await; // 可见页 screencast 拉起；首帧到达前走下面的回落
    if let Some(f) = tab.frame.lock().await.clone() {
        return Ok((f.data, "image/jpeg"));
    }
    Ok((panel_screenshot(id).await?, "image/png"))
}

/// 面板设置标签页视口（截图尺寸跟随面板大小——固定 1280×900 会在窄面板里
/// 显示得又小又留大黑边，ZCode 的面板浏览器就是视口=面板尺寸）。P22：x/y 是面板
/// 内容区在壳窗口里的左上角（shell 模式据此把 child webview 摆到面板位置上）。
/// `native` = 调用方就是壳里那个客户端（见 BrowserSession::panel_showing 的注释）——
/// **只有它的 x/y 是壳窗口坐标**。浏览器直开 5173 的第二个客户端也走这个接口（截图要跟
/// 它自己的面板尺寸），但它的 x/y 是那个浏览器窗口的坐标：拿来挪壳里的 webview，页面就
/// 整体漂到别的位置上去（用户实锤「界面卡位」：壳里网页浮在错误位置、盖住聊天列右侧）。
pub async fn panel_viewport(id: &str, x: i64, y: i64, w: i64, h: i64, native: bool) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    if native {
        session.set_shell_tab(id);
        session.set_showing(true);
    }
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    let w = w.clamp(320, 1600);
    let h = h.clamp(400, 2400);
    // shell 模式跳过 Emulation 覆盖：壳里 webview 的 HWND 尺寸就是视口，页面按新尺寸
    // 自己重排，CDP 覆盖既多余又每次多一个往返（拖面板宽度时 100ms 一同步，能省则省）
    if session.mode != HostMode::Shell {
        apply_viewport_override(&tab, w, h).await?;
    }
    *tab.viewport.lock().await = Some((w, h));
    // shell 模式：把壳里的 webview 挪到面板内容区（用户看到的就是 CDP 驱动的那个页面）。
    // 矩形一到就立即同步显隐——不然要等最多 5s 的巡检，面板刚打开那会儿会白一片。
    // 非壳客户端（浏览器直开 5173）的矩形只用于它自己的截图尺寸，**不碰壳的 webview**。
    if session.mode == HostMode::Shell
        && native
        && let Some(label) = tab.shell_label.as_deref()
    {
        // 合理范围门槛（只挡真垃圾，不挡合法布局）：旧版这里还有 y>=100——按「web 标签条
        // 常在」的旧布局写死（rp-tabs 44 + web-tabbar 44 + toolbar 39 ≈ 125）。但标签条是
        // {#if webTabs.length} 条件渲染：会话无标签页时整行消失，真实内容顶掉到 ~90——
        // 正确矩形被当成「量早了」全部拒收，webview 冻结在旧矩形上（盖进聊天列、顶在
        // 工具栏下 35px，2026-09-27 用户截图实锤）。「量早了」的瞬态改由前端就绪门挡
        // （状态未加载前不上报），这里只挡越界。
        if !(x >= 0 && y >= 0 && w >= 200 && h >= 200) {
            return Ok(json!({ "ok": true, "skipped": "rect-insane", "w": w, "h": h }));
        }
        *session.last_rect.lock().unwrap_or_else(|e| e.into_inner()) = Some((x, y, w, h));
        shell::rect(label, x as f64, y as f64, w as f64, h as f64);
        let tabs = session.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let shell_tab = session.shell_tab();
        sync_shell_visibility(&session, &tabs, &shell_tab).await;
    }
    Ok(json!({ "ok": true, "w": w, "h": h }))
}

/// 用系统默认浏览器打开一个地址（explorer.exe 转发，URL 里的 & 不会被 cmd 吃掉）。
pub(crate) fn open_url_in_system(url: &str) -> Result<Value, String> {
    Command::new("explorer")
        .arg(url)
        .creation_flags(0x0800_0000)
        .spawn()
        .map_err(|e| format!("打开系统浏览器失败: {e}"))?;
    Ok(json!({ "ok": true }))
}

/// 「在默认浏览器中打开」当前标签页。
pub async fn panel_open_external(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    let url = tab
        .cdp
        .evaluate_in_isolated_world(ISOLATED_WORLD, "location.href")
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    if url.is_empty() || url == "about:blank" {
        return Err("当前是空白页，没有可打开的地址".into());
    }
    open_url_in_system(&url)
}

/// 「打开调试工具」：打开 CDP 自带的调试端点页（`http://127.0.0.1:端口/json`）——
/// 页面里每个标签的 inspect 链接就是真正的 DevTools（Chromium 官方远程调试流程）。
/// 不直接用 /json/list 返回的 devtoolsFrontendUrl：Edge 新版把它指向 aka.ms 短链，
/// 该短链已失效（302 跳必应，真机踩过）；devtools:// scheme 不是系统注册 scheme，
/// 外链起不来。
pub async fn panel_devtools_url(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    // shell 模式：child webview 支持真 DevTools——直接指挥壳开（无头时代做不到的事）
    if session.mode == HostMode::Shell {
        let Some(label) = tab.shell_label.as_deref() else {
            return Err("该标签页不在桌面壳里（无头模式）".into());
        };
        shell::devtools(label, true);
        return Ok(json!({ "ok": true, "opened": true }));
    }
    Ok(json!({
        "ok": true,
        "opened": false,
        "hint": "无头模式的页面没有真 DevTools；点「在默认浏览器中打开」后按 F12"
    }))
}

/// 面板点击（视口坐标）→ 等可能的导航沉降。
pub async fn panel_click(id: &str, x: i64, y: i64) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    click_and_settle(&tab, x, y, MouseButton::Left, 0)
        .await
        .map_err(|e| format!("点击失败: {e}"))?;
    Ok(json!({ "ok": true }))
}

/// 面板输入：向当前聚焦元素插文本（点过输入框后用）。
pub async fn panel_type(id: &str, text: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    if !text.is_empty() {
        tab.cdp.call("Input.insertText", json!({ "text": text })).await?;
    }
    Ok(json!({ "ok": true }))
}

/// 面板按键（如 Enter 提交搜索）。
pub async fn panel_key(id: &str, key: &str, code: i64) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    for t in ["rawKeyDown", "keyUp"] {
        tab.cdp
            .call(
                "Input.dispatchKeyEvent",
                json!({ "type": t, "key": key, "windowsVirtualKeyCode": code }),
            )
            .await?;
    }
    Ok(json!({ "ok": true }))
}

/// 面板滚轮。
pub async fn panel_scroll(id: &str, delta_y: i64) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    tab.cdp
        .call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseWheel", "x": VIEWPORT_W / 2, "y": VIEWPORT_H / 2, "deltaX": 0, "deltaY": delta_y }),
        )
        .await?;
    Ok(json!({ "ok": true }))
}

pub async fn panel_back(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    history_go(&tab, -1).await.map_err(|e| e.to_string())?;
    tab.cdp.invalidate_worlds();
    Ok(json!({ "ok": true }))
}

pub async fn panel_forward(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    history_go(&tab, 1).await.map_err(|e| e.to_string())?;
    tab.cdp.invalidate_worlds();
    Ok(json!({ "ok": true }))
}

pub async fn panel_reload(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    session.set_visible(id);
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    tab.cdp.call("Page.reload", json!({})).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    tab.cdp.invalidate_worlds();
    Ok(json!({ "ok": true }))
}

/// 面板关标签页；全部关完浏览器进程自然退出。
pub async fn panel_close_tab(id: &str) -> Result<Value, String> {
    let session = current_session().await?;
    close_tab(&session, id).await;
    Ok(json!({ "ok": true }))
}

/// 「⋯」菜单开关的**服务端唯一真相源**。壳模式菜单是壳里的浮层 webview，它的关闭
/// （点菜单项/backdrop/Esc）由浮层页面自己发请求，前端主界面未必知道——所以状态记在
/// 服务端：前端的「⋯」点击一律发 toggle，由这里翻转，两边不会失同步。
pub(crate) static MENU_OPEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 「⋯」菜单开合（P20.4 补丁四）。shell 模式下开菜单 = 让壳建一个**透明置顶的菜单
/// webview** 铺在面板矩形上（`shell::menu`）：它压在下层内容 webview 之上，菜单因此浮在
/// 网页上面——**网页纹丝不动、不被遮也不被藏**（对齐 ZCode；主界面的 HTML 菜单做不到，
/// 因为 child webview 是 HWND，永远盖在 Svelte 之上）。菜单页由服务同源提供
/// （/menu.html，锚点走 URL query），点击经同源 fetch 调 open-external/devtools 端点。
/// 无头模式没有壳：菜单是前端 HTML 下拉（盖在截图上，z-index 正常），这里只记账。
pub async fn panel_overlay(
    open: bool,
    toggle: bool,
    panel: Option<(f64, f64, f64, f64)>,
    anchor: Option<(f64, f64, f64, f64)>,
) -> Result<Value, String> {
    let open = if toggle {
        !MENU_OPEN.load(std::sync::atomic::Ordering::Relaxed)
    } else {
        open
    };
    MENU_OPEN.store(open, std::sync::atomic::Ordering::Relaxed);
    if !open {
        shell::menu_close();
        return Ok(json!({ "ok": true, "open": false }));
    }
    let (Some(panel), Some(anchor)) = (panel, anchor) else {
        MENU_OPEN.store(false, std::sync::atomic::Ordering::Relaxed);
        return Err("开菜单需要面板矩形与按钮锚点".into());
    };
    let session = current_session().await?;
    session.touch();
    if session.mode != HostMode::Shell {
        return Ok(json!({ "ok": true, "open": true, "shell": false }));
    }
    // 菜单项要操作的那一页：壳里正显示的那一页（没有就空——页面会禁用菜单项）
    let tab = session.shell_tab().unwrap_or_default();
    shell::menu(panel.0, panel.1, panel.2, panel.3, anchor, &tab);
    Ok(json!({ "ok": true, "open": true, "shell": true, "tab": tab }))
}

/// 面板上报可见标签页（驱动 screencast 与驻留；对齐 ZCode activateTab）。
/// 空 id = 「面板不看了」——shell 模式下这是壳里 webview 隐藏的唯一信号。
/// `native` = 调用方就是壳里那个客户端：只有它的「在看/不看」能置壳里 webview 的显隐
/// （浏览器直开 5173 的第二个客户端切走页签，不该把壳里的网页一起藏了）。
pub async fn panel_activate(id: &str, native: bool) -> Result<Value, String> {
    // 「面板不看了」在根本没开浏览器时是 no-op：current_session 失败也别 400——
    // 前端关面板/切走页签都会发空 id，没有网页可藏就是成功
    if id.is_empty() {
        if let Ok(session) = current_session().await {
            session.touch();
            session.clear_visible();
            if native {
                session.clear_shell_tab();
                session.set_showing(false);
            }
            sweep_session(&session).await;
        }
        return Ok(json!({ "ok": true }));
    }
    let session = current_session().await?;
    session.touch();
    // 菜单开着时激活/切页签 = 浮层锚定的那一页失效了，顺手关掉壳里浮层（幂等）
    if MENU_OPEN.swap(false, std::sync::atomic::Ordering::Relaxed) {
        shell::menu_close();
    }
    let tab = get_tab(&session, id)?;
    ensure_live(&tab).await?;
    session.set_visible(id);
    if native {
        session.set_shell_tab(id);
        session.set_showing(true);
    }
    sweep_session(&session).await;
    Ok(json!({ "ok": true }))
}

/// 面板处理 JS 弹窗（页面被 alert/confirm 卡住时，用户直接在这里确定/取消）。
pub async fn panel_dialog(id: &str, accept: bool, prompt_text: Option<String>) -> Result<Value, String> {
    let session = current_session().await?;
    session.touch();
    let tab = get_tab(&session, id)?;
    if tab.dialog.lock().await.is_none() {
        return Err("当前没有弹窗".into());
    }
    let mut params = json!({ "accept": accept });
    if let Some(t) = prompt_text {
        params["promptText"] = json!(t);
    }
    tab.cdp.call("Page.handleJavaScriptDialog", params).await?;
    *tab.dialog.lock().await = None;
    Ok(json!({ "ok": true }))
}

