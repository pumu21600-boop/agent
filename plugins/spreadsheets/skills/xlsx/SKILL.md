---
name: xlsx
description: 读/建 .xlsx 与 csv 表格：read 把各 sheet 数据导出为 JSON，make 按规格生成 xlsx，csv 与 xlsx 互转
---

# xlsx 技能（表格）

适用：读表格数据做分析/汇总、按数据生成 .xlsx、csv 与 xlsx 互转。
走 Node + SheetJS，不依赖 Excel/WPS。

## 0. 环境准备（一次性）

```
npm install --prefix "%USERPROFILE%\.agent\skill-modules" xlsx
```

依赖装不上时如实报错停下，不要拿 csv 改后缀名冒充 xlsx。

## 1. 命令（`<技能目录>` 见 read_skill 返回的头部标注）

- `node "<技能目录>\scripts\xlsx.js" read 数据.xlsx [sheet名] [行数上限]`
  不带 sheet 名先列全部 sheet 名与行数；带则导出该 sheet 为 JSON（首行作表头）。
  默认最多 200 行，要更多就传行数上限，别一次倒几十万行进上下文。
- `node "<技能目录>\scripts\xlsx.js" make sheet-spec.json 输出.xlsx`
- `node "<技能目录>\scripts\xlsx.js" csv 数据.csv 输出.xlsx`
- `node "<技能目录>\scripts\xlsx.js" tocsv 数据.xlsx [sheet名] 输出.csv`

## 2. make 规格

```json
{
  "sheets": [
    { "name": "月度", "rows": [["月份", "销售额"], ["1月", 1200], ["2月", 1580]] },
    { "name": "明细", "rows": [["日期", "金额"], ["2026-01-05", 320.5]] }
  ]
}
```

- rows 是二维数组，数字/布尔/字符串按原类型写入，日期一律先转成字符串。
- 多个 sheet 就放多项；要公式时直接写字符串 `=SUM(B2:B13)`（打开文件时由 Excel 计算）。

## 3. 边界（如实告知用户）

- 读取丢格式：颜色/合并单元格/图表拿不到，导出的是**数据**；要做图表请生成后让用户在 Excel 里加，或走图表库另行出图。
- make 生成的是新表，不做「在原文件上小改」——改已有文件 = 读出数据 → 改数据 → 生成新版，原样式不保留，先说明再动手。
- 大文件先 `read <文件> <sheet> 5` 看结构，确认列含义再决定导多少。
