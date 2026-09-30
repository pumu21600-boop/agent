---
name: xlsx
description: 创建与「原地」改 .xlsx：read 读出各 sheet 为 JSON、改完 make 覆盖原路径；csv 互转；勿另造脚本勿用 Python
---

# xlsx 技能（表格）

## 硬约束（先读完再动手）

1. **所有命令用 read_skill 返回的绝对「技能目录」路径开头**；read_file/shell 的相对路径相对**工作区**，不许拿它们读技能文件——必报 os error 2。
2. **只用 `scripts/xlsx.js`（SheetJS），不许自造解析脚本、不许上 Python**（本机没有 Python；pandas/openpyxl 路线直接堵死）。
3. **「改」已有表格 = `read` 出数据 → 改 JSON → `make` 输出覆盖原路径**（用户视角的原地改）。不新建 `xxx2.xlsx`，除非用户明确要求。
4. 不用 subagent_spawn 验证；改完 `read` 回读核对数字行数就是验证。
5. 依赖缺失只许装到 `~/.agent/skill-modules`；装不上停下来如实报告。

## 0. 环境检测（任务第一步）

```
node "<技能目录>\scripts\xlsx.js"
```

打印 usage = 就绪；missing dependency → 跑页尾安装命令。

## 1. 工作流

1. `read_skill("xlsx")`（已做；返回头部有绝对技能目录与资产清单）
2. `node "<技能目录>\scripts\xlsx.js" read <文件.xlsx>` 先列全部 sheet 与规模，再按需 `read <文件> <sheet> [行数上限]` 出 JSON
3. 改数据写成工作区内 `.json`（临时文件用完即删）
4. `node "<技能目录>\scripts\xlsx.js" make <spec.json> <输出.xlsx>`（原地改 = 原路径覆盖）
5. `read <输出.xlsx> <sheet> 3` 抽查前几行，确认数字没变样

## 2. read 输出

`{"sheet":"月度","total_rows_limited_to":200,"rows":[[表头],[行],...]}`，首行即表头；日期单元格已转成 `YYYY-MM-DD` 人类可读串（不是序列号）。

## 3. make 规格

```json
{
  "sheets": [
    { "name": "月度", "rows": [["月份", "销售额"], ["1月", 1200], ["合计", "=SUM(B2:B3)"]] }
  ]
}
```

- rows 是二维数组，数字/布尔按原类型写入，日期一律先转字符串。
- 公式直接写字符串 `=SUM(...)`（打开文件时由 Excel 计算）。
- 多 sheet 就放多项。

## 4. 边界（如实告知用户）

- 读取丢格式：颜色/合并单元格/图表拿不到，导出的是**数据**；`make` 是重写文件，原样式不保留——「改几个数」可以，「保留原排版只改字」做不到，先说清再动手。
- 大表先 `read <文件> <sheet> 5` 探结构，再决定导多少行，别一次倒几十万行进上下文。
- `csv <in.csv> <out.xlsx>` / `tocsv <in.xlsx> [sheet] <out.csv>` 可互转；csv 里像日期/百分比的非纯数字串按原文保留。

## 5. 环境安装（仅缺依赖时）

```
npm install --prefix "%USERPROFILE%\.agent\skill-modules" xlsx
```
