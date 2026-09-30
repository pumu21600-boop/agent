---
name: pptx
description: 创建与读取 .pptx 演示文稿：make 按 JSON 规格生成标题+要点式幻灯片，text 逐页读出已有 pptx 的文字内容
---

# pptx 技能（演示文稿）

适用：生成标题+要点式幻灯片；读已有 pptx 的文字（逐页）。
走 Node：生成用 pptxgenjs，读取解包 OOXML。

## 0. 环境准备（一次性）

```
npm install --prefix "%USERPROFILE%\.agent\skill-modules" pptxgenjs jszip
```

依赖装不上时如实报错停下。

## 1. 生成：make

1. 写规格 JSON（如 `deck-spec.json`）：

```json
{
  "title": "季度复盘",
  "slides": [
    { "title": "结论先行", "bullets": ["营收超预期 12%", "两个风险点见下页"], "notes": "口播提示" },
    { "title": "风险", "bullets": [{ "text": "供应链", "level": 0 }, { "text": "备选厂商", "level": 1 }] }
  ]
}
```

2. `node "<技能目录>\scripts\pptx.js" make deck-spec.json 输出.pptx`
   （`<技能目录>` 见 read_skill 返回的头部标注；带 title 时第 1 页为封面）

## 2. 读取：text

`node "<技能目录>\scripts\pptx.js" text 已有.pptx` —— 逐页列出文字（含备注）。
版式/配色/图片拿不到，导出的是文字内容；要做「改几页」= 读出 → 改文案 → 生成新版。

## 3. 边界（如实告知用户）

- 生成的版式是「标题 + 项目符号列表」基础款；图表、图片、动画不做，要更复杂的设计说明此局限。
- 中文默认微软雅黑；模板复刻（按公司模板出片）不支持。
