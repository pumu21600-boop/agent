'use strict';
/*
 * xlsx 技能 CLI —— 表格读写（SheetJS）+ 保真增量修改（ExcelJS）。
 * 用法：
 *   node xlsx.js read <in.xlsx|in.csv> [sheet名] [行数上限]
 *   node xlsx.js make <spec.json|-> <out.xlsx>
 *   node xlsx.js patch <ops.json|-> <in.xlsx> <out.xlsx>
 *   node xlsx.js csv <in.csv> <out.xlsx>
 *   node xlsx.js tocsv <in.xlsx> [sheet名] <out.csv>
 * patch = 改「已有文件」的正门：ExcelJS 载入原工作簿，按 ops 增量改，其余
 * 样式/公式/合并单元格/列宽原样保留。默认交付 _updated 副本（out 自定）。
 * 依赖解析顺序：~/.pawch-agent/skill-modules → 本脚本目录 → 当前目录。
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
  console.error('usage: node xlsx.js read <in> [sheet] [limit] | make <spec.json|-> <out.xlsx> | patch <ops.json|-> <in.xlsx> <out.xlsx> | csv <in.csv> <out.xlsx> | tocsv <in.xlsx> [sheet] <out.csv>');
  process.exit(1);
}

// ---------------- patch：ExcelJS 保真增量改 ----------------

function colToNum(col) {
  let n = 0;
  for (const ch of col.toUpperCase()) n = n * 26 + (ch.charCodeAt(0) - 64);
  return n;
}

// 值类型推断：数字字面量→number；"=" 开头→公式；其余→文本。
// 注意：ExcelJS 不认 "=..." 字符串（会被当文本写坏，2026-10-06 实测），必须给 { formula } 对象。
function coerceValue(v) {
  if (typeof v === 'number' || typeof v === 'boolean') return v;
  if (typeof v !== 'string') return v;
  const t = v.trim();
  if (t.startsWith('=')) return { formula: t.slice(1) };
  if (/^-?\d+(\.\d+)?$/.test(t)) return Number(t);
  return v;
}

// setStyle 增量合并：ExcelJS 赋 cell.font= 会整体替换，先读旧样式再逐键合
function padArgb(c) {
  const s = String(c).replace('#', '');
  return s.length === 6 ? `FF${s}` : s; // Excel 惯例：8 位 AARRGGBB
}

function applyStyle(cell, st) {
  if (!st || typeof st !== 'object') return;
  const font = {};
  if (st.bold !== undefined) font.bold = !!st.bold;
  if (st.italic !== undefined) font.italic = !!st.italic;
  if (st.underline !== undefined) font.underline = !!st.underline;
  if (st.fontSize !== undefined) font.size = st.fontSize;
  if (st.fontName !== undefined) font.name = st.fontName;
  if (st.color !== undefined) font.color = { argb: padArgb(st.color) };
  if (Object.keys(font).length) {
    const old = cell.font || {};
    cell.font = Object.assign({}, old, font, {
      color: font.color || old.color,
      size: font.size || old.size,
      name: font.name || old.name,
    });
  }
  if (st.bg !== undefined) {
    const argb = padArgb(st.bg);
    cell.fill = { type: 'pattern', pattern: 'solid', fgColor: { argb } };
  }
  const align = {};
  if (st.align !== undefined) align.horizontal = st.align; // left|center|right
  if (st.valign !== undefined) align.vertical = st.valign; // top|middle|bottom
  if (st.wrap !== undefined) align.wrapText = !!st.wrap;
  if (Object.keys(align).length) {
    const old = cell.alignment || {};
    cell.alignment = Object.assign({}, old, align);
  }
  if (st.numFmt !== undefined) cell.numFmt = st.numFmt; // 如 '0.00%' 'yyyy-mm-dd'
  if (st.border !== undefined) cell.border = st.border; // 高级用法：直接传 ExcelJS border 对象
}

async function runPatch(opsPath, inFile, outFile) {
  const ExcelJS = need('exceljs');
  const specText = opsPath === '-' ? fs.readFileSync(0, 'utf8') : fs.readFileSync(opsPath, 'utf8');
  const spec = JSON.parse(specText);
  if (!Array.isArray(spec.ops) || !spec.ops.length) throw new Error('ops 为空：spec 须为 {"ops":[...]}');
  const wb = new ExcelJS.Workbook();
  await wb.xlsx.readFile(inFile);

  const wsOf = (name) => {
    const ws = wb.getWorksheet(name);
    if (!ws) throw new Error(`no such sheet "${name}"; available: ${wb.worksheets.map((w) => w.name).join(', ')}`);
    return ws;
  };

  let n = 0;
  for (const op of spec.ops) {
    const ws = op.op === 'addSheet' || op.op === 'delSheet' ? null : wsOf(op.sheet);
    switch (op.op) {
      case 'set':
      case 'setText':
      case 'formula': {
        const c = ws.getCell(op.cell);
        const v = op.op === 'setText' ? String(op.value) : coerceValue(op.value);
        // formula op 强制公式：值可带可不带 "="（coerceValue 已把 "=..." 转成 { formula }）
        c.value = op.op === 'formula' && typeof v === 'string' ? { formula: v.replace(/^=/, '') } : v;
        if (op.style) applyStyle(c, op.style);
        break;
      }
      case 'addRow':
        ws.insertRow(Number(op.at), (op.values || []).map(coerceValue), 'i+');
        break;
      case 'delRow':
        // ExcelJS splice 系列是 1-based（at=7 删第 7 行）
        ws.spliceRows(Number(op.at), 1);
        break;
      case 'addCol': {
        // col=「插在第 col 列之前」（1-based）；undefined 填充保列存在、原列连带右移
        const col = Math.max(1, colToNum(op.col || ''));
        const fill = Array.from({ length: ws.rowCount }, () => undefined);
        ws.spliceColumns(col, 0, fill);
        if (op.header !== undefined && op.header !== null) ws.getCell(1, col).value = op.header;
        (op.values || []).forEach((v, i) => {
          if (v !== null && v !== undefined) ws.getCell(2 + i, col).value = coerceValue(v);
        });
        break;
      }
      case 'delCol':
        ws.spliceColumns(colToNum(op.col), 1);
        break;
      case 'setStyle':
        if (op.range) {
          const [from, to] = op.range.split(':');
          const top = ws.getCell(from), bottom = ws.getCell(to);
          for (let r = top.row; r <= bottom.row; r++) {
            for (let c = top.col; c <= bottom.col; c++) applyStyle(ws.getCell(r, c), op.style);
          }
        } else {
          applyStyle(ws.getCell(op.cell), op.style);
        }
        break;
      case 'merge':
        ws.mergeCells(op.range);
        break;
      case 'unmerge':
        ws.unMergeCells(op.range);
        break;
      case 'colWidth':
        ws.getColumn(colToNum(op.col)).width = Number(op.width);
        break;
      case 'addSheet': {
        const name = op.sheet;
        if (wb.getWorksheet(name)) throw new Error(`sheet "${name}" 已存在`);
        const nws = wb.addWorksheet(name);
        (op.rows || []).forEach((row) => {
          const r = nws.addRow(row.map(coerceValue));
          if (op.headerStyle) applyStyle(r.getCell(1), op.headerStyle);
        });
        break;
      }
      case 'delSheet': {
        // removeWorksheet 只认 id/名字，传实例会静默不删（getWorksheet(实例) 查不到）
        const target = wb.getWorksheet(op.sheet);
        if (!target) throw new Error(`no such sheet "${op.sheet}"; available: ${wb.worksheets.map((w) => w.name).join(', ')}`);
        wb.removeWorksheet(target.name);
        break;
      }
      case 'renameSheet':
        if (wb.getWorksheet(op.to)) throw new Error(`目标表名 "${op.to}" 已存在`);
        wsOf(op.sheet).name = op.to;
        break;
      default:
        throw new Error(`未知 op "${op.op}"（受支持：set/setText/formula/addRow/delRow/addCol/delCol/setStyle/merge/unmerge/colWidth/addSheet/delSheet/renameSheet）`);
    }
    n++;
  }

  await wb.xlsx.writeFile(outFile);
  const summary = wb.worksheets
    .map((w) => `${w.name}(${w.rowCount}x${w.columnCount})`)
    .join(', ');
  console.log(`OK patch ${outFile} (${n} ops; sheets: ${summary})`);
}

function loadWorkbook(file) {
  const XLSX = need('xlsx');
  const wb = XLSX.readFile(file, { cellDates: true, dense: false });
  return { XLSX, wb };
}

// 找不到就返回 null（不退出）——给「补全」这类非硬依赖用
function softNeed(name) {
  const bases = [
    path.join(os.homedir(), '.pawch-agent', 'skill-modules', 'node_modules'),
    path.join(__dirname, 'node_modules'),
    path.join(process.cwd(), 'node_modules'),
  ];
  for (const b of bases) {
    if (fs.existsSync(path.join(b, name))) return require(path.join(b, name));
  }
  return null;
}

// SheetJS 0.18.5 会把「无缓存值的公式单元格」整个读丢（程序写出的新公式、Excel 尚未自算
// 的文件都这样，试遍读选项无解）。用 ExcelJS 补一遍：把 { formula } 单元格灌回网格并显示成
// "=FORMULA"，否则 read 里公式看着像丢了、验收会误判。有缓存值的公式不受影响（照旧显示值）。
async function fillUncachedFormulas(XLSX, wb, file) {
  if (!/\.xlsx$/i.test(file)) return; // .xls/.ods 老格式 ExcelJS 打不开，跳过
  const ExcelJS = softNeed('exceljs');
  if (!ExcelJS) return;
  try {
    const ewb = new ExcelJS.Workbook();
    await ewb.xlsx.readFile(file);
    for (const ews of ewb.worksheets) {
      const ws = wb.Sheets[ews.name];
      if (!ws) continue;
      ews.eachRow((row, rn) => {
        row.eachCell({ includeEmpty: false }, (cell, cn) => {
          const v = cell.value;
          if (v && typeof v === 'object' && typeof v.formula === 'string') {
            const ref = XLSX.utils.encode_cell({ r: rn - 1, c: cn - 1 });
            const cur = ws[ref];
            if (!cur || cur.v === undefined || cur.v === null) {
              ws[ref] = { t: 'n', f: v.formula, v: '=' + v.formula };
              const rng = XLSX.utils.decode_range(ws['!ref'] || 'A1');
              rng.e.r = Math.max(rng.e.r, rn - 1);
              rng.e.c = Math.max(rng.e.c, cn - 1);
              ws['!ref'] = XLSX.utils.encode_range(rng);
            }
          }
        });
      });
    }
  } catch {
    /* 补全失败不致命：read 主体照旧（错误在别处会正常抛出） */
  }
}

