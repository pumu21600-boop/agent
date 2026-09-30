'use strict';
/*
 * docx 技能 CLI —— .docx 生成（docx-js）、结构化读出（jszip 解包 OOXML）、字体核验。
 * 用法：
 *   node docx.js nodes <in.docx>            # 读成可编辑 spec JSON（改完喂回 make）
 *   node docx.js read  <in.docx>            # 读成 HTML 结构视图（给人看）
 *   node docx.js fonts <in.docx>            # 核验字体/字号明细（改字体后验这个）
 *   node docx.js make  <spec.json|-> <out.docx>
 * 依赖解析顺序：~/.agent/skill-modules → 本脚本目录 → 当前目录。
 * 状态行一律 ASCII：cmd GBK 环境下中文 stdout 会乱码，结论看数据不看横幅。
 */
const fs = require('fs');
const path = require('path');
const os = require('os');

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
  console.error('usage: node docx.js nodes <in.docx> | read <in.docx> | fonts <in.docx> | make <spec.json|-> <out.docx>');
  process.exit(1);
}

const ALIGN = { center: 'center', right: 'right', justify: 'justify' };

// —— nodes：解包 OOXML → 本技能 make 的 spec 格式（支持读出→改→make 覆盖原路径） ——
async function toSpec(file) {
  const JSZip = need('jszip');
  const zip = await JSZip.loadAsync(fs.readFileSync(file));
  const xml = await zip.file('word/document.xml').async('string');
  const numFmt = await abstractNumFormats(zip); // numId → decimal/bullet
  const start = xml.indexOf('<w:body>');
  const end = xml.lastIndexOf('</w:body>');
  if (start < 0 || end < 0) throw new Error('word/document.xml has no w:body (not a .docx?)');
  let rest = xml.slice(start + '<w:body>'.length, end);
  // 按文档序切顶层块（p/tbl），嵌套表格靠 findClose 配平
  const order = [];
  while (rest.length) {
    const pp = rest.search(/<w:p\b/);
    const tt = rest.search(/<w:tbl\b/);
    if (pp < 0 && tt < 0) break;
    const at = (t) => (t < 0 ? Infinity : t);
    if (at(pp) <= at(tt)) {
      const len = findClose(rest.slice(pp), 'p');
      order.push(rest.slice(pp, pp + len));
      rest = rest.slice(pp + len);
    } else {
      const len = findClose(rest.slice(tt), 'tbl');
      order.push(rest.slice(tt, tt + len));
      rest = rest.slice(tt + len);
    }
  }
  const fontCount = {};
  const bump = (f) => { if (f) fontCount[f] = (fontCount[f] || 0) + 1; };
  const children = [];
  let openList = null;
  for (const blk of order) {
    if (blk.startsWith('<w:tbl')) {
      openList = null;
      children.push(readTable(blk));
      continue;
    }
    const parsed = parseP(blk, numFmt, bump);
    if (!parsed) continue;
    if (parsed.type === 'bullets' || parsed.type === 'numbered') {
      if (openList && openList.type === parsed.type) openList.items.push(parsed.item);
      else {
        openList = { type: parsed.type, items: [parsed.item] };
        children.push(openList);
      }
    } else {
      openList = null;
      children.push(parsed.block);
    }
  }
  const topFont = Object.entries(fontCount).sort((a, b) => b[1] - a[1])[0];
  return { font: topFont ? topFont[0] : 'Microsoft YaHei', children };
}

function findClose(s, kind) {
  // 从 s[0] 的 <w:p / <w:tbl 找匹配闭合（支持嵌套）
  const open = new RegExp(`<w:${kind}\\b`);
  const close = new RegExp(`</w:${kind}>`);
  let depth = 0;
  let i = 0;
  while (i < s.length) {
    const o = s.slice(i).search(open);
    const c = s.slice(i).search(close);
    if (c < 0) return s.length;
    if (o >= 0 && o < c) {
      depth++;
      i = i + o + `<w:${kind}`.length;
    } else {
      depth--;
      i = i + c + `</w:${kind}>`.length;
      if (depth === 0) return i;
    }
  }
  return s.length;
}

