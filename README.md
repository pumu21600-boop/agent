# Pawch Agent

Rust 后端（axum）+ Svelte 前端（Vite）+ SSE 流式。版本号以 `Cargo.toml` 为准（当前 0.32.x）。
源码 crate 与二进制的技术标识符仍叫 `agent`（`agent.exe` / `agent-desktop.exe`、数据目录 `.agent/`），产品显示名是 **Pawch Agent**。
逐阶段施工记录与踩坑见 [docs/ROADMAP.md](docs/ROADMAP.md)，日志格式见 [docs/jsonl-schema.md](docs/jsonl-schema.md)，全部 HTTP/SSE 接口见 [docs/api.md](docs/api.md)。

## 功能总览

- **核心循环**：模型带 `tools` 决策 → 本地执行 → 结果回填 → 直到纯文本回答；**取消会话化**（停止只停当前会话的这一轮）；同轮只读工具并行执行，同路径写冲突自动跳过并回喂模型。
- **工具**：`read_file` / `read_page` / `search`(rg) / `web_search`（联网搜索，无需 key）/ `web_fetch` / `browser`（内置浏览器一个工具全覆盖：navigate/snapshot/click/fill/scroll/wait_for…，语义定位为主、坐标兜底，快照带稳定 ref 与增量 diff；与右侧「网页」面板共享实例）/ `write_file` / `edit_file`（局部替换）/ `shell` / `git` / `subagent_spawn`（派发子代理：独立会话跑完子任务，回答回注父任务；一轮多个即并行，成本记父任务），外加动态挂载的 MCP 工具（`mcp__<server>__<tool>`）与插件工具（`wasm__<plugin>__<tool>`）。
- **审批**：三档 `read-only` / `auto` / `full-access`（表见下）；档位按会话记忆、运行中切换下一步即生效，风险分级引擎逐条判定。Plan 是**接口层能力**（请求级 `plan_mode`：只放行只读工具，写类操作直接驳回并回喂模型调整计划），界面不提供手动切换。
- **执行沙箱**：Job Objects 整树击杀 + 受限 token + 环境白名单（密钥不进沙箱）为默认档；`appcontainer` 为实验档（探测链不过自动回退，UI 明示当前生效档）。浏览器工具走 Chromium 自带站点隔离，不进 Windows 沙箱。
- **上下文与恢复**：token 预算裁剪、工具结果头尾压缩、`read_page` 分页；danger≥1 工具执行前拍影子仓库检查点，撤销只回退本任务改过的文件（绝不整树 reset）。
- **会话**：每会话一个文件落盘，刷新/重启自动恢复；置顶/归档/分叉/重命名/批量管理；侧栏项目树（工作区维度）。
- **记忆**：双层存储（全局 `~/.agent` + 项目 `.agent/memory`）+ events.log 事件流水（默认脱敏）+ AGENTS.md 双层注入 + 滚动摘要；系统约定（`prompts/system.md`）每轮任务实时读盘。记忆库星云：会话压成检索正文，语义向量 + 词法混合排序，前端体积渲染。
- **Skills**：`skills/` 下的 `SKILL.md`（frontmatter：name/description）；system 提示只注入「名字+一句话」，模型按需用 `read_skill` 拉全文。
- **插件（契约 v1）**：一个目录 + 一份 manifest.json = 一个插件，清单只声明「有什么」（面板/工具/权限），不分类型；对外只留四个词：**Skill / MCP / 插件 / 沙箱**。面板跑在沙箱 iframe 里经 postMessage 白名单桥取数据；模型自产插件走 draft 隔离区（内存、权限清空、一键转正）；「插件制作器」内置插件把「说清需求 → 选定工作区 → 新对话自动带上规范提示词」跑成一键流程。契约全文见 [docs/plugin-contract-v1.md](docs/plugin-contract-v1.md)。
- **用量与成本**：每次模型调用在网关单点记账（token 分 prompt/completion/cache 三路 + 请求/失败/延迟分位 + 记账时算定冻结的费用），落 `logs/usage/ledger.jsonl`；「用量价格」插件在右侧面板显示**当前对话**的用量与费用，价格按「渠道 host + 模型 id」每百万 token 单价手填；红线 = 全局花费上限硬闸（`[cost] spend_cap_usd`）。
- **模型配置**：渠道/模型/角色三层解耦；API Key 按配置 DPAPI 加密保存；能力标记（工具/视觉/视频/音频）手动勾选优先于按名推导；思考档位按厂商规范适配。
- **观测与工程**：JSONL 六类事件 + 轮转、trace 时间线面板、eval 黑盒评测（12 个任务）；CI 跑 clippy + 全量测试 + 前端构建；写盘全部原子写。
- **界面**：Svelte 经典聊天布局 + 深/浅双主题 + 顶栏三面板（终端 / 产物与预览 / 网页——内置浏览器，桌面壳里是**真实网页**（child webview，可直接点击/滚动/输入，还能开真 DevTools），普通浏览器打开时回落截图帧流；agent 的 browser 工具打开的页面同场可见）+ 右侧面板页签含插件贡献的 dock 页签；设置面板八分区（外观 / 模型配置 / 功能桥接 / 记忆 / 技能 / MCP / 插件 / 定时）。

