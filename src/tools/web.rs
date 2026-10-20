//! web_fetch / web_search:网页抓取正文与联网搜索(Bing RSS/HTML + DuckDuckGo 解析,SearchHit 留共享根)。

use super::*;

/// 极简 HTML → 文本：剔 script/style，去标签，解常见实体，压缩空行。
pub(crate) fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut skip_until: Option<&[u8]> = None;
    // 小写副本逐字节 ASCII 映射：标签判定只涉及 ASCII（<script/<style/>），
    // 等长映射保证与 html 字节偏移一一对应。不能用 to_lowercase()——完整
    // Unicode 映射会改变字节长度（U+212A K→k 少 2 字节、İ→i̇ 多 1 字节），
    // 拿 html 的偏移切 lower 要么 panic 要么静默切错内容（2026-10-08 修）。
    let lower: Vec<u8> = html.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let bytes = html.as_bytes();
    let mut i = 0usize;

    while i < html.len() {
        if let Some(end_tag) = skip_until {
            match find_sub(&lower[i..], end_tag) {
                Some(p) => {
                    i += p + end_tag.len();
                    skip_until = None;
                }
                None => break,
            }
            continue;
        }
        match bytes[i] {
            b'<' => {
                let rest = &lower[i..];
                if rest.starts_with(b"<script") {
                    skip_until = Some(b"</script>");
                } else if rest.starts_with(b"<style") {
                    skip_until = Some(b"</style>");
                } else if let Some(end) = rest.iter().position(|&c| c == b'>') {
                    i += end + 1;
                    out.push(' ');
                    continue;
                } else {
                    break;
                }
            }
            _ => {
                let ch = html[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
                continue;
            }
        }
    }
    // 实体与空白收尾
    let text = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut collapsed = String::new();
    let mut blank = 0;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank += 1;
            if blank <= 1 {
                collapsed.push('\n');
            }
        } else {
            blank = 0;
            collapsed.push_str(t);
            collapsed.push('\n');
        }
    }
    collapsed.trim().to_string()
}

/// 字节切片里找子串（needle 为 ASCII 标签名；独立纯函数便于单测）。
fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// web_fetch / web_search 共用的 HTTP 客户端（浏览器 UA：搜索页与部分站点会按 UA 拦爬虫）。
pub(crate) fn web_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(10))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
        // 搜索引擎按 TLS/协议指纹分流：HTTP/2 客户端会被当成爬虫，回一张「查询对得上、结果
        // 全是别的东西」的页面（真机踩过：查「rust 语言」返回西班牙股票、德国天气）。HTTP/1.1 正常。
        .http1_only()
        // 重定向不再交给 reqwest 自动跟（审计 12 收口）：自动跟的闸只能看 IP 字面量、
        // 域名一律放行，「公网网页 302 跳 169.254.169.254（云元数据）/localhost（本机
        // 管理口）」这条经典 SSRF 打法拦不住。改为 send_with_ssrf_guard 手动逐跳走
        // ensure_public_target（域名也解析判段）。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))
}

/// 响应体字节上限：流式读到这就断，后续分片丢弃，再走字符截断（N3）。
/// 旧写法 `resp.text()` 整体读——超大/无尽头响应先撑爆内存，max_chars 字符截断拦不住。
const WEB_BODY_MAX_BYTES: usize = 2 * 1024 * 1024;

/// 带逐跳 SSRF 闸的 GET（审计 12）：reqwest 自动重定向已关（web_client），这里手动跟，
/// 每一跳（含 302/307 的 Location 目标）都先过 ensure_public_target 再发——域名目标也
/// 解析判段，重定向进内网的路彻底堵死。预算与旧 Policy 对齐：连首发最多 6 次请求。
async fn send_with_ssrf_guard(client: &reqwest::Client, url: &str) -> Result<reqwest::Response, String> {
    let mut current = url.to_string();
    for hop in 0..=5 {
        ensure_public_target(&current).await?;
        let resp = client
            .get(&current)
            .send()
            .await
            .map_err(|e| format!("抓取失败: {e}"))?;
        let status = resp.status();
        if !status.is_redirection() {
            return Ok(resp);
        }
        if hop == 5 {
            return Err("重定向次数超限（>5 跳），放弃抓取".to_string());
        }
        let loc = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| format!("服务器返回 {status} 但没有 Location"))?;
        current = redirect_target(resp.url(), &loc)?;
    }
    unreachable!("hop==5 的分支已在循环内返回")
}