function parseP(p, numFmt, bump) {
  const runs = [];
  for (const rm of p.matchAll(/<w:r\b[^>]*>([\s\S]*?)<\/w:r>/g)) {
    const t = [...rm[1].matchAll(/<w:t(?:\s[^>]*)?>([\s\S]*?)<\/w:t>/g)].map((x) => x[1]).join('');
    if (!t) continue;
    const f = (rm[1].match(/w:eastAsia="([^"]+)"/) || [])[1];
    bump(f);
    runs.push({ t, f, b: hasTrue(rm[1], 'b'), i: hasTrue(rm[1], 'i') });
  }
  const text = runs.map((r) => r.t).join('');
  const hasNum = /<w:numPr>/.test(p);
  if (!text && !hasNum) return null;
  const first = runs.find((r) => r.f) || {};
  const item = { text: decode(text) };
  if (first.f) item.font = first.f;
  if (first.b) item.bold = true;
  if (first.i) item.italic = true;
  const jc = (p.match(/<w:jc w:val="([^"]+)"/) || [])[1];
  if (jc && ALIGN[jc]) item.align = jc;
  const styleVal = (p.match(/<w:pStyle w:val="([^"]+)"/) || [])[1] || '';
  const hd = styleVal.match(/(?:Heading|标题)\s*(\d)/i);
  if (hd) {
    return { type: 'heading', block: { type: 'heading', level: Math.min(+hd[1], 6), text: item.text, font: item.font, bold: item.bold } };
  }
  if (hasNum) {
    const listMatch = p.match(/<w:numId w:val="(\d+)"/);
    const fmt = listMatch ? numFmt[listMatch[1]] : undefined;
    return { type: fmt === 'decimal' ? 'numbered' : 'bullets', item };
  }
  return { type: 'para', block: Object.assign({ type: 'para' }, item) };
}

// docx-js 对非加粗也写 <w:b w:val="false"/>——只看存在会全误判成加粗
function hasTrue(rPrXml, tag) {
  const m = rPrXml.match(new RegExp(`<w:${tag}(?=[\\s/>])[^>]*/?>`));
  if (!m) return false;
  return !/w:val="(?:false|0)"/.test(m[0]);
}

function readTable(tbl) {
  const rows = [];
  for (const tr of tbl.matchAll(/<w:tr\b[\s\S]*?<\/w:tr>/g)) {
    const cells = [];
    for (const tc of tr[0].matchAll(/<w:tc>[\s\S]*?<\/w:tc>/g)) {
      cells.push(decode([...tc[0].matchAll(/<w:t(?:\s[^>]*)?>([\s\S]*?)<\/w:t>/g)].map((m) => m[1]).join('')));
    }
    if (cells.some((c) => c !== '')) rows.push(cells);
  }
  const header = rows.length ? rows[0] : [];
  return { type: 'table', header, rows: rows.slice(1) };
}

async function abstractNumFormats(zip) {
  // numId → numFmt（decimal/bullet），用来区分有序列表与无序列表
  const out = {};
  const numberingXml = (await zip.file('word/numbering.xml') && (await zip.file('word/numbering.xml').async('string'))) || '';
  const absFmt = {};
  for (const a of numberingXml.matchAll(/<w:abstractNum w:abstractNumId="(\d+)"[\s\S]*?<\/w:abstractNum>/g)) {
    const lvl0 = a[0].match(/<w:lvl w:ilvl="0"[^>]*>[\s\S]*?<w:numFmt w:val="([^"]+)"/);
    if (lvl0) absFmt[a[1]] = lvl0[1];
  }
  for (const n of numberingXml.matchAll(/<w:num w:numId="(\d+)"[^>]*>\s*<w:abstractNumId w:val="(\d+)"/g)) {
    out[n[1]] = absFmt[n[2]] || 'bullet';
  }
  return out;
}

