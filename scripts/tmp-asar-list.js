// 列 asar 内文件（只读调研用，临时脚本）
const fs = require('fs');
const path = process.argv[2];
const buf = fs.readFileSync(path);
// asar 头部：4 字节对齐的 pickle
function readHeader() {
  const size = buf.readUInt32LE(12); // 头部 JSON 长度（含 padding）
  const jsonStr = buf.toString('utf8', 16, 16 + size).replace(/\0+$/, '');
  return JSON.parse(jsonStr);
}
const hdr = readHeader();
const out = [];
const walk = (node, prefix) => {
  for (const [name, v] of Object.entries(node.files || {})) {
    const p = prefix + '/' + name;
    if (v.files) walk(v, p);
    else out.push({ path: p, size: v.size || 0 });
  }
};
walk(hdr, '');
const want = process.argv[3] || 'skill';
const hits = out.filter((f) => f.path.toLowerCase().includes(want.toLowerCase()));
console.log('总文件数:', out.length, '| 命中', want, ':', hits.length);
hits.slice(0, 60).forEach((f) => console.log(String(f.size).padStart(8), f.path));