/// Location → 绝对 URL（相对路径按当前响应 URL 拼；非法即错）。独立纯函数便于单测。
fn redirect_target(base: &reqwest::Url, loc: &str) -> Result<String, String> {
    base.join(loc)
        .map(|u| u.to_string())
        .map_err(|e| format!("重定向地址非法（{loc}）: {e}"))
}

/// 流式读响应体（带上限）。utf8 按整体 lossy 解码：截断处可能切在汉字中间，
/// 末字符变 U+FFFD 无所谓——紧接着还有 max_chars 截断，且这是抓取不是流式正文。
async fn read_body_capped(resp: reqwest::Response) -> Result<String, String> {
    use futures_util::StreamExt;
    let mut body: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("读响应失败: {e}"))?;
        if body.len() >= WEB_BODY_MAX_BYTES {
            break;
        }
        let take = chunk.len().min(WEB_BODY_MAX_BYTES - body.len());
        body.extend_from_slice(&chunk[..take]);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// 内网/环回/链路本地段判定（SSRF 黑名单，N3）。
fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unique_local() // fc00::/7
                // IPv6 链路本地 fe80::/10（std 无稳定判定，按首段掩码手算）
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // IPv4-mapped / IPv4-compatible（::ffff:127.0.0.1 这类）按内段判
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private_ip(std::net::IpAddr::V4(v4)))
                || v6.to_ipv4().is_some_and(|v4| is_private_ip(std::net::IpAddr::V4(v4)))
        }
    }
}

/// SSRF 预检（N3 + 审计 12 收口）：拒绝指向环回/内网/链路本地的抓取目标。IP 字面量直接
/// 判段；域名解析后逐个 IP 判段。**解析失败不放行**（fail-closed）：放行等于没检——
/// 「公网域名解析到内网 IP」正是 SSRF 的主打法，本机查不到就无法验证归属。代理场景
/// （DNS 在代理侧完成）本地确实可能查不到，显式设 AGENT_WEB_ALLOW_PRIVATE=1 放开。
/// 本地 dev 要抓 127.0.0.1 也走这个开关（与 AGENT_WEB_DEBUG_DIR / AGENT_BROWSER_PATH 同款）。
async fn ensure_public_target(url: &str) -> Result<(), String> {
    if std::env::var("AGENT_WEB_ALLOW_PRIVATE").is_ok() {
        return Ok(());
    }
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("url 解析失败: {e}"))?;
    let host = parsed.host_str().ok_or("url 缺少主机名")?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addrs: Vec<std::net::IpAddr> = match host.parse::<std::net::IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => match tokio::net::lookup_host((host, port)).await {
            Ok(it) => it.map(|s| s.ip()).collect(),
            Err(e) => {
                return Err(format!(
                    "无法验证抓取目标：域名 {host} 解析失败（{e}）。\
                     为防内网穿透（SSRF）不放行解析不了的目标；\
                     本地调试或代理场景可设 AGENT_WEB_ALLOW_PRIVATE=1 放开"
                ));
            }
        },
    };
    if addrs.is_empty() {
        return Err(format!("无法验证抓取目标：域名 {host} 没有解析到任何地址（SSRF 预检不放行）"));
    }
    for ip in addrs {
        if is_private_ip(ip) {
            return Err(format!(
                "拒绝抓取内网地址 {host}（{ip}）：web_fetch 不抓环回/内网/链路本地目标（SSRF 防护）。\
                 本地调试可设 AGENT_WEB_ALLOW_PRIVATE=1 放开"
            ));
        }
    }
    Ok(())
}

/// 静态抓取取到的正文少到这个数（且是 HTML）→ 判定 JS 渲染骨架页，回落浏览器。
/// 不是「页面就一定坏」——真这么小的页面浏览器也只会取回同样小的正文，多走一趟
/// 内核的代价换「SPA 页不再直接报错」，值。
const RENDER_FALLBACK_MIN_CHARS: usize = 200;

