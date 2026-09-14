# agent —— Windows 原生类 Codex 自主编码 Agent

技术栈：Rust 后端（axum）+ Svelte 前端（Vite）+ SSE 流式。
当前进度：**P14 完成**（记忆库星云 + Skills + 服务自管理 + 设置-记忆补丁）；P13 桌面壳按用户指示搁置（web 定稿后启用）。
逐阶段排产与踩坑记录见 [docs/ROADMAP.md](docs/ROADMAP.md)，日志格式见 [docs/jsonl-schema.md](docs/jsonl-schema.md)。

## 功能总览

- **核心循环**：模型带 `tools` 决策 → 本地执行 → 结果回填 → 直到纯文本回答；步数上限、取消贯通；同轮只读工具并行执行，同路径写冲突自动跳过并回喂模型。
- **工具**：`read_file` / `read_page` / `search`(rg) / `web_fetch` / `write_file` / `shell` / `git`，外加动态挂载的 MCP（`mcp__<server>__<tool>`）与 WASM 插件（`wasm__<plugin>__<tool>`）。
- **审批与 Plan**：三档 `read-only` / `auto` / `full-access`（侧栏即时切换、请求级可覆盖），风险分级引擎逐条判定，「本会话同类允许」记忆授权范围；Plan 档只放行只读操作，最终回答可产出结构化计划卡（批准后自动转 Act 执行）。
- **执行沙箱**：Job Objects 整树击杀 + 受限 token + 环境白名单（密钥不进沙箱）；`[sandbox] mode = "appcontainer"` 为实验特性（LowBox + 能力 SID、默认断网）。
- **上下文工程**：token 预算裁剪、工具结果头尾压缩、`read_page` 分页读大文件。
- **恢复与撤销**：danger≥1 工具执行前拍影子仓库检查点；撤销只回退本任务改过的文件（绝不整树 reset）；对话流内显示「N 个文件已更改 +x −y ↩ 撤销」。
- **会话**：每会话一个文件落盘，刷新/重启自动恢复；置顶/归档/分叉/重命名/批量管理；侧栏项目树（工作区维度）；HANDOFF 交接文档（手动或上下文 ≥80% 自动）。
- **记忆**：双层存储（全局 `~/.agent` + 项目 `<workspace>/.agent/memory`）、events.log 事件流水（默认脱敏）、AGENTS.md 双层注入、滚动摘要；系统约定（`prompts/system.md`）**每轮任务实时读盘**，面板保存即生效。
- **记忆库与星云**：会话压成检索正文，EMBEDDING 角色席位走真语义向量（未配置时用本地双字哈希向量），词法+向量混合排序；前端体积渲染星云（离线烘焙 cubemap）按上下文长度驱动光点大小，支持检索高亮与相似会话。
- **Skills**：`<workspace>/skills/` 与 `~/.agent/skills/` 下的 `SKILL.md`（frontmatter：name/description）；system 提示只注入「名字+一句话」，模型按需用 `read_skill` 拉全文。
- **模型配置**：渠道/模型/角色三层解耦；API Key 按配置 DPAPI 加密保存；模型能力标记（工具/视觉/视频/音频）；思考档位按厂商规范适配（reasoning_content / thinking_param / effort_param 等）。
- **观测**：JSONL 六类事件（含 `kind` 子类，schema 定死）、日志按大小轮转、trace 时间线面板、eval 黑盒评测（`cargo run --bin eval`）。
- **界面**：Svelte 经典聊天布局 + 深/浅双主题 + 设置面板六分区（外观 / 模型配置 / 模型参数 / 角色分工 / 记忆 / 插件 / 工作区）。

## 目录结构