## 快速启动

**日常使用**：双击 `start.bat`（隐藏窗口，会一并起桌面壳），双击 `stop.bat` 停止；打开 http://127.0.0.1:5173 。

```powershell
cd web; npm install; npm run build   # 前端构建（产物 web/dist，服务直接托管）
cd ..; cargo build --release         # 构建 release 二进制（start.bat 依赖）
cargo run                            # 前台调试运行（关终端即停）
```

`dev.bat` 起 vite 5174 热更（/api 自动代理到 5173 主服务），改 `web/src` 即时生效。

**无头会话**（程序化唤醒，子代理/定时/脚本集成走这条）：`agent.exe run <消息> [--session <id>] [--mode <档位>] [--timeout <秒>] [--approve] [--json]`——唤醒一条会话跑完整轮，打印最终回答（退出码 0=完成）。

**定时任务**：设置 → 定时 页签（5 段 cron，本地时区）；到点唤醒 agent 跑一轮，完成记录可回看；任务类型 = 固定消息 / 记忆提醒（到点先检索最近对话让 agent 列「要跟进/待办」）。

**子代理**：`subagent_spawn` 把独立子任务派给子代理——独立会话跑完整轮（看不到父历史），回答回注父任务；一轮多个即并行，花费记父任务；点「停止」连带停掉它的全部子代理。

## 目录结构

```
agent/
├── src/            # Rust 后端
│   ├── server/     #   axum：全部 HTTP/SSE 接口 + 审批门 + 网关装配（16 个子模块）
│   ├── core/       #   核心循环、取消注册表（会话级）、审批规则引擎（rules.rs）
│   ├── gateway/    #   模型网关：providers 数据加载、端点/思考形态探测、降级链
│   ├── browser/    #   浏览器内核：CDP 客户端 + 标签页会话管理 + DOM 快照/diff
│   ├── sandbox/    #   执行沙箱：SandboxTier trait + 探针与档位探测链
│   ├── plugin/     #   插件核心：契约 v1 + draft 隔离区 + 面板扫描 + WASM 桥
│   ├── mcp/        #   MCP 独立层：stdio / Streamable HTTP / SSE 三 transport + OAuth
│   ├── usage/      #   用量台账：记账（费用冻结）+ 价格匹配 + 报表 + 花费硬闸
│   ├── session/ memory/ tools/ recovery/ schedule/ context/ config/ observability/
│   ├── skills.rs service.rs storage.rs hooks.rs main.rs
├── web/            # Svelte + Vite 聊天界面（含 galaxy/ 体积星云渲染）
├── plugins/        # 内置插件 9 个：board 项目文件 / browser 浏览器 / css-ui 界面微调 /
│                   #   disconnect-cancel 断线取消 / nebula 记忆库 / roles 功能桥接 /
│                   #   selfmod 中枢 / trace 时间线 / usage 用量价格
├── desktop/        # Tauri 桌面壳（内置浏览器宿主：child webview + 控制通道 + CDP）
├── evals/ skills/ prompts/ providers.json config.toml
├── docs/           # ROADMAP / api / plugin-contract-v1 / jsonl-schema / workorders/
├── scripts/        # package_release.ps1 / bundle_git.ps1 / mock_provider.mjs
└── LICENSE         # MIT
```

### 配置面地图（三类五面，写入口各一）

