---
name: xlsx
description: 创建与「保真」改 .xlsx：patch 增量改已有文件（公式/样式/合并/列宽全保留）、make 新建、read/tocsv 读出、csv 互转；勿另造脚本勿用 Python
---

# xlsx 技能（表格）

## 硬约束（先读完再动手）

1. **所有命令用 read_skill 返回的绝对「技能目录」路径开头**；read_file/shell 的相对路径相对**工作区**，不许拿它们读技能文件——必报 os error 2。
2. **只用 `scripts/xlsx.js`，不许自造解析脚本、不许上 Python**（本机没有 Python；pandas/openpyxl 路线直接堵死）。脚本双引擎：`xlsx`(SheetJS) 管 read/make/csv/tocsv，`exceljs` 管 patch。
3. **改已有表格一律走 `patch` 保真增量改，不要走 `read → 改 JSON → make` 重写**（make 是整表重打，颜色/合并单元格/列宽全丢）。patch 只动 ops 指定的单元格，其余原样。
4. **默认交付 `<原名>_updated.xlsx` 副本，保原件**；用户明说「原地改/覆盖原文件」才 out 写原路径。
5. 不用 subagent_spawn 验证；改完 `read <输出> <sheet> 3` 抽查前几行，**再跑 `check.js` 结构验收（上方 patch 步骤）**，check 有 FAIL 就不许收尾。
6. 依赖缺失只许装到 `~/.agent/skill-modules`；装不上停下来如实报告。

## 0. 环境检测（任务第一步）

```
node "<技能目录>\scripts\xlsx.js"
```

打印 usage = 就绪；missing dependency → 跑页尾安装命令。

## 1. 工作流

### 改已有文件（用户最常见场景）

1. `read_skill("xlsx")`（已做，注意返回头部的绝对技能目录）
2. `node "<技能目录>\scripts\xlsx.js" read <原文件.xlsx>` 先列全部 sheet 与规模，再按需 `read <文件> <sheet> [行数上限]` 出 JSON 摸清结构
3. 把 ops 写成工作区内 `ops.json`（临时文件用完即删）
4. `node "<技能目录>\scripts\xlsx.js" patch ops.json <原文件.xlsx> <输出.xlsx>`（输出默认给 `<原名>_updated.xlsx`）
5. `read <输出.xlsx> <sheet> 3` 抽查，把结果文件路径报给用户

### 收尾验收（改完必跑，不过不收尾）

```
node "<技能目录>\scripts\check.js" <输出.xlsx> --expect "新值" --not "旧值" --sheets 2
```

- `--expect`/`--not` 按 ops 改了什么传什么；`--sheets N` 核对最终 sheet 数。
- `--render` 可选：有 LibreOffice 时做渲染冒烟，没有自动跳过。
- check 报 FAIL = 结构坏/该改的没改/数字变样：回滚输出、修 ops 重做，**不要向用户报告「已完成」**。

### 新建表格

1. 写 spec JSON（见第 3 节）到工作区
2. `node "<技能目录>\scripts\xlsx.js" make <spec.json> <新文件.xlsx>`

## 2. patch 规格（ops）

```json
{
  "ops": [
    { "op": "set", "sheet": "月度", "cell": "B2", "value": 1200 },
    { "op": "formula", "sheet": "月度", "cell": "D5", "value": "=SUM(B2:B9)" },
    { "op": "setText", "sheet": "月度", "cell": "A1", "value": "表头文字" },
    { "op": "addRow", "sheet": "月度", "at": 5, "values": ["三月", 2800, ""] },
    { "op": "delRow", "sheet": "月度", "at": 7 },
    { "op": "addCol", "sheet": "月度", "col": "D", "header": "同比", "values": ["+12%", "-3%"] },
    { "op": "delCol", "sheet": "月度", "col": "E" },
    { "op": "setStyle", "sheet": "月度", "cell": "B2", "style": { "bold": true, "color": "#FF0000", "bg": "#FFFF00", "align": "center", "fontSize": 14 } },
    { "op": "merge", "sheet": "月度", "range": "A1:D1" },
    { "op": "unmerge", "sheet": "月度", "range": "A1:D1" },
    { "op": "colWidth", "sheet": "月度", "col": "A", "width": 22 },
    { "op": "addSheet", "sheet": "汇总", "rows": [["指标", "值"], ["总销售额", "=SUM(月度!B2:B9)"]] },
    { "op": "delSheet", "sheet": "旧表" },
    { "op": "renameSheet", "sheet": "旧名", "to": "新名" }
  ]
}
```

- `set` 的值自动推断类型：数字字面量→数值，`=` 开头→公式，其余→文本；`setText` 强制文本（长编号/电话别被转成数字）；`formula` 强制公式。
- `addCol` 的 col 是「插在该列之前」（1-based，A=1）；列号也支持 Excel 列字母。
- `addRow` 的 at 是行号（1-based，插在该行之前）；`delRow`/`delCol` 同样按人话行/列号。
- `setStyle` 是**增量合并**：只改指定的字体/填充/对齐槽位，不动其他样式属性；range 可批量（`"range": "A1:D1"`）。
- style 可用键：`bold/italic/underline/fontSize/fontName/color/bg/align(left|center|right)/valign/wrap/numFmt(如 "0.00%")`，`border` 高级用法可直接传 ExcelJS border 对象。
- 单个 patch 里 ops 按数组顺序执行，注意相互影响（先删列再按列号操作会错位）。
- `at`/`cell` 越界、sheet 名不存在会直接报错并列出可用 sheet——报错信息就是改 ops 的依据。

## 3. read 输出 / make 规格

read：`{"sheet":"月度","total_rows_limited_to":200,"rows":[[表头],[行],...]}`，首行即表头；日期单元格已转成 `YYYY-MM-DD` 人类可读串（不是序列号）。**read 出来的是数据不是排版**——颜色/合并/图表看不到，判断排版用预览面板。

make（仅新建用）：`{"sheets":[{"name":"月度","rows":[["月份","销售额"],["1月",1200],["合计","=SUM(B2:B3)"]]}]}`，rows 二维数组，公式写 `=...` 字符串。

## 4. 边界（如实告知用户）

- patch 保真：数值/文本/公式/字体/填充/边框/对齐/合并单元格/列宽都在；**ExcelJS 不保真的：图表、条件格式、数据透视表、VBA 宏**——遇到「这表里有图表/透视表」的需求，先如实说这些会丢，问用户是否接受（或改让他们用 WPS/Excel 手动）。
- read 丢格式（颜色/合并/图表），那是给人看的不是给改的。
- 大表先 `read <文件> <sheet> 5` 探结构，再决定导多少行，别一次倒几十万行进上下文。
- `csv <in.csv> <out.xlsx>` / `tocsv <in.xlsx> [sheet] <out.csv>` 可互转；csv 里像日期/百分比的非纯数字串按原文保留。

## 5. 环境安装（仅缺依赖时）

```
npm install --prefix "%USERPROFILE%\.agent\skill-modules" xlsx exceljs
```
