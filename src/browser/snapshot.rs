use super::*;

/// 页面结构抽取：可交互元素（elements，带 ref/selector/rect/inViewport/parentRef/disabled/
/// checked）+ 语义正文节点（dom，不占 ref）。双结构移植自 ZCode browserSnapshotSchema——
/// 正文节点不消耗 action ref 预算；截断时先保证模型看到页面语义。
pub(crate) const SNAPSHOT_JS: &str = r#"((MAXN_IN, INCLUDE_HIDDEN) => {
  const MAXN = MAXN_IN || 400, MAXTEXT = 120, MAXDEPTH = 24;
  const out = [];
  let seq = 0;
  document.querySelectorAll('[data-agent-ref]').forEach((el) => {
    const n = parseInt(el.getAttribute('data-agent-ref').slice(1), 10);
    if (!isNaN(n) && n > seq) seq = n;
  });
  const INTERACTIVE = 'a[href],button,input,textarea,select,[role],[onclick],[tabindex],summary,details,label,[contenteditable]';
  const skip = new Set(['script','style','noscript','template','svg','path','head','meta','link','iframe','canvas']);
  const SEMANTIC = ['p','li','td','th','label','blockquote','figcaption','dd','dt','summary','span','h1','h2','h3','h4','h5','h6','pre','code','strong','em','b','i','a'];
  const inView = (el) => {
    const r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) return false;
    return r.bottom > 0 && r.right > 0 && r.top < innerHeight && r.left < innerWidth;
  };
  const roleOf = (el) => {
    const r = el.getAttribute('role'); if (r) return r;
    const t = el.tagName.toLowerCase();
    if (t === 'a') return 'link';
    if (t === 'button' || (t === 'input' && (el.type === 'button' || el.type === 'submit'))) return 'button';
    if (t === 'input' || t === 'textarea') return 'textbox';
    if (t === 'select') return 'combobox';
    if (/^h[1-6]$/.test(t)) return 'heading';
    if (t === 'img') return 'image';
    return null;
  };
  const nameOf = (el) => {
    let s = el.getAttribute('aria-label') || el.getAttribute('placeholder')
      || el.getAttribute('title') || el.getAttribute('alt') || '';
    return s.trim().slice(0, MAXTEXT);
  };
  const directText = (el) => {
    let s = '';
    for (const n of el.childNodes) if (n.nodeType === 3) s += n.textContent;
    return s.replace(/\s+/g, ' ').trim();
  };
  const depthOf = (el) => { let d = 0, p = el.parentElement; while (p && d < MAXDEPTH) { d++; p = p.parentElement; } return d; };
  const parentRefOf = (el) => {
    let p = el.parentElement;
    while (p) { const r = p.getAttribute && p.getAttribute('data-agent-ref'); if (r) return r; p = p.parentElement; }
    return null;
  };
  // 有界稳定 selector：id / data-testid / aria-label / name / 标签+nth-of-type
  const selOf = (el) => {
    try {
      if (el.id) return '#' + CSS.escape(el.id);
      const t = el.getAttribute('data-testid'); if (t) return '[data-testid="' + t + '"]';
      const a = el.getAttribute('aria-label'); if (a) return '[aria-label="' + a + '"]';
      const n = el.getAttribute('name');
      if (n && ['INPUT','TEXTAREA','SELECT'].includes(el.tagName)) return el.tagName.toLowerCase() + '[name="' + n + '"]';
      let node = el, path = el.tagName.toLowerCase(), hop = 0;
      while (node.parentElement && hop < 4) {
        const p = node.parentElement;
        const sibs = Array.prototype.filter.call(p.children, (c) => c.tagName === node.tagName);
        if (sibs.length > 1) { path += ':nth-of-type(' + (sibs.indexOf(node) + 1) + ')'; break; }
        node = p; hop++;
      }
      return path;
    } catch (e) { return el.tagName.toLowerCase(); }
  };
  const pushEl = (el) => {
    if (out.length >= MAXN) return;
    const tag = el.tagName.toLowerCase();
    const rect = el.getBoundingClientRect();
    let ref = el.getAttribute('data-agent-ref');
    if (!ref) { seq += 1; ref = 'e' + seq; el.setAttribute('data-agent-ref', ref); }
    out.push({
      ref, tag, role: roleOf(el),
      name: nameOf(el),
      text: directText(el).slice(0, MAXTEXT),
      value: (tag === 'input' || tag === 'textarea' || tag === 'select') ? String(el.value ?? '').slice(0, MAXTEXT) : null,
      href: tag === 'a' && el.href ? el.href.slice(0, 200) : null,
      disabled: !!(el.disabled || el.getAttribute('aria-disabled') === 'true'),
      checked: (tag === 'input' && (el.type === 'checkbox' || el.type === 'radio')) ? !!el.checked : null,
      selector: selOf(el),
      rect: [Math.round(rect.x), Math.round(rect.y), Math.round(rect.width), Math.round(rect.height)],
      inViewport: inView(el),
      parentRef: parentRefOf(el),
      depth: depthOf(el),
    });
  };
  document.querySelectorAll(INTERACTIVE).forEach((el) => {
    const tag = el.tagName.toLowerCase();
    if (skip.has(tag)) return;
    if (!INCLUDE_HIDDEN && !el.getClientRects().length) return; // 不可见（display:none / 零尺寸）
    pushEl(el);
  });
  const dom = [];
  const domPush = (el, depth) => {
    if (!el || dom.length >= MAXN || depth > MAXDEPTH) return;
    const tag = el.tagName.toLowerCase();
    if (skip.has(tag)) return;
    const text = directText(el);
    if (SEMANTIC.includes(tag) && text) {
      dom.push({
        tag, depth, text: text.slice(0, MAXTEXT),
        ref: el.getAttribute('data-agent-ref') || null,
        role: el.getAttribute('role') || null,
        name: nameOf(el),
        inViewport: inView(el),
      });
    }
    for (const c of el.children) domPush(c, depth + 1);
  };
  domPush(document.body, 0);
  return {
    url: location.href, title: document.title,
    elements: out, dom,
    truncated: out.length >= MAXN, domTruncated: dom.length >= MAXN,
  };
})"#;