`config.toml` 与 `providers.json`（静态种子，随包只读）无运行时写；**`settings.json` 是唯一运行时用户配置**（写入口唯一：`storage::write_settings`）；`mcp_tokens.json`（DPAPI 逐条加密的运行时机密）与 `plugin_data/`（插件命名空间 KV）是**运行时状态不是配置**。字段名地图见 `src/config/settings.rs` 模块头。

## 运行形态（开发 / 打包两态）

资源定位只有一条规则（`config::app_root()`）：exe 同级没有 `config.toml` / `web` = **开发形态**（回落编译期源码目录）；有 = **打包形态**（配置/日志/插件全落 exe 同级，整个目录可拷到任何地方直接双击 `start.bat`）。侧栏底部版本行悬停显示当前「运行根」。

**来源校验**：服务只接受本机来源的请求（`Host` 必须回环、带 `Origin` 必须同源同端口）——绑定 127.0.0.1 挡不住 DNS rebinding，这层校验补的就是它；curl / eval / 服务自管理等无 Origin 客户端照常可用。

## 模型设置（侧栏「设置 → 模型配置」，改动自动保存即时生效）

配置 chips 可起名切换（至少保留一套）；字段改完即存即生效（无保存按钮）；API Key 按配置 DPAPI 加密（同供应商换地址=独立 key，也支持 `DEEPSEEK_API_KEY` 环境变量兜底，永不写日志）；「功能桥接」里 SUMMARY / TITLE / EMBEDDING 可各指定专用渠道（前两者回落主渠道，EMBEDDING 用本地哈希向量兜底）；能力标记手动勾选优先于按名推导。中转渠道：新建配置 → 选供应商 → 填 Base URL 与 key → 获取并勾选模型。

## 审批档位

| 档位 | 读 | 写 | 执行（shell/git） | 说明 |
| --- | --- | --- | --- | --- |
| `read-only` | 自动 | 要批 | 要批 | 只读档 |
| `auto`（默认） | 自动 | 自动 | 要批 | 自动编辑 |
| `full-access` | 自动 | 自动 | 自动 | 脱沙箱直跑，需 `config.toml [agent] allow_unsandboxed = true`；`unsandboxed_approval = true` 可保留执行审批 |

审批卡片显示风险徽章与判定依据，可勾「本会话同类允许」；120s 超时视为拒绝。Plan 档只放行只读工具、写类操作直接驳回并回喂模型（`plan_mode` 请求字段或 eval 任务 `plan-mode-write-guard` 可验证）。

## MCP 三 transport

设置 → MCP → 添加 server，transport 三选一：`stdio`（本地子进程，环境白名单重建，主服务密钥绝不进去）/ `http`（远端 Streamable HTTP：POST JSON-RPC，响应支持 json 与 event-stream，`Mcp-Session-Id` 自动回带）/ `sse`（GET 长连接收响应 + POST 发请求）。请求头按每行一条 `Key: Value` 填，只存本地与内存、不进日志；传输层失败标记离线，下次调用自动重连。本地冒烟：`node evals/mcp-http-server.mjs` 后添加 `http://127.0.0.1:9401/mcp`。

# 插件（契约 v1，二合一）

一个插件 = 一个目录 + 一份 manifest.json，打开：设置 → 插件 → 打开（面板停靠右侧）；纯工具插件只显示工具清单。iframe 用 `sandbox="allow-scripts"` 隔离（不透明源，读不到宿主 DOM/localStorage/cookie，也 fetch 不到 `/api`），唯一通道是 postMessage 白名单桥：入口页发 `agent:hello` 握手，宿主回 `agent:init`（版本/运行根/主题/当前会话），之后 `agent:request` 调 `app.info` / `workspace.info` / `workspace.files` / `git.status` / `memory.search` / `session.info` / `composer.insert` / `notify` / `bus.publish` / `workspace.write` / `storage.read·write·delete` / `usage.session` / `usage.prices` / `usage.prices.save` / `usage.recompute`——**未在 manifest 声明权限的一律拒绝**（各方法权限与语义、manifest 全部字段见 [docs/plugin-contract-v1.md](docs/plugin-contract-v1.md)）。

