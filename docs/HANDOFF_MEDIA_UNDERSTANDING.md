# 多模态媒体转接与理解系统交接文档 (Handoff Document)

> **版本**：v1.0.0  
> **分支**：`dev`  
> **对齐标准**：`deepseek-harness` (`@deepseek-ai/dsh-media-understanding`)  
> **更新时间**：2026-09-04  

---

## 1. 系统背景与目标

### 1.1 业务诉求
本项目主模型主要为纯文本/代码推理模型（如 `deepseek-v4-flash`），该类模型原生不支持图片、音频和视频的直接多模态二进制输入。
- **严禁全局切换**：不能因为用户发送了一张图片或一段录音，就将整个会话的主控模型切换给多模态模型（会丢失主力模型的深度推理和工具链能力）；
- **后端旁路投影**：必须在后端服务层完成多模态附件的旁路理解，将理解模型生成的结构化描述/逐字转录包装后，以文本形式投喂给主模型；
- **前端无缝体验**：输入栏支持截图直接粘贴（Ctrl+V）、文件拖拽放入、以及点击 📎 按钮选取文件，并在对话历史中原生渲染图片预览、音频/视频播放器。

### 1.2 参考实现
对齐用户指定的工业级参考实现：  
`C:\Users\大脚\Desktop\DSH\deepseek-harness\packages\llm\media-understanding`

---

## 2. 核心问题复盘与抓包根因分析

在开发测试过程中，曾出现“用户发送录音，主模型 DeepSeek 却回复一段 AI 声明自己听不了音频”的典型乌龙现象。通过抓包与分析原始日志（`logs/agent.jsonl`），查明了完整的技术链路：

```
[用户上传音频]
      │
      ▼
[后端构造 input_audio 发往反代 (tokeness / New-API)]
      │
      ├─ 异常 1: New-API 接收 input_audio 时返回 200 OK，但 JSON Body 内为业务错误：
      │         {"error": {"message": "Invalid input", "param": "messages.0.content"}}
      │         (旧代码未检测 200 响应体内的 error 字段，误以为返回空内容)
      │
      ▼
[后端自动尝试格式 2 (audio_url)]
      │
      ├─ 异常 2: New-API 不支持 audio_url 字段，静默剥离了该媒体块，
      │         仅将纯文本提示词 [text: "Transcribe this audio..."] 转发给 mimo-v2.5
      │
      ▼
[mimo-v2.5 (原生全模态大模型) 接收到请求]
      │
      ├─ 现象: mimo-v2.5 上下文中没有音频数据，只有一句转录提示词
      │       因此礼貌回复："I'd be happy to help... However, it looks like no audio file was attached to your message."
      │
      ▼
[旧逻辑漏洞]
      │
      └─ 旧代码仅过滤了中文拒答词，漏过了英文未见附件声明，将其当作“真实转录”
         包装为 [Audio understanding (custom/mimo-v2.5): ...] 喂给 DeepSeek。
         DeepSeek 以为音频内容就是这段英文，进而在推理链和回答中围绕这段文字进行解释。
```

---

## 3. 架构设计与关键改动

本次改动涵盖了从前端交互、配置推导、模态预检门禁、网关传输、到结果投影的全链路改造：

```
┌─────────────────────────────────────────────────────────────┐
│ 1. 前端层 (web/src/App.svelte)                               │
│    - 剪贴板粘贴 / 拖拽 / 📎 附件选择器 (支持图片、视频、音频)       │
│    - 气泡渲染：图片(点击大图预览)、<video> 原生播放、<audio> 控件   │
│    - 角色分工配置：视觉 / 视频 / 音频 专属模型下拉绑定               │
└──────────────────────────────┬──────────────────────────────┘
                               │ POST /api/chat (带 attachments)
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ 2. 服务转接层 (src/server.rs)                                 │
│    - 附件安全落盘：Base64 宽容解码写入 <workspace>/.attachments/ │
│    - 严格路由隔离：仅读取 roles 中显式配置的渠道，绝不盲目跨配置猜测 │
│    - acceptsMedia 门禁：检测模型是否支持对应模态，不支持则确定性占位 │
│    - 多载荷试探：支持 input_audio、audio_url、video_url 多格式探测 │
│    - 拒答与未见附件语义拦截：过滤中英文拒绝与未附带音频声明         │
└──────────────────────────────┬──────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ 3. 网关与会话层 (src/gateway/ & src/session/)                 │
│    - gateway.complete 增加 JSON Body error 拦截 (防 200 OK 假成功)  │
│    - 全模态推导机制：标定 mimo 等模型具备 vision/video/audio 能力  │
│    - 会话标题过滤：剔除前置 [...] 媒体块标签，防止侧栏标题被污染    │
│    - 细粒度审批规则引擎：Safe(0) / Normal(1) / Dangerous(2) 分级卡点│
└─────────────────────────────────────────────────────────────┘
```