```
agent/
├── src/                  # Rust 后端
│   ├── main.rs           # 启动（配置/日志/工作区/记忆层装配）
│   ├── server.rs         # axum：静态页 + 全部 HTTP/SSE 接口 + 审批门 + 网关装配
│   ├── service.rs        # 服务自管理（start/stop/status/--install/双击智能启动）
│   ├── skills.rs         # SKILL.md 目录约定（frontmatter 解析、索引注入、读写）
│   ├── core/             # 核心循环、取消标志、审批规则引擎（rules.rs）
│   ├── gateway/          # 模型网关：providers 数据加载、端点/思考形态探测、降级链
│   ├── config/           # config.toml + settings.json（DPAPI 存 key）
│   ├── observability/    # JSONL 日志（6 类事件 + 轮转）+ trace 读取
│   ├── sandbox/          # 执行沙箱：Job Objects + 受限 token + AppContainer + 探针
│   ├── context/          # 上下文工程：token 估算/压缩/裁剪
│   ├── recovery/         # 影子仓库检查点 + HANDOFF 交接
│   ├── session/          # 会话落盘与恢复
│   ├── tools/            # 工具系统（内置 + 并行调度 + git 发现链）
│   ├── memory/           # 记忆 v1（events.log/AGENTS.md/摘要）+ library.rs 记忆库索引
│   └── plugin/           # mcp.rs（MCP stdio client）+ wasm.rs（WASM 插件宿主）
├── web/                  # Svelte + Vite 聊天界面（含 galaxy/ 体积星云渲染）
├── desktop/              # P13 Tauri 桌面壳（脚手架，搁置）
├── evals/                # eval 评测：tasks/ 任务集 + mcp-mini-server.mjs 冒烟工具
├── skills/               # 项目级技能（每个技能一个目录 + SKILL.md）
├── plugins/              # WASM 插件（*.wat 零工具链依赖）
├── prompts/system.md     # 系统约定（每轮任务实时读盘）
├── providers.json        # 供应商预设（数据驱动）
├── docs/                 # ROADMAP / jsonl-schema / workorders/ / 交接文档
├── config.toml           # 服务/网关/审批/沙箱/上下文/恢复/记忆配置
└── scripts/mock_provider.mjs  # 无 key 冒烟用的本地模拟渠道（测试工具，非 provider）
```

## 快速启动

**日常使用（脱离终端，推荐）**：双击 `start.bat` 后台启动（隐藏窗口），双击 `stop.bat` 停止。
打开 http://127.0.0.1:5173 。

**开发模式**：

```powershell
cd web; npm install; npm run build   # 前端构建（产物 web/dist，服务直接托管）
cd ..; cargo build --release         # 构建 release 二进制（start.bat 依赖）
cargo run                            # 前台调试运行（关终端即停）
```

**前端热更**：双击 `dev.bat`（vite 起在 5174，/api 与 /cancel 自动代理到 5173 主服务），改 `web/src` 即时生效，无需构建。

## 本机工具链（2026-09-14 装齐）

- **Rust**：`rust-toolchain.toml` 钉死 `stable-x86_64-pc-windows-msvc`（本机 GNU 工具链的 dlltool 不可用，别改回）。
- **Git**：PortableGit 2.55.0.windows.5，装在 `%LOCALAPPDATA%\PortableGit`，已加入用户 PATH；
  程序自身的 git 发现链（`AGENT_GIT_PATH` → PATH → 注册表 → 常见位置 → 捆绑 `tools/git`）也会自动找到它。
- **Node.js**：v24.21.0 LTS（含 npm 11），装在 `%LOCALAPPDATA%\Programs\nodejs`，已加入用户 PATH；`dev.bat` 与前端构建都用它。
- **提交身份**：用户级 `~/.gitconfig` 写的是 `大脚 <dajiao@local>`（与仓库既有历史一致）；要换名字：
  `git config --global user.name "你的名字"`。

## 分发：内置 git（用户不用装任何东西）

产品自带一份 **MinGit**（最小 git，下载 37MB / 解压 90MB；不需要管理员、不需要联网）：