function decode(s) {
  return s
    .replace(/&lt;/g, '<').replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"').replace(/&apos;/g, "'")
    .replace(/&#(\d+);/g, (_, d) => String.fromCodePoint(+d))
    .replace(/&amp;/g, '&');
}

// —— make：spec → docx ——
async function makeDoc(a, b) {
  const docx = need('docx');
  const specText = a === '-' ? fs.readFileSync(0, 'utf8') : fs.readFileSync(a, 'utf8');
  const spec = JSON.parse(specText);
  const {
    Document, Packer, Paragraph, TextRun, HeadingLevel, AlignmentType,
    Table, TableRow, TableCell, WidthType, PageBreak,
  } = docx;
  const bodyFont = spec.font || 'Microsoft YaHei';
  const headFont = spec.headingFont || bodyFont;
  const levelOf = (n) =>
    [HeadingLevel.HEADING_1, HeadingLevel.HEADING_2, HeadingLevel.HEADING_3, HeadingLevel.HEADING_4, HeadingLevel.HEADING_5, HeadingLevel.HEADING_6][
      Math.min(Math.max(n || 1, 1), 6) - 1
    ];
  const alignOf = (s) => (s === 'center' ? AlignmentType.CENTER : s === 'right' ? AlignmentType.RIGHT : s === 'justify' ? AlignmentType.JUSTIFIED : AlignmentType.LEFT);
  const run = (t, { bold, italics, font }) =>
    new TextRun({ text: String(t ?? ''), bold: !!bold, italics: !!italics, font: font || bodyFont });
  const kids = [];
  for (const item of spec.children || []) {
    switch (item.type) {
      case 'heading':
        kids.push(new Paragraph({ heading: levelOf(item.level), children: [run(item.text, { bold: true, font: item.font || headFont })] }));
        break;
      case 'para':
        kids.push(new Paragraph({ alignment: alignOf(item.align), children: [run(item.text, { bold: item.bold, italics: item.italic, font: item.font })] }));
        break;
      case 'bullets':
        for (const it of item.items || []) {
          const o = typeof it === 'string' ? { text: it } : it;
          kids.push(new Paragraph({ bullet: { level: 0 }, children: [run(o.text, { bold: o.bold, italics: o.italic, font: o.font })] }));
        }
        break;
      case 'numbered':
        for (const it of item.items || []) {
          const o = typeof it === 'string' ? { text: it } : it;
          kids.push(new Paragraph({ numbering: { reference: 'agent-numbered', level: 0 }, children: [run(o.text, { bold: o.bold, italics: o.italic, font: o.font })] }));
        }
        break;
      case 'pagebreak':
        kids.push(new Paragraph({ children: [new PageBreak()] }));
        break;
      case 'table': {
        const cell = (t, fn) => new TableCell({ children: [new Paragraph({ children: [run(t, { bold: false, font: fn })] })] });
        const rows = [new TableRow({ tableHeader: true, children: (item.header || []).map((h) => cell(h, headFont)) })];
        for (const r of item.rows || []) rows.push(new TableRow({ children: r.map((c) => cell(c, null)) }));
        kids.push(new Table({ rows, width: { size: 100, type: WidthType.PERCENTAGE } }));
        break;
      }
      default:
        throw new Error(`unknown block type: ${item.type} (heading/para/bullets/numbered/table/pagebreak)`);
    }
  }
  const doc = new Document({
    styles: { default: { document: { run: { font: bodyFont, size: 24 } } } },
    numbering: { config: [{ reference: 'agent-numbered', levels: [{ level: 0, format: 'decimal', text: '%1.', alignment: AlignmentType.LEFT }] }] },
    sections: [{ children: kids }],
  });
  const buf = await Packer.toBuffer(doc);
  fs.writeFileSync(b, buf);
  console.log(`OK make ${b} (${buf.length} bytes, ${kids.length} blocks, font=${bodyFont})`);
}

// —— fonts：核验全文 run 的字体/字号（子代理/Python 都免了） ——
async function fontsOf(file) {
  const JSZip = need('jszip');
  const zip = await JSZip.loadAsync(fs.readFileSync(file));
  const xml = await zip.file('word/document.xml').async('string');
  const seen = {};
  let runs = 0;
  for (const rm of xml.matchAll(/<w:rPr>[\s\S]*?<\/w:rPr>[\s\S]*?<\/w:r>/g)) {
    runs++;
    const f = (rm[0].match(/w:eastAsia="([^"]+)"/) || [null])[1] || '(default)';
    const sz = (rm[0].match(/<w:sz w:val="(\d+)"/) || [])[1];
    const pt = sz ? (parseInt(sz, 10) / 2) : '(default)';
    const key = `${f} ${pt}pt`;
    seen[key] = (seen[key] || 0) + 1;
  }
  console.log(JSON.stringify({ file: path.basename(file), total_runs: runs, fonts: Object.entries(seen).map(([k, n]) => `${k} x${n}`) }, null, 1));
}

async function main() {
  const [cmd, a, b] = process.argv.slice(2);
  if (cmd === 'make') {
    if (!a || !b) usage();
    await makeDoc(a, b);
  } else if (cmd === 'nodes') {
    if (!a) usage();
    const spec = await toSpec(a);
    console.log(JSON.stringify(spec, null, 1));
  } else if (cmd === 'read') {
    if (!a) usage();
    const mammoth = need('mammoth');
    const r = await mammoth.convertToHtml({ path: a });
    console.log(r.value.replace(/<img[^>]*>/g, '[image]'));
    if (r.messages && r.messages.length) {
      console.error(`note: ${r.messages.length} conversion warnings; first: ${r.messages[0].message}`);
    }
  } else if (cmd === 'fonts') {
    if (!a) usage();
    await fontsOf(a);
  } else {
    usage();
  }
}

main().catch((e) => {
  console.error(`FAIL: ${e.message}`);
  process.exit(1);
});