- **来源与版本**：`source` 三值 built_in / user / model_generated；`apiVersion` 不匹配 → 拒载并提示升级（无 shim）；未知字段一律忽略。
- **draft 隔离区**（模型自产）：只在内存（重启即失）、权限一律清空；「转正」（落盘、来源改 user）或「丢弃」。
- **dock 页签与设置页**：manifest 声明 `ui:[{slot:"dock"}]` 的启用插件在右侧面板多一个页签（停用即消失）；`settings_entry` 让插件卡片多一个「设置」按钮。
- **插件自带技能**：manifest `skills: "skills"` → 技能进系统提示索引；停用的插件不贡献技能。
- **准入机制**：2026-10-08 起静态审核与 pin 验签按用户拍板移除（生态起来再重建，git 历史留有完整实现）——写进 `plugins/` 即加载；仍在的边界是 manifest 契约校验、停用语义与 draft 隔离区。
- 参考实现：`plugins/board/`（项目文件）、`plugins/plugin-maker/`（表单 + session.create 桥）。清单写坏不会静默：标红给原因，「打开」禁用。

# 用量台账与成本红线

每一次模型调用（主对话 / 滚动摘要 / 标题生成 / 媒体旁路 / 降级链重试）都在**网关单点**记一条账，落 `<日志目录>/usage/ledger.jsonl`：token 分 prompt / completion / cache 三路（渠道不回 usage 的记 0 但请求与延迟照记）、请求次数、失败与取消、延迟分位、**记账时算定冻结**的费用——之后改价只影响新请求，历史账一分不动（价格页有「按当前价格重算历史」的显式补救按钮）。子代理账记 `parent_session` 可按父任务聚合。报表：`GET /api/usage?group=provider|model&range=today|7d|30d|all&session=<id>`（`session` 非空 = 只算该会话）。

**「用量价格」插件**（随包内置，`plugins/usage/`）：右侧面板在「产物 / 预览 / 网页」旁多一个**用量**页签，经只读桥拉**当前会话**统计（输入 token 含缓存命中 / 输出 / 请求次数 / 本次费用，页脚累计花费与上限）；插件停用则页签消失，**台账与硬闸照常跑**。价格设置页一行 = **渠道 host + 模型 id** + 三个单价（输入/缓存输入/输出，每百万 token，`*` 通配兜底，精确匹配优先）——同模型 id 不同供应商是两行，中转与官方永远不会搞混；美元/人民币切换只换展示不动账。价格表落 `settings.json` 的 `cost` 节。

**红线一：全局花费上限（硬闸）**——`[cost] spend_cap_usd`（美元，累计达到即拒绝新的模型请求，主对话与辅助调用同一道闸，0 = 不限）；上限看**全量**台账（与面板「当前对话」口径分层），启动扫台账装入、之后增量更新，触发时返回人话错误并记 `cost.cap_blocked` 审计。**预算降级渠道**：`[roles] budget`——单任务超 `[hooks] budget_tokens` 后剩余步骤改走便宜渠道（Notice 明示）。

## 日志与排查

- **进程日志**：`logs/agent.jsonl`（单行 JSON，六类事件），超 `[logging].max_mb`（默认 20MB）自动归档、保留份数走 `keep_archives`；schema 见 [docs/jsonl-schema.md](docs/jsonl-schema.md)。
- **时间线**：侧栏「时间线」面板（`GET /api/trace`）按任务切分事件流、分片聚合、工具配对耗时。
- **会话**：`logs/sessions/<id>.json`，每条会话一个文件；项目记忆流水在 `<workspace>/.agent/memory/events.log`（默认脱敏），与进程日志是两回事。

```powershell
Select-String -Path logs\agent.jsonl -Pattern 'gateway.request'   # 真实 base_url + 完整请求体
Select-String -Path logs\agent.jsonl -Pattern 'model_chunk'       # 流式分片
```

## 接口

全部 HTTP / SSE 端点（对话 / 取消 / 会话 / 定时 / 插件 / MCP / 沙箱 / 网页面板等 50+ 条，含方法与参数）见 [docs/api.md](docs/api.md)。

## CI

`.github/workflows/ci.yml`（windows-latest）三步，提交前本地等价自查：

```powershell
cargo clippy --all-targets -- -D warnings
cargo test --no-fail-fast
cd web; npm ci; npm run build
```

## 无 key 冒烟（可选）

```powershell
node scripts/mock_provider.mjs          # 终端 1：127.0.0.1:9399（模拟 DeepSeek SSE 渠道）
$env:DEEPSEEK_BASE_URL = "http://127.0.0.1:9399"; $env:DEEPSEEK_API_KEY = "sk-test"
cargo run                                # 终端 2：127.0.0.1:5173
```

