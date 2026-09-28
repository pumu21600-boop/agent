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