/// 解析快照 JS 返回值 → (elements, dom)。
pub(crate) fn parse_snapshot(value: &Value) -> Result<(Vec<OutlineNode>, Vec<DomNode>), String> {
    let mut elements = Vec::new();
    if let Some(arr) = value.get("elements").and_then(Value::as_array) {
        for n in arr {
            elements.push(OutlineNode {
                reference: n.get("ref").and_then(Value::as_str).unwrap_or_default().to_string(),
                tag: n.get("tag").and_then(Value::as_str).unwrap_or_default().to_string(),
                role: n.get("role").and_then(Value::as_str).map(str::to_string),
                name: n.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                text: n.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
                value: n.get("value").and_then(Value::as_str).map(str::to_string),
                href: n.get("href").and_then(Value::as_str).map(str::to_string),
                depth: n.get("depth").and_then(Value::as_u64).unwrap_or(0) as usize,
                selector: n.get("selector").and_then(Value::as_str).unwrap_or_default().to_string(),
                in_viewport: n.get("inViewport").and_then(Value::as_bool).unwrap_or(false),
                disabled: n.get("disabled").and_then(Value::as_bool).unwrap_or(false),
                checked: n.get("checked").and_then(Value::as_bool),
                parent_reference: n.get("parentRef").and_then(Value::as_str).map(str::to_string),
            });
        }
    }
    let mut dom = Vec::new();
    if let Some(arr) = value.get("dom").and_then(Value::as_array) {
        for n in arr {
            dom.push(DomNode {
                tag: n.get("tag").and_then(Value::as_str).unwrap_or_default().to_string(),
                depth: n.get("depth").and_then(Value::as_u64).unwrap_or(0) as usize,
                in_viewport: n.get("inViewport").and_then(Value::as_bool).unwrap_or(false),
                reference: n.get("ref").and_then(Value::as_str).map(str::to_string),
                role: n.get("role").and_then(Value::as_str).map(str::to_string),
                name: n.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                text: n.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
            });
        }
    }
    Ok((elements, dom))
}

/// 渲染一行可交互元素。
pub(crate) fn render_node(n: &OutlineNode) -> String {
    let indent = "  ".repeat(n.depth.min(12));
    let role = n.role.clone().unwrap_or_else(|| n.tag.clone());
    let mut line = format!("{indent}[{role} {}]", n.reference);
    if n.disabled {
        line.push_str(" 禁用");
    }
    if n.checked == Some(true) {
        line.push_str(" 已选");
    }
    if !n.in_viewport {
        line.push_str(" 屏外");
    }
    if !n.name.is_empty() {
        line.push_str(&format!(" \"{}\"", n.name));
    }
    if n.name.is_empty() && !n.text.is_empty() {
        line.push_str(&format!(" \"{}\"", n.text));
    }
    if let Some(v) = &n.value {
        line.push_str(&format!(" value={v:?}"));
    }
    if let Some(h) = &n.href {
        line.push_str(&format!(" → {h}"));
    }
    line
}

