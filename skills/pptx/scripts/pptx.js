'use strict';
/*
 * pptx 技能 CLI —— 幻灯片生成（pptxgenjs）、文字提取、保真增量修改（jszip）。
 * 用法：
 *   node pptx.js make <spec.json|-> <out.pptx>
 *   node pptx.js text <in.pptx>
 *   node pptx.js replace <ops.json|-> <in.pptx> <out.pptx>
 * replace = 改「已有 pptx」的正门：只动 ops 指定的文本/页，动画/主题/版式
 * 原样保留。默认交付 _updated 副本。
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
  console.error('usage: node pptx.js make <spec.json|-> <out.pptx> | text <in.pptx> | replace <ops.json|-> <in.pptx> <out.pptx>');
  process.exit(1);
}

// ---------------- replace：保真增量改（只动文本/页序，不动版式动画） ----------------

function xmlEsc(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}
function xmlUnesc(s) {
  return String(s)
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&apos;/g, "'")
    .replace(/&amp;/g, '&');
}

function slideNames(zip) {
  return Object.keys(zip.files)
    .filter((n) => /^ppt\/slides\/slide\d+\.xml$/.test(n))
    .sort((x, y) => parseInt(x.match(/(\d+)/)[1], 10) - parseInt(y.match(/(\d+)/)[1], 10));
}

// 在单页 xml 里做查找替换：先按 <a:t> run 内替（保 run 样式），
// 文本被拆进多个 run 的整句替换退化为「首 run 承载全段新文本、其余 run 清空」。
// 返回替换次数。
function replaceInSlideXml(xml, find, replace) {
  let count = 0;
  // run 内快速通道
  xml = xml.replace(/<a:t>([\s\S]*?)<\/a:t>/g, (m, raw) => {
    const t = xmlUnesc(raw);
    if (!t.includes(find) || t === replace) return m;
    const next = t.split(find).join(replace);
    count += t.split(find).length - 1;
    return `<a:t>${xmlEsc(next)}</a:t>`;
  });
  // 跨 run 的段落级替换
  xml = xml.replace(/<a:p\b[^>]*>[\s\S]*?<\/a:p>/g, (para) => {
    const runs = [...para.matchAll(/<a:t>([\s\S]*?)<\/a:t>/g)];
    if (!runs.length) return para;
    const concat = runs.map((r) => xmlUnesc(r[1])).join('');
    if (!concat.includes(find)) return para;
    const nextConcat = concat.split(find).join(replace);
    count += concat.split(find).length - 1;
    let first = true;
    return para.replace(/<a:t>[\s\S]*?<\/a:t>/g, () => {
      if (first) {
        first = false;
        return `<a:t>${xmlEsc(nextConcat)}</a:t>`;
      }
      return '<a:t></a:t>';
    });
  });
  return { xml, count };
}

async function runReplace(opsPath, inFile, outFile) {
  const JSZip = need('jszip');
  const specText = opsPath === '-' ? fs.readFileSync(0, 'utf8') : fs.readFileSync(opsPath, 'utf8');
  const spec = JSON.parse(specText);
  if (!Array.isArray(spec.ops) || !spec.ops.length) throw new Error('ops 为空：spec 须为 {"ops":[...]}');
  const zip = await JSZip.loadAsync(fs.readFileSync(inFile));
  const slides = slideNames(zip);
  if (!slides.length) throw new Error('no slides found: 不是有效 pptx 或空演示文稿，如实报告');

  let replaced = 0;
  // ---- 文本替换 ----
  for (const op of spec.ops.filter((o) => o.op === 'replaceText')) {
    if (op.find === undefined || op.replace === undefined) throw new Error('replaceText 需要 find 与 replace');
    if (String(op.find) === '') throw new Error('find 不许为空串（会匹配一切）');
    let targets = slides;
    if (op.slide !== undefined) {
      const i = Number(op.slide) - 1;
      if (!(i >= 0 && i < slides.length)) throw new Error(`slide ${op.slide} 不存在（共 ${slides.length} 页）`);
      targets = [slides[i]];
    }
    for (const name of targets) {
      const xml = await zip.files[name].async('string');
      const r = replaceInSlideXml(xml, String(op.find), String(op.replace));
      if (r.count > 0) zip.file(name, r.xml);
      replaced += r.count;
    }
  }
  if (replaced === 0 && spec.ops.some((o) => o.op === 'replaceText')) {
    console.error('WARN: 没有任何 text 命中 find——deck 里找不到要替换的文本，检查 find 串（大小写/空格）');
  }

  // ---- 结构类操作（动 presentation.xml 注册表；链式读取当前 zip，页码按 sldIdLst 当前顺序=用户视角）----
  const structOps = spec.ops.filter((o) => o.op !== 'replaceText');
  if (structOps.length) {
    if (!zip.files['ppt/presentation.xml']) throw new Error('ppt/presentation.xml 缺失，不是有效 pptx');
    if (!zip.files['ppt/_rels/presentation.xml.rels']) throw new Error('presentation.xml.rels 缺失，文件结构异常');

    // 当前页序（sldIdLst 顺序）→ [slide 文件名]，以及 rid→文件名
    const orderOf = async () => {
      const pres = await zip.files['ppt/presentation.xml'].async('string');
      const rels = await zip.files['ppt/_rels/presentation.xml.rels'].async('string');
      const rid2t = {};
      for (const m of rels.matchAll(/<Relationship\b[^>]*>/g)) {
        const tag = m[0];
        if (!/Type="[^"]*\/slide"/.test(tag) || /notesSlide/.test(tag)) continue;
        rid2t[tag.match(/Id="([^"]+)"/)[1]] = tag.match(/Target="([^"]+)"/)[1].replace(/^\.\.\//, '').replace(/^ppt\//, '');
      }
      const sldIds = [...pres.matchAll(/<p:sldId\b[^>]*\/>/g)].map((m) => ({
        tag: m[0],
        rid: m[0].match(/r:id="([^"]+)"/)[1],
      }));
      const list = pres.match(/<p:sldIdLst>[\s\S]*?<\/p:sldIdLst>/);
      if (!list) throw new Error('presentation.xml 无 sldIdLst');
      return { pres, rels, rid2t, sldIds, list: list[0] };
    };

    for (const op of structOps) {
      if (op.op === 'moveSlide') {
        const {
          pres, sldIds, list,
        } = await orderOf();
        const from = Number(op.slide) - 1;
        const to = Number(op.to) - 1;
        if (!(from >= 0 && from < sldIds.length) || !(to >= 0 && to < sldIds.length))
          throw new Error(`moveSlide 页码越界（共 ${sldIds.length} 页）`);
        const moved = sldIds.splice(from, 1)[0];
        sldIds.splice(to, 0, moved);
        zip.file('ppt/presentation.xml', pres.replace(list, `<p:sldIdLst>${sldIds.map((s) => s.tag).join('')}</p:sldIdLst>`));
      } else if (op.op === 'deleteSlide') {
        const { pres, rels, sldIds, list } = await orderOf();
        const i = Number(op.slide) - 1;
        if (!(i >= 0 && i < sldIds.length)) throw new Error(`slide ${op.slide} 不存在（共 ${sldIds.length} 页）`);
        if (sldIds.length === 1) throw new Error('只剩最后一页，拒绝删除');
        const victim = sldIds[i];
        const { rid2t } = await orderOf();
        const relTarget = rid2t[victim.rid];
        if (!relTarget) throw new Error(`sldIdLst 第 ${i + 1} 页的关系缺失，结构异常，停手如实报告`);
        const slideFile = `ppt/slides/${path.basename(relTarget)}`;
        // 连带删：slide rels 里挂的 notesSlide
        const relsName = slideFile.replace('slides/slide', 'slides/_rels/slide') + '.rels';
        const cascade = [];
        if (zip.files[relsName]) {
          const srels = await zip.files[relsName].async('string');
          for (const m of srels.matchAll(/<Relationship\b[^>]*>/g)) {
            if (!/notesSlide/.test(m[0])) continue;
            // slide rels 的 Target 相对 slides/：../notesSlides/x.xml → ppt/notesSlides/x.xml
            const t = m[0].match(/Target="([^"]+)"/)[1].replace(/^(\.\.\/)+/, '');
            cascade.push(`ppt/${t}`);
          }
        }
        zip.remove(slideFile);
        zip.remove(relsName);
        for (const c of cascade) {
          zip.remove(c);
          zip.remove(c.replace('slides/', 'slides/_rels/') + '.rels');
        }
        // 三处注册同步删：sldIdLst / presentation rels / Content_Types
        zip.file('ppt/presentation.xml', pres.replace(list, list.replace(victim.tag, '')));
        let rels2 = rels;
        rels2 = rels2.replace(new RegExp(`<Relationship[^>]*Id="${victim.rid}"[^>]*/>`, 'g'), '');
        zip.file('ppt/_rels/presentation.xml.rels', rels2);
        const ctName = '[Content_Types].xml';
        const ct = await zip.files[ctName].async('string');
        zip.file(ctName, ct.replace(new RegExp(`<Override PartName="/${slideFile}"[^>]*/>`, 'g'), ''));
      } else if (op.op === 'duplicateSlide') {
        const { pres, rels, rid2t, sldIds, list } = await orderOf();
        const i = Number(op.slide) - 1;
        if (!(i >= 0 && i < sldIds.length)) throw new Error(`slide ${op.slide} 不存在（共 ${sldIds.length} 页）`);
        const srcTarget = rid2t[sldIds[i].rid];
        if (!srcTarget) throw new Error(`第 ${i + 1} 页的关系缺失，结构异常，停手如实报告`);
        const srcName = `ppt/slides/${path.basename(srcTarget)}`;
        // 新页号 = 现有 slide 文件最大号 + 1
        const maxNo = Math.max(...slideNames(zip).map((s) => parseInt(s.match(/(\d+)/)[1], 10)));
        const newName = `ppt/slides/slide${maxNo + 1}.xml`;
        const newRelsName = newName.replace('slides/slide', 'slides/_rels/slide') + '.rels';
        const sxml = await zip.files[srcName].async('string');
        const relsPath = srcName.replace('slides/slide', 'slides/_rels/slide') + '.rels';
        let srels = zip.files[relsPath] ? await zip.files[relsPath].async('string') : '';
        // 图片复制：media 新文件 + rels Target 重写；图表/嵌入对象/音视频 → 放弃（保真优先）
        if (/chart|oleObject|video|audio/.test(srels)) {
          throw new Error(`第 ${i + 1} 页含图表/嵌入对象/音视频，duplicate 不支持（丢了就是事故）；请用 make 新建类似页或让用户在 PowerPoint 里复制`);
        }
        let newSrels = srels;
        let mediaIdx = 0;
        for (const m of srels.matchAll(/<Relationship\b[^>]*>/g)) {
          const tag = m[0];
          if (!/Type="[^"]*\/image"/.test(tag)) continue;
          const t = tag.match(/Target="([^"]+)"/)[1].replace(/^\.\.\//, '');
          const ext = path.extname(t) || '.png';
          const mediaName = `ppt/media/imageCopy_${Date.now().toString(36)}_${mediaIdx++}${ext}`;
          newSrels = newSrels.replace(tag, tag.replace(t, `../media/${path.basename(mediaName)}`));
          zip.file(mediaName, await zip.files[`ppt/${t}`].async('nodebuffer'));
        }
        zip.file(newName, sxml);
        zip.file(newRelsName, newSrels);
        // 注册：Content_Types override + presentation rels + sldIdLst（皆读当前 zip）
        const ctName = '[Content_Types].xml';
        const ct = await zip.files[ctName].async('string');
        if (!ct.includes('</Types>')) throw new Error('[Content_Types].xml 无 </Types> 结尾，文件损坏');
        zip.file(
          ctName,
          ct.replace(
            '</Types>',
            `<Override PartName="/${newName}" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/></Types>`
          )
        );
        const ridNums = [...rels.matchAll(/Id="rId(\d+)"/g)].map((m) => parseInt(m[1], 10));
        const newRid = `rId${Math.max(0, ...ridNums) + 1}`;
        zip.file(
          'ppt/_rels/presentation.xml.rels',
          rels.replace(
            '</Relationships>',
            `<Relationship Id="${newRid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/${path.basename(newName)}"/></Relationships>`
          )
        );
        const ids = [...pres.matchAll(/<p:sldId\b[^>]*id="(\d+)"/g)].map((m) => parseInt(m[1], 10));
        const newSid = Math.max(256, ...ids) + 1;
        zip.file('ppt/presentation.xml', pres.replace(list, list.replace('</p:sldIdLst>', `<p:sldId id="${newSid}" r:id="${newRid}"/></p:sldIdLst>`)));
      } else {
        throw new Error(`未知 op "${op.op}"（受支持：replaceText/moveSlide/deleteSlide/duplicateSlide）`);
      }
    }
  }

  const buf = await zip.generateAsync({ type: 'nodebuffer', compression: 'DEFLATE' });
  fs.writeFileSync(outFile, buf);
  console.log(`OK replace ${outFile} (replaceText x${replaced}${structOps.length ? `, ${structOps.length} struct ops` : ''}; ${buf.length} bytes)`);
}

async function main() {
  const [cmd, a, b, c] = process.argv.slice(2);
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
    console.log(`OK make ${b} (${(spec.slides || []).length + (spec.title ? 1 : 0)} slides, ${buf.length} bytes)`);
  } else if (cmd === 'text') {
    if (!a) usage();
    const JSZip = need('jszip');
    const zip = await JSZip.loadAsync(fs.readFileSync(a));
    const slideFiles = Object.keys(zip.files)
      .filter((n) => /^ppt\/slides\/slide\d+\.xml$/.test(n))
      .sort((x, y) => parseInt(x.match(/(\d+)/)[1], 10) - parseInt(y.match(/(\d+)/)[1], 10));
    if (!slideFiles.length) {
      console.error('no slides found (ppt/slides/*.xml missing): not a pptx, or empty deck. Report honestly.');
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
      console.log(`-- slide ${name.match(/(\d+)/)[1]} --`);
      console.log(texts.join('\n'));
      if (notes) console.log(`(notes: ${notes})`);
      console.log('');
    }
  } else if (cmd === 'replace') {
    if (!a || !b || !c) usage();
    await runReplace(a, b, c);
  } else {
    usage();
  }
}

main().catch((e) => {
  console.error(`FAIL: ${e.message}`);
  process.exit(1);
});