- **打包**：`powershell -ExecutionPolicy Bypass -File scripts\bundle_git.ps1`
  —— 首次从华为云镜像下载 MinGit（GitHub 兜底）、做体积 + 归档完整性双重校验，
  然后放到 `<exe 同级>\tools\git-runtime.zip`（默认 `target\release\tools\`）。
- **运行时发现顺序**（`src/tools/mod.rs::git_exe`）：
  1. `AGENT_GIT_PATH` 环境变量；
  2. 系统 PATH → 3. 注册表 → 4. 常见安装位置（用户自己装的 git，优先使用）；
  5. exe 同级 `tools\git\`（想手动放便携版就放这）；
  6. **内置 zip 懒解压**：解压到 `%LOCALAPPDATA%\agent\runtime\git` 再使用
     （只发生在没装 git 的机器上，解压一次记 `git.runtime_extracted`；之后启动直接复用）。
- 覆盖 zip 位置可用环境变量 `AGENT_GIT_ZIP`（调试用）。
- 已知差异：MinGit 不带 git-lfs / bash / 图形界面——agent 的提交、diff、撤销、检查点都不需要它们；
  用户项目若使用 LFS，需另装完整版 git。

## 模型设置（侧栏「设置 → 模型配置」，改动自动保存即时生效）

- **配置 chips**：每个配置可起名，chip 显示名称 + key 状态圆点（绿=已存 key），点一下即切换；「＋ 新建配置」「✕ 删除当前」（至少保留一套）。
- 字段改完**自动保存并立刻生效**（无"保存/应用"按钮）：供应商下拉、Base URL、API Key（失焦/回车即存）、模型下拉、「获取模型」弹窗。
- **API Key 按配置独立加密保存**（DPAPI）：同一供应商换地址=独立一格 key，互不串；key 密文不出现明文，重启仍在；
  也支持环境变量 `DEEPSEEK_API_KEY` 兜底。key 永不写日志。
- **角色分工**：SUMMARY（滚动摘要）/ TITLE（会话标题）/ EMBEDDING（记忆库向量）可各指定专用渠道；
  前两者未配置回落主渠道，EMBEDDING **不走主渠道兜底**（未配置用本地哈希向量）。
- **能力标记**：模型支持工具/视觉/视频/音频由你在配置里勾选（用户标记优先于按模型名推导，可一键重新推导）。
- 用中转渠道：新建一套配置 → 选供应商 → Base URL 填中转地址 → 填 key → 获取并勾选模型。

## 审批档位

| 档位 | 读 | 写 | 执行（shell/git） | 说明 |
| --- | --- | --- | --- | --- |
| `read-only` | 自动 | 要批 | 要批 | 只读档 |
| `auto`（默认） | 自动 | 自动 | 要批 | 自动编辑 |
| `full-access` | 自动 | 自动 | 自动 | 脱沙箱直跑，需 `config.toml [agent] allow_unsandboxed = true`；`unsandboxed_approval = true` 可保留执行审批 |

- 审批卡片显示风险档位徽章与判定依据，可勾「本会话同类允许」；120s 超时视为拒绝。
- Plan 档（输入框 Plan/Act 切换）只放行只读工具，写类操作不进审批弹窗、直接驳回并回喂模型调整计划。

## 日志与排查

- **进程日志**：`logs/agent.jsonl`（单行 JSON，六类事件）。超过 `[logging].max_mb`（默认 20MB）自动归档成
  `agent.jsonl.1`（只留一份）并开新文件；`max_mb = 0` 关闭轮转。
- **时间线**：侧栏「时间线」面板（`GET /api/trace`）按任务切分展示事件流、分片聚合、工具配对耗时。
- **项目记忆流水**：`<workspace>/.agent/memory/events.log`（跟 git 仓库走，默认脱敏），与本条目的进程日志是两回事。
- **会话**：`logs/sessions/<id>.json`，每条会话一个文件。

```powershell
# 查真实 base_url + 完整请求体
Select-String -Path logs\agent.jsonl -Pattern 'gateway.request'
# 查流式分片
Select-String -Path logs\agent.jsonl -Pattern 'model_chunk'
```

schema 见 [docs/jsonl-schema.md](docs/jsonl-schema.md)。

## 接口

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/` | Svelte 页面（未构建时内置页） |
| POST | `/api/chat` | 对话主入口，SSE 流式（content / reasoning / tool_call / tool_result / approval_request / plan_proposal / notice / error / done） |
| POST | `/cancel` | 置位全局取消标志 |
| GET | `/api/cancel` | 查询取消标志 |
| GET | `/api/health` | 健康检查 |
| POST | `/api/approvals` | 审批决定（允许/拒绝 + 本会话同类允许） |
| POST | `/api/approval-mode` | 切换审批档（对进行中的请求即时生效） |
| POST | `/api/shell` | 终端面板：沙箱内跑一条用户命令 |
| GET | `/api/settings` / PUT | 设置读取 / 保存（清除语义见 ROADMAP 坑 14） |
| GET | `/api/providers` | 供应商预设列表 |
| POST | `/api/models` | 按当前渠道拉取模型清单 |
| POST | `/api/configs` | 配置管理：new / delete / apply（切换激活） |
| GET | `/api/sessions` | 会话列表（新→旧，含项目名/置顶/归档） |
| GET / DELETE | `/api/session?id=` | 恢复 / 删除单条会话 |
| POST | `/api/sessions/new` | 新建空会话并立即落盘 |
| POST | `/api/sessions/open` | 打开会话（工作区不同时先切换） |
| POST | `/api/sessions/meta` | 置顶 / 归档 / 重命名 |
| POST | `/api/sessions/fork` | 分叉会话（复制消息/摘要/工作区） |
| POST | `/api/sessions/batch` | 批量归档 / 取消归档 / 置顶 |
| POST | `/api/handoff` | 手动生成 HANDOFF.md |
| GET | `/api/changes?baseline=` | 本任务相对基线的改动文件清单（产物标签） |
| GET | `/api/checkpoints` | 影子仓库检查点列表（≤20） |
| POST | `/api/checkpoints/restore` | 撤销：逐文件回退检查点之后 agent 改过的文件 |
| GET | `/api/git/status` | 工作区 git 概览（分支/头部/脏文件/领先落后） |
| POST | `/api/git/commit` | 用户手动提交（按选中路径 stage + commit） |
| GET | `/api/trace?limit=N` | 最近 N 个任务的事件流（时间线数据源） |
| GET | `/api/workspace` | 当前工作区 + 最近项目列表 |
| POST | `/api/workspace` | 切换工作区 |
| GET | `/api/workspace/files` | 工作区文件相对路径列表（「@ 添加上下文」数据源） |
| GET | `/api/workspace/file?path=` | 读工作区文本文件（预览，沿用 read_file 越界防护） |
| POST | `/api/workspace/picker` | 弹 Win 原生选文件夹窗口 |
| POST | `/api/workspace/remove` | 从项目列表移除（不删目录） |
| POST | `/api/workspace/alias` | 项目重命名（别名，空名=清除） |
| GET | `/api/memory` | 记忆层状态 + 三类约定文件原文（system/global/project） |
| POST | `/api/memory/agents` | 保存约定文件（system 保存即生效） |
| GET | `/api/memory/library` | 记忆库光点数据（每次调用做有界增量同步） |
| POST | `/api/memory/library/reindex` | 强制重建向量 |
| POST | `/api/memory/search` | 检索（`query` 或 `like=<会话id>`） |
| GET / POST | `/api/mcp` | MCP server + WASM 插件状态 / 配置管理与生命周期动作 |
| GET / POST | `/api/skills` | 技能列表 / 保存·删除·读取 |
| GET | `/api/sandbox/selftest` | 沙箱探针套件（真实拉起受限进程） |