---

## 4. 文件改动对照清单

| 文件路径 | 改动核心职责 |
| :--- | :--- |
| `src/server.rs` | 1. 增加 `accepts_modality` 能力门禁（对齐 DSH `acceptsMedia` 与 `unsupportedRouteText`）；<br>2. 附件解码并写入 `<workspace>/.attachments/<文件名>`；<br>3. 废除全局配置跨渠道猜测逻辑；<br>4. 增加多格式载荷试探及中英文未见附件/拒答语义拦截；<br>5. 增加附件编解码单元测试。 |
| `src/gateway/mod.rs` | 1. 在 `Gateway::complete` 中增加对返回 JSON 中 `error` 字段的显式解析，彻底堵住反代以 `200 OK` 返回业务错误导致的误判漏洞。 |
| `src/config/settings.rs` | 1. 在 `ModelCapabilities::infer_from_model_id` 中为 `mimo` 模型系列补充 `video: true` 与 `audio: true`，标定原生全模态支持。 |
| `src/session/mod.rs` | 1. 在 `first_user_title` 提取首条消息生成标题时，剥离前置的 `[Image understanding...]` / `[Audio understanding...]` / `[... omitted ...]` 等标签，避免侧栏标题污染。 |
| `src/core/rules.rs` | 1. 新增细粒度工具调用审批规则引擎（P9 规划）；<br>2. 区分只读安全（Safe=0）、常规操作（Normal=1）与高危破坏/系统命令（Dangerous=2）。 |
| `src/core/agent.rs` | 1. 接入 `evaluate_risk`，针对不同风险等级执行智能审批，高危命令携带原因与风险等级上报前端。 |
| `src/core/mod.rs` | 1. 导出 `rules` 模块。 |
| `web/src/App.svelte` | 1. 增加剪贴板 `paste`、文件 `drop` 及 `fileInput` 📎 按钮选取；<br>2. 待发送区域展示附件卡片（名称、大小缩略、移除按钮）；<br>3. 消息气泡内渲染图片（带大图模态框）、原生 `<audio>` 与 `<video>` 控件；<br>4. 设置中增加音频、视频专职角色绑定；<br>5. 前端能力推导补充 `mimo` 的全模态支持。 |

---

## 5. DSH 规范对齐矩阵

| DSH 原版机制 (`media-understanding`) | 本项目 Rust 实现位置 | 对齐状态 |
| :--- | :--- | :---: |
| `dedicatedRoute(feature)` 显式路由判定 | `src/server.rs` 中的 `role_target` 解析 | ✅ 完全一致 |
| 无配置时输出 `[... omitted because no ... model is configured]` | `src/server.rs` 中的 `omitted` 占位生成 | ✅ 完全一致 |
| `acceptsMedia` 门禁与 `unsupportedRouteText` 拦截 | `src/server.rs` 中的 `accepts_modality` | ✅ 完全一致 |
| 媒体在前，提示词在后（`userParts` 顺序） | `src/server.rs` `candidate_prompts` 结构 | ✅ 完全一致 |
| 辅助调用限制上限 1500 tokens，去除 thinking | `src/server.rs` 中 `params.max_tokens = Some(1500)` | ✅ 完全一致 |
| 本地文件系统持久化查看支持 | 落地至工作区 `.attachments/` 目录 | ✅ 增强实现 |

---

## 6. 测试与验证记录

### 6.1 单元测试（自动化）
执行 `cargo test`，涵盖网关重试、思考标签剥离、附件编解码、会话持久化、沙箱隔离及规则引擎等全部测试集：
```text
test result: ok. 67 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.21s
```

### 6.2 前端构建
在 `web/` 目录下执行打包验证：
```text
npm run build
dist/index.html                  0.41 kB │ gzip:  0.30 kB
dist/assets/index-ZrByqOg0.css  19.76 kB │ gzip:  3.80 kB
dist/assets/index-DD69D6XH.js   78.99 kB │ gzip: 26.74 kB
✓ built in 605ms
```

---

## 7. 生产部署与使用注意事项

1. **音频直连渠道配置**：
   - 若要实现真正的语音转文字和音频内容理解，推荐在“设置 → 角色分工”的“音频理解专家”中配置支持原生音频输入的端点（如 Gemini 2.0 Flash、Groq Whisper 或 SiliconFlow SenseVoice 等）；
   - 若使用第三方中转代理站点（如 One-API / New-API），需确保该代理站对应渠道的配置中开启了多模态/文件转发，并未在中间层剥离 `input_audio` 载荷。
2. **本地附件清理**：
   - 附件落地于工作区根目录下的 `.attachments/` 隐藏目录中，用户可直接查看、播放或在沙箱工具中引用；若工作区清理时可安全删除该目录。