/// 静态抓取的结果要不要回落浏览器渲染（纯函数，便于单测）：
/// HTML 且正文薄（空或少于 RENDER_FALLBACK_MIN_CHARS）= JS 渲染骨架页。
/// 非 HTML（JSON/纯文本/XML）再薄也不回落——那不是渲染问题。
pub(crate) fn needs_render_fallback(is_html: bool, text_chars: usize) -> bool {
    is_html && text_chars < RENDER_FALLBACK_MIN_CHARS
}

/// JS 渲染回退：浏览器 navigate + page_text（整页 innerText）。
/// 浏览器插件停用/内核没起时错误原样带回——browser::command 自带能力闸人话。
/// SSRF 闸与静态抓取同一道：浏览器本身没有内外网判别，回落前自己检。
async fn web_fetch_rendered(url: &str, max_chars: usize) -> Result<String, String> {
    ensure_public_target(url).await?;
    let nav = json!({ "method": "navigate", "url": url }).to_string();
    // navigate 自己等 Page.loadEventFired；SPA 的 load 只是骨架，再给一小段水合时间
    crate::browser::command(&nav, 2000).await?;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    crate::browser::command(r#"{"method":"page_text"}"#, max_chars).await
}

pub(crate) async fn web_fetch(args: &str, max_chars: usize) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let url = v.get("url").and_then(Value::as_str).ok_or("缺少 url 参数")?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("url 必须以 http:// 或 https:// 开头".to_string());
    }
    // render 档（2026-10-20 免打扰模式第 3 轮）：
    //   auto（默认）= 静态抓，取不到正文（空/极少，JS 渲染骨架页）就回落浏览器渲染；
    //   always      = 直接浏览器渲染（已知是 SPA/要登录后渲染的页面）；
    //   never       = 只要静态抓取（旧行为；浏览器插件停用时的显式选择）。
    let render = v
        .get("render")
        .and_then(Value::as_str)
        .unwrap_or("auto")
        .trim()
        .to_lowercase();
    if render == "always" {
        return web_fetch_rendered(url, max_chars).await;
    }
    let client = web_client()?;
    let resp = send_with_ssrf_guard(&client, url).await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("渠道返回 {status}"));
    }
    let body = read_body_capped(resp).await?;
    let is_html = body.trim_start().starts_with('<') || body.contains("<html");
    let text = if is_html { html_to_text(&body) } else { body };
    let too_thin = needs_render_fallback(is_html, text.chars().count());
    if render != "never" && (text.trim().is_empty() || too_thin) {
        // 纯 JS 渲染页（SPA 骨架）：回落浏览器。回退也失败就说两句实话——
        // 别让模型以为抓取功能坏了，指引它换路（browser 工具/让用户开插件）。
        match web_fetch_rendered(url, max_chars).await {
            Ok(rendered) if !rendered.trim().is_empty() => {
                return Ok(format!("（这是 JS 渲染页面，静态抓取只拿到空骨架；以下为浏览器渲染后的正文）\n{rendered}"));
            }
            Ok(_) => {}
            Err(e) => {
                return Err(format!(
                    "页面内容为空（纯 JS 渲染页），浏览器渲染回退失败：{e}。可换 browser 工具手动 navigate + snapshot，或让用户在 设置→插件 确认「浏览器」已开"
                ));
            }
        }
    }
    if text.trim().is_empty() {
        return Err("页面内容为空（可能是纯 JS 渲染页。可传 render=auto 让浏览器渲染，或直接用 browser 工具）".to_string());
    }
    Ok(cap(&text, max_chars))
}

