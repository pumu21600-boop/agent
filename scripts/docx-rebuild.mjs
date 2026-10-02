// docx 文本重建：预览面板「编辑 → 保存」的后端执行体。
// 输入 JSON（stdin）：{ out: 绝对路径, text: 纯文本正文 }
// 输出 JSON（stdout）：{ ok: true, paragraphs: N } 或 { ok: false, error }
// 依赖与 documents 插件同源：%USERPROFILE%\.agent\skill-modules\node_modules\docx
// 语义：编辑底稿 = 后端 doctext 抽出的纯文本，保存按段落重建 docx（排版简化：
// 统一正文样式，不保留原文档字体/表格/图片——预览里说的都在 note 里写明）。
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const modules = path.join(process.env.USERPROFILE || process.env.HOME, '.agent', 'skill-modules', 'node_modules');

function readStdin() {
  return new Promise((resolve, reject) => {
    const chunks = [];
    process.stdin.on('data', (d) => chunks.push(d));
    process.stdin.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    process.stdin.on('error', reject);
  });
}

try {
  const { out, text } = JSON.parse(await readStdin() || '{}');
  if (!out || typeof text !== 'string') throw new Error('参数不完整（out/text）');
  const { Document, Packer, Paragraph } = require(path.join(modules, 'docx'));
  const paras = text.split(/\r?\n/).map((line) => new Paragraph({ text: line }));
  const doc = new Document({ sections: [{ children: paras }] });
  const buf = await Packer.toBuffer(doc);
  fs.writeFileSync(out, buf);
  process.stdout.write(JSON.stringify({ ok: true, paragraphs: paras.length }));
} catch (e) {
  process.stdout.write(JSON.stringify({ ok: false, error: String(e && e.message || e) }));
}
