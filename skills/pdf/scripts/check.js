'use strict';
/*
 * 文档改完验收（check.js）——每个技能自带一份，改完**必须跑，不过不许说完成**。
 * 用法：
 *   node check.js <out.docx|xlsx|pptx|pdf> [选项]
 * 选项：
 *   --expect "新文本"     改后文档里必须出现（可多次）
 *   --not "旧文本"        改后文档里必须不再出现（可多次；PDF/记修订场景慎用）
 *   --slides N            pptx 期望页数（不传则只查结构完整）
 *   --sheets N            xlsx 期望 sheet 数
 *   --pages N             pdf 期望页数
 *   --render              有 LibreOffice 时额外做 soffice→PDF 渲染冒烟（页数一致）
 * 退出码 0 = 全过；1 = 有 FAIL；2 = 结构坏（文件根本不是有效文档）。
 * 环境无关：soffice/mammoth 等不是硬依赖，缺了就跳过对应项并如实标注 skipped。
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
  for (const b of bases) if (fs.existsSync(path.join(b, name))) return require(path.join(b, name));
  return null;
}

function parseArgs(argv) {
  const file = argv[0];
  const o = { expect: [], not: [], render: false };
  for (let i = 1; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--expect') o.expect.push(argv[++i]);
    else if (a === '--not') o.not.push(argv[++i]);
    else if (a === '--slides') o.slides = parseInt(argv[++i], 10);
    else if (a === '--sheets') o.sheets = parseInt(argv[++i], 10);
    else if (a === '--pages') o.pages = parseInt(argv[++i], 10);
    else if (a === '--render') o.render = true;
  }
  return { file, opts: o };
}

const results = [];
const ck = (name, cond, detail) => results.push({ name, ok: !!cond, detail: detail || '' });

function sofficeBin() {
  const cands = [
    process.env.SOFFICE_PATH,
    'C:/Program Files/LibreOffice/program/soffice.exe',
    'C:/Program Files (x86)/LibreOffice/program/soffice.exe',
  ].filter(Boolean);
  return cands.find((p) => fs.existsSync(p));
}

async function renderSmoke(file, kindName, expectedUnits) {
  const bin = sofficeBin();
  if (!bin) {
    results.push({ name: 'render(skip)', ok: true, detail: 'soffice 不存在，跳过渲染冒烟（结构+文本已过）' });
    return;
  }
  const { execFileSync } = require('child_process');
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'chk-'));
  try {
    execFileSync(bin, ['--headless', '--convert-to', 'pdf', '--outdir', tmp, path.resolve(file)], {
      stdio: 'ignore',
      timeout: 120000,
    });
    const out = path.join(tmp, path.basename(file).replace(/\.[^.]+$/, '.pdf'));
    if (!fs.existsSync(out)) {
      results.push({ name: 'render', ok: false, detail: 'soffice 跑了但没产出 PDF——文件可能损坏' });
      return;
    }
    // 渲染成功且非空 = 文件能被真实排版引擎打开（比 zip 完整性强得多）
    ck('render', fs.statSync(out).size > 500, `${fs.statSync(out).size} bytes`);
    if (expectedUnits) {
      const { PDFDocument } = (() => {
        const p = need('pdf-lib');
        return p ? p : {};
      })();
      if (PDFDocument) {
        const d = await PDFDocument.load(fs.readFileSync(out));
        ck(`render页数(${expectedUnits})`, d.getPageCount() === expectedUnits, `got ${d.getPageCount()}`);
      }
    }
  } catch (e) {
    results.push({ name: 'render', ok: false, detail: `soffice 转换失败: ${e.message}` });
  } finally {
    // 2026-10-08 修：rmSync 递归一把收——unlinkSync 遇子目录抛 EISDIR，
    // 异常从 finally 逃逸把整个验收以退出码 2 结束（前面的结构断言全 PASS 也白搭），
    // rmdirSync 同样非递归。清理代码不该污染验收结论。
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

async function main() {
  const { file, opts } = parseArgs(process.argv.slice(2));
  if (!file) {
    console.error('usage: node check.js <out.docx|xlsx|pptx|pdf> [--expect t] [--not t] [--slides N] [--sheets N] [--pages N] [--render]');
    process.exit(1);
  }
  if (!fs.existsSync(file)) {
    console.error(`FAIL: 文件不存在 ${file}`);
    process.exit(2);
  }
  const ext = path.extname(file).toLowerCase();
  const JSZip = need('jszip');
  const bytes = fs.readFileSync(file);

  // ---- OOXML 三兄弟：zip 完整性 + 主 part + 数量/文本断言 ----
  if (ext === '.docx' || ext === '.xlsx' || ext === '.pptx') {
    if (!JSZip) {
      console.error('FAIL: jszip 不可用（装 ~/.agent/skill-modules），无法做结构验收');
      process.exit(2);
    }
    let zip;
    try {
      zip = await JSZip.loadAsync(bytes);
    } catch (e) {
      console.error(`FAIL: 不是有效 OOXML zip: ${e.message}`);
      process.exit(2);
    }
    const main = { '.docx': 'word/document.xml', '.xlsx': 'xl/workbook.xml', '.pptx': 'ppt/presentation.xml' }[ext];
    ck(`主part ${main}`, !!zip.file(main));
    ck('[Content_Types].xml', !!zip.file('[Content_Types].xml'));

    if (ext === '.pptx') {
      const slides = Object.keys(zip.files).filter((n) => /^ppt\/slides\/slide\d+\.xml$/.test(n));
      const pres = await zip.file('ppt/presentation.xml').async('string');
      const ids = [...pres.matchAll(/<p:sldId\b[^>]*\/>/g)].length;
      ck('slide 文件数 = sldIdLst 注册数', slides.length === ids, `files=${slides.length} lst=${ids}`);
      if (opts.slides) ck(`页数=${opts.slides}`, ids === opts.slides, `got ${ids}`);
      const text = await Promise.all(slides.map((n) => zip.file(n).async('string')));
      const all = text.join('');
      for (const t of opts.expect) ck(`应含「${t}」`, textIncludes(all, t));
      for (const t of opts.not) ck(`应不含「${t}」`, !textIncludes(all, t));
    } else if (ext === '.xlsx') {
      const XLSX = need('xlsx');
      if (XLSX) {
        const wb = XLSX.read(bytes, { type: 'buffer' });
        if (opts.sheets) ck(`sheet数=${opts.sheets}`, wb.SheetNames.length === opts.sheets, `got ${wb.SheetNames.length}`);
        const all = wb.SheetNames.map((n) => JSON.stringify(XLSX.utils.sheet_to_json(wb.Sheets[n], { header: 1 }))).join('');
        for (const t of opts.expect) ck(`应含「${t}」`, all.includes(t));
        for (const t of opts.not) ck(`应不含「${t}」`, !all.includes(t));
      } else {
        results.push({ name: 'xlsx文本断言(skip)', ok: true, detail: 'xlsx 库不可用' });
      }
    } else {
      const mammoth = need('mammoth');
      const xml = await zip.file('word/document.xml').async('string');
      if (mammoth) {
        const r = await mammoth.convertToHtml({ buffer: bytes });
        for (const t of opts.expect) ck(`应含「${t}」`, r.value.includes(t));
        for (const t of opts.not) ck(`应不含「${t}」`, !r.value.includes(t));
      } else {
        for (const t of opts.expect) ck(`应含「${t}」`, textIncludes(xml, t));
      }
      if (opts.render) await renderSmoke(file, 'docx', undefined);
    }
  } else if (ext === '.pdf') {
    const { PDFDocument } = need('pdf-lib') || {};
    if (!PDFDocument) {
      console.error('FAIL: pdf-lib 不可用，无法验收');
      process.exit(2);
    }
    let doc;
    try {
      doc = await PDFDocument.load(bytes, { ignoreEncryption: true });
    } catch (e) {
      console.error(`FAIL: 不是有效 PDF: ${e.message}`);
      process.exit(2);
    }
    ck('PDF 可加载', true);
    if (opts.pages) ck(`页数=${opts.pages}`, doc.getPageCount() === opts.pages, `got ${doc.getPageCount()}`);
    const pdfParse = need('pdf-parse');
    if (pdfParse && (opts.expect.length || opts.not.length)) {
      // 2026-10-08 修：max:50 只解前 50 页——文本在第 50 页之后的文档，
      // --expect 断言会误报（文档明明对）。断言要覆盖整本，去掉页数上限
      // （pdf-parse 对超大文档只是慢一点，验收是一次性的）。
      const parsed = await pdfParse(bytes);
      for (const t of opts.expect) ck(`应含「${t}」`, parsed.text.includes(t));
      for (const t of opts.not) ck(`应不含「${t}」`, !parsed.text.includes(t));
    }
    if (opts.render) await renderSmoke(file, 'pdf', opts.pages);
  } else {
    console.error(`FAIL: 不认的扩展名 ${ext}（支持 docx/xlsx/pptx/pdf）`);
    process.exit(2);
  }

  const failed = results.filter((r) => !r.ok);
  for (const r of results) console.log(`${r.ok ? 'PASS' : 'FAIL'} ${r.name}${r.detail ? ` — ${r.detail}` : ''}`);
  console.log(`--- ${results.length - failed.length}/${results.length} passed${failed.length ? `, ${failed.length} FAILED` : ''} ---`);
  if (failed.length) {
    console.error('结论：验收未过，不要告诉用户「已完成」——回滚 out、修 ops 重来，或如实报告卡点。');
    process.exit(1);
  }
}

// a:t/delText 内容可能有 XML 转义，断言前先 unescape 一次
function textIncludes(xml, t) {
  return xml.includes(t) || xml.includes(xmlEsc(t));
}
function xmlEsc(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

main().catch((e) => {
  console.error(`FAIL: ${e.message}`);
  process.exit(2);
});
