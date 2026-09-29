// 纯格式化/判定助手（P19 App.svelte 拆分）：无组件状态依赖，App 与各视图组件共用。

export function fmtBytes(b) {
  if (!b) return '0B';
  if (b < 1024) return `${b}B`;
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)}KB`;
  return `${(b / (1024 * 1024)).toFixed(1)}MB`;
}

export function relTime(iso) {
  if (!iso) return '';
  const ms = Date.now() - new Date(iso).getTime();
  const m = Math.floor(ms / 60000);
  if (m < 1) return '刚刚';
  if (m < 60) return `${m}分钟前`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}小时前`;
  const d = Math.floor(h / 24);
  if (d < 30) return `${d}天前`;
  return new Date(iso).toLocaleDateString();
}

export function fmtTok(n) {
  if (n >= 10000) return (n / 10000).toFixed(1).replace(/\.0$/, '') + '万';
  return String(n);
}

export function norm(url) {
  return (url || '').trim().replace(/\/+$/, '').toLowerCase();
}

export function configLabel(c) {
  return c.name || '未命名';
}

// P23.1：inferCapabilities（按模型名猜图片/视频/音频能力）已随智能预测机制一并删除——
// 聚合渠道上名字与真实能力对不上；能力一律以设置里手动勾选为准。

export function gitStatusLabel(s) {
  const c = String(s || '');
  if (c.includes('?')) return '未跟踪';
  if (c.includes('U')) return '冲突';
  if (c.includes('R')) return '重命名';
  if (c.includes('A')) return '新增';
  if (c.includes('D')) return '删除';
  if (c.includes('M')) return '修改';
  return c || '改动';
}

export function webNormalizeUrl(u) {
  const s = (u || '').trim();
  if (!s) return '';
  if (s === 'about:blank' || /^(https?|file):\/\//.test(s)) return s;
  return 'https://' + s;
}
// —— P6 恢复与续命 ——
export function contentText(c) {
  if (typeof c === 'string') return c;
  if (Array.isArray(c)) return c.map((p) => p.text || '').join('');
  return '';
}
export function normPath(p) {
  return (p || '').replace(/[\\/]+$/, '');
}
// 时间用 DSH 短格式：4天 / 3小时 / 刚刚
export function relShort(iso) {
  if (!iso) return '';
  const ms = Date.now() - new Date(iso).getTime();
  const m = Math.floor(ms / 60000);
  if (m < 1) return '刚刚';
  if (m < 60) return `${m}分钟`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}小时`;
  return `${Math.floor(h / 24)}天`;
}
// 档位名兜底（与 core::rules::RiskLevel::label 对齐；正常情况 SSE 已带 risk_label）
export function riskLabelFor(risk) {
  return risk >= 2 ? '高危操作' : risk === 1 ? '常规改动' : '安全';
}
