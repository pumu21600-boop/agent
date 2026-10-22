---
name: docx
description: 创建与保真修改 .docx：改字不动版式，原地覆盖原文件
---

# docx 技能（Word 文档）

## 硬约束（先读完这 6 条再动手，违反任一就是把事做坏）

1. **所有命令用 read_skill 返回的绝对「技能目录」路径开头**（返回头部已列好资产清单）。read_file/shell 的相对路径是相对**工作区**的，绝不许拿它们读技能文件或脚本——必报 os error 2。
2. **只用本技能自带 `scripts/docx.js`，不许自己另写生成脚本、不许装其他 docx 库**。自造脚本会丢字体/结构语义，是「改完不对版」的头号原因。
3. **「改」已有文档 = nodes 读出 → 改 spec → make 输出到原同一路径覆盖**。这就是用户视角的原地改：不新建 `xxx2.docx`、不留中间文件（除非用户明确要求保留）。
4. **不许用 subagent_spawn 验证**——本技能的 `fonts`/`read` 就是验证工具。**本机没有 Python**，任何「用 python-pptx/Python 脚本处理」的想法都是错的，直接走本脚本。
5. 改完必须回读验证：动了字体就跑 `fonts`，动了内容就跑 `read`；**最稳妥是两条都跑再加 `check.js`**（下方 1.5 节），check 有 FAIL 就不许收尾。
6. 依赖缺失时只允许跑页尾的安装命令装到 `~/.pawch-agent/skill-modules`；装不上就停下来如实报告，不要降级凑合。

## 0. 环境检测（任务第一步，不跳过）

```
node "<技能目录>\scripts\docx.js"
```

打印 usage = 就绪；提示 missing dependency = 跑一次安装（见下）。

## 0.5 最快路径：只换字体（最常见的活，两条命令）

「把字体换成 X」「全文改楷体」——**别走 nodes/make 全套**，原位手术一条命令：

```
node "<技能目录>\scripts\docx.js" setfont <文件.docx> 行书
node "<技能目录>\scripts\docx.js" fonts <文件.docx>     # 核验：全篇 run 应为新字体
```

- 只改字体槽位，版式/字号/加粗/表格**原样不动**；要标题另用一种字体：`setfont <文件> 正文体 标题体`。
- 改内容/加章节才走下面的流程。

## 0.6 微创 patch：只改目标文字/段落（改已有文档的正门）

「把第三段加粗」「把某某改成某某」「删掉某段」「加一段」——**优先走 patch 微创**，别 text→spec→make 全篇重建（重建会丢原文档的部分语义）：

```
node "<技能目录>\scripts\docx.js" patch <ops.json|-> <in.docx> <out.docx>
```

ops 规格：

```json
{
  "tracked": false,
  "ops": [
    { "op": "replaceText", "find": "2024 年 3 月", "replace": "2025 年 10 月" },
    { "op": "styleRun", "find": "营收超额完成", "style": { "bold": true, "color": "#FF0000" } },
    { "op": "insertAfter", "find": "历时", "text": "（新增：第二阶段同步启动。）" },
    { "op": "deletePara", "find": "本段将被删除" },
    { "op": "setParaAlign", "find": "项目总结", "align": "center" }
  ]
}
```

- `replaceText` 在段落内替换；文本被拆进多个 run 的整句替换会合并为首 run 承载（run 间样式差异统一为首 run 样式）。
- `styleRun`：bold/italic/underline/color/highlight/size(pt) 只加不删，保留原 rPr 其他属性；可多次对不同文本用。
- `insertAfter` 在含 find 的段落后插入新段（继承该段首个 run 的样式）；`deletePara` 删整个段落。
- `setParaAlign`：center/right/left/justify。
- **`"tracked": true` = 红修订模式**：旧文本包 `w:del`、新文本包 `w:ins`（author=agent），用户在 Word 里看到修订痕迹，可逐条接受/拒绝。客户文档、审阅场景默认用它。
- 查找不到会打 WARN 或直接报错，按报错核对 find 串；**find 在页眉页脚/文本框里的文本 patch 不动**（只动 document.xml 正文）。
- **默认交付 `<原名>_updated.docx` 副本**；用户明说「原地改/覆盖」才 out 写原路径。
- 改完 `read <输出.docx>` 回读验证。

### 1.5 收尾验收（改完必跑，不过不收尾）

```
node "<技能目录>\scripts\check.js" <输出.docx> --expect "新文本" --not "旧文本"
```

- `--expect`/`--not` 按 ops 改了什么传什么（可多次）；`--render` 可选——有 LibreOffice 时额外做 soffice→PDF 渲染冒烟，没有就自动跳过（输出会标 render(skip)）。
- check 报 FAIL = 文件结构坏/该改的没改/不该动的动了：回滚输出、修 ops 重做，**不要向用户报告「已完成」**。

## 1. 工作流（创建 或 改内容）

1. `read_skill("docx")`（已做，注意返回头部的绝对技能目录与资产清单）
2. **有原文件先 `nodes <原文件>`** 读成 spec JSON（含字体/层级/表格）；只想看内容用 `read`
3. 按改动性质选路线：**换字体→setfont；只改文字/段落/样式→patch 微创；整篇重写/大改结构→改 spec → make**
4. `node "<技能目录>\scripts\docx.js" make <spec.json> <输出.docx>` 或 patch 的输出路径
   ——原地改时输出路径就是原文件路径（默认 _updated 副本，用户明说才覆盖）
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
| `patch <ops.json\|-> <in.docx> <out.docx>` | 微创增量改（替换/样式/增删段/对齐，可红修订） |
| `setfont <in.docx> <字体> [标题字体]` | 原位换字体（字槽手术） |
| `read <in.docx>` | HTML 结构视图（结构/表格全景） |
| `fonts <in.docx>` | 逐字体 run 计数核验（换字体后必跑） |

## 4. 常见失败与处置

- `os error 2` = 你又拿相对路径读技能文件了 → 看约束 1，一律绝对路径。
- 字体没变 = 只改了块级 `font` 漏了顶层（或反之）→ `fonts` 核验看实际 run。
- stdout 乱码：脚本状态行是 ASCII 的，以 JSON 数据为准，乱码只可能是你传的 spec 文件编码问题（spec 一律 UTF-8 无 BOM 写入）。

## 5. 环境安装（仅缺依赖时）

```
npm install --prefix "%USERPROFILE%\.pawch-agent\skill-modules" docx mammoth jszip
```
