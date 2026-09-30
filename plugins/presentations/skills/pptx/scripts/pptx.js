'use strict';
/*
 * pptx 技能 CLI —— 幻灯片生成（pptxgenjs）与文字提取（jszip 解包 OOXML）。
 * 用法：
 *   node pptx.js make <spec.json|-> <out.pptx>
 *   node pptx.js text <in.pptx>
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
  console.error('用法: node pptx.js make <spec.json|-> <out.pptx> | text <in.pptx>');
  process.exit(1);
}

async function main() {
  const [cmd, a, b] = process.argv.slice(2);
  if (cmd === 'make') {
    if (!a || !b) usage();
    const PptxGenJS = need('pptxgenjs');
    const specText = a === '-' ? fs.readFileSync(0, 'utf8') : fs.readFileSync(a, 'utf8');
    const spec = JSON.parse(specText);
    const pptx = new PptxGenJS();
    pptx.layout = 'LAYOUT_16x9';
    pptx.theme = { headFontFace: 'Microsoft YaHei', bodyFontFace: 'Microsoft YaHei' };
    if (spec.title) {
      pptx.addSlide().addText(spec.title, { x: 0.5, y: 2.2, w: '90%', fontSize: 40, bold: true, align: 'center' });
    }
    for (const s of spec.slides || []) {
      const slide = pptx.addSlide();
      slide.addText(String(s.title ?? ''), { x: 0.5, y: 0.4, w: '90%', fontSize: 28, bold: true });
      const items = (s.bullets || []).map((it) => {
        const o = typeof it === 'string' ? { text: it } : it;
        return { text: String(o.text ?? ''), options: { bullet: true, level: Math.min(o.level || 0, 4) } };
      });
      if (items.length) slide.addText(items, { x: 0.7, y: 1.4, w: '86%', fontSize: 18, lineSpacingMultiple: 1.3 });
      if (s.notes) slide.addNotes(String(s.notes));
    }
    const buf = await pptx.write({ outputType: 'nodebuffer' });
    fs.writeFileSync(b, buf);
    console.log(`已生成 ${b}（${(spec.slides || []).length + (spec.title ? 1 : 0)} 页，${buf.length} 字节）`);
  } else if (cmd === 'text') {
    if (!a) usage();
    const JSZip = need('jszip');
    const zip = await JSZip.loadAsync(fs.readFileSync(a));
    const slideFiles = Object.keys(zip.files)
      .filter((n) => /^ppt\/slides\/slide\d+\.xml$/.test(n))
      .sort((x, y) => parseInt(x.match(/(\d+)/)[1], 10) - parseInt(y.match(/(\d+)/)[1], 10));
    if (!slideFiles.length) {
      console.error('这个文件里没有幻灯片（ppt/slides/*.xml 不存在）：要么不是 pptx，要么是空演示文稿。如实告知用户。');
      process.exit(1);
    }
    for (const name of slideFiles) {
      const xml = await zip.files[name].async('string');
      const texts = [...xml.matchAll(/<a:t>([\s\S]*?)<\/a:t>/g)].map((m) => m[1]);
      const notesName = name.replace('slides/slide', 'notesSlides/notesSlide');
      let notes = '';
      if (zip.files[notesName]) {
        const nxml = await zip.files[notesName].async('string');
        // 备注页里混着幻灯片页码占位符（纯数字），剔掉只留真实备注
        notes = [...nxml.matchAll(/<a:t>([\s\S]*?)<\/a:t>/g)]
          .map((m) => m[1])
          .filter((t) => !/^\d+$/.test(t.trim()))
          .join(' ')
          .trim();
      }
      console.log(`## 第 ${name.match(/(\d+)/)[1]} 页`);
      console.log(texts.join('\n'));
      if (notes) console.log(`（备注: ${notes}）`);
      console.log('');
    }
  } else {
    usage();
  }
}

main().catch((e) => {
  console.error(`失败: ${e.message}`);
  process.exit(1);
});
