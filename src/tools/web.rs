//! web_fetch / web_search:网页抓取正文与联网搜索(Bing RSS/HTML + DuckDuckGo 解析,SearchHit 留共享根)。

use super::*;

/// 极简 HTML → 文本：剔 script/style，去标签，解常见实体，压缩空行。
pub(crate) fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut skip_until: Option<&str> = None;
    let lower = html.to_lowercase();
    let mut i = 0usize;
    let bytes = html.as_bytes();
    while i < html.len() {
        if let Some(end_tag) = skip_until {
            match lower[i..].find(end_tag) {
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
                if rest.starts_with("<script") {
                    skip_until = Some("</script>");
                } else if rest.starts_with("<style") {
                    skip_until = Some("</style>");
                } else if let Some(end) = rest.find('>') {
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

/// web_fetch / web_search 共用的 HTTP 客户端（浏览器 UA：搜索页与部分站点会按 UA 拦爬虫）。
pub(crate) fn web_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(10))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36")
        // 搜索引擎按 TLS/协议指纹分流：HTTP/2 客户端会被当成爬虫，回一张「查询对得上、结果
        // 全是别的东西」的页面（真机踩过：查「rust 语言」返回西班牙股票、德国天气）。HTTP/1.1 正常。
        .http1_only()
        .build()
        .map_err(|e| format!("构建 HTTP 客户端失败: {e}"))
}

pub(crate) async fn web_fetch(args: &str, max_chars: usize) -> Result<String, String> {
    let v: Value = serde_json::from_str(args).map_err(|e| format!("参数解析失败: {e}"))?;
    let url = v.get("url").and_then(Value::as_str).ok_or("缺少 url 参数")?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("url 必须以 http:// 或 https:// 开头".to_string());
    }
    let client = web_client()?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("抓取失败: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("渠道返回 {status}"));
    }
    let body = resp.text().await.map_err(|e| format!("读响应失败: {e}"))?;
    let is_html = body.trim_start().starts_with('<') || body.contains("<html");
    let text = if is_html { html_to_text(&body) } else { body };
    if text.trim().is_empty() {
        return Err("页面内容为空（可能是纯 JS 渲染页，本工具不做 JS 渲染）".to_string());
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
        match client.get(url).send().await {
            Ok(r) if r.status().is_success() => {
                // 关键：只认落在「搜索结果页」上的响应。Bing 会把请求重定向到首页（/?q=…），
                // 首页上的推荐链接也带同类 CSS 类名，照解析就会把推荐位当搜索结果（真机踩过）。
                let effective = r.url().clone();
                let html = r.text().await.unwrap_or_default();
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
