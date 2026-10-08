// 前端 fetch 静态体检：抓「fetch 之后不解析 JSON」的漏网点。
//
// 为什么值得一个脚本：这个 bug 类在本仓已经复发两次（网页面板改造丢了三处，
// 2026-09-24 又扫出十几处），而且两种死法都很难看——
//   ① 把 fetch 的 Response 当数据用（r.path / r.extensions / r.title 恒 undefined）：
//      界面静默显示空值或「已保存到 undefined」，没有任何报错；
//   ② 把 Response 丢进 postMessage：结构化克隆直接抛
//      "Failed to execute 'postMessage' on 'Window': Response object could not be cloned."，
//     插件面板整个瘫掉，插件侧只看到这句天书（用量面板与价格页就是这么瘫的）。
// 判据很粗但够用：每个 fetch( 调用点，往后 16 行内必须出现 .json()（或 .text()），
// 否则报错；确实不该解析的（流式 SSE、发了就忘的 DELETE、apiJson 自身）进白名单并写理由。
//
// 用法：node scripts/check_fetch_json.mjs   （退出码非 0 = 有漏网，CI/提交前跑一遍）
import fs from 'node:fs';
import path from 'node:path';

const ROOT = path.resolve(import.meta.dirname, '..');
const SRC = path.join(ROOT, 'web', 'src');
const LOOKAHEAD = 16;

// 故意不解析的调用点：正则匹配**那一行**，理由写清楚（免得后人抄着抄着又漏）。
const ALLOW = [
  [/resp = await fetch\(path, opts\)/, 'apiJson 自身：下面统一走 resp.text() 再 JSON.parse'],
  [/await fetch\('\/api\/chat'/, 'SSE 流式响应：走 res.body.getReader()，不能整体 json()'],
  [/await fetch\(`\/api\/session\?id=/, '发了就忘的 DELETE，不读响应体'],
  [/await fetch\('\/api\/sessions\/batch'/, '发了就忘，成功与否都由后续 loadSessions 体现'],
  [/await fetch\('\/api\/workspace\/remove'/, '发了就忘，后续 loadSettings 会反映'],
  [/await fetch\('\/api\/sessions\/meta'/, '发了就忘，后续 loadSessions 会反映'],
  [/await fetch\('\/api\/approvals'/, '发了就忘（后端另有 approval_decided 事件回传）'],
  [/fetch\('\/cancel'/, '会话级停止：发了就忘，效果由本地 controller.abort() 立即体现'],
  [/fetch\('\/api\/sessions\/meta'/, '发了就忘，.then 里跟着 loadSessions 刷新'],
  [/fetch\('\/api\/workspace\/alias'/, '发了就忘，.then 里跟着 loadSettings 刷新'],
  // 2026-10-08 修：原正则匹配任意以 .then((r) => { 结尾的行——会把未来任何
  // 「真的忘了 .json()」的调用点洗白（正是本脚本要抓的 bug 类）。收窄到
  // workspace/raw 端点本身：只有它才走二进制取回（arrayBuffer/blob）。
  [/workspace\/raw.*\)\.then\(\(r\) => \{$/, 'workspace/raw 二进制取回（图片/pdf/xlsx 预览）：then 体内读 arrayBuffer()/blob()，16 行扫描窗看不见'],
  [/await fetch\('\/api\/ui\/state'/, 'state 上报：发了就忘，故意不读响应体（多窗口后报者覆盖前报者）'],
];

function walk(dir, out = []) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p, out);
    else if (/\.(svelte|js|ts)$/.test(e.name)) out.push(p);
  }
  return out;
}

const bad = [];
let checked = 0;
for (const file of walk(SRC)) {
  const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/);
  lines.forEach((line, i) => {
    if (!/\bfetch\(/.test(line)) return;
    if (line.trim().startsWith('//') || line.trim().startsWith('*')) return;
    checked += 1;
    const win = lines.slice(i, i + LOOKAHEAD).join('\n');
    if (/\.json\(\)|\.text\(\)/.test(win)) return;
    const rel = path.relative(ROOT, file).replace(/\\/g, '/');
    if (ALLOW.some(([re]) => re.test(line))) return;
    bad.push(`${rel}:${i + 1}  ${line.trim().slice(0, 100)}`);
  });
}

if (bad.length) {
  console.error(`fetch 漏解析 ${bad.length} 处（往后 ${LOOKAHEAD} 行内没有 .json()/.text()）：`);
  for (const b of bad) console.error('  ' + b);
  console.error('\n修法：接 .then((r) => r.json())，或确实不读响应体就加进 ALLOW 并写理由。');
  process.exit(1);
}
console.log(`fetch 体检通过：${checked} 个调用点全部有解析（或已在白名单）。`);
