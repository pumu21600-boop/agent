'use strict';
/*
 * pdf 技能 CLI —— 文本提取（pdf-parse）、页面操作（pdf-lib）、HTML 转 PDF（本机 Edge/Chrome）。
 * 用法：
 *   node pdf.js text <in.pdf> [页数上限]
 *   node pdf.js pages <in.pdf>
 *   node pdf.js merge <out.pdf> <in1.pdf> <in2.pdf> ...
 *   node pdf.js split <in.pdf> <输出前缀>
 *   node pdf.js fromhtml <in.html> <out.pdf>
 * 依赖解析顺序：~/.pawch-agent/skill-modules → 本脚本目录 → 当前目录。
 */
const fs = require('fs');
const path = require('path');
const os = require('os');
const { execFileSync } = require('child_process');

function need(name) {
  const bases = [
    path.join(os.homedir(), '.pawch-agent', 'skill-modules', 'node_modules'),
    path.join(__dirname, 'node_modules'),
    path.join(process.cwd(), 'node_modules'),
  ];
  for (const b of bases) {
    if (fs.existsSync(path.join(b, name))) return require(path.join(b, name));
  }
  console.error(
    `missing dependency ${name}. install once (usable from any workspace):\n` +
      `  npm install --prefix "%USERPROFILE%\\.pawch-agent\\skill-modules" ${name}\n` +
      `or in the workspace: npm install ${name}`
  );
  process.exit(2);
}

