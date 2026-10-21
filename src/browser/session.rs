use super::*;

/// 杀掉浏览器进程并回收（Arc 里的 Child 需经 Mutex 拿可变引用；shell 模式无进程）。
/// job 字段的存在意义就是把保险丝持有到会话结束（Drop = TerminateJobObject 团灭整树），
/// 这里读它一面上日志、一面让"字段没被读到"的告警闭嘴。
pub(crate) fn kill_child(s: &BrowserSession) {
    if let Some(c) = s.child.as_ref()
        && let Ok(mut c) = c.lock()
    {
        let _ = c.kill();
        let _ = c.wait();
    }
    if s.job.is_some() {
        log("browser.job.reap", json!({}));
    }
}

/// 清理别的进程留下的旧浏览器 profile（名字前缀相同、不属于本进程）。
/// 被占用的目录（残留浏览器还活着）删不动就跳过，错误一律忽略。
pub(crate) fn clean_stale_profiles(keep: &PathBuf) {
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for e in entries.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("agent-browser-profile") {
                let p = e.path();
                if p != *keep {
                    let _ = std::fs::remove_dir_all(&p);
                }
            }
        }
    }
}

// —— 浏览器发现与拉起 ——

/// 收集 WebView2 运行时目录下的 msedge.exe（真机踩过：用户机器**没装完整版
/// Edge/Chrome**，界面本身跑在 WebView2 里——运行时在
/// `Program Files (x86)\Microsoft\EdgeCore\<版本>\msedge.exe`，同样能
/// `--headless=new --remote-debugging-port` 起无头 CDP（实测 HeadlessChrome/153 OK）。
/// EdgeWebKit\Application\<版本> 是老布局，一并扫。只收数字开头的版本目录
/// （EdgeCore\Optimized 是占位副本，不是完整浏览器）。
pub(crate) fn webview2_runtime_exes(pf: &str, pf86: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for base in [pf86, pf] {
        for prefix in ["Microsoft\\EdgeCore", "Microsoft\\EdgeWebKit\\Application"] {
            let dir = PathBuf::from(base).join(prefix);
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            let mut found: Vec<PathBuf> = entries
                .flatten()
                .filter(|e| {
                    e.path().is_dir()
                        && e.file_name().to_string_lossy().starts_with(|c: char| c.is_ascii_digit())
                })
                .map(|e| e.path().join("msedge.exe"))
                .filter(|p| p.is_file())
                .collect();
            found.sort();
            out.extend(found);
        }
    }
    out
}

pub(crate) fn find_browser_exe() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("AGENT_BROWSER_PATH")
        && !p.trim().is_empty()
    {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
    }
    // 环境变量可能整体缺失（如从某些最小环境里拉起服务）——回落硬编码常见路径
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into());
    let pf86 = std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| "C:\\Program Files (x86)".into());
    let lf = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let mut candidates: Vec<PathBuf> = vec![
        format!("{pf86}\\Microsoft\\Edge\\Application\\msedge.exe").into(),
        format!("{pf}\\Microsoft\\Edge\\Application\\msedge.exe").into(),
        format!("{pf}\\Google\\Chrome\\Application\\chrome.exe").into(),
        format!("{pf86}\\Google\\Chrome\\Application\\chrome.exe").into(),
        format!("{lf}\\Google\\Chrome\\Application\\chrome.exe").into(),
    ];
    // WebView2 运行时殿后（没装完整版浏览器时的唯一 Chromium 来源）
    candidates.extend(webview2_runtime_exes(&pf, &pf86));
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            "找不到 Edge/Chrome/WebView2 运行时（设 AGENT_BROWSER_PATH 指到浏览器 exe 可用）".to_string()
        })
}

/// CDP HTTP 端点客户端（环回、no_proxy——坑 17：本机代理环境变量会拦环回请求）。
pub(crate) fn dev_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))
}

