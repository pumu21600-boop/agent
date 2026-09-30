'use strict';
/*
 * pdf 技能 CLI —— 文本提取（pdf-parse）、页面操作（pdf-lib）、HTML 转 PDF（本机 Edge/Chrome）。
 * 用法：
 *   node pdf.js text <in.pdf> [页数上限]
 *   node pdf.js pages <in.pdf>
 *   node pdf.js merge <out.pdf> <in1.pdf> <in2.pdf> ...
 *   node pdf.js split <in.pdf> <输出前缀>
 *   node pdf.js fromhtml <in.html> <out.pdf>
 * 依赖解析顺序：~/.agent/skill-modules → 本脚本目录 → 当前目录。
 */
const fs = require('fs');
const path = require('path');
const os = require('os');
const { execFileSync } = require('child_process');

function need(name) {
  const bases = [
    path.join(os.homedir(), '.agent', 'skill-modules', 'node_modules'),
    path.join(__dirname, 'node_modules'),
    path.join(process.cwd(), 'node_modules'),
  ];
  for (const b of bases) {
    if (fs.existsSync(path.join(b, name))) return require(path.join(b, name));
  }
  console.error(
    `缺少依赖 ${name}。先装一次（装在任何工作区都能用）：\n` +
      `  npm install --prefix "%USERPROFILE%\\.agent\\skill-modules" ${name}\n` +
      `或直接在工作目录: npm install ${name}`
  );
  process.exit(2);
}

function usage() {
  console.error('用法: node pdf.js text <in.pdf> [maxPages] | pages <in.pdf> | merge <out.pdf> <in...> | split <in.pdf> <prefix> | fromhtml <in.html> <out.pdf>');
  process.exit(1);
}

// 浏览器发现链与产品内置浏览器一致：AGENT_BROWSER_PATH → 标准路径 → WebView2 运行时
function findBrowser() {
  const env = process.env.AGENT_PDF_BROWSER || process.env.AGENT_BROWSER_PATH;
  const pf = process.env.ProgramFiles || 'C:\\Program Files';
  const pf86 = process.env['ProgramFiles(x86)'] || 'C:\\Program Files (x86)';
  const lf = process.env.LOCALAPPDATA || '';
  const cands = [];
  if (env) cands.push(env);
  cands.push(
    path.join(pf86, 'Microsoft', 'Edge', 'Application', 'msedge.exe'),
    path.join(pf, 'Microsoft', 'Edge', 'Application', 'msedge.exe'),
    path.join(pf, 'Google', 'Chrome', 'Application', 'chrome.exe'),
    path.join(pf86, 'Google', 'Chrome', 'Application', 'chrome.exe'),
    path.join(lf, 'Google', 'Chrome', 'Application', 'chrome.exe')
  );
  for (const base of [pf86, pf]) {
    for (const prefix of ['Microsoft\\EdgeCore', 'Microsoft\\EdgeWebKit\\Application']) {
      const dir = path.join(base, prefix);
      let entries = [];
      try {
        entries = fs.readdirSync(dir, { withFileTypes: true });
      } catch {
        continue;
      }
      for (const e of entries) {
        if (e.isDirectory() && /^\d/.test(e.name)) {
          const exe = path.join(dir, e.name, 'msedge.exe');
          if (fs.existsSync(exe)) cands.push(exe);
        }
      }
    }
  }
  return cands.find((p) => fs.existsSync(p));
}

async function main() {
  const [cmd, ...rest] = process.argv.slice(2);
  if (cmd === 'text') {
    const [file, maxArg] = rest;
    if (!file) usage();
    const pdfParse = need('pdf-parse');
    const buf = fs.readFileSync(file);
    const max = parseInt(maxArg || '30', 10) || 30;
    const doc = await pdfParse(buf, { max });
    const text = doc.text.trim();
    console.log(`页数(全文档): ${doc.numpages}，本次提取前 ${max} 页，字符数: ${text.length}`);
    if (!text) {
      console.error('⚠️ 提取结果为 0 字：这多半是扫描件/图片型 PDF（没有文字层），如实告知用户，不要编造内容。');
      return;
    }
    console.log('---');
    console.log(text);
  } else if (cmd === 'pages') {
    const file = rest[0];
    if (!file) usage();
    const { PDFDocument } = need('pdf-lib');
    const doc = await PDFDocument.load(fs.readFileSync(file), { ignoreEncryption: true });
    console.log(JSON.stringify({ pages: doc.getPageCount(), title: doc.getTitle() || '', author: doc.getAuthor() || '', producer: doc.getProducer() || '' }));
  } else if (cmd === 'merge') {
    const [out, ...inputs] = rest;
    if (!out || inputs.length < 1) usage();
    const { PDFDocument } = need('pdf-lib');
    const merged = await PDFDocument.create();
    for (const f of inputs) {
      const src = await PDFDocument.load(fs.readFileSync(f), { ignoreEncryption: true });
      const pages = await merged.copyPages(src, src.getPageIndices());
      pages.forEach((p) => merged.addPage(p));
    }
    fs.writeFileSync(out, await merged.save());
    console.log(`已生成 ${out}（${merged.getPageCount()} 页，来自 ${inputs.length} 个文件）`);
  } else if (cmd === 'split') {
    const [file, prefix] = rest;
    if (!file || !prefix) usage();
    const { PDFDocument } = need('pdf-lib');
    const src = await PDFDocument.load(fs.readFileSync(file), { ignoreEncryption: true });
    for (let i = 0; i < src.getPageCount(); i++) {
      const one = await PDFDocument.create();
      const [p] = await one.copyPages(src, [i]);
      one.addPage(p);
      fs.writeFileSync(`${prefix}-${i + 1}.pdf`, await one.save());
    }
    console.log(`已拆分为 ${src.getPageCount()} 个文件（${prefix}-1.pdf … ${prefix}-${src.getPageCount()}.pdf）`);
  } else if (cmd === 'fromhtml') {
    const html = rest[0];
    const out = rest[1];
    if (!html || !out) usage();
    const browser = findBrowser();
    if (!browser) {
      console.error('没找到 Edge/Chrome/WebView2 运行时（设 AGENT_BROWSER_PATH 指到浏览器 exe 可用）：HTML 转 PDF 无法进行，如实告知用户。');
      process.exit(1);
    }
    const absHtml = path.resolve(html);
    const absOut = path.resolve(out);
    execFileSync(
      browser,
      [`--headless=new`, '--disable-gpu', '--no-pdf-header-footer', `--print-to-pdf=${absOut}`, `file:///${absHtml.replace(/\\/g, '/')}`],
      { stdio: 'ignore', timeout: 60000 }
    );
    if (!fs.existsSync(absOut)) {
      console.error('浏览器跑了但没产出 PDF，检查 HTML 是否可渲染；如实告知用户。');
      process.exit(1);
    }
    console.log(`已生成 ${out}（${fs.statSync(absOut).size} 字节，浏览器: ${path.basename(browser)}）`);
  } else {
    usage();
  }
}

main().catch((e) => {
  console.error(`失败: ${e.message}`);
  process.exit(1);
});