评测：`cargo run --bin eval`（临时工作区隔离 + 断言引擎 + 报告与 exit code，任务集在 `evals/tasks/`，12 个任务）。

## 已知边界（未做 / 搁置）

- **桌面壳**：已启用 = 内置浏览器宿主（child webview + 控制通道 9225 + CDP 9223），用户日常使用中。留口：`desktop/` 仍是开发布局（服务 exe 路径按 desktop/ 上两级解析），打包分发时改同级目录探测；壳启动后窗口会自行 resize 一次（机制待查）。
- **MCP**：tools + resources + prompts（三 transport）；**OAuth 授权流程已做**（授权码 / client_credentials，DPAPI 逐条加密存 `mcp_tokens.json`）。
- **工具插件（WASM）**：ABI v1，**零 host function** 纯计算沙箱——碰不到文件/网络/UI。
- **插件准入**：审核与验签已移除（2026-10-08 用户拍板），当前靠沙箱隔离 + manifest 契约 + 权限白名单兜底；重建入口见 docs/plugin-contract-v1.md 第八节。
- **用量台账**：价格表靠手填（不抓取、不同步、不多币种）；未命中价格计入「未计价」不计费；花费上限是全累计口径，调到 0 即关闭。
- **钩子**：四挂点（before_tool 仅否决权 / after_tool / session_end / budget_exceeded），超时与失败一律放行。
- **eval**：12 个任务；2026-09-23 已在真实渠道（StepFun `step-5-preview`，隔离临时实例）跑过 **53/53 断言全过**；复跑流程与两个本机坑见 ROADMAP「eval 真模型验证」行。
- **CI**：只在 GitHub Actions 上跑（本机没有 hook 强制）。**远端备份**：GitHub 公开仓库 `pumu21600-boop/pawch-agent`（origin：`dev` 为主线 + 全部标签；2026-10-21 由 `agent` 改名，旧地址 GitHub 自动跳转）。

## 分发：内置 git（用户不用装任何东西）

产品可内置一份 **MinGit**（最小 git；不需要管理员、不需要联网）：

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
- **界面上能看到**：设置 → 插件 页的「宿主能力」区有「Git 运行时」一行（git 运行时状态卡片，
  2026-10-06 插件重整后从 git 插件分区搬来），显示当前用的是系统 Git / 便携版 / 内置运行时、
  版本与路径，「重新检测」按钮手动刷新；没检测到 git 时会明说「用不了」并给出安装方法。
- 已知差异：MinGit 不带 git-lfs / bash / 图形界面——agent 的提交、diff、撤销、检查点都不需要它们；
  用户项目若使用 LFS，需另装完整版 git。

## 打包发布（一键出可独立运行的目录）

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package_release.ps1
# 只组装不重编译：-SkipBuild -SkipFrontend / 只出目录不打 zip：-SkipZip
```

产出 `dist\agent-<版本>-win64\`（+ 同名 zip，约 43MB）：`agent.exe`（服务二进制，start/stop/status 自管理内置）、`config.toml` `providers.json` `prompts\` `plugins\`、`web\dist\`（前端产物）、`tools\git-runtime.zip`（内置 MinGit，未跑 bundle_git.ps1 时缺，缺了就要系统 git）、`start.bat` `stop.bat` `README.md` `LICENSE`。把整个目录拷到任意位置（含没装 Rust/Node 的机器）双击 `start.bat` 即用——exe 同级就是运行根，配置、会话、日志、插件全落在那个目录里，不碰源码树。

### 两种包：完整版 / 精简版

发布产物分两档，区别只有一件事：**带不带内置 MinGit**。

| | 完整版（默认，`agent-<版本>-win64`） | 精简版（`-Slim`，`agent-<版本>-win64-slim`） |
|---|---|---|
| 内置 MinGit | 带（`tools\git-runtime.zip`，zip 大约 +37MB） | 不带，包体更小 |
| 机器没装 git 时 | 检查点/撤销/改动清单/时间线任务快照照常用（懒解压内置包） | 这些功能不可用；界面照实说「没检测到 git」并给出安装方法，不静默变样 |
| 机器装了 git 时 | 与精简版完全一致（永远优先用系统 git） | 同左 |

```powershell
# 完整版（默认）
powershell -ExecutionPolicy Bypass -File scripts\package_release.ps1
# 精简版
powershell -ExecutionPolicy Bypass -File scripts\package_release.ps1 -Slim
```