/// 探测 CDP HTTP 端点，拿到页面 target 的 (id, ws 地址)。
pub(crate) async fn wait_page_target(port: u16, budget: Duration) -> Result<(String, String), String> {
    let client = dev_client()?;
    let deadline = tokio::time::Instant::now() + budget;
    let url = format!("http://127.0.0.1:{port}/json/list");
    while tokio::time::Instant::now() < deadline {
        if let Ok(resp) = client.get(&url).send().await
            && let Ok(text) = resp.text().await
            && let Ok(v) = serde_json::from_str::<Value>(&text)
            && let Some(targets) = v.as_array()
            && let Some(page) = targets
                .iter()
                .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
            && let Some(id) = page.get("id").and_then(Value::as_str)
            && let Some(ws) = page.get("webSocketDebuggerUrl").and_then(Value::as_str)
        {
            return Ok((id.to_string(), ws.to_string()));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err("等待浏览器 CDP 端点超时（10s 内 /json/list 无 page target）".into())
}

/// 浏览器进程是否还活着（/json/version 可达）。
pub(crate) async fn browser_alive(port: u16) -> bool {
    if let Ok(c) = dev_client() {
        return c
            .get(format!("http://127.0.0.1:{port}/json/version"))
            .send()
            .await
            .is_ok();
    }
    false
}

/// 经 CDP HTTP 端点开一个新标签页（PUT /json/new；老版本只认 GET，失败回退）。
/// 固定 about:blank 起步，导航统一走 CDP（能等加载完成）。
pub(crate) async fn dev_new_target(port: u16) -> Result<(String, String), String> {
    let client = dev_client()?;
    for method in ["PUT", "GET"] {
        let u = format!("http://127.0.0.1:{port}/json/new?about:blank");
        let req = if method == "PUT" { client.put(&u) } else { client.get(&u) };
        if let Ok(resp) = req.send().await
            && let Ok(text) = resp.text().await
            && let Ok(v) = serde_json::from_str::<Value>(&text)
            && let Some(id) = v.get("id").and_then(Value::as_str)
            && let Some(ws) = v.get("webSocketDebuggerUrl").and_then(Value::as_str)
        {
            return Ok((id.to_string(), ws.to_string()));
        }
    }
    Err("无法创建标签页（/json/new 不可用）".into())
}

/// 关闭标签页（GET /json/close）。
pub(crate) async fn dev_close_target(port: u16, id: &str) {
    if let Ok(c) = dev_client() {
        let _ = c.get(format!("http://127.0.0.1:{port}/json/close/{id}")).send().await;
    }
}

/// 建一条标签页连接（Page.enable + 可选导航等待加载完成）并挂进会话。
pub(crate) async fn create_tab(session: &BrowserSession, url: &str, agent: bool) -> Result<Arc<WebTab>, String> {
    let (id, ws, shell_label) = match session.mode {
        // shell 模式一律 about:blank 起步：WebView2 的 child webview 带初始 URL 创建时，
        // 导航一提交 CDP target 就被换掉（实测 Edg/153：about:blank target 上线 180ms 后
        // 随导航消失），先建空白页再走 CDP Page.navigate 才能稳住 target
        HostMode::Shell => shell_create_target(session).await?,
        HostMode::Headless => {
            let (id, ws) = dev_new_target(session.port).await?;
            (id, ws, None)
        }
    };
    let cdp = cdp::Cdp::connect(&ws).await?;
    cdp.call("Page.enable", json!({})).await?;
    let tab = WebTab::new(id, agent, cdp, shell_label);
    spawn_tab_events(tab.clone());
    session
        .tabs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(tab.clone());
    if url != "about:blank" && !url.is_empty() {
        if let Err(nav_err) = navigate_tab(&tab, url).await {
            if nav_err.contains("连接已断开") {
                return Err(friendly_backend_error(&nav_err));
            }
            // 导航失败（CDP 超时/事件超时）：等错误页落地，落点诊断比原始错误有用
            return Err(chrome_error_note_wait(&tab, 12_000)
                .await
                .unwrap_or_else(|| friendly_backend_error(&nav_err)));
        }
        // 成功路径也查：错误页同样会触发 load 事件
        if let Some(msg) = chrome_error_note(&tab).await {
            return Err(msg);
        }
    }
    log(
        "browser.tab.new",
        json!({ "id": tab.id, "agent": agent, "url": url, "shell": session.mode == HostMode::Shell }),
    );
    Ok(tab)
}

/// shell 模式建标签页：指挥壳建 about:blank child webview → 在壳的 CDP 端点上发现新
/// target → 连上它的页面级 WS。返回 (target id, ws url, shell label)。
pub(crate) async fn shell_create_target(session: &BrowserSession) -> Result<(String, String, Option<String>), String> {
    if !shell::available() {
        return Err("桌面壳控制通道未连接（壳没跑或已断开）".into());
    }
    // label 单调递增不复用（2026-10-08 修）：wv-{len+1} 在关一个再建时复用同名，
    // 壳侧异步销毁窗口里两个 webview 同名，close/visible/rect 作用对象歧义。
    let label = format!(
        "wv-{}",
        session.shell_label_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    // 初始矩形：有面板同步过的矩形就用；没有（面板收起过/从未同步）建到**屏幕外**——
    // 壳的 add_child 是先可见再按 visible=false 隐藏，建在 (0,0) 或失效矩形上会闪一个
    // 莫名窗口（用户实测实锤）；屏幕外建、保持隐藏，等真正的 rect+visible 指令再归位。
    let (x, y, w, h) = session
        .last_rect
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|(x, y, w, h)| (x as f64, y as f64, w as f64, h as f64))
        .unwrap_or((-4000.0, -4000.0, 1024.0, 768.0));
    // 发现基线：**发指令前**快照全部 page target id——壳里上一个会话残留的 webview
    // （服务重启后壳不重启时就有）target 还在列表里，只排除自己的 tab 会认领到将死的
    // 旧 target，连上就断
    let known = all_page_target_ids(session.port).await;
    shell::create(&label, "about:blank", x, y, w, h, false);
    match wait_new_page_target(session.port, &known, Duration::from_secs(8)).await {
        Ok((id, ws)) => Ok((id, ws, Some(label))),
        Err(e) => {
            shell::close(&label); // 建出来没人认领的 webview 回收掉
            Err(format!(
                "等待桌面壳 webview 上线超时（CDP 端口 {port} 不可用？被别的程序占了？）: {e}",
                port = session.port
            ))
        }
    }
}

/// 拉一次壳 CDP 端点的 page target 列表（端点不可用/解析失败返回空）。
pub(crate) async fn fetch_page_targets(port: u16) -> Vec<Value> {
    let Ok(client) = dev_client() else { return Vec::new() };
    let Ok(resp) = client.get(format!("http://127.0.0.1:{port}/json/list")).send().await else {
        return Vec::new();
    };
    let Ok(text) = resp.text().await else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    v.as_array()
        .map(|a| {
            a.iter()
                .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// 轮询 page target 列表直到 `until` 满足或超时，返回最后一次看到的列表。
pub(crate) async fn poll_page_targets(port: u16, budget: Duration, until: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let list = fetch_page_targets(port).await;
        if until(&list) {
            return list;
        }
        if tokio::time::Instant::now() >= deadline {
            return list;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// 当前 CDP 端口上全部 page target id（发现基线用；端点不可用时返回空）。
pub(crate) async fn all_page_target_ids(port: u16) -> Vec<String> {
    fetch_page_targets(port)
        .await
        .iter()
        .filter_map(|t| t.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// 等「非壳内部」的 page target 全部消失（reset 后旧 webview 异步销毁，见 shell_launch）。
/// 只剩壳自己的 UI/DevTools 窗口即算稳了；超时也放行（不阻塞启动）。
pub(crate) async fn wait_targets_settled(port: u16, budget: Duration) {
    poll_page_targets(port, budget, |list| {
        list.iter().filter(|t| !is_shell_internal_target(t)).count() == 0
    })
    .await;
}

/// 轮询 /json/list 找新 page target（壳的 UI 自己的 webview 与 DevTools 窗口要排除）。
pub(crate) async fn wait_new_page_target(port: u16, known: &[String], budget: Duration) -> Result<(String, String), String> {
    let list = poll_page_targets(port, budget, |list| {
        list.iter().any(|t| {
            !known.iter().any(|k| Some(k.as_str()) == t.get("id").and_then(Value::as_str))
                && !is_shell_internal_target(t)
        })
    })
    .await;
    list.iter()
        .find(|t| {
            !known.iter().any(|k| Some(k.as_str()) == t.get("id").and_then(Value::as_str))
                && !is_shell_internal_target(t)
        })
        .and_then(|t| {
            let id = t.get("id").and_then(Value::as_str)?;
            let ws = t.get("webSocketDebuggerUrl").and_then(Value::as_str)?;
            Some((id.to_string(), ws.to_string()))
        })
        .ok_or_else(|| format!("{budget:?} 内没出现新的 page target"))
}

/// 壳自己的 UI webview（加载 5173 的那个）与 DevTools 窗口——都不是浏览器标签页。
pub(crate) fn is_shell_internal_target(t: &Value) -> bool {
    let url = t.get("url").and_then(Value::as_str).unwrap_or("");
    url.starts_with("devtools://") || url.starts_with("chrome-devtools://") || url.contains("127.0.0.1:5173")
}

/// 关闭并移除一个标签页（连接即弃，进程侧异步关）。
pub(crate) async fn close_tab(session: &BrowserSession, id: &str) -> bool {
    let removed = {
        let mut tabs = session.tabs.lock().unwrap_or_else(|e| e.into_inner());
        let pos = tabs.iter().position(|t| t.id == id);
        if let Some(pos) = pos {
            let tab = tabs.remove(pos);
            tab.retired.store(true, Ordering::Relaxed);
            // shell 模式：壳里的 child webview 也要关
            if let Some(label) = tab.shell_label.as_deref() {
                shell::close(label);
            }
        }
        pos.is_some()
    };
    if removed {
        if session.visible().as_deref() == Some(id) {
            session.clear_visible();
        }
        if session.mode == HostMode::Headless {
            dev_close_target(session.port, id).await;
        }
        log("browser.tab.close", json!({ "id": id }));
    }
    removed
}

/// agent 工具的当前工作页：① 会话记录的 agent 页（跨站点导航迁到新页后指向新页）；
/// ② 兜底找任何带 agent 标记的页（旧会话迁移）；③ 都没有就建一个。
pub(crate) async fn agent_tab(session: &BrowserSession) -> Result<Arc<WebTab>, String> {
    {
        let tabs = session.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(id) = session.agent_tab_now()
            && let Some(t) = tabs.iter().find(|t| t.id == id)
        {
            return Ok(t.clone());
        }
        if let Some(t) = tabs.iter().find(|t| t.agent.load(Ordering::Relaxed)) {
            session.set_agent_tab(&t.id);
            return Ok(t.clone());
        }
    }
    let tab = create_tab(session, "about:blank", true).await?;
    session.set_agent_tab(&tab.id);
    Ok(tab)
}

/// URL 的 host（小写、剥开头显 www；解析不了返回空串）。
pub(crate) fn host_of(url: &str) -> String {
    let after = match url.find("://") {
        Some(i) => &url[i + 3..],
        None => url,
    };
    let host = after.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
    host.strip_prefix("www.").unwrap_or(&host).to_string()
}

/// 两个 URL 是否同站点（host 等价，www 显式前缀不算差异）。
pub(crate) fn same_site(a: &str, b: &str) -> bool {
    let (ha, hb) = (host_of(a), host_of(b));
    !ha.is_empty() && ha == hb
}

/// agent 要浏览的下一个页面：**跨站点就开新标签页**（同站点复用当前 agent 页；当前页
/// 是空白页时直接用它）。用户预期「打开官网」是新开一页，旧页面也留在面板里可回看；
/// 与 skills/browser-use 的「同站点复用 tab」一致。返回 (目标页, 是否新开)。
pub(crate) async fn agent_tab_for_navigate(
    session: &Arc<BrowserSession>,
    url: &str,
) -> Result<(Arc<WebTab>, bool), String> {
    let cur = agent_tab(session).await?;
    let cur_url = tab_title_url(&cur).await.1;
    if cur_url.is_empty() || cur_url == "about:blank" || same_site(&cur_url, url) {
        return Ok((cur, false));
    }
    // 换站点：新开一页给 agent，旧页摘掉 agent 徽标（徽标语义 = agent 当前工作页）
    let fresh = create_tab(session, "about:blank", true).await?;
    cur.agent.store(false, Ordering::Relaxed);
    session.set_agent_tab(&fresh.id);
    Ok((fresh, true))
}

/// 按 id 取标签页。
pub(crate) fn get_tab(session: &BrowserSession, id: &str) -> Result<Arc<WebTab>, String> {
    session
        .tabs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|t| t.id == id)
        .cloned()
        .ok_or_else(|| format!("标签页不存在: {id}"))
}

/// 桌面壳没跑时给人的话（不静默、不偷偷拉 Edge）。
pub(crate) const NO_SHELL_HINT: &str = "桌面壳没运行，内置浏览器不可用。网页在桌面壳的 child webview 里跑（不再另起 Edge 进程）：请启动 Pawch Agent 桌面壳（start.bat 现在会一起起壳），或在本面板「⋯ → 在默认浏览器中打开」看网页。确需无头模式（自动化/CI）设环境变量 AGENT_BROWSER_HEADLESS=1。";

/// 是否允许无头模式（默认关——没有壳就不起 Edge）。
pub(crate) fn headless_allowed() -> bool {
    std::env::var("AGENT_BROWSER_HEADLESS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub(crate) async fn launch() -> Result<Arc<BrowserSession>, String> {
    // shell 优先：桌面壳在跑就把浏览器搬进壳的 child webview（不再另起无头进程）
    if shell::available() {
        match shell_launch().await {
            Ok(s) => return Ok(s),
            Err(e) => {
                // 壳接着但 CDP 不可用（端口被抢/旧版壳）：禁用 shell 模式
                shell::disable();
                log("browser.shell.fallback", json!({ "err": e }));
            }
        }
    }
    // 没有壳：**默认不再自动拉无头 Edge**——用户多次明令不要起 Edge 进程，而 
    // 浏览器的家就是壳。确需无头（自动化 / CI / 特意不用壳）时设 AGENT_BROWSER_HEADLESS=1。
    if !headless_allowed() {
        return Err(NO_SHELL_HINT.to_string());
    }
    let exe = find_browser_exe()?;
    // 预占一个环回空闲端口（进程退出即释放；极小概率被抢，重试一次）
    let mut last_err = String::new();
    for _ in 0..2 {
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
            l.local_addr().map_err(|e| e.to_string())?.port()
        };
        // profile 按进程 PID 隔离：宿主被强杀时无头浏览器会残留并握住 profile 单例锁
        // （真机踩过：残留 Edge 让下一次 launch「拉起后立即退出」）。顺手清掉别的旧 profile。
        let profile = std::env::temp_dir().join(format!("agent-browser-profile-{}", std::process::id()));
        clean_stale_profiles(&profile);
        let _ = std::fs::create_dir_all(&profile);
        let child = Command::new(&exe)
            .args([
                "--headless=new",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--disable-background-networking",
                "--mute-audio",
                "--hide-scrollbars",
                "--window-size=1280,900",
                &format!("--remote-debugging-port={port}"),
                // iframe 内嵌 DevTools 时前端会带 Origin 头连 ws，Chromium 默认拒——
                // 本实例只听环回，放行任意 Origin
                "--remote-allow-origins=*",
                &format!("--user-data-dir={}", profile.display()),
                "about:blank",
            ])
            .stdout(Stdio::null())
            // stderr 留管道：秒退时的退出码与报错原文是定位「为什么拉不起」的唯一线索
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return Err(format!("拉起浏览器失败（{}）: {e}", exe.display())),
        };
        // 生命周期保险丝：服务被硬杀（stop.bat 走 taskkill /F）时无头浏览器跟着死，
        // 不留孤儿 Edge 占内存（真机踩过：用户机器上留了个杀不掉的提权孤儿）
        let job = crate::jobguard::ChildJob::new();
        if let Some(j) = job.as_ref() {
            use std::os::windows::io::AsRawHandle;
            j.assign(child.as_raw_handle() as isize);
        }
        // 拉起后立刻探活：老版本 Edge 不认 --headless=new 会秒退
        tokio::time::sleep(Duration::from_millis(300)).await;
        if let Ok(Some(code)) = child.try_wait() {
            // 收 stderr（管道里已有内容；进程退出后读到 EOF 为止，限时收）
            let mut tail = String::new();
            if let Some(mut err) = child.stderr.take() {
                use std::io::Read;
                let deadline = Instant::now() + Duration::from_millis(500);
                let mut buf = [0u8; 1024];
                loop {
                    if Instant::now() > deadline {
                        break;
                    }
                    match err.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            tail.push_str(&String::from_utf8_lossy(&buf[..n]));
                            if tail.len() > 2000 {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            let tail = tail.lines().rev().take(3).collect::<Vec<_>>().join(" | ");
            last_err = format!("浏览器拉起后立即退出（exit={code:?}，{}）：{tail}", exe.display());
            continue;
        }
        match wait_page_target(port, Duration::from_secs(10)).await {
            Ok((target_id, ws)) => {
                let cdp = cdp::Cdp::connect(&ws).await?;
                // loadEventFired 需要 Page 域使能
                cdp.call("Page.enable", json!({})).await?;
                log("browser.launch", json!({ "exe": exe.display().to_string(), "port": port }));
                // 启动自带的 about:blank 页即 agent 工具的专用标签页
                let first = WebTab::new(target_id, true, cdp, None);
                spawn_tab_events(first.clone());
                let session = Arc::new(BrowserSession {
                    mode: HostMode::Headless,
                    child: Some(std::sync::Mutex::new(child)),
                    job,
                    port,
                    tabs: std::sync::Mutex::new(vec![first]),
                    last_used_ms: AtomicU64::new(now_ms()),
                    visible_tab: std::sync::Mutex::new(None),
                    shell_tab: std::sync::Mutex::new(None),
                    panel_showing: std::sync::Mutex::new(false),
                    shell_shown: std::sync::Mutex::new(None),
                    last_rect: std::sync::Mutex::new(None),
                    last_frame_poll_ms: AtomicU64::new(0),
                    agent_touched_ms: AtomicU64::new(0),
                    agent_tab_id: std::sync::Mutex::new(None),
                    shell_label_seq: std::sync::atomic::AtomicU64::new(1),
                });
                ensure_reaper();
                return Ok(session);
            }
            Err(e) => {
                let _ = child.kill();
                last_err = e;
            }
        }
    }
    Err(last_err)
}

/// 壳 CDP 端口轻量探活（GET /json/version）——shell_launch 用它验证壳的 CDP 真的
/// 可用（不可用就整体回退无头模式）。**不再靠预建一个空白 agent 页来验证**：那个页
/// 会以「空白页 agent」的身份排在用户标签前面，用户明确反馈多余（ agent 页按需创建）。
pub(crate) async fn probe_shell_cdp(port: u16) -> bool {
    let Ok(client) = dev_client() else { return false };
    matches!(
        client.get(format!("http://127.0.0.1:{port}/json/version")).send().await,
        Ok(r) if r.status().is_success()
    )
}

/// shell 模式起会话：不预建任何页（用户打开面板看到的就是空态；agent 页在模型第一次
/// 用浏览器工具时按需创建）。这一步同时验证壳的 CDP 端口真的可用——不可用就整体
/// 回退无头模式。
pub(crate) async fn shell_launch() -> Result<Arc<BrowserSession>, String> {
    let session = Arc::new(BrowserSession {
        mode: HostMode::Shell,
        child: None,
        job: None,
        port: shell::SHELL_CDP_PORT,
        tabs: std::sync::Mutex::new(Vec::new()),
        last_used_ms: AtomicU64::new(now_ms()),
        visible_tab: std::sync::Mutex::new(None),
        shell_tab: std::sync::Mutex::new(None),
        panel_showing: std::sync::Mutex::new(false),
        shell_shown: std::sync::Mutex::new(None),
        last_rect: std::sync::Mutex::new(None),
        last_frame_poll_ms: AtomicU64::new(0),
        agent_touched_ms: AtomicU64::new(0),
        agent_tab_id: std::sync::Mutex::new(None),
                    shell_label_seq: std::sync::atomic::AtomicU64::new(1),
    });
    // 壳不重启而服务重启过时，上一个会话的 webview 还活着——先清干净再建，
    // 免得残留 target 干扰发现、白白常驻
    shell::reset();
    // 关掉旧 webview 后**等它们的 target 从 /json/list 消失**：实测旧 webview 的销毁
    // 是异步的（~180ms），这期间建出来的新 webview，其 target 会跟着旧的一起从列表
    // 里消失（CDP 连上就断）
    wait_targets_settled(session.port, Duration::from_secs(3)).await;
    if !probe_shell_cdp(session.port).await {
        return Err(format!("壳 CDP 端口 {} 探活失败", session.port));
    }
    ensure_reaper();
    log("browser.launch", json!({ "mode": "shell", "port": shell::SHELL_CDP_PORT }));
    Ok(session)
}

/// 确保有一个活的浏览器会话（死会话清理后重新拉起）。
pub(crate) async fn ensure_session() -> Result<Arc<BrowserSession>, String> {
    let mut guard = SESSION.lock().await;
    if let Some(s) = guard.as_ref() {
        s.touch();
        if browser_alive(s.port).await {
            // 端口活着不代表会话里的 tab 还活着：壳重启后 9223 复生，但旧连接的
            // actor 早退了（僵尸 tab，title/url 全空）。摘掉死 tab；全死就重建会话。
            if reap_dead_tabs(&mut guard).await? {
                return Ok(guard.as_ref().unwrap().clone());
            }
            return Ok(guard.as_ref().unwrap().clone());
        }
        log("browser.stale", json!({}));
        if let Some(dead) = guard.take() {
            kill_child(&dead);
        }
    }
    let s = launch().await?;
    *guard = Some(s.clone());
    Ok(s)
}

/// 摘掉 CDP 已断的 tab（壳重启/浏览器进程死了就会这样），全死就连会话一起重建。
/// **读路径（panel_state / current_session）也必须调**：只挂在 ensure_session 上的话，
/// 面板一打开拿到的就是僵尸 tab 列表（title/url 全空），地址栏回车打到死连接上直接
/// 「CDP actor 已退出」——真机复现过。返回 true = 调用方手里的 guard 已 refreshes。
pub(crate) async fn reap_dead_tabs(guard: &mut Option<Arc<BrowserSession>>) -> Result<bool, String> {
    let Some(s) = guard.as_ref() else { return Ok(false) };
    let dead: Vec<String> = s
        .tabs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|t| !t.cdp.is_alive())
        .map(|t| t.id.clone())
        .collect();
    if dead.is_empty() {
        return Ok(false);
    }
    for id in &dead {
        close_tab(s, id).await;
    }
    let empty = s.tabs.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
    if empty {
        log("browser.stale", json!({ "reason": "tabs-dead" }));
        if let Some(dead_session) = guard.take() {
            kill_child(&dead_session);
        }
        let s = launch().await?;
        *guard = Some(s);
        return Ok(true);
    }
    Ok(false)
}

/// 空转收割 + 驻留巡检 + screencast 同步（5s 一拍）。
pub(crate) fn ensure_reaper() {
    REAPER.get_or_init(|| {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SWEEP_INTERVAL).await;
                let session = {
                    let guard = SESSION.lock().await;
                    match guard.as_ref() {
                        Some(s) => {
                            s.touch();
                            s.clone()
                        }
                        None => continue,
                    }
                };
                sweep_session(&session).await;
                if session.idle_secs() > IDLE_CLOSE.as_secs() {
                    close_internal("idle").await;
                }
            }
        });
    });
}

/// 驻留巡检：screencast 只服务「有客户端在轮询帧」的可见页；后台超时空闲且活跃页
/// 超限 → 挂起最久空闲者；shell 模式顺带同步壳里 webview 的显隐与矩形。
pub(crate) async fn sweep_session(session: &Arc<BrowserSession>) {
    let tabs = session.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let visible = session.visible();
    let shell_tab = session.shell_tab();
    // screencast 同步：可见页 + 仍有客户端在轮询帧才开（壳里看的是真页面，不白编码；
    // 用户从普通浏览器开面板时轮询恢复，帧流自动跟上）
    let want_frames = now_ms().saturating_sub(session.last_frame_poll_ms.load(Ordering::Relaxed)) < 15_000;
    for t in &tabs {
        let on = visible.as_deref() == Some(t.id.as_str()) && !t.suspended.load(Ordering::Relaxed) && want_frames;
        ensure_screencast(t, on).await;
    }
    // shell 显隐同步：只看变化的（壳没连时指令静默丢弃）。显隐只认壳客户端选的页
    // （shell_tab），screencast 才认任意客户端的可见页（visible）
    sync_shell_visibility(session, &tabs, &shell_tab).await;
    // 驻留淘汰：非 agent、非可见、空闲超限、未挂起；活跃数超 LIVE_TAB_LIMIT 才动手
    let live: Vec<&Arc<WebTab>> = tabs.iter().filter(|t| !t.suspended.load(Ordering::Relaxed)).collect();
    if live.len() <= LIVE_TAB_LIMIT {
        return;
    }
    let mut victims: Vec<&Arc<WebTab>> = live
        .into_iter()
        .filter(|t| !t.agent.load(Ordering::Relaxed) && visible.as_deref() != Some(t.id.as_str()) && t.idle_secs() >= SUSPEND_IDLE.as_secs())
        .collect();
    // 最久空闲者先淘汰
    victims.sort_by_key(|t| t.last_active_ms.load(Ordering::Relaxed));
    for t in victims {
        let still_live = session
            .tabs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|x| !x.suspended.load(Ordering::Relaxed))
            .count();
        if still_live <= LIVE_TAB_LIMIT {
            break;
        }
        suspend_tab(t).await;
    }
}

/// shell 模式同步壳里 webview 的显隐与矩形。**两道门控缺一不可**：
/// ① `panel_showing` + `shell_tab`——只有壳里那个客户端自己在看时才显示，且只显示它
///    选中的那一页。agent 工具的 new_tab/activate_tab 也会写 visible_tab（无头时代
///    那是驱动 screencast 用的，浏览器直开 5173 的第二个客户端也写它），但那不是"壳里
///    人在看"——不分开的话，那个客户端一开工就把壳的 webview 弹到聊天区上空/换页
///    （真机踩过）；
/// ② 矩形已知——面板还没同步过视口（首次打开/切页签的防抖窗口）时先藏着，
///    免得 webview 在错误位置闪一下。
/// 「⋯」菜单不走这里的显隐——它是壳里一个**透明置顶的独立 webview**（见 `shell::menu`），
/// 浮在下层内容 webview 之上，网页全程不动、不被遮也不被藏。
pub(crate) async fn sync_shell_visibility(session: &Arc<BrowserSession>, tabs: &[Arc<WebTab>], shell_tab: &Option<String>) {
    if session.mode != HostMode::Shell {
        return;
    }
    // 矩形合理性门槛：面板内容区永远在页签栏+工具栏下方（实测 y≈125——rp-tabs 44 +
    // web-tabbar 44 + toolbar 39），y<100 只可能是量早了/量错了元素——这种矩形下显示
    // webview 会把它盖到面板 chrome 上（用户看到的就是"右侧边栏没了、页面从窗口顶开始"，
    // 且被盖住的 chrome 点不到）。宁可先藏着等下一次同步，也不能错位显示。
    let rect_sane = session
        .last_rect
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|(_, y, w, h)| y >= 100 && w >= 200 && h >= 200)
        .unwrap_or(false);
    let want_label: Option<String> = if session.showing() && rect_sane {
        match shell_tab.as_deref() {
            Some(vid) => tabs.iter().find(|t| t.id == vid).and_then(|t| t.shell_label.clone()),
            None => None,
        }
    } else {
        None
    };
    let cur = session.shell_shown.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if cur == want_label {
        return;
    }
    if let Some(old) = cur.as_deref() {
        shell::visible(old, false);
    }
    if let (Some(label), Some(tab)) = (
        want_label.as_deref(),
        tabs.iter().find(|t| t.shell_label.as_deref() == want_label.as_deref()),
    ) {
        // 先摆正矩形再显示，避免在错误位置闪一下
        let (x, y, w, h) = shell_rect_for(session, tab).await;
        shell::rect(label, x, y, w, h);
        shell::visible(label, true);
    }
    *session.shell_shown.lock().unwrap_or_else(|e| e.into_inner()) = want_label;
}

/// shell 模式下一个标签页该占的矩形：**以面板最近一次上报的矩形为准**（x/y/w/h 四元
/// 一起来自同一次 panel_viewport，天然自洽）。以前 w/h 取 tab.viewport——那是按标签页
/// 记的，面板在别的标签页上被拖宽/拖窄后，切回来的这个标签还留着旧尺寸，webview 会先
/// 按旧宽高闪一帧再被新矩形套进去（用户截图里的"卡位/宽窄不对"）。last_rect 没有时
/// （从未同步过）才回落 tab.viewport，再没有用默认值。
pub(crate) async fn shell_rect_for(session: &Arc<BrowserSession>, tab: &WebTab) -> (f64, f64, f64, f64) {
    let last = *session.last_rect.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((x, y, w, h)) = last {
        return (x as f64, y as f64, w as f64, h as f64);
    }
    let (w, h) = match *tab.viewport.lock().await {
        Some((w, h)) => (w as f64, h as f64),
        None => (1024.0, 768.0),
    };
    (0.0, 0.0, w, h)
}

/// 挂起一个后台标签页：停 screencast → frozen（停 JS 定时器与渲染产出）。
pub(crate) async fn suspend_tab(tab: &Arc<WebTab>) {
    ensure_screencast(tab, false).await;
    if let Err(e) = tab.cdp.call("Page.setWebLifecycleState", json!({ "state": "frozen" })).await {
        log("browser.suspend.fail", json!({ "id": tab.id, "err": e }));
        return;
    }
    tab.suspended.store(true, Ordering::Relaxed);
    log("browser.tab.suspend", json!({ "id": tab.id }));
}

/// 恢复一个被挂起的标签页：解冻 + 重放视口覆盖 + 补一刀 Page.enable。
pub(crate) async fn ensure_live(tab: &Arc<WebTab>) -> Result<(), String> {
    if !tab.suspended.swap(false, Ordering::Relaxed) {
        return Ok(());
    }
    tab.cdp.call("Page.setWebLifecycleState", json!({ "state": "active" })).await?;
    let _ = tab.cdp.call("Page.enable", json!({})).await;
    if let Some((w, h)) = *tab.viewport.lock().await {
        let _ = apply_viewport_override(tab, w, h).await;
    }
    tab.touch();
    log("browser.tab.restore", json!({ "id": tab.id }));
    Ok(())
}

pub(crate) async fn apply_viewport_override(tab: &WebTab, w: i64, h: i64) -> Result<(), String> {
    tab.cdp
        .call(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": w, "height": h, "deviceScaleFactor": 1, "mobile": false }),
        )
        .await
        .map(|_| ())
}

pub(crate) async fn close_internal(reason: &str) {
    let dead = SESSION.lock().await.take();
    if let Some(s) = dead {
        // shell 模式：壳里的 child webview 逐个关掉（没有进程可杀）
        if s.mode == HostMode::Shell {
            let tabs = s.tabs.lock().unwrap_or_else(|e| e.into_inner()).clone();
            for t in &tabs {
                if let Some(label) = t.shell_label.as_deref() {
                    shell::close(label);
                }
            }
            *s.shell_shown.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
        kill_child(&s);
        log("browser.close", json!({ "reason": reason }));
    }
}

// —— 标签页事件任务（screencast 帧 / JS 弹窗）——

/// 每标签页一个事件任务：消费 CDP 事件广播。retired 后退出；actor 退出（WS 断）即退。
pub(crate) fn spawn_tab_events(tab: Arc<WebTab>) {
    tokio::spawn(async move {
        let mut rx = tab.cdp.subscribe_events();
        loop {
            if tab.retired.load(Ordering::Relaxed) {
                break;
            }
            // 2026-10-08 修：retired 原先只在循环顶检查，而 recv() 会永久 park——
            // 关标签页后事件任务 + 整个 WebTab（含最新帧、ref_selectors）一起泄漏，
            // 只有恰好再来一个 CDP 事件才退出（广播发送端被本任务自己持有的
            // Arc<WebTab> 保活，通道永不关闭）。改为带超时的 recv（broadcast 的
            // recv 是 cancel-safe）：超时即回循环顶复查 retired，最坏 500ms 后退出。
            match tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await {
                Ok(Ok((method, params))) => match method.as_str() {
                    "Page.screencastFrame" => {
                        if tab.screencast_on.load(Ordering::Relaxed)
                            && let Some(data) = params.get("data").and_then(Value::as_str)
                            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data)
                        {
                            *tab.frame.lock().await = Some(Frame { data: bytes });
                        }
                        // ack 必须回：不回 Chromium 会停止产帧
                        let _ = tab
                            .cdp
                            .call(
                                "Page.screencastFrameAck",
                                json!({ "sessionId": params.get("sessionId").cloned().unwrap_or(Value::Null) }),
                            )
                            .await;
                    }
                    "Page.javascriptDialogOpening" => {
                        let info = DialogInfo {
                            kind: params.get("type").and_then(Value::as_str).unwrap_or("alert").to_string(),
                            message: params.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
                            default_prompt: params
                                .get("defaultPrompt")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        };
                        *tab.dialog.lock().await = Some(info);
                        log("browser.dialog", json!({ "id": tab.id, "type": params.get("type") }));
                    }
                    "Page.javascriptDialogClosed" => {
                        *tab.dialog.lock().await = None;
                    }
                    // 页面导航（用户点链接、JS 跳转等不走 navigate_tab 的路径）会销毁
                    // 隔离世界 context——这里主动清缓存，下次求值自动重建
                    "Page.loadEventFired" | "Page.frameNavigated" | "Page.navigatedWithinDocument" => {
                        tab.cdp.invalidate_worlds();
                    }
                    _ => {}
                },
                // 帧消费慢导致丢中间帧：最新帧胜出，可接受；dialog 低频不受影响
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(_)) => break,
                // 超时（无事件到达）：回循环顶复查 retired，关掉的标签页有界退出
                Err(_) => continue,
            }
        }
    });
}

/// screencast 开关（幂等）：开=Page.startScreencast(jpeg q75)，关=Page.stopScreencast。
pub(crate) async fn ensure_screencast(tab: &Arc<WebTab>, on: bool) {
    if on {
        if tab.screencast_on.swap(true, Ordering::Relaxed) {
            return;
        }
        let r = tab
            .cdp
            .call("Page.startScreencast", json!({ "format": "jpeg", "quality": 75, "everyNthFrame": 1 }))
            .await;
        if r.is_err() {
            tab.screencast_on.store(false, Ordering::Relaxed);
        }
    } else if tab.screencast_on.swap(false, Ordering::Relaxed) {
        let _ = tab.cdp.call("Page.stopScreencast", json!({})).await;
    }
}

/// 页面标题与地址（两次轻量求值；隔离世界，页面篡改不了）。
pub(crate) async fn tab_title_url(tab: &WebTab) -> (String, String) {
    // 有 JS 弹窗时页面脚本全阻塞（隔离世界求值也跑不了）——别白等 10s 超时，
    // 标题/URL 留空，面板走弹窗条
    if tab.dialog.lock().await.is_some() {
        return (String::new(), String::new());
    }
    let title = tab
        .cdp
        .evaluate_in_isolated_world(ISOLATED_WORLD, "document.title")
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let url = tab
        .cdp
        .evaluate_in_isolated_world(ISOLATED_WORLD, "location.href")
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    (title, url)
}

// —— DOM 快照（JS 注入，隔离世界）——