/// 渲染一行语义正文节点。
pub(crate) fn render_dom_node(n: &DomNode) -> String {
    let indent = "  ".repeat(n.depth.min(12));
    let role = n.role.clone().unwrap_or_else(|| n.tag.clone());
    let mut line = format!("{indent}⟨{role}");
    if let Some(r) = &n.reference {
        line.push_str(&format!(" {r}"));
    }
    line.push('⟩');
    if !n.name.is_empty() {
        line.push_str(&format!(" \"{}\"", n.name));
    }
    if !n.text.is_empty() {
        line.push_str(&format!(" {}", n.text));
    }
    if !n.in_viewport {
        line.push_str("（屏外）");
    }
    line
}

/// 与上次快照按 ref 做 diff，返回（摘要行, 明细行列表）。
pub(crate) fn diff_outline(old: &[OutlineNode], new: &[OutlineNode]) -> (String, Vec<String>) {
    let old_map: std::collections::HashMap<&str, &OutlineNode> =
        old.iter().map(|n| (n.reference.as_str(), n)).collect();
    let new_map: std::collections::HashMap<&str, &OutlineNode> =
        new.iter().map(|n| (n.reference.as_str(), n)).collect();
    let added: Vec<&OutlineNode> = new.iter().filter(|n| !old_map.contains_key(n.reference.as_str())).collect();
    let removed: Vec<&OutlineNode> = old.iter().filter(|n| !new_map.contains_key(n.reference.as_str())).collect();
    let changed: Vec<(&OutlineNode, &OutlineNode)> = new
        .iter()
        .filter_map(|n| old_map.get(n.reference.as_str()).map(|o| (o, n)))
        .filter(|(o, n)| o.text != n.text || o.name != n.name || o.value != n.value || o.href != n.href)
        .map(|(o, n)| (*o, n))
        .collect();

    if old.is_empty() {
        return ("首次快照（无基线可比）".into(), vec![]);
    }
    let summary = format!("增量: +{} 新增 -{} 移除 ~{} 变化", added.len(), removed.len(), changed.len());
    let mut detail = Vec::new();
    for n in added.iter().take(15) {
        detail.push(format!("  + {}", render_node(n).trim_start()));
    }
    for n in removed.iter().take(15) {
        detail.push(format!("  - [{}]", n.reference));
    }
    for (o, n) in changed.iter().take(15) {
        let before = o.value.clone().unwrap_or_else(|| o.text.clone());
        let after = n.value.clone().unwrap_or_else(|| n.text.clone());
        detail.push(format!("  ~ [{}] \"{}\" → \"{}\"", n.reference, truncate_str(&before, 40), truncate_str(&after, 40)));
    }
    (summary, detail)
}

pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

/// 快照：隔离世界抽结构 → diff → 渲染（elements + dom 双结构）。
/// max_elements/include_hidden 来自 snapshot 命令（对齐 ZCode 的同名参数）。
pub(crate) async fn take_snapshot(
    tab: &WebTab,
    max_chars: usize,
    max_elements: Option<usize>,
    include_hidden: bool,
) -> Result<String, String> {
    // 2026-10-08 修：max_elements 由模型直传、原先无 clamp——传 1e8 时 JS 仍遍历
    // 全量 NodeList 且每元素 getClientRects/selOf（渲染器卡死），returnByValue 的
    // 超大结果 JSON 还会整块进 Rust 堆（10s CDP 预算只限时间不限大小）。封顶 2000：
    // 交互元素到这个量级早已不可读，工具文本本就会截断。
    const MAX_ELEMENTS_CAP: usize = 2_000;
    let expr = format!(
        "{SNAPSHOT_JS}({}, {})",
        max_elements.unwrap_or(0).min(MAX_ELEMENTS_CAP),
        include_hidden
    );
    let value = tab
        .cdp
        .evaluate_in_isolated_world(ISOLATED_WORLD, &expr)
        .await
        .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e).to_string())?;
    let url = value.get("url").and_then(Value::as_str).unwrap_or("").to_string();
    let title = value.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    let truncated = value.get("truncated").and_then(Value::as_bool).unwrap_or(false);
    let dom_truncated = value.get("domTruncated").and_then(Value::as_bool).unwrap_or(false);
    let (nodes, dom) = parse_snapshot(&value)?;

    // ref → selector 映射落盘（ref 失效后兜底定位）
    for n in &nodes {
        tab.remember_selector(&n.reference, &n.selector).await;
    }

    let mut guard = tab.last_outline.lock().await;
    let (summary, detail) = diff_outline(&guard, &nodes);
    *guard = nodes.clone();
    drop(guard);

    let mut out = format!("页面: {title}（{url}）\n{summary}\n");
    if !detail.is_empty() {
        out.push_str(&detail.join("\n"));
        out.push('\n');
    }
    out.push_str("--- 可交互元素（ref 供 click/type/fill/hover/select/check 定位；「屏外」需先 scroll）---\n");
    out.push_str(&nodes.iter().map(render_node).collect::<Vec<_>>().join("\n"));
    if truncated {
        out.push_str("\n（元素过多，快照已截断——可用更精确的 ref 操作已知元素）");
    }
    if !dom.is_empty() {
        out.push_str("\n--- 页面正文 ---\n");
        out.push_str(&dom.iter().map(render_dom_node).collect::<Vec<_>>().join("\n"));
        if dom_truncated {
            out.push_str("\n（正文节点过多，已截断）");
        }
    }
    Ok(crate::context::compress(&out, max_chars))
}

