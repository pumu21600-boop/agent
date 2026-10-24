// 极简 markdown → HTML（聊天助手消息渲染；2026-10-04 用户拍板「星号太多太乱」）。
// 安全第一：先整体转义 HTML，再在已转义文本上做结构替换——{@html} 吃到的串里
// 任何原始 < > 都已是 &lt; &gt;，模型输出/插件消息里的 HTML 永远不会被执行。
// 支持：``` 围栏代码块、`行内代码`、**粗体**、*斜体*、### 标题、- 列表、1. 有序列表、
// > 引用、--- 分隔线、GFM 表格（| 表头 | + | --- | 分隔行）、[文字](http(s)链接)。链接只放行 http/https，其余协议剥成纯文本。
export function md(src) {
  if (!src) return '';
  // 1) 整体转义（引号也转，防属性注入）
  let t = String(src)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
  // 2) 围栏代码块（先把代码内容抠出来占位，别被后面的行内规则误伤）
  const blocks = [];
  t = t.replace(/```(\w*)\n?([\s\S]*?)```/g, (_, lang, code) => {
    blocks.push(`<pre class="md-pre"><code>${code.replace(/\n$/, '')}</code></pre>`);
    return `\u0000${blocks.length - 1}\u0000`;
  });
  // 3) 行内代码（同样占位）
  const codes = [];
  t = t.replace(/`([^`\n]+)`/g, (_, c) => {
    codes.push(`<code class="md-code">${c}</code>`);
    return `\u0001${codes.length - 1}\u0001`;
  });
  // 4) 标题（### 以内；聊天里没 h1/h2）
  t = t
    .replace(/^######\s+(.+)$/gm, '<h6 class="md-h">$1</h6>')
    .replace(/^#####\s+(.+)$/gm, '<h5 class="md-h">$1</h5>')
    .replace(/^####\s+(.+)$/gm, '<h4 class="md-h">$1</h4>')
    .replace(/^###\s+(.+)$/gm, '<h4 class="md-h">$1</h4>')
    .replace(/^##\s+(.+)$/gm, '<h4 class="md-h">$1</h4>')
    .replace(/^#\s+(.+)$/gm, '<h4 class="md-h">$1</h4>');
  // 5) 分隔线
  t = t.replace(/^---+\s*$/gm, '<hr class="md-hr" />');
  // 6) 粗体/斜体（粗体在先，防 ** 被 * 规则吃掉）
  t = t
    .replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>')
    .replace(/(^|[^*\w])\*([^*\n]+)\*(?=[^*\w]|$)/g, '$1<em>$2</em>');
  // 7) 链接：放行 http/https（外链）与 /api/workspace/（站内预览，宿主拦截跳预览标签）
  t = t.replace(/\[([^\]\n]+)\]\(([^)\s]+)\)/g, (m, text, url) => {
    if (/^https?:\/\//i.test(url)) return `<a class="md-a" href="${url}" target="_blank" rel="noreferrer">${text}</a>`;
    if (url.startsWith('/api/workspace/')) return `<a class="md-a doc-cite" href="${url}">${text}</a>`;
    return text;
  });
  // 8) GFM 表格：首行含 |、次行是 | --- | 分隔行（:--- 左对齐只作识别、对齐样式不做）
  t = t.replace(/(?:^[ \t]*\|?.*\|.*\n?)+/gm, (block) => {
    const lines = block.trimEnd().split('\n').map((l) => l.trim());
    if (lines.length < 2 || !/^\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/.test(lines[1])) return block;
    const cells = (l) => l.replace(/^\|/, '').replace(/\|$/, '').split('|').map((c) => c.trim());
    const head = cells(lines[0]).map((c) => `<th>${c}</th>`).join('');
    const rows = lines.slice(2).filter((l) => /\|/.test(l))
      .map((l) => `<tr>${cells(l).map((c) => `<td>${c}</td>`).join('')}</tr>`).join('');
    return `<table class="md-table"><thead><tr>${head}</tr></thead><tbody>${rows}</tbody></table>`;
  });
  // 9) 列表（- / * / + 与 1. 两种；连续行整组成 ul/ol）
  t = t.replace(/(?:^[ \t]*[-*+]\s+.+\n?)+/gm, (block) => {
    const items = block.trimEnd().split('\n').map((l) => `<li>${l.replace(/^[ \t]*[-*+]\s+/, '')}</li>`).join('');
    return `<ul class="md-ul">${items}</ul>`;
  });
  t = t.replace(/(?:^[ \t]*\d+[.、]\s+.+\n?)+/gm, (block) => {
    const items = block.trimEnd().split('\n').map((l) => `<li>${l.replace(/^[ \t]*\d+[.、]\s+/, '')}</li>`).join('');
    return `<ol class="md-ol">${items}</ol>`;
  });
  // 10) 引用
  t = t.replace(/(?:^&gt;\s?.+\n?)+/gm, (block) => {
    const lines = block.trimEnd().split('\n').map((l) => l.replace(/^&gt;\s?/, '')).join('<br />');
    return `<blockquote class="md-quote">${lines}</blockquote>`;
  });
  // 11) 段落与换行：双换行分段，单换行转 <br>
  t = t
    .split(/\n{2,}/)
    .map((p) => (p.trim() && !/^<(h[1-6]|ul|ol|pre|hr|blockquote|div|table)\b/i.test(p.trim()) ? `<p class="md-p">${p.trim()}</p>` : p.trim()))
    .join('');
  t = t.replace(/\n/g, '<br />');
  // 12) 还原占位
  t = t.replace(/\u0000(\d+)\u0000/g, (_, i) => blocks[+i]).replace(/\u0001(\d+)\u0001/g, (_, i) => codes[+i]);
  return t;
}
