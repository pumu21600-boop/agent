---
name: pdf
description: PDF 处理：提取文本、合并/拆分/查页信息；有 Edge/Chrome 时把 HTML 排版转成 PDF（中文海报/报告的可行出片路径）
---

# pdf 技能

适用：读 PDF 文本做摘要/提取、合并拆分、查页数元信息、把排好版的 HTML 转 PDF。
走 Node：文本提取用 pdf-parse，页面操作用 pdf-lib。

## 0. 环境准备（一次性）

```
npm install --prefix "%USERPROFILE%\.agent\skill-modules" pdf-parse@1.1.1 pdf-lib
```

依赖装不上时如实报错停下。

## 1. 命令（`<技能目录>` 见 read_skill 返回的头部标注）

- `node "<技能目录>\scripts\pdf.js" text 文件.pdf [页数上限]` —— 提取文本（默认前 30 页；
  扫描件/图片型 PDF 提不出字，会如实报 0 字，此时别硬编内容）
- `node "<技能目录>\scripts\pdf.js" pages 文件.pdf` —— 页数 + 元信息（标题/作者）
- `node "<技能目录>\scripts\pdf.js" merge 输出.pdf 输入1.pdf 输入2.pdf …` —— 按顺序合并
- `node "<技能目录>\scripts\pdf.js" split 文件.pdf 输出前缀` —— 每页拆一个
  （生成 `前缀-1.pdf`、`前缀-2.pdf`…，页数多先跟用户确认）
- `node "<技能目录>\scripts\pdf.js" fromhtml 页面.html 输出.pdf` —— HTML 排版转 PDF
  （自动找 Edge/Chrome；都没有就报错，不要交白卷或假完成）

## 2. 做 PDF 报告/海报的推荐路径

1. 写一个自包含 HTML（内联 CSS；中文用系统字体栈 "Microsoft YaHei"；页面尺寸用
   `@page { size: A4; margin: 0 }` 控制）
2. `fromhtml 页面.html 输出.pdf`
3. 抽查：`text 输出.pdf 2` 确认文字进去了。

## 3. 边界（如实告知用户）

- 直接「从零排 PDF」不做（pdf-lib 画不了中文标准字体），一律走 HTML 排版转 PDF。
- 提取结果乱码 = 原文件字体缺 ToUnicode 映射，如实说明并建议要原文档的文本版。
- merge/split 会重写文件结构，表单字段与签名可能失效，动之前提醒。
