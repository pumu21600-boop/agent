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
    `missing dependency ${name}. install once (usable from any workspace):\n` +
      `  npm install --prefix "%USERPROFILE%\\.agent\\skill-modules" ${name}\n` +
      `or in the workspace: npm install ${name}`
  );
  process.exit(2);
}

function usage() {
  console.error('usage: node pdf.js text <in.pdf> [maxPages] | pages <in.pdf> | merge <out.pdf> <in...> | split <in.pdf> <prefix> | fromhtml <in.html> <out.pdf>');
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
    console.log(`pages(all): ${doc.numpages}, extracted(first ${max}), chars: ${text.length}`);
    if (!text) {
      console.error('⚠️ EMPTY TEXT: scanned/image-only PDF (no text layer). Report honestly, do not invent content.');
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
    console.log(`OK merge ${out} (${merged.getPageCount()} pages from ${inputs.length} files)`);
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
    console.log(`OK split into ${src.getPageCount()} files (${prefix}-1.pdf ... ${prefix}-${src.getPageCount()}.pdf)`);
  } else if (cmd === 'fromhtml') {
    const html = rest[0];
    const out = rest[1];
    if (!html || !out) usage();
    const browser = findBrowser();
    if (!browser) {
      console.error('no Edge/Chrome/WebView2 runtime found (set AGENT_BROWSER_PATH). Cannot convert HTML; report honestly.');
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
      console.error('browser ran but produced no PDF; check that the HTML renders. Report honestly.');
      process.exit(1);
    }
    console.log(`OK fromhtml ${out} (${fs.statSync(absOut).size} bytes, browser: ${path.basename(browser)})`);
  } else {
    usage();
  }
}

main().catch((e) => {
  console.error(`FAIL: ${e.message}`);
  process.exit(1);
});
