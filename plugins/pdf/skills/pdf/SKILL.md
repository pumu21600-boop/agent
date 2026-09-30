---
name: pdf
description: PDF 文本提取、合并/拆分/查页信息；有 Edge/Chrome/WebView2 时把 HTML 排版转成 PDF（中文出片的可靠路径）
---

# pdf 技能

## 硬约束（先读完再动手）

1. **所有命令用 read_skill 返回的绝对「技能目录」路径开头**；read_file/shell 的相对路径相对**工作区**，不许拿它们读技能文件——必报 os error 2。
2. **只用 `scripts/pdf.js`，不许自造解析脚本、不许上 Python**（本机没有 Python；pypdf/reportlab 路线直接堵死）。
3. 生成 PDF **一律走 `fromhtml`**（HTML+CSS 排版，中文用系统字体栈）；不要去拼 pdf-lib 画布——它画不了中文标准字体，拼出来是乱码/豆腐块。
4. 不用 subagent_spawn 验证；`text <out.pdf> 2` 回抽文字就是验证。
5. 依赖缺失只许装到 `~/.agent/skill-modules`；装不上停下来如实报告。

## 0. 环境检测（任务第一步）

```
node "<技能目录>\scripts\pdf.js"
```

打印 usage = 就绪；missing dependency → 跑页尾安装命令。

## 1. 命令速查

| 命令 | 用途 |
| --- | --- |
| `text <in.pdf> [maxPages]` | 提取文本（默认前 30 页；扫描件会如实报 0 字） |
| `pages <in.pdf>` | 页数 + 元信息 |
| `merge <out.pdf> <in1.pdf> <in2.pdf> …` | 按顺序合并 |
| `split <in.pdf> <前缀>` | 每页拆一个（页数多先跟用户确认） |
| `fromhtml <in.html> <out.pdf>` | HTML 排版转 PDF |

## 2. 做 PDF 报告/海报的推荐路径

1. 写一个**自包含 HTML**：内联 CSS，中文字体栈 `"Microsoft YaHei", sans-serif`；页面尺寸用 `@page { size: A4; margin: 18mm }` 控制。
2. `node "<技能目录>\scripts\pdf.js" fromhtml page.html out.pdf`
   （浏览器发现链与产品内置浏览器一致：`AGENT_BROWSER_PATH` → 标准路径 → WebView2 运行时）
3. 验证：`node "<技能目录>\scripts\pdf.js" text out.pdf 2` 确认文字进去且中文不乱码。

## 3. 边界（如实告知用户）

- 提取乱码 = 原文件字体缺 ToUnicode 映射，如实说明，建议对方提供文本版；**不要拿图片 OCR 猜**。
- `merge`/`split` 会重写文件结构，表单字段与签名可能失效，动之前提醒。
- 本机没有 Edge/Chrome 完整版时会用 WebView2 运行时出片，效果一致；连 WebView2 都没有时脚本会明确报错，不要谎称完成。

## 4. 环境安装（仅缺依赖时）

```
npm install --prefix "%USERPROFILE%\.agent\skill-modules" pdf-parse@1.1.1 pdf-lib
```