// —— 元素定位（语义为主，selector 兜底） ——

/// 定位 JS 返回：{ok, x, y, tag, name, how} 或 {ok:false}。
/// 顺序：ref → selector → 文本/占位符语义匹配。
pub(crate) const LOCATE_JS: &str = r#"((ref, selector, text, label) => {
  let el = null, how = '';
  if (ref) { try { el = document.querySelector('[data-agent-ref="' + ref + '"]'); how = 'ref'; } catch (e) {} }
  if (!el && selector) { try { el = document.querySelector(selector); how = 'selector'; } catch (e) {} }
  const want = (text || label || '').trim().toLowerCase();
  if (!el && want) {
    const cands = [];
    for (const el2 of document.querySelectorAll('a,button,input,textarea,select,[role],[onclick]')) {
      const role = el2.getAttribute('role') || el2.tagName.toLowerCase();
      const t2 = (el2.innerText || el2.value || el2.getAttribute('placeholder') || el2.getAttribute('aria-label') || el2.getAttribute('title') || '').trim().toLowerCase();
      const hit = text ? (t2 === want || (t2.includes(want) && want.length >= 2)) : label ? (t2.includes(want) || (el2.placeholder || '').includes(want)) : false;
      if (hit && el2.getClientRects().length > 0) {
        let w = 0, x3 = el2;
        while (x3) { w++; x3 = x3.parentElement; }
        cands.push({ el: el2, w, role });
      }
    }
    const rank = (r) => (r === 'button' || r === 'link' || r === 'textbox' || r === 'combobox') ? 0 : 1;
    cands.sort((a, b) => (rank(a.role) - rank(b.role)) || (a.w - b.w));
    el = cands[0] ? cands[0].el : null;
    if (el) how = text ? 'text' : 'label';
  }
  if (!el) return { ok: false };
  el.scrollIntoView({ block: 'center' });
  const r = el.getBoundingClientRect();
  if (r.width <= 0 || r.height <= 0) return { ok: false };
  return {
    ok: true,
    x: Math.round(r.left + r.width / 2),
    y: Math.round(r.top + r.height / 2),
    tag: el.tagName.toLowerCase(),
    name: (el.getAttribute('aria-label') || el.getAttribute('placeholder') || el.innerText || '').trim().slice(0, 60),
    how,
  };
})"#;

/// 修饰键 → CDP 位掩码（Alt=1, Ctrl=2, Meta=4, Shift=8）。
pub(crate) fn modifier_mask(modifiers: &Option<Vec<KeyModifier>>) -> i64 {
    let Some(list) = modifiers else { return 0 };
    let mut mask = 0;
    for m in list {
        mask |= match m {
            KeyModifier::Alt => 1,
            KeyModifier::Control => 2,
            KeyModifier::Meta => 4,
            KeyModifier::Shift => 8,
        };
    }
    mask
}