/// 联网搜索（026）：Bing（cn 域名，国内可直连）为主，取不到结果时回落 DuckDuckGo HTML。
/// 两个引擎都不需要 key；搜到网址后用 web_fetch 抓正文。
pub(crate) async fn web_search(args: &str, max_chars: usize) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let query = v.get("query").and_then(Value::as_str).map(str::trim).unwrap_or("");
    if query.is_empty() {
        return Err("query 为空".to_string());
    }
    let count = v.get("count").and_then(Value::as_u64).unwrap_or(6).clamp(1, 10) as usize;
    let client = web_client()?;
    let q = urlencode(query);
    // 引擎顺序按「直连可用性」排：www.bing.com 直连会 302 到 region 站、走代理也能出结果，
    // cn.bing.com 直连可用但走代理会被重定向成首页，DDG 只在墙外好用——三个都试一遍。
    // 引擎顺序：Bing 的 **RSS 输出**优先——那是给阅读器用的接口，结果里是干净的真 URL
    // （HTML 版的结果链接被包成 ck/a 跳转，且对程序化客户端可能返回「标题对、内容不是这回事」
    // 的页面，真机踩过）。RSS 不行再退回 HTML 版，最后 DuckDuckGo（墙外可用）。
    let engines: [(&str, String, &str); 5] = [
        ("Bing 中国站 RSS", format!("https://cn.bing.com/search?q={q}&format=rss&count={count}"), "rss"),
        ("Bing RSS", format!("https://www.bing.com/search?q={q}&format=rss&count={count}"), "rss"),
        ("Bing 中国站", format!("https://cn.bing.com/search?q={q}&count={}", count * 2), "bing"),
        ("Bing", format!("https://www.bing.com/search?q={q}&count={}", count * 2), "bing"),
        ("DuckDuckGo", format!("https://html.duckduckgo.com/html/?q={q}"), "ddg"),
    ];
    let mut hits = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for (name, url, kind) in engines {
        match send_with_ssrf_guard(&client, &url).await {
            Ok(r) if r.status().is_success() => {
                // 关键：只认落在「搜索结果页」上的响应。Bing 会把请求重定向到首页（/?q=…），
                // 首页上的推荐链接也带同类 CSS 类名，照解析就会把推荐位当搜索结果（真机踩过）。
                let effective = r.url().clone();
                let html = read_body_capped(r).await.unwrap_or_default();
                let got = match kind {
                    "rss" => parse_bing_rss(&html, count),
                    "bing" => parse_bing(&html, count),
                    _ => parse_ddg(&html, count),
                };
                // 调试开关：设了 AGENT_WEB_DEBUG_DIR 就把原始页面落盘（排查搜索/抓取解析问题用）
                if let Ok(dir) = std::env::var("AGENT_WEB_DEBUG_DIR") {
                    let file = format!("web_{}.html", name.replace(' ', "_"));
                    let _ = std::fs::write(std::path::Path::new(&dir).join(file), &html);
                }
                // 诊断信息常驻日志（搜索出问题时靠它定位：哪个引擎、落到哪个 URL、页面标题是什么）
                crate::observability::try_event(
                    crate::observability::EventType::State,
                    json!({
                        "kind": "web.search",
                        "engine": name,
                        "url": effective.as_str(),
                        "page_title": html_title(&html),
                        "bytes": html.len(),
                        "hits": got.len(),
                    }),
                );
                if kind != "rss" && !is_search_url(effective.path()) {
                    notes.push(format!("{name}: 被重定向到 {}（不是结果页）", effective));
                } else if got.is_empty() {
                    notes.push(format!("{name}: 页面里没解析到结果（多半是反爬或代理中转页）"));
                } else {
                    hits = got;
                    break;
                }
            }
            Ok(r) => notes.push(format!("{name}: HTTP {}", r.status())),
            Err(e) => notes.push(format!("{name}: 请求失败 {e}")),
        }
    }
    if hits.is_empty() {
        return Err(format!(
            "搜索没有取到结果（{}）：换关键词再试，或直接用 web_fetch 抓已知网址",
            notes.join("；")
        ));
    }
    let mut out = format!("搜索「{query}」取到 {} 条：\n", hits.len());
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, h.title, h.url));
        if !h.snippet.is_empty() {
            out.push_str(&format!("   {}\n", h.snippet));
        }
    }
    Ok(cap(&out, max_chars))
}

