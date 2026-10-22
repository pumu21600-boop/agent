'use strict';
/*
 * docx 技能 CLI —— .docx 生成（docx-js）、结构化读出（jszip 解包 OOXML）、字体核验。
 * 用法：
 *   node docx.js nodes <in.docx>            # 读成可编辑 spec JSON（改完喂回 make）
 *   node docx.js read  <in.docx>            # 读成 HTML 结构视图（给人看）
 *   node docx.js fonts <in.docx>            # 核验字体/字号明细（改字体后验这个）
 *   node docx.js make  <spec.json|-> <out.docx>
 * 依赖解析顺序：~/.pawch-agent/skill-modules → 本脚本目录 → 当前目录。
 * 状态行一律 ASCII：cmd GBK 环境下中文 stdout 会乱码，结论看数据不看横幅。
 */
const fs = require('fs');
const path = require('path');
const os = require('os');

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
  console.error('usage: node docx.js setfont <in.docx> <font> [headingFont] | nodes <in.docx> | read <in.docx> | fonts <in.docx> | make <spec.json|-> <out.docx> | patch <ops.json|-> <in.docx> <out.docx>');
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

// —— setfont：原位换字体（OOXML 手术：只改字体槽位，版式/字号/加粗全不动） ——
// 用户最常见的活就是「把全文换成 X 字体」——一条命令，别走 nodes/make 全套。
async function setFont(file, font, headingFont) {
  const JSZip = need('jszip');
  const zip = await JSZip.loadAsync(fs.readFileSync(file));
  let east = 0;
  const bumpSlots = (xml, f) =>
    xml
      .replace(/(w:eastAsia=")[^"]*(")/g, (_m, a, b) => { east++; return a + f + b; })
      .replace(/(w:ascii=")[^"]*(")/g, `$1${f}$2`)
      .replace(/(w:hAnsi=")[^"]*(")/g, `$1${f}$2`)
      .replace(/(w:cs=")[^"]*(")/g, `$1${f}$2`);
  for (const name of ['word/document.xml', 'word/styles.xml', 'word/numbering.xml']) {
    const f = zip.file(name);
    if (!f) continue;
    zip.file(name, bumpSlots(await f.async('string'), font));
  }
  if (headingFont) {
    const dn = zip.file('word/document.xml');
    if (dn) {
      const xml = await dn.async('string');
      zip.file(
        'word/document.xml',
        xml.replace(/<w:p\b[\s\S]*?<\/w:p>/g, (p) =>
          /w:pStyle w:val="(?:Heading[1-9]|[^"]*标题[^"]*)"/i.test(p)
            ? p.replace(/(w:(?:eastAsia|ascii|hAnsi|cs)=")[^"]*(")/g, `$1${headingFont}$2`)
            : p
        )
      );
    }
  }
  fs.writeFileSync(file, await zip.generateAsync({ type: 'nodebuffer' }));
  console.log(`OK setfont ${file} (body=${font}${headingFont ? `, heading=${headingFont}` : ''}, eastAsia replaced: ${east})`);
}

// —— patch：OOXML 微创手术（只动目标 run/段落，版式/图片/表格结构不动） ——
// 与「nodes→改 spec→make 全篇重建」互补：微创保真，重建改结构。
function jcVal(a) {
  return { center: 'center', right: 'right', both: 'both', justify: 'both', left: 'left' }[a] || 'left';
}

// 段落 XML 里按 find 替换文本：返回 {xml, count}。段内文本常被拆多个 run，
// 合并替换后首 run 承载全段新文本、其余 run 的 w:t 清空（保 run 属性=首 run）。
function replaceInParagraph(p, find, replace) {
  let count = 0;
  let p2 = p.replace(/<w:t(?:\s[^>]*)?>[\s\S]*?<\/w:t>/g, (m) => {
    const inner = m.replace(/^<w:t(?:\s[^>]*)?>/, '').replace(/<\/w:t>$/, '');
    const t = decode(inner);
    if (!t.includes(find)) return m;
    const next = t.split(find).join(replace);
    count += t.split(find).length - 1;
    return `<w:t xml:space="preserve">${escapeXml(next)}</w:t>`;
  });
  if (count > 0) return { xml: p2, count };
  // 跨 run：拼接段内全部 w:t 做段级替换
  const texts = [...p.matchAll(/<w:t(?:\s[^>]*)?>([\s\S]*?)<\/w:t>/g)];
  if (!texts.length) return { xml: p, count: 0 };
  const concat = texts.map((m) => decode(m[1])).join('');
  if (!concat.includes(find)) return { xml: p, count: 0 };
  const nextConcat = concat.split(find).join(replace);
  count = concat.split(find).length - 1;
  let first = true;
  p2 = p.replace(/<w:t(?:\s[^>]*)?>[\s\S]*?<\/w:t>/g, () => {
    if (first) {
      first = false;
      return `<w:t xml:space="preserve">${escapeXml(nextConcat)}</w:t>`;
    }
    return '<w:t xml:space="preserve"></w:t>';
  });
  return { xml: p2, count };
}

// 红修订包裹：旧段全部 w:t → w:delText 包进 w:del，新文本 w:ins 插段首
function toTracked(p, find, replace, seq) {
  const texts = [...p.matchAll(/<w:t(?:\s[^>]*)?>([\s\S]*?)<\/w:t>/g)];
  if (!texts.length) return { xml: p, count: 0 };
  const concat = texts.map((m) => decode(m[1])).join('');
  if (!concat.includes(find)) return { xml: p, count: 0 };
  const count = concat.split(find).length - 1;
  const date = new Date().toISOString().replace(/\.\d+Z$/, 'Z');
  const attrs = (p.match(/^<w:p(\b[^>]*)>/) || ['', ''])[1];
  const body = p.replace(/^<w:p\b[^>]*>/, '').replace(/<\/w:p>$/, '');
  const delBody = body.replace(/<w:t(?:\s[^>]*)?>([\s\S]*?)<\/w:t>/g, (_m, t) => `<w:delText xml:space="preserve">${t}</w:delText>`);
  const nextText = concat.split(find).join(replace);
  return {
    xml: `<w:p${attrs}><w:ins w:id="${seq.i++}" w:author="agent" w:date="${date}"><w:r><w:t xml:space="preserve">${escapeXml(nextText)}</w:t></w:r></w:ins><w:del w:id="${seq.i++}" w:author="agent" w:date="${date}">${delBody}</w:del></w:p>`,
    count,
  };
}

function escapeXml(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

// 给含 find 的 run 增强 rPr（保留原属性，只动指定槽位）
function styleRunsInPara(p, find, st) {
  const rPrAdd = [];
  if (st.bold !== undefined) rPrAdd.push(st.bold ? '<w:b/>' : '<w:b w:val="0"/>');
  if (st.italic !== undefined) rPrAdd.push(st.italic ? '<w:i/>' : '<w:i w:val="0"/>');
  if (st.underline !== undefined) rPrAdd.push(st.underline ? '<w:u w:val="single"/>' : '<w:u w:val="none"/>');
  if (st.color) rPrAdd.push(`<w:color w:val="${st.color.replace('#', '')}"/>`);
  if (st.highlight) rPrAdd.push(`<w:highlight w:val="${st.highlight}"/>`);
  if (st.size) rPrAdd.push(`<w:sz w:val="${Math.round(st.size * 2)}"/>`); // pt → half-point
  if (!rPrAdd.length) return p;
  return p.replace(/<w:r>([\s\S]*?)<\/w:r>/g, (run) => {
    const t = (run.match(/<w:t(?:\s[^>]*)?>([\s\S]*?)<\/w:t>/) || [null, ''])[1];
    if (!decode(t).includes(find)) return run;
    if (/<w:rPr>[\s\S]*?<\/w:rPr>/.test(run)) {
      return run.replace(/<w:rPr>/, `<w:rPr>${rPrAdd.join('')}`);
    }
    return run.replace(/<w:r>/, `<w:r><w:rPr>${rPrAdd.join('')}</w:rPr>`);
  });
}

async function runPatch(opsPath, inFile, outFile) {
  const JSZip = need('jszip');
  const specText = opsPath === '-' ? fs.readFileSync(0, 'utf8') : fs.readFileSync(opsPath, 'utf8');
  const spec = JSON.parse(specText);
  if (!Array.isArray(spec.ops) || !spec.ops.length) throw new Error('ops 为空：spec 须为 {"ops":[...]}');
  const zip = await JSZip.loadAsync(fs.readFileSync(inFile));
  if (!zip.file('word/document.xml')) throw new Error('word/document.xml 缺失：不是有效 docx');
  let xml = await zip.file('word/document.xml').async('string');
  const paraRe = /<w:p\b[^>]*>[\s\S]*?<\/w:p>/g;
  const seq = { i: 1000 };
  let stats = { replace: 0, delPara: 0, insert: 0, style: 0 };

  for (const op of spec.ops) {
    if (op.op === 'replaceText') {
      if (!op.find || String(op.find) === '') throw new Error('replaceText 需要非空 find');
      xml = xml.replace(paraRe, (p) => {
        const r = spec.tracked ? toTracked(p, String(op.find), String(op.replace), seq) : replaceInParagraph(p, String(op.find), String(op.replace));
        stats.replace += r.count;
        return r.xml;
      });
    } else if (op.op === 'styleRun') {
      if (!op.find) throw new Error('styleRun 需要 find');
      xml = xml.replace(paraRe, (p) => {
        const next = styleRunsInPara(p, String(op.find), op.style || {});
        if (next !== p) stats.style++;
        return next;
      });
    } else if (op.op === 'insertAfter') {
      if (!op.find) throw new Error('insertAfter 需要 find（锚定段落）');
      if (op.text === undefined) throw new Error('insertAfter 需要 text（新段落内容）');
      let done = 0;
      xml = xml.replace(paraRe, (p) => {
        if (done || !decode((p.match(/<w:t(?:\s[^>]*)?>([\s\S]*?)<\/w:t>/) || [null, ''])[1]).includes(String(op.find)))
          return p;
        done++;
        const rPr = (p.match(/<w:rPr>[\s\S]*?<\/w:rPr>/) || [''])[0]; // 新段继承锚点段首个 run 的样式
        const newP = `<w:p><w:r>${rPr}<w:t xml:space="preserve">${escapeXml(op.text)}</w:t></w:r></w:p>`;
        return p + newP;
      });
      if (!done) throw new Error(`insertAfter 找不到锚点段落（find=${op.find}）`);
      stats.insert += done;
    } else if (op.op === 'deletePara') {
      if (!op.find) throw new Error('deletePara 需要 find');
      xml = xml.replace(paraRe, (p) => {
        const t = decode((p.match(/<w:t(?:\s[^>]*)?>([\s\S]*?)<\/w:t>/) || [null, ''])[1]);
        if (!t.includes(String(op.find))) return p;
        stats.delPara++;
        return '';
      });
    } else if (op.op === 'setParaAlign') {
      if (!op.find) throw new Error('setParaAlign 需要 find');
      const jc = `<w:jc w:val="${jcVal(op.align)}"/>`;
      xml = xml.replace(paraRe, (p) => {
        if (!decode((p.match(/<w:t(?:\s[^>]*)?>([\s\S]*?)<\/w:t>/) || [null, ''])[1]).includes(String(op.find))) return p;
        const stripped = p.replace(/<w:jc w:val="[^"]*"\/>/, '');
        if (/<w:pPr>/.test(stripped)) return stripped.replace(/<w:pPr>/, `<w:pPr>${jc}`);
        // 无 pPr：pPr 必须是段的第一个子元素
        return stripped.replace(/^(<w:p\b[^>]*>)/, `$1<w:pPr>${jc}</w:pPr>`);
      });
    } else {
      throw new Error(`未知 op "${op.op}"（受支持：replaceText/styleRun/insertAfter/deletePara/setParaAlign）`);
    }
  }

  if (stats.replace === 0 && spec.ops.some((o) => o.op === 'replaceText')) {
    console.error('WARN: 没有任何段落命中 find——document.xml 里找不到该文本，检查 find 串');
  }
  zip.file('word/document.xml', xml);
  const buf = await zip.generateAsync({ type: 'nodebuffer' });
  fs.writeFileSync(outFile, buf);
  console.log(`OK patch ${outFile} (replace x${stats.replace}, styleRuns x${stats.style}, insert x${stats.insert}, delPara x${stats.delPara}${spec.tracked ? ', TRACKED' : ''}; ${buf.length} bytes)`);
}

async function main() {
  const [cmd, a, b, c] = process.argv.slice(2);
  if (cmd === 'make') {
    if (!a || !b) usage();
    await makeDoc(a, b);
  } else if (cmd === 'setfont') {
    if (!a || !b) {
      console.error('usage: node docx.js setfont <in.docx> <font> [headingFont]');
      process.exit(1);
    }
    await setFont(a, b, c || '');
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
  } else if (cmd === 'patch') {
    if (!a || !b || !c) {
      console.error('usage: node docx.js patch <ops.json|-> <in.docx> <out.docx>');
      process.exit(1);
    }
    await runPatch(a, b, c);
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
