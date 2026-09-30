---
name: docx
description: 创建与读取 .docx Word 文档：make 按 JSON 规格生成（标题/段落/列表/表格/分页），read 转结构化 HTML 查看内容与格式
---

# docx 技能（Word 文档）

适用：生成 .docx（报告/合同/简历/公文等）与读取已有 .docx 的内容。
本机无 LibreOffice/Office 依赖，走 Node：生成用 docx-js，读取用 mammoth。

## 0. 环境准备（一次性）

先探测：`node "%USERPROFILE%\.agent\skill-modules\node_modules\docx\package.json"` 存在即已装。
没装就跑（要联网，告诉用户这一步在装什么）：

```
npm install --prefix "%USERPROFILE%\.agent\skill-modules" docx mammoth
```

依赖装不上时**如实报错并停下**，不要用改后缀名 .txt/.html 冒充 docx。

## 1. 生成：make

1. 把文档规格写成 JSON 文件（格式见下），如 `doc-spec.json`
2. 跑：`node "<技能目录>\scripts\docx.js" make doc-spec.json 输出.docx`
   （`<技能目录>` 见 read_skill 返回的头部标注）

规格格式（children 按序渲染）：

```json
{
  "children": [
    { "type": "heading", "level": 1, "text": "一级标题" },
    { "type": "para", "text": "普通段落，支持 bold/italic/align:center" },
    { "type": "bullets", "items": ["无序列表项"] },
    { "type": "numbered", "items": ["有序列表项"] },
    { "type": "table", "header": ["列A", "列B"], "rows": [["a1", "b1"], ["a2", "b2"]] },
    { "type": "pagebreak" }
  ]
}
```

默认字体微软雅黑 12pt。长文档一次写全 spec，不要分段多次生成再拼接。

## 2. 读取：read

`node "<技能目录>\scripts\docx.js" read 已有文档.docx` —— 输出结构化 HTML
（标题层级/段落/表格都在，图片以 [图片] 占位）。读表格数据优先用本命令，别用 read_file 直接读二进制。

## 3. 边界（如实告知用户）

- **改已有 docx**：本技能只覆盖「读出内容 → 生成新版」，不做原地编辑；要保留原排版做小改，说明此局限。
- **转 PDF**：不经本技能；有 Edge/Chrome 时可用 pdf 技能的 from-html 路径，效果不如 Word 另存，先说明。
- 生成的文件落在工作区，用相对路径传给脚本即可。
