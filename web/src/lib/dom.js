// DOM/CSS 纯助手(P19 范式 App.svelte 拆分):无组件状态依赖,App 与各视图组件共用。
// 给每条顶层规则的选择器加 #app 前缀：压过 Svelte scoped 样式（.composer.s-xxx 两级类名）；
// @media/@keyframes 等整块原样保留
export function escalateCss(css) {
  let out = '';
  let i = 0;
  const n = css.length;
  while (i < n) {
    const brace = css.indexOf('{', i);
    if (brace === -1) {
      out += css.slice(i);
      break;
    }
    const selector = css.slice(i, brace);
    if (selector.trim().startsWith('@')) {
      let depth = 1;
      let j = brace + 1;
      while (j < n && depth > 0) {
        if (css[j] === '{') depth++;
        else if (css[j] === '}') depth--;
        j++;
      }
      out += css.slice(i, j);
      i = j;
    } else {
      const prefixed = selector
        .split(',')
        .map((s) => {
          const t = s.trim();
          return t ? `#app ${t}` : t;
        })
        .join(', ');
      out += `${prefixed}{`;
      // 跳过这条规则的声明块，直到它的关闭 '}'（声明不嵌套）
      const close = css.indexOf('}', brace + 1);
      const end = close === -1 ? n : close + 1;
      out += css.slice(brace + 1, end);
      i = end;
    }
  }
  return out;
}
// 元素别名表：语义名 → 稳定选择器（Svelte scoped 类名跨构建不稳定，禁止用原始类名当选择器）。
// 每个别名可带 pick：命中大容器时进一步收敛到具体控件，避免"一点按钮选中整条底板"。
//  - 'self'：只在点中容器自身（padding/空隙）时才算容器，点中子元素时继续往下落
//  - 选择器：命中容器后，在容器内按该选择器找离点击最近的祖先，找到就用它
export const ELEMENT_ALIASES = [
  { sel: '.composer', name: '输入框', pick: 'self' },
  { sel: '.composer-input', name: '输入文本区' },
  { sel: '.composer-bar', name: '输入框工具栏', pick: 'self' },
  { sel: '.icon-btn', name: '图标按钮' },
  { sel: '.pill', name: '工具栏胶囊按钮' },
  { sel: '.send-btn', name: '发送按钮' },
  { sel: '.ctx-ring', name: '上下文圆环' },
  { sel: '.user-bubble', name: '用户消息气泡' },
  { sel: '.asst-bubble', name: 'AI 消息气泡', pick: 'self' },
  { sel: '.bubble', name: '消息气泡', pick: 'self' },
  { sel: '.error-bubble', name: '错误消息气泡' },
  { sel: '.tool-card', name: '工具卡' },
  { sel: '.tool-preview', name: '工具输出（代码块）' },
  { sel: '.tool-head', name: '工具卡头部' },
  { sel: '.tool-badge', name: '工具状态徽章' },
  { sel: '.tool-name', name: '工具名' },
  { sel: '.tool-args', name: '工具参数' },
  { sel: '.thinking', name: '思考折叠块', pick: 'self' },
  { sel: '.thinking-body', name: '思考内容' },
  { sel: '.approval-box', name: '审批框' },
  { sel: '.term-panel', name: '终端面板', pick: 'self' },
  { sel: '.term-head', name: '终端标题栏' },
  { sel: '.term-title', name: '终端标题' },
  { sel: '.term-hint', name: '终端提示文字' },
  { sel: '.term-clear', name: '终端清空按钮' },
  { sel: '.term-close', name: '终端关闭按钮' },
  { sel: '.term-in', name: '终端输入栏' },
  { sel: '.term-out', name: '终端输出区' },
  { sel: '.term-line', name: '终端输出行' },
  { sel: '.term-prompt', name: '终端命令提示符' },
  { sel: '.term-text', name: '终端输出文本' },
  { sel: '.cp-bar', name: '改动条', pick: 'self' },
  { sel: '.cp-btn', name: '改动条按钮' },
  { sel: '.new-chat', name: '新会话按钮' },
  { sel: '.chat-head', name: '顶栏', pick: 'self' },
  { sel: '.right-panel', name: '产物面板', pick: 'self' },
  { sel: '.rp-tabs', name: '产物面板标签栏', pick: 'self' },
  { sel: '.rp-tabs button', name: '产物面板标签' },
  { sel: '.rp-body', name: '产物面板内容区' },
  { sel: '.rp-empty', name: '产物面板空提示' },
  { sel: '.rp-refresh', name: '产物刷新按钮' },
  { sel: '.rp-file', name: '产物文件条目' },
  { sel: '.rp-preview', name: '预览代码块' },
  { sel: '.rp-preview-path', name: '预览文件路径' },
  { sel: '.modal.settings-modal', name: '设置弹窗', pick: 'self' },
  { sel: '.settings-nav button', name: '设置导航按钮' },
  { sel: '.theme-opt', name: '主题选项按钮' },
  { sel: '.agents-editor', name: 'CSS 注入框' },
  { sel: '.field input', name: '设置输入框' },
  { sel: '.field select', name: '设置下拉框' },
  { sel: '.btn', name: '按钮' },
  { sel: '.csel', name: '下拉框' },
  { sel: '.csel-caret', name: '下拉箭头' },
  { sel: '.csel-menu', name: '下拉菜单', pick: 'self' },
  { sel: '.csel-item', name: '下拉选项' },
  { sel: '.chip', name: '配置标签' },
  { sel: '.sidebar', name: '侧栏', pick: 'self' },
  { sel: '.side-head', name: '侧栏头部', pick: 'self' },
  { sel: '.row-btn', name: '侧栏工具按钮' },
  { sel: '.conv', name: '会话条目' },
  { sel: '.conv-title', name: '会话标题' },
  { sel: '.conv-time', name: '会话时间' },
  { sel: '.conv-more', name: '会话更多按钮' },
  { sel: '.proj', name: '项目行' },
  { sel: '.proj-name', name: '项目名' },
  { sel: '.foot-btn', name: '侧栏底部按钮' },
  { sel: '.cp-dock', name: '改动条容器', pick: 'self' },
  { sel: '.cp-undo', name: '撤销按钮' },
  { sel: '.ctx-wrap', name: '上下文圆环容器', pick: 'self' },
  { sel: '.ctx-card', name: '上下文明细卡' },
  { sel: '.head-btn', name: '顶栏按钮' },
  { sel: '.user-text', name: '用户消息文字' },
  { sel: '.bar-dropdown', name: '下拉菜单', pick: 'self' },
  { sel: '.bar-item', name: '下拉菜单项' },
];
// 取色/透明度解析：同时支持 rgb()/rgba() 与 Chromium 解析 color-mix 后给出的
// color(srgb r g b / a)（分量是 0-1 小数）。取不出返回 null，由调用方决定不存该属性，
// 禁止再兜底成 #000000（那是黑底叠层的直接来源）
export function parseCssColor(c) {
  const s = String(c || '').trim();
  let m = s.match(/^rgba?\(\s*([\d.]+)[,\s]+([\d.]+)[,\s]+([\d.]+)(?:\s*[,/]\s*([\d.]+%?))?\s*\)$/);
  if (m) {
    const a = m[4] == null ? 1 : m[4].endsWith('%') ? parseFloat(m[4]) / 100 : parseFloat(m[4]);
    return { r: +m[1], g: +m[2], b: +m[3], a };
  }
  m = s.match(/^color\(srgb\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)(?:\s*\/\s*([\d.]+))?\s*\)$/);
  if (m) {
    return { r: Math.round(+m[1] * 255), g: Math.round(+m[2] * 255), b: Math.round(+m[3] * 255), a: m[4] == null ? 1 : +m[4] };
  }
  return null;
}
export function rgbToHex(rgb) {
  const c = parseCssColor(rgb);
  if (!c) return null;
  return '#' + [c.r, c.g, c.b].map((x) => Math.max(0, Math.min(255, Math.round(x))).toString(16).padStart(2, '0')).join('');
}
// 解析不出的按不透明处理
export function rgbAlpha(rgb) {
  const c = parseCssColor(rgb);
  return c ? Math.round(c.a * 100) : 100;
}
// 未登记元素：生成结构选择器。优先用稳定类名锚点（.icon-btn/.pill/.btn 等），
// 比纯 tag:nth-of-type 链更抗排序变化；实在没有类名才回落序号链（止于 #app）
export function structuralSelector(el) {
  const clsOf = (node) => {
    const cls = typeof node.className === 'string' ? node.className : (node.getAttribute && node.getAttribute('class')) || '';
    return cls.split(/\s+/).filter((c) => c && !/^s-[A-Za-z0-9_-]+$/.test(c));
  };
  // 自身或近祖先有稳定类名：直接用 ".类名" 锚定（多类取首个非 svelte 作用域类）
  let cur = el;
  for (let up = 0; up < 3 && cur && cur.id !== 'app'; up++) {
    const stable = clsOf(cur).find((c) => /^(icon-btn|pill|btn|chip|bar-item|conv|proj|row-btn|foot-btn|theme-opt|agents-editor|ctx-ring|ctx-card|send-btn|new-chat|composer-input|tool-card|tool-preview|tool-head|thinking-body|approval-box|cp-bar|cp-undo|head-btn|user-text|proj-name|conv-title)$/.test(c));
    if (stable) return `.${stable}`;
    cur = cur.parentElement;
  }
  const parts = [];
  cur = el;
  while (cur && cur.id !== 'app') {
    if (cur.id && cur !== el) {
      parts.unshift(`#${CSS.escape(cur.id)}`);
      return parts.join(' > ');
    }
    const parent = cur.parentElement;
    if (!parent) break;
    const sibs = [...parent.children].filter((c) => c.tagName === cur.tagName);
    let seg = cur.tagName.toLowerCase();
    if (sibs.length > 1) seg += `:nth-of-type(${sibs.indexOf(cur) + 1})`;
    parts.unshift(seg);
    cur = parent;
  }
  return parts.join(' > ');
}
// —— 单元素定位（修"改一处、同类全改"）：给具体元素盖一个 data-mt 唯一标记，
// 规则写成 #app [data-mt="mtXXXX"]，只作用这一个元素；Svelte 重渲染会重建节点丢标记，
// 所以存下"从 #app 起的孩子序号路径"，每次更新后按路径找回并补盖 ——
export function pathOf(el) {
  const parts = [];
  let cur = el;
  while (cur && cur.id !== 'app') {
    const parent = cur.parentElement;
    if (!parent) break;
    parts.unshift(`${cur.tagName.toLowerCase()}:${[...parent.children].indexOf(cur)}`);
    cur = parent;
  }
  return parts.join('/');
}
export function findByPath(path) {
  let cur = document.getElementById('app');
  for (const seg of String(path || '').split('/').filter(Boolean)) {
    const i = seg.lastIndexOf(':');
    const tag = seg.slice(0, i);
    const idx = Number(seg.slice(i + 1));
    if (!cur || !cur.children[idx]) return null;
    cur = cur.children[idx];
    if (tag && cur.tagName.toLowerCase() !== tag) return null;
  }
  return cur;
}
export function makeUid() {
  return 'mt' + Math.random().toString(36).slice(2, 8);
}
export function hexWithAlpha(hex, opacityPct) {
  const m = /^#([0-9a-f]{6})$/i.exec(hex || '');
  if (!m) return hex;
  const a = Math.min(1, Math.max(0, (opacityPct ?? 100) / 100));
  const r = parseInt(m[1].slice(0, 2), 16);
  const g = parseInt(m[1].slice(2, 4), 16);
  const b = parseInt(m[1].slice(4, 6), 16);
  return `rgba(${r}, ${g}, ${b}, ${a})`;
}
// 微调规则 → CSS（拼在手动 CSS 之前，手写 CSS 永远有最终话语权）；
// 只输出当初亲手改过的属性（有键才输出），没动过的背景/颜色不会凭空出现。
// background 用 background-color 而非简写，避免盖掉原有的渐变/图片/transparent 语义
export function microCss(tweaks) {
  let out = '';
  for (const t of Object.values(tweaks || {})) {
    if (!t || !t.sel) continue;
    const decls = [];
    if (t.color) decls.push(`color: ${t.color}`);
    if (t.bg) decls.push(`background-color: ${hexWithAlpha(t.bg, t.bgOpacity ?? 100)}`);
    if (t.border) decls.push(`border-style: solid`, `border-width: ${t.border}`);
    if (t.borderColor) decls.push(`border-color: ${t.borderColor}`);
    if (t.blur != null && String(t.blur) !== '' && (parseInt(t.blur, 10) || 0) !== 0) decls.push(`backdrop-filter: blur(${parseInt(t.blur, 10) || 0}px)`);
    if (t.radius) decls.push(`border-radius: ${t.radius}`);
    if (t.fontSize) decls.push(`font-size: ${t.fontSize}`);
    if (!decls.length) continue;
    // 单个元素：用 data-mt 唯一标记；同类全部：用元素选择器
    if (t.uid) out += `/* 微调:${t.name || t.sel} · 仅此元素 */\n#app [data-mt="${t.uid}"] { ${decls.join('; ')} }\n`;
    else out += `/* 微调:${t.name || t.sel} · 同类全部 */\n#app ${t.sel} { ${decls.join('; ')} }\n`;
  }
  return out;
}
