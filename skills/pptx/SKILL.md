---
name: pptx
description: 创建与保真修改 .pptx：增量改字不动版式
---

# pptx 技能（演示文稿）

## 硬约束（先读完再动手）

1. **所有命令用 read_skill 返回的绝对「技能目录」路径开头**；read_file/shell 的相对路径相对**工作区**，不许拿它们读技能文件——必报 os error 2。
2. **只用 `scripts/pptx.js`，不许自造生成脚本、不许上 Python**（本机没有 Python；python-pptx 路线直接堵死）。
3. **改已有 pptx 优先走 `replace` 保真增量改，不是 `text→spec→make` 重排**。replace 只动 ops 指定的文字/页，动画、主题、模板、母版原样保留；make 是整册重打，只用于「新建」或「用户明确接受版式重排的整册重做」。
4. **默认交付 `<原名>_updated.pptx` 副本，保原件**；用户明说「原地改/覆盖原文件」才 out 写原路径。
5. 不用 subagent_spawn 验证；改完 `text <输出.pptx>` 回读核对页数与文案，**再跑 `check.js` 结构验收（1.5 节）**，check 有 FAIL 就不许收尾。
6. 依赖缺失只许装到 `~/.agent/skill-modules`；装不上停下来如实报告。

## 0. 环境检测（任务第一步）

```
node "<技能目录>\scripts\pptx.js"
```

打印 usage = 就绪；missing dependency → 跑页尾安装命令。

## 1. 工作流

### 改已有文件（用户最常见场景）

1. `read_skill("pptx")`（已做，注意返回头部的绝对技能目录）
2. `node "<技能目录>\scripts\pptx.js" text <原文件.pptx>` 读出逐页文字（含备注），定位要改的页与文案
3. 把 ops 写成工作区内 `ops.json`（临时文件用完即删）
4. `node "<技能目录>\scripts\pptx.js" replace ops.json <原文件.pptx> <输出.pptx>`（输出默认给 `<原名>_updated.pptx`）
5. `text <输出.pptx>` 回读核对，把结果文件路径报给用户

### 新建演示

1. 写 spec JSON（见第 3 节）到工作区
2. `node "<技能目录>\scripts\pptx.js" make <spec.json> <新文件.pptx>`

## 2. replace 规格（ops）

```json
{
  "ops": [
    { "op": "replaceText", "find": "2025 财年", "replace": "2026 财年" },
    { "op": "replaceText", "find": "营收增长", "replace": "业绩增长", "slide": 2 },
    { "op": "deleteSlide", "slide": 3 },
    { "op": "duplicateSlide", "slide": 1 },
    { "op": "moveSlide", "slide": 4, "to": 2 }
  ]
}
```

- `replaceText`：`slide` 省略=全 deck 替换；页码按**演示顺序**（sldIdLst 当前顺序，即用户看到的第几页）。
- 文本被 PPT 拆进多个 run 的整句替换：首 run 承载全段新文本、其余 run 清空——run 间样式差异会统一成首 run 的样式，动手前如察觉风险先说明。
- find 为空串会直接报错（会匹配一切）；0 命中会打 WARN 并继续，回读发现没改成就核对 find 串。
- `duplicateSlide` 复制整页（含图片，media 一并复制）；**含图表/嵌入对象/音视频的页拒绝复制**并如实报告，那类改动用 make 重做或让用户在 PowerPoint 里操作。
- `deleteSlide` 连带删该页备注，只剩最后一页时拒绝删。
- ops 按数组顺序执行，页数随删/复制变化，**后面的页码按执行到那一刻的 deck 算**（先删后 dup 要注意序号）。
- 页码越界/结构异常会报错并说明现状，报错信息就是改 ops 的依据。

### 收尾验收（改完必跑，不过不收尾）

```
node "<技能目录>\scripts\check.js" <输出.pptx> --expect "新文本" --not "旧文本" --slides 4
```

- `--expect`/`--not` 按 ops 改了什么传什么；`--slides N` 核对最终页数（还顺手查「slide 文件数 == sldIdLst 注册数」，专抓删页/复制页的注册残留）。
- `--render` 可选：有 LibreOffice 时做渲染冒烟，没有自动跳过。
- check 报 FAIL = 结构坏/该改的没改：回滚输出、修 ops 重做，**不要向用户报告「已完成」**。

## 3. spec 格式（make 新建用）

```json
{
  "title": "季度复盘",
  "slides": [
    { "title": "结论先行", "bullets": ["营收超预期 12%"], "notes": "口播提示" },
    { "title": "风险", "bullets": [{ "text": "供应链", "level": 0 }, { "text": "备选厂商", "level": 1 }] }
  ]
}
```

- `title` 有值时第 1 页为封面；bullets 支持字符串或 `{text, level}`（level 0-4）。

## 4. 边界（如实告知用户）

- replace 保真：文字、字体颜色、版式、动画、切换、母版、主题全保留；**不改**版式本身（移动/缩放元素、改模板配色这类诉求做不到，如实说）。
- make 版式是「标题 + 项目符号」基础款；图表/图片/复杂设计不做，用户要更复杂的先说明局限。
- `text` 只导文字：表格/图示内容可能不在文字流里，凭 text 判断「deck 里有什么」会漏，重要读取可以先预览面板看渲染效果。
- 页序说明：`text` 输出按文件编号不是演示顺序；判断最终页序用 `replace` 摘要或预览面板。

## 5. 环境安装（仅缺依赖时）

```
npm install --prefix "%USERPROFILE%\.agent\skill-modules" pptxgenjs jszip
```