function usage() {
  console.error('usage: node pdf.js text <in.pdf> [maxPages] | pages <in.pdf> | merge <out.pdf> <in1.pdf> <in2.pdf> ... | split <in.pdf> <prefix> | edit <ops.json|-> <in.pdf> <out.pdf> | fromhtml <in.html> <out.pdf>');
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

// —— edit：pdf-lib 页面编排 + 盖章（不改已有文字——content stream 不可逆，见边界） ——
// 页序按 1-based；改完是「另存」语义，out 与 in 分开。
const FONT_CANDIDATES = [
  'C:/Windows/Fonts/simhei.ttf',
  'C:/Windows/Fonts/simsun.ttf',
  'C:/Windows/Fonts/msyh.ttf',
  'C:/Windows/Fonts/simfang.ttf',
];

// 软依赖：找不到返回 null（不得像 need 那样 exit——字体是可选能力）
function softNeed(name) {
  const bases = [
    path.join(os.homedir(), '.pawch-agent', 'skill-modules', 'node_modules'),
    path.join(__dirname, 'node_modules'),
    path.join(process.cwd(), 'node_modules'),
  ];
  for (const b of bases) {
    const p = path.join(b, name);
    if (fs.existsSync(p)) {
      try {
        return require(p);
      } catch (_e) {
        return null;
      }
    }
  }
  return null;
}

async function loadFont(pdfDoc) {
  const fontkit = softNeed('@pdf-lib/fontkit');
  if (fontkit) pdfDoc.registerFontkit(fontkit);
  for (const p of FONT_CANDIDATES) {
    if (!fs.existsSync(p)) continue;
    try {
      return await pdfDoc.embedFont(fs.readFileSync(p));
    } catch (_e) {
      // ttc 集合 pdf-lib 吃不下，试下一个
    }
  }
  return null;
}

function posOf(page, font, text, size, position) {
  const { width: w, height: h } = page.getSize();
  const tw = font.widthOfTextAtSize(text, size);
  const pad = 28;
  switch (position) {
    case 'center':
      return { x: (w - tw) / 2, y: h / 2, rotate: -Math.PI / 6 };
    case 'top':
      return { x: (w - tw) / 2, y: h - pad - size, rotate: 0 };
    case 'bottom':
      return { x: (w - tw) / 2, y: pad, rotate: 0 };
    case 'bottom-right':
      return { x: w - tw - pad, y: pad, rotate: 0 };
    case 'top-left':
      return { x: pad, y: h - pad - size, rotate: 0 };
    default:
      return { x: (w - tw) / 2, y: h / 2, rotate: -Math.PI / 6 };
  }
}

function parseColor(hex) {
  const s = String(hex || '#FF0000').replace('#', '');
  const v = s.length === 6 ? parseInt(s, 16) : 0xff0000;
  return { r: ((v >> 16) & 255) / 255, g: ((v >> 8) & 255) / 255, b: (v & 255) / 255 };
}

async function runEdit(opsPath, inFile, outFile) {
  const specText = opsPath === '-' ? fs.readFileSync(0, 'utf8') : fs.readFileSync(opsPath, 'utf8');
  const spec = JSON.parse(specText);
  if (!Array.isArray(spec.ops) || !spec.ops.length) throw new Error('ops 为空：spec 须为 {"ops":[...]}');
  const { PDFDocument, degrees, rgb } = need('pdf-lib');
  const src = await PDFDocument.load(fs.readFileSync(inFile), { ignoreEncryption: true });
  let doc;
  // extract/remove/move/insert 重组页：先铺全量基底（copyPages），后续 op 对「当前 deck」生效
  const reorder = spec.ops.some((o) => ['extract', 'remove', 'move', 'insert'].includes(o.op));
  if (reorder) {
    doc = await PDFDocument.create();
    const all = await doc.copyPages(src, src.getPageIndices());
    all.forEach((p) => doc.addPage(p));
  } else {
    doc = src;
  }

  for (const op of spec.ops) {
    if (op.op === 'extract') {
      // 语义=只留选中页：删掉其余（对当前 deck 生效）
      const keep = new Set((op.pages || []).map((n) => n - 1));
      if (!keep.size) throw new Error('extract 需要 pages');
      for (let i = doc.getPageCount() - 1; i >= 0; i--) {
        if (!keep.has(i)) doc.removePage(i);
      }
    } else if (op.op === 'remove') {
      // 倒序删，防止删一个崩一个
      const idx = (op.pages || []).map((n) => n - 1).filter((i) => i >= 0 && i < doc.getPageCount()).sort((a, b) => b - a);
      if (doc.getPageCount() - idx.length < 1) throw new Error('remove 之后不剩任何页，拒绝');
      for (const i of idx) doc.removePage(i);
    } else if (op.op === 'move') {
      const from = (op.page || 0) - 1;
      const to = (op.to || 0) - 1;
      if (!(from >= 0 && from < doc.getPageCount()) || !(to >= 0 && to < doc.getPageCount()))
        throw new Error(`move 页码越界（共 ${doc.getPageCount()} 页）`);
      const pages = doc.getPageIndices().map((i) => doc.getPage(i));
      const [moved] = pages.splice(from, 1);
      pages.splice(to, 0, moved);
      // pdf-lib 无 movePage：删全部重加（copyPages 会保内容）
      while (doc.getPageCount()) doc.removePage(0);
      for (const p of pages) doc.addPage(p);
    } else if (op.op === 'insert') {
      if (!op.from) throw new Error('insert 需要 from（源 pdf 路径）');
      const other = await PDFDocument.load(fs.readFileSync(op.from), { ignoreEncryption: true });
      const idx = (op.pages || []).map((n) => n - 1).filter((i) => i >= 0 && i < other.getPageCount());
      const at = Math.max(0, Math.min(op.at || doc.getPageCount(), doc.getPageCount()));
      const copied = await doc.copyPages(other, idx);
      copied.forEach((p, k) => doc.insertPage(at + k, p));
    } else if (op.op === 'rotate') {
      const idx = (op.pages || []).map((n) => n - 1).filter((i) => i >= 0 && i < doc.getPageCount());
      for (const i of idx) {
        const cur = doc.getPage(i).getRotation().angle || 0;
        doc.getPage(i).setRotation(degrees((parseInt(op.deg || 90, 10) + cur) % 360));
      }
    } else if (op.op === 'stamp') {
      const pages = (op.pages || doc.getPageIndices().map((i) => i + 1)).map((n) => n - 1).filter((i) => i >= 0 && i < doc.getPageCount());
      const font = await loadFont(doc);
      if (!font) throw new Error('找不到可嵌入的中文字体（simhei/simsun/msyh 均缺失）——盖章改用「修订文字叠加」无法完成，如实报告');
      const c = parseColor(op.color);
      const size = Number(op.size || 24);
      for (const i of pages) {
        const page = doc.getPage(i);
        const { x, y, rotate } = posOf(page, font, String(op.text), size, op.position);
        page.drawText(String(op.text), {
          x,
          y,
          size,
          font,
          color: rgb(c.r, c.g, c.b),
          opacity: Number(op.opacity != null ? op.opacity : 0.35),
          rotate: degrees((rotate * 180) / Math.PI),
        });
      }
    } else {
      throw new Error(`未知 op "${op.op}"（受支持：extract/remove/move/insert/rotate/stamp）`);
    }
  }

  fs.writeFileSync(outFile, await doc.save());
  console.log(`OK edit ${outFile} (${doc.getPageCount()} pages${reorder ? ', rebuilt' : ''})`);
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
  } else if (cmd === 'edit') {
    const [opsPath, inFile, outFile] = rest;
    if (!opsPath || !inFile || !outFile) usage();
    await runEdit(opsPath, inFile, outFile);
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
