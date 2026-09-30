'use strict';
/*
 * docx 技能 CLI —— .docx 生成（docx-js）与读取（mammoth 转 HTML）。
 * 用法：
 *   node docx.js make <spec.json|-> <out.docx>
 *   node docx.js read <in.docx>
 * 依赖解析顺序：~/.agent/skill-modules → 本脚本目录 → 当前目录（都装在各自的 node_modules）。
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

function specFrom(arg) {
  const text = arg === '-' ? fs.readFileSync(0, 'utf8') : fs.readFileSync(arg, 'utf8');
  return JSON.parse(text);
}

function usage() {
  console.error('用法: node docx.js make <spec.json|-> <out.docx> | read <in.docx>');
  process.exit(1);
}

async function main() {
  const [cmd, a, b] = process.argv.slice(2);
  if (cmd === 'make') {
    if (!a || !b) usage();
    const docx = need('docx');
    const spec = specFrom(a);
    const {
      Document, Packer, Paragraph, TextRun, HeadingLevel, AlignmentType,
      Table, TableRow, TableCell, WidthType, PageBreak,
    } = docx;
    const levelOf = (n) =>
      [HeadingLevel.HEADING_1, HeadingLevel.HEADING_2, HeadingLevel.HEADING_3, HeadingLevel.HEADING_4, HeadingLevel.HEADING_5, HeadingLevel.HEADING_6][
        Math.min(Math.max(n || 1, 1), 6) - 1
      ];
    const alignOf = (s) => (s === 'center' ? AlignmentType.CENTER : s === 'right' ? AlignmentType.RIGHT : s === 'justify' ? AlignmentType.JUSTIFIED : AlignmentType.LEFT);
    const kids = [];
    for (const item of spec.children || []) {
      switch (item.type) {
        case 'heading':
          kids.push(new Paragraph({ heading: levelOf(item.level), children: [new TextRun({ text: String(item.text ?? '') })] }));
          break;
        case 'para':
          kids.push(
            new Paragraph({
              alignment: alignOf(item.align),
              children: [new TextRun({ text: String(item.text ?? ''), bold: !!item.bold, italics: !!item.italic })],
            })
          );
          break;
        case 'bullets':
          for (const t of item.items || []) kids.push(new Paragraph({ text: String(t), bullet: { level: 0 } }));
          break;
        case 'numbered':
          for (const t of item.items || []) kids.push(new Paragraph({ text: String(t), numbering: { reference: 'agent-numbered', level: 0 } }));
          break;
        case 'pagebreak':
          kids.push(new Paragraph({ children: [new PageBreak()] }));
          break;
        case 'table': {
          const cell = (t, bold) =>
            new TableCell({ children: [new Paragraph({ children: [new TextRun({ text: String(t ?? ''), bold })] })] });
          const rows = [new TableRow({ tableHeader: true, children: (item.header || []).map((h) => cell(h, true)) })];
          for (const r of item.rows || []) rows.push(new TableRow({ children: r.map((c) => cell(c, false)) }));
          kids.push(new Table({ rows, width: { size: 100, type: WidthType.PERCENTAGE } }));
          break;
        }
        default:
          throw new Error(`未知块类型: ${item.type}（支持 heading/para/bullets/numbered/table/pagebreak）`);
      }
    }
    const doc = new Document({
      styles: { default: { document: { run: { font: 'Microsoft YaHei', size: 24 } } } },
      numbering: {
        config: [
          { reference: 'agent-numbered', levels: [{ level: 0, format: 'decimal', text: '%1.', alignment: AlignmentType.LEFT }] },
        ],
      },
      sections: [{ children: kids }],
    });
    const buf = await Packer.toBuffer(doc);
    fs.writeFileSync(b, buf);
    console.log(`已生成 ${b}（${buf.length} 字节，${kids.length} 个内容块）`);
  } else if (cmd === 'read') {
    if (!a) usage();
    const mammoth = need('mammoth');
    const r = await mammoth.convertToHtml({ path: a });
    const html = r.value.replace(/<img[^>]*>/g, '[图片]');
    console.log(html);
    if (r.messages && r.messages.length) {
      console.error(`\n（转换提示 ${r.messages.length} 条，前 3 条: ${r.messages.slice(0, 3).map((m) => m.message).join('；')}）`);
    }
  } else {
    usage();
  }
}

main().catch((e) => {
  console.error(`失败: ${e.message}`);
  process.exit(1);
});