## 无 key 冒烟（可选）

本地起一个模拟 DeepSeek SSE 渠道，验证流式 + 取消 + 日志全链路：

```powershell
node scripts/mock_provider.mjs          # 终端 1：127.0.0.1:9399
$env:DEEPSEEK_BASE_URL = "http://127.0.0.1:9399"
$env:DEEPSEEK_API_KEY = "sk-test"
cargo run                                # 终端 2：127.0.0.1:5173
```

评测：`cargo run --bin eval`（临时工作区隔离 + 断言引擎 + 报告与 exit code，任务集在 `evals/tasks/`）。

## 已知边界（未做 / 搁置）

- **P13 桌面壳**：`desktop/` 脚手架已入库（窗口 + 托盘 + 服务保活），运行时冒烟未验证——按用户指示等 web 定稿后启用。
- **组件总线（UI 插槽）**：仅设计留口，尚未开工（需要运行时动态组件加载方案）。
- **MCP**：只做了 stdio transport + tools；HTTP/SSE transport、resources/prompts 未做。
- **WASM 插件**：单函数 ABI（alloc/describe/run）、零 host function 的纯计算沙箱。
- **eval 任务集**：首批 3 个（hello-file / fix-json / read-only-plan），后续功能未覆盖。
- **远端备份**：已推送到 GitHub 私有仓库 `https://github.com/pumu21600-boop/agent`（origin：`dev` 为主线 + 8 个历史阶段分支 + 全部标签）。