/// Bing RSS 输出解析（给阅读器用的接口，链接是干净的真 URL，还带摘要）。
pub(crate) fn parse_bing_rss(xml: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut from = 0usize;
    while hits.len() < limit {
        let Some(rel) = xml[from..].find("<item>") else { break };
        let start = from + rel + "<item>".len();
        let Some(end_rel) = xml[start..].find("</item>") else { break };
        let item = &xml[start..start + end_rel];
        from = start + end_rel + "</item>".len();
        let title = xml_tag(item, "title");
        let url = xml_tag(item, "link");
        if title.is_empty() || url.is_empty() {
            continue;
        }
        hits.push(SearchHit {
            title: clean_text(&title),
            url: clean_text(&url),
            snippet: clean_text(&xml_tag(item, "description")),
        });
    }
    hits
}

/// 取 XML 里某个标签的文本（含实体解码；找不到返回空串）。
pub(crate) fn xml_tag(item: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let Some(i) = item.find(&open) else { return String::new() };
    let rest = &item[i + open.len()..];
    let Some(j) = rest.find(&close) else { return String::new() };
    decode_entities(rest[..j].trim())
}

/// 页面 `<title>`（诊断用：判断拿到的到底是结果页还是反爬/中转页）。
pub(crate) fn html_title(html: &str) -> String {
    extract_tag(html, "<title", "</title>").map(|t| clean_text(&t)).unwrap_or_default()
}

/// 是不是「搜索结果页」的路径（Bing 首页 / 各种中转页都排除掉）。
pub(crate) fn is_search_url(path: &str) -> bool {
    matches!(path, "/search" | "/html/" | "/lite/" | "/html" | "/lite")
}

/// Bing 结果页解析：`class="b_algo"` 切块，块内 `<h2>` 拿标题与链接、`b_lineclamp` 拿摘要。
pub(crate) fn parse_bing(html: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut from = 0usize;
    while hits.len() < limit {
        let Some(rel) = html[from..].find("class=\"b_algo\"") else { break };
        let start = from + rel;
        let block_end = html[start + 1..]
            .find("class=\"b_algo\"")
            .map(|p| start + 1 + p)
            .unwrap_or(html.len());
        let block = &html[start..block_end];
        from = block_end;
        let Some(h2_start) = block.find("<h2") else { continue };
        let Some(h2_end) = block[h2_start..].find("</h2>").map(|p| h2_start + p) else { continue };
        let h2 = &block[h2_start..h2_end];
        let Some(href_at) = h2.find("href=\"") else { continue };
        let raw = h2[href_at + 6..].split('"').next().unwrap_or("").trim();
        let url = bing_real_url(raw);
        let title = clean_text(h2);
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let snippet = extract_after_class(block, "class=\"b_lineclamp")
            .or_else(|| extract_tag(block, "<p", "</p>"))
            .map(|s| clean_text(&s))
            .unwrap_or_default();
        hits.push(SearchHit { title, url, snippet });
    }
    hits
}

/// DuckDuckGo HTML 版解析：`result__a` 是标题与链接（链接常是 /l/?uddg= 跳转，取真身）。
pub(crate) fn parse_ddg(html: &str, limit: usize) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut from = 0usize;
    while hits.len() < limit {
        let Some(rel) = html[from..].find("class=\"result__a\"") else { break };
        let start = from + rel;
        let Some(gt) = html[start..].find('>').map(|p| start + p) else { break };
        let Some(end) = html[gt..].find("</a>").map(|p| gt + p) else { break };
        let anchor = &html[start..gt];
        let url = anchor
            .find("href=\"")
            .map(|h| ddg_real_url(&anchor[h + 6..]))
            .unwrap_or_default();
        let title = clean_text(&html[gt + 1..end]);
        from = end + 4;
        if url.is_empty() || title.is_empty() {
            continue;
        }
        let snippet = extract_after_class(&html[from..], "class=\"result__snippet")
            .map(|s| clean_text(&s))
            .unwrap_or_default();
        hits.push(SearchHit { title, url, snippet });
    }
    hits
}