/// 解析定位参数 → 视口坐标与目标描述。ref → selector（兜底）→ 文本语义 → 坐标。
pub(crate) async fn locate(
    tab: &WebTab,
    reference: &str,
    text: &str,
    label: &str,
) -> Result<(i64, i64, String), BrowserError> {
    let reference = reference.trim();
    let text = text.trim();
    let label = label.trim();
    let mut selector = String::new();
    if !reference.is_empty() {
        // ref 在页面上已失效时，用快照时存下的 selector 再试一次
        selector = tab.selector_for(reference).await.unwrap_or_default();
    }
    if !reference.is_empty() || !selector.is_empty() || !text.is_empty() || !label.is_empty() {
        let expr = format!(
            "{LOCATE_JS}({}, {}, {}, {})",
            serde_json::to_string(reference).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(&selector).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into()),
            serde_json::to_string(label).unwrap_or_else(|_| "\"\"".into())
        );
        let val = tab
            .cdp
            .evaluate_in_isolated_world(ISOLATED_WORLD, &expr)
            .await
            .map_err(|e| BrowserError::new(ErrorCode::ExecutionError, e))?;
        if val.get("ok").and_then(Value::as_bool) == Some(true) {
            let x = val.get("x").and_then(Value::as_i64).unwrap_or(0);
            let y = val.get("y").and_then(Value::as_i64).unwrap_or(0);
            let tag = val.get("tag").and_then(Value::as_str).unwrap_or("?").to_string();
            let name = val.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let how = val.get("how").and_then(Value::as_str).unwrap_or("?");
            return Ok((x, y, format!("{tag} \"{name}\"（{how} 定位）")));
        }
        if !reference.is_empty() {
            return Err(BrowserError::new(
                ErrorCode::RefNotFound,
                format!("ref {reference} 在页面上已不存在（DOM 变了）。先 browser snapshot 拿最新 ref；或给 text/label 语义定位、x/y 坐标兜底"),
            ));
        }
        return Err(BrowserError::new(
            ErrorCode::RefNotFound,
            "语义定位失败：文本/占位符没有可见匹配。可先 browser snapshot 拿最新 ref，或用坐标 x/y 兜底",
        ));
    }
    Err(BrowserError::new(
        ErrorCode::CapabilityUnsupported,
        "缺少定位参数：给 ref（快照编号）、text/label（语义匹配）或 x/y（视口坐标）之一",
    ))
}

pub(crate) async fn dispatch_click(
    tab: &WebTab,
    x: i64,
    y: i64,
    button: MouseButton,
    modifiers: i64,
) -> Result<(), String> {
    let (x, y) = (x as f64, y as f64);
    tab.cdp
        .call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseMoved", "x": x, "y": y, "modifiers": modifiers }),
        )
        .await?;
    for t in ["mousePressed", "mouseReleased"] {
        tab.cdp
            .call(
                "Input.dispatchMouseEvent",
                json!({ "type": t, "x": x, "y": y, "button": button.as_str(), "clickCount": 1, "modifiers": modifiers }),
            )
            .await?;
    }
    Ok(())
}

/// 点击后的「沉降」：多数点击不触发导航，不能死等——轮询 readyState，
/// 没出现 loading（约 400ms 内）就返回；真导航了则等到加载完成（上限 6s）。
pub(crate) async fn click_and_settle(
    tab: &WebTab,
    x: i64,
    y: i64,
    button: MouseButton,
    modifiers: i64,
) -> Result<(), String> {
    dispatch_click(tab, x, y, button, modifiers).await?;
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut navigated = false;
    let mut polls = 0u32;
    loop {
        if Instant::now() > deadline {
            break; // 兜底：导航卡住也返回（截图会拿到当前状态）
        }
        let state = tab
            .cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": "document.readyState", "returnByValue": true }),
            )
            .await
            .ok()
            .and_then(|v| v.get("result").and_then(|r| r.get("value")).and_then(Value::as_str).map(str::to_string));
        match state.as_deref() {
            // 求值失败 = 上下文正在换页（旧 context 已销毁），视为导航中
            None => navigated = true,
            Some("loading") => navigated = true,
            Some("complete") => {
                if navigated {
                    break; // 导航完成
                }
                // 连续两次 complete 且从未见 loading = 本次点击没有导航，快速返回
                if polls >= 1 {
                    break;
                }
            }
            _ => {}
        }
        polls += 1;
        tokio::time::sleep(Duration::from_millis(180)).await;
    }
    if navigated {
        // 给新页面渲染留一点时间再截图
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    tab.cdp.invalidate_worlds();
    Ok(())
}

// —— 对工具层暴露的操作（统一命令分发；P21）——