// 日期单元格统一转成人类可读字符串（序列号只对 Excel 有意义，模型看不懂 46027）
function normalize(rows) {
  const iso = (d) => {
    const pad = (n) => String(n).padStart(2, '0');
    const date = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
    const time = `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
    return time === '00:00:00' ? date : `${date} ${time}`;
  };
  return rows.map((r) => r.map((c) => (c instanceof Date ? iso(c) : c)));
}

async function runRead(file, sheetName, limitArg) {
  const { XLSX, wb } = loadWorkbook(file);
  await fillUncachedFormulas(XLSX, wb, file);
  const limit = Math.min(Math.max(parseInt(limitArg || '200', 10) || 200, 1), 100000);
  const names = wb.SheetNames;
  if (!sheetName) {
    console.log(`sheets: ${names.join(', ')}`);
    for (const n of names) {
      const rows = XLSX.utils.sheet_to_json(wb.Sheets[n], { header: 1, blankrows: false });
      console.log(`- ${n}: ${rows.length} rows x ${rows.reduce((m, r) => Math.max(m, r.length), 0)} cols`);
    }
    console.log('(use: read <file> <sheet> [limit] to dump a sheet as JSON)');
    return;
  }
  if (!names.includes(sheetName)) {
    console.error(`no such sheet "${sheetName}"; available: ${names.join(', ')}`);
    process.exit(1);
  }
  const rows = normalize(
    XLSX.utils.sheet_to_json(wb.Sheets[sheetName], { header: 1, blankrows: false }).slice(0, limit)
  );
  console.log(JSON.stringify({ sheet: sheetName, total_rows_limited_to: limit, rows }));
}

function main() {
  const [cmd, a, b, c] = process.argv.slice(2);
  if (cmd === 'read') {
    if (!a) usage();
    runRead(a, b, c).catch((e) => {
      console.error(`FAIL: ${e.message}`);
      process.exit(1);
    });
  } else if (cmd === 'make') {
    if (!a || !b) usage();
    const XLSX = need('xlsx');
    const specText = a === '-' ? fs.readFileSync(0, 'utf8') : fs.readFileSync(a, 'utf8');
    const spec = JSON.parse(specText);
    const wb = XLSX.utils.book_new();
    (spec.sheets || []).forEach((s, i) => {
      if (!Array.isArray(s.rows)) throw new Error(`sheets[${i}].rows must be a 2D array`);
      const ws = XLSX.utils.aoa_to_sheet(s.rows);
      // SheetJS 会把 "=..." 字符串当文本写——补成真公式（对齐 SKILL 的 make 规格）
      for (const addr of Object.keys(ws)) {
        if (addr.startsWith('!')) continue;
        const cell = ws[addr];
        if (cell && cell.t === 's' && typeof cell.v === 'string' && cell.v.trim().startsWith('=')) {
          ws[addr] = { t: 'n', f: cell.v.trim().slice(1) };
        }
      }
      XLSX.utils.book_append_sheet(wb, ws, s.name || `Sheet${i + 1}`);
    });
    if (!wb.SheetNames.length) throw new Error('spec.sheets is empty, nothing to write');
    XLSX.writeFile(wb, b);
    console.log(`OK make ${b} (sheets: ${wb.SheetNames.join(', ')})`);
  } else if (cmd === 'patch') {
    if (!a || !b || !c) usage();
    runPatch(a, b, c).catch((e) => {
      console.error(`FAIL: ${e.message}`);
      process.exit(1);
    });
  } else if (cmd === 'csv') {
    if (!a || !b) usage();
    const XLSX = need('xlsx');
    const text = fs.readFileSync(a, 'utf8').replace(/^\ufeff/, ''); // 去掉给 Excel 看的 BOM，别混进首个表头
    const wb = XLSX.read(text, { type: 'string' });
    // SheetJS 会把日期字符串转成序列号还丢掉日期格式（Excel 里显示成 46027）。
    // 规则收干净：原文不是纯数字字面量、却被解析成数字的（日期/百分比/货币），一律按原文文本写入。
    const raw = XLSX.read(text, { type: 'string', raw: true });
    for (const name of wb.SheetNames) {
      const ts = wb.Sheets[name];
      const rs = raw.Sheets[name];
      for (const addr of Object.keys(ts)) {
        if (addr.startsWith('!')) continue;
        const tc = ts[addr];
        const rc = rs[addr];
        if (tc && tc.t === 'n' && rc && typeof rc.v === 'string' && !/^-?\d+(\.\d+)?([eE][+-]?\d+)?$/.test(rc.v.trim())) {
          ts[addr] = { t: 's', v: rc.v };
        }
      }
    }
    XLSX.writeFile(wb, b);
    console.log(`OK make ${b} (from ${a}, sheets: ${wb.SheetNames.join(', ')})`);
  } else if (cmd === 'tocsv') {
    if (!a || !c) usage();
    const { XLSX, wb } = loadWorkbook(a);
    const name = b && wb.SheetNames.includes(b) ? b : wb.SheetNames[0];
    const csv = XLSX.utils.sheet_to_csv(wb.Sheets[name]);
    fs.writeFileSync(c, '\ufeff' + csv, 'utf8'); // BOM：Excel 直接打开不乱码
    console.log(`OK tocsv ${c} (from ${a} sheet "${name}", ${csv.split('\n').length - 1} rows)`);
  } else {
    usage();
  }
}

try {
  main();
} catch (e) {
  console.error(`FAIL: ${e.message}`);
  process.exit(1);
}