/// Bing 的跳转链接还原：结果链接常是 `bing.com/ck/a?…&u=a1<base64url 真 URL>&…`
/// （`u=` 的值前缀 `a1` 是 Bing 的标记，后面才是编码过的网址）。
pub(crate) fn bing_real_url(href: &str) -> String {
    if href.contains("bing.com/ck/a")
        && let Some(i) = href.find("u=")
    {
        let val = href[i + 2..].split('&').next().unwrap_or("");
        let enc = val.strip_prefix("a1").unwrap_or(val);
        if let Some(url) = base64url_decode(enc) {
            return url;
        }
    }
    href.to_string()
}

/// base64url 解码（无填充也接受）；失败返回 None。
pub(crate) fn base64url_decode(s: &str) -> Option<String> {
    use base64::Engine;
    let mut t: String = s
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    while !t.len().is_multiple_of(4) {
        t.push('=');
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(t.as_bytes()).ok()?;
    String::from_utf8(bytes).ok()
}

/// DuckDuckGo 的跳转链接还原：`//duckduckgo.com/l/?uddg=<百分号编码的真 URL>&rut=…`。
pub(crate) fn ddg_real_url(href: &str) -> String {
    let href = href.trim();
    if let Some(i) = href.find("uddg=") {
        let rest = &href[i + 5..];
        let enc = rest.split('&').next().unwrap_or("");
        return percent_decode(enc);
    }
    if let Some(stripped) = href.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    href.to_string()
}

/// 取 `class="<cls>` 所在元素的文本：跳过开标签（后面可能还有别的属性），
/// 一直取到该元素的闭合标签（摘要里常夹 `<b>` 等内联标签，取到第一个 `<` 会截断）。
pub(crate) fn extract_after_class(html: &str, cls: &str) -> Option<String> {
    let i = html.find(cls)?;
    let gt = html[i..].find('>')? + i;
    let tail = &html[gt + 1..];
    let end = ["</p>", "</div>", "</a>", "</span>", "</li>"]
        .iter()
        .filter_map(|c| tail.find(c))
        .min()?;
    Some(tail[..end].to_string())
}

/// 取某个开标签到闭合标签之间的文本。
pub(crate) fn extract_tag(html: &str, open: &str, close: &str) -> Option<String> {
    let i = html.find(open)?;
    let gt = html[i..].find('>')? + i;
    let end = html[gt..].find(close)? + gt;
    Some(html[gt + 1..end].to_string())
}

/// 去标签 → 解实体 → 压空白（搜索结果的标题/摘要用）。
pub(crate) fn clean_text(s: &str) -> String {
    decode_entities(&strip_tags(s)).split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// HTML 实体解码（搜索页里 `&ensp;` / `&#0183;` 这类很多）。
pub(crate) fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let window = &tail[..tail.len().min(12)];
        let Some(semi) = window.find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let ent = &tail[1..semi];
        let decoded = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" | "ensp" | "emsp" | "thinsp" => Some(' '),
            "hellip" => Some('…'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "middot" => Some('·'),
            _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
            }
            _ if ent.starts_with('#') => ent[1..].parse::<u32>().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 查询串编码（空格用 %20：`+` 只在表单编码里有「空格」含义，百分号更无歧义）。
pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*b as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 百分号解码（UTF-8 字节流）。
pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'%'
            && i + 3 <= b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ssrf_guard_blocks_private_target_before_connecting() {
        // 审计 12：闸在发送之前——环回目标第一跳 ensure_public_target 就该拒，
        // 连接根本不建立（本机若显式开了私网抓取开关，本用例失去前提，如实跳过）
        if std::env::var("AGENT_WEB_ALLOW_PRIVATE").is_ok() {
            return;
        }
        let client = web_client().unwrap();
        let r = send_with_ssrf_guard(&client, "http://127.0.0.1:9/x").await;
        assert!(r.is_err() && r.unwrap_err().contains("SSRF"), "环回目标应在发送前被拒");
    }

    #[test]
    fn html_to_text_survives_unicode_case_mapping() {
        // 2026-10-08 修：İ 的小写映射 2 字节→3 字节（U+212A 则反向少 2 字节），
        // 偏移错位后拿 html 的字节下标切 to_lowercase() 副本会 panic 或切错内容。
        // ASCII 等长小写副本后：标签照剥、正文按原样保留。
        let html = "a\u{0130}<b>bold</b><script>evil()</script>tail";
        let t = html_to_text(html);
        assert!(t.contains("bold"), "加粗正文应保留: {t:?}");
        assert!(!t.contains("evil"), "script 块应被剥掉: {t:?}");
        assert!(t.contains("tail"), "script 后的正文应保留: {t:?}");
        // 大写SCRIPT/Script 变体同样剥（ASCII 小写副本覆盖）
        let t2 = html_to_text("<SCRIPT>evil()</SCRIPT><Style>x{}</Style>ok");
        assert!(!t2.contains("evil") && !t2.contains("x{}"), "大小写变体应剥掉: {t2:?}");
        assert!(t2.contains("ok"), "style 后正文应保留: {t2:?}");
    }

    #[test]
    fn redirect_target_joins_relative_locations() {        // 审计 12：Location 相对路径按当前响应 URL 绝对化；非法 Location 明确报错
        let base: reqwest::Url = "https://example.com/a/b".parse().unwrap();
        assert_eq!(redirect_target(&base, "https://elsewhere.com/x").unwrap(), "https://elsewhere.com/x");
        assert_eq!(redirect_target(&base, "c").unwrap(), "https://example.com/a/c");
        assert_eq!(redirect_target(&base, "/root").unwrap(), "https://example.com/root");
        // url 库把 "://bad" 当相对路径宽容收下；真非法的（坏 host）才报错
        assert!(redirect_target(&base, "http://[").is_err());
    }

    #[test]
    fn is_private_ip_covers_ssrf_targets() {
        // N3：环回/私网/链路本地（云元数据 169.254.169.254）/未指定/组播一律算内网
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "::ffff:127.0.0.1", // IPv4-mapped 环回
            "::ffff:169.254.169.254",
            "fc00::1",  // 唯一本地地址
            "fe80::1",  // 链路本地
            "::",       // 未指定
        ] {
            let a: std::net::IpAddr = ip.parse().unwrap();
            assert!(is_private_ip(a), "{ip} 应判为内网");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "104.16.0.1", "2606:4700::1111", "2001:4860:4860::8888"] {
            let a: std::net::IpAddr = ip.parse().unwrap();
            assert!(!is_private_ip(a), "{ip} 是公网地址，不该误拦");
        }
    }

    /// 渲染回退判定（2026-10-20 第 3 轮）：HTML 骨架页（空/薄）才回落浏览器；
    /// 非 HTML 再薄也不回落（JSON/纯文本不是渲染问题）。
    #[test]
    fn render_fallback_decision() {
        assert!(needs_render_fallback(true, 0), "空 HTML = 纯 JS 骨架，必回落");
        assert!(needs_render_fallback(true, 50), "薄 HTML（< 200 字）判骨架");
        assert!(needs_render_fallback(true, 199), "199 字仍算薄");
        assert!(!needs_render_fallback(true, 200), "200 字整不算薄");
        assert!(!needs_render_fallback(true, 5000), "正常页面不回落");
        assert!(!needs_render_fallback(false, 0), "空 JSON/纯文本不回落（不是渲染问题）");
        assert!(!needs_render_fallback(false, 10), "薄 API 响应不回落");
    }

    /// render 档位解析：auto/always/never/缺省/大小写/空白都归位；
    /// 非法值按 auto（缺省）走——最稳的默认。
    #[test]
    fn render_mode_parsing() {
        let mode = |args: &str| -> String {
            let v: Value = serde_json::from_str(args).unwrap();
            v.get("render")
                .and_then(Value::as_str)
                .unwrap_or("auto")
                .trim()
                .to_lowercase()
        };
        assert_eq!(mode(r#"{"url":"x"}"#), "auto", "缺省 auto");
        assert_eq!(mode(r#"{"url":"x","render":"always"}"#), "always");
        assert_eq!(mode(r#"{"url":"x","render":" NEVER "}"#), "never", "大小写与空白都要容");
        assert_eq!(mode(r#"{"url":"x","render":"Auto"}"#), "auto");
    }
}
