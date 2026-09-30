'use strict';
/*
 * xlsx 技能 CLI —— 表格读写（SheetJS）。
 * 用法：
 *   node xlsx.js read <in.xlsx|in.csv> [sheet名] [行数上限]
 *   node xlsx.js make <spec.json|-> <out.xlsx>
 *   node xlsx.js csv <in.csv> <out.xlsx>
 *   node xlsx.js tocsv <in.xlsx> [sheet名] <out.csv>
 * 依赖解析顺序：~/.agent/skill-modules → 本脚本目录 → 当前目录。
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
    `缺少依赖 ${name}。先装一次（装在任何工作区都能用）：\n` +
      `  npm install --prefix "%USERPROFILE%\\.agent\\skill-modules" ${name}\n` +
      `或直接在工作目录: npm install ${name}`
  );
  process.exit(2);
}

function usage() {
  console.error('用法: node xlsx.js read <in> [sheet] [limit] | make <spec.json|-> <out.xlsx> | csv <in.csv> <out.xlsx> | tocsv <in.xlsx> [sheet] <out.csv>');
  process.exit(1);
}

function loadWorkbook(file) {
  const XLSX = need('xlsx');
  const wb = XLSX.readFile(file, { cellDates: true, dense: false });
  return { XLSX, wb };
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

function main() {
  const [cmd, a, b, c] = process.argv.slice(2);
  if (cmd === 'read') {
    if (!a) usage();
    const { XLSX, wb } = loadWorkbook(a);
    const limit = Math.min(Math.max(parseInt(c || '200', 10) || 200, 1), 100000);
    const names = wb.SheetNames;
    if (!b) {
      console.log(`sheets: ${names.join(', ')}`);
      for (const n of names) {
        const rows = XLSX.utils.sheet_to_json(wb.Sheets[n], { header: 1, blankrows: false });
        console.log(`- ${n}: ${rows.length} 行 × ${rows.reduce((m, r) => Math.max(m, r.length), 0)} 列`);
      }
      console.log('（用 read <文件> <sheet名> [行数上限] 导出具体 sheet 的 JSON 数据）');
      return;
    }
    if (!names.includes(b)) {
      console.error(`没有 sheet「${b}」，可用的有: ${names.join(', ')}`);
      process.exit(1);
    }
    const rows = normalize(
      XLSX.utils.sheet_to_json(wb.Sheets[b], { header: 1, blankrows: false }).slice(0, limit)
    );
    console.log(JSON.stringify({ sheet: b, total_rows_limited_to: limit, rows }));
  } else if (cmd === 'make') {
    if (!a || !b) usage();
    const XLSX = need('xlsx');
    const specText = a === '-' ? fs.readFileSync(0, 'utf8') : fs.readFileSync(a, 'utf8');
    const spec = JSON.parse(specText);
    const wb = XLSX.utils.book_new();
    (spec.sheets || []).forEach((s, i) => {
      if (!Array.isArray(s.rows)) throw new Error(`sheets[${i}].rows 必须是二维数组`);
      XLSX.utils.book_append_sheet(wb, XLSX.utils.aoa_to_sheet(s.rows), s.name || `Sheet${i + 1}`);
    });
    if (!wb.SheetNames.length) throw new Error('spec.sheets 为空，没有可写的内容');
    XLSX.writeFile(wb, b);
    console.log(`已生成 ${b}（sheets: ${wb.SheetNames.join(', ')}）`);
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
    console.log(`已生成 ${b}（来自 ${a}，sheets: ${wb.SheetNames.join(', ')}）`);
  } else if (cmd === 'tocsv') {
    if (!a || !c) usage();
    const { XLSX, wb } = loadWorkbook(a);
    const name = b && wb.SheetNames.includes(b) ? b : wb.SheetNames[0];
    const csv = XLSX.utils.sheet_to_csv(wb.Sheets[name]);
    fs.writeFileSync(c, '\ufeff' + csv, 'utf8'); // BOM：Excel 直接打开不乱码
    console.log(`已生成 ${c}（来自 ${a} 的 sheet「${name}」，${csv.split('\n').length - 1} 行）`);
  } else {
    usage();
  }
}

try {
  main();
} catch (e) {
  console.error(`失败: ${e.message}`);
  process.exit(1);
}
