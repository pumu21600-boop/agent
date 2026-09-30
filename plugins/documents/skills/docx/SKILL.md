---
name: docx
description: 创建与「原地」修改 .docx：nodes 读出为可编辑 spec、改完 make 输出覆盖原路径、fonts 核验字体；勿另造脚本
---

# docx 技能（Word 文档）

## 硬约束（先读完这 6 条再动手，违反任一就是把事做坏）

1. **所有命令用 read_skill 返回的绝对「技能目录」路径开头**（返回头部已列好资产清单）。read_file/shell 的相对路径是相对**工作区**的，绝不许拿它们读技能文件或脚本——必报 os error 2。
2. **只用本技能自带 `scripts/docx.js`，不许自己另写生成脚本、不许装其他 docx 库**。自造脚本会丢字体/结构语义，是「改完不对版」的头号原因。
3. **「改」已有文档 = nodes 读出 → 改 spec → make 输出到原同一路径覆盖**。这就是用户视角的原地改：不新建 `xxx2.docx`、不留中间文件（除非用户明确要求保留）。
4. **不许用 subagent_spawn 验证**——本技能的 `fonts`/`read` 就是验证工具。**本机没有 Python**，任何「用 python-pptx/Python 脚本处理」的想法都是错的，直接走本脚本。
5. 改完必须回读验证：动了字体就跑 `fonts`，动了内容就跑 `read`，把验证结果报告给用户再收尾。
6. 依赖缺失时只允许跑页尾的安装命令装到 `~/.agent/skill-modules`；装不上就停下来如实报告，不要降级凑合。

## 0. 环境检测（任务第一步，不跳过）

```
node "<技能目录>\scripts\docx.js"
```

打印 usage = 就绪；提示 missing dependency = 跑一次安装（见下）。

## 1. 工作流（创建 或 原地改，都走这 5 步）

1. `read_skill("docx")`（已做，注意返回头部的绝对技能目录与资产清单）
2. **有原文件先 `nodes <原文件>`** 读成 spec JSON（含字体/层级/表格）；只想看内容用 `read`
3. 写/改 spec 为工作区内一个 `.json`（临时文件改完即删）
4. `node "<技能目录>\scripts\docx.js" make <spec.json> <输出.docx>`
   ——原地改时输出路径就是原文件路径
5. `node "<技能目录>\scripts\docx.js" fonts <输出.docx>` + `read <输出.docx>` 验证并汇报

## 2. spec 格式

```json
{
  "font": "宋体",                 // 全局正文字体（默认 Microsoft YaHei）——换字体改这里
  "headingFont": "黑体",          // 全局标题字体（默认跟随正文）
  "children": [
    { "type": "heading", "level": 1, "text": "标题" },
    { "type": "para", "text": "正文，可加 bold/italic/align" },
    { "type": "bullets", "items": ["要点", { "text": "带格式的要点", "bold": true }] },
    { "type": "numbered", "items": ["第一步", "第二步"] },
    { "type": "table", "header": ["列A", "列B"], "rows": [["a1", "b1"]] },
    { "type": "pagebreak" }
  ]
}
```

- **全文换字体 = 改顶层 `font`（+`headingFont`）**；只想换某一段就在那一块加 `"font": "楷体"`（块级覆盖顶层）。
- `nodes` 读出的 spec 直接可喂回 `make`：读→改→覆盖，闭环。
- 图片：v1 技能不处理图文混排（插入图片请说明局限，不要伪造）。

## 3. 命令速查

| 命令 | 用途 |
| --- | --- |
| `nodes <in.docx>` | 读出可编辑 spec（字体/结构/表格都在） |
| `make <spec.json\|-> <out.docx>` | 生成/覆盖 docx |
| `read <in.docx>` | HTML 结构视图（结构/表格全景） |
| `fonts <in.docx>` | 逐字体 run 计数核验（换字体后必跑） |

## 4. 常见失败与处置

- `os error 2` = 你又拿相对路径读技能文件了 → 看约束 1，一律绝对路径。
- 字体没变 = 只改了块级 `font` 漏了顶层（或反之）→ `fonts` 核验看实际 run。
- stdout 乱码：脚本状态行是 ASCII 的，以 JSON 数据为准，乱码只可能是你传的 spec 文件编码问题（spec 一律 UTF-8 无 BOM 写入）。

## 5. 环境安装（仅缺依赖时）

```
npm install --prefix "%USERPROFILE%\.agent\skill-modules" docx mammoth jszip
```
