---
name: pptx
description: 创建与「原地」改 .pptx：make 按 JSON 规格生成幻灯片、text 逐页读出文字（含备注）；勿另造脚本勿用 Python
---

# pptx 技能（演示文稿）

## 硬约束（先读完再动手）

1. **所有命令用 read_skill 返回的绝对「技能目录」路径开头**；read_file/shell 的相对路径相对**工作区**，不许拿它们读技能文件——必报 os error 2。
2. **只用 `scripts/pptx.js`，不许自造生成脚本、不许上 Python**（本机没有 Python；python-pptx 路线直接堵死）。
3. **「改」已有 pptx = `text` 读出全部文字 → 改 spec → `make` 输出覆盖原路径**（用户视角的原地改）。版式会重排——原模板/动画/图表不保留，动手前先说明这个局限。
4. 不用 subagent_spawn 验证；改完 `text` 回读核对就是验证。
5. 依赖缺失只许装到 `~/.agent/skill-modules`；装不上停下来如实报告。

## 0. 环境检测（任务第一步）

```
node "<技能目录>\scripts\pptx.js"
```

打印 usage = 就绪；missing dependency → 跑页尾安装命令。

## 1. 工作流

1. `read_skill("pptx")`（已做；返回头部有绝对技能目录与资产清单）
2. 已有文件先 `node "<技能目录>\scripts\pptx.js" text <原文件.pptx>` 读出逐页文字（含备注）
3. 写/改 spec 为工作区内 `.json`（临时文件用完即删）
4. `node "<技能目录>\scripts\pptx.js" make <spec.json> <输出.pptx>`（原地改 = 原路径覆盖）
5. `text <输出.pptx>` 回读核对页数与文案

## 2. spec 格式

```json
{
  "title": "季度复盘",
  "slides": [
    { "title": "结论先行", "bullets": ["营收超预期 12%"], "notes": "口播提示" },
    { "title": "风险", "bullets": [{ "text": "供应链", "level": 0 }, { "text": "备选厂商", "level": 1 }] }
  ]
}
```

- `title` 有值时第 1 页为封面。
- bullets 支持字符串或 `{text, level}`（level 0-4）。

## 3. 边界（如实告知用户）

- 版式为「标题 + 项目符号」基础款；图表、图片、动画、模板复刻都不做，要更复杂的设计说明局限。
- `text` 只导文字：旧文件里的表格/图示不在文字流里，改版时可能丢失，先说明再动手。

## 4. 环境安装（仅缺依赖时）

```
npm install --prefix "%USERPROFILE%\.agent\skill-modules" pptxgenjs jszip
```
