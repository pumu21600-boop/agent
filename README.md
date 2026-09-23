# agent —— Windows 原生类 Codex 自主编码 Agent

技术栈：Rust 后端（axum）+ Svelte 前端（Vite）+ SSE 流式。
当前进度：**P21 完成**（成本台账 + 红线收口：每次模型调用在网关单点记账，token 分 prompt/completion/cache 三路 + 请求/失败/延迟分位/费用，价格表带生效时间，按供应商/模型出报表；红线一 = 全局花费上限硬闸，红线二 = 第三方插件来源验签（pin 内容指纹））；此前 P20.4 内置浏览器搬进桌面壳（壳里跑 child webview 真页面，服务经 CDP + 控制通道驱动，壳不在自动回落无头）、P20 浏览器内核（CDP 直连 + 语义定位点击 + DOM 增量 diff）与沙箱统一（SandboxTier trait 包住三档 + 档位探测链 + UI 明示档位）；P19 巨石拆分（server.rs 九域 / App.svelte 拆分 / 内置插件试点）；P18 契约收敛（插件二合一 / 存储一门 / 钩子四挂点 / draft 隔离区）；P17 搜索 + MCP 资源 + 组件总线 v1；
P14 记忆库星云；P13 桌面壳 = P20.4 起的内置浏览器宿主（已启用）。
逐阶段排产与踩坑记录见 [docs/ROADMAP.md](docs/ROADMAP.md)，日志格式见 [docs/jsonl-schema.md](docs/jsonl-schema.md)。

## 功能总览

- **核心循环**：模型带 `tools` 决策 → 本地执行 → 结果回填 → 直到纯文本回答；步数上限、**取消会话化**（停止只停当前会话的这一轮，别的会话不受影响）；同轮只读工具并行执行，同路径写冲突自动跳过并回喂模型。
- **工具**：`read_file` / `read_page` / `search`(rg) / **`web_search`（联网搜索，无需 key：Bing RSS 主引擎 + HTML/DuckDuckGo 回退）** / `web_fetch` / **`browser_*`（内置无头浏览器五件套：open / snapshot / click / type / close，语义定位为主、坐标兜底，快照带稳定 ref 与增量 diff；与右侧「网页」面板共享同一浏览器实例）** / `write_file` / **`edit_file`（局部替换，改大文件不必重写全文）** / `shell` / `git`，外加动态挂载的 MCP（`mcp__<server>__<tool>`，声明了 resources/prompts 的还会挂 `list_resources`/`read_resource`/`list_prompts`/`get_prompt`）与插件工具（`wasm__<plugin>__<tool>`；契约 v1 起 WASM 是插件工具的一种实现，见 `docs/plugin-contract-v1.md`）。
- **审批与 Plan**：四档 `read-only`（只读）`sandbox`（沙箱）`auto`（自动）`full-access`（完全访问）——只读=写/执行逐次询问；沙箱=只在沙箱内执行，工作区与电脑都不改（写文件直接驳回）；自动=可动工作区与整台电脑（越界写按高危先问，shell 仍在沙箱）；完全访问=全免审 + 本机直跑（需 `allow_unsandboxed`）。档位按会话记忆、运行中切换下一步即生效，风险分级引擎逐条判定，「本会话同类允许」记忆授权范围。Plan 档是**接口层能力**（请求级 `plan_mode`：只放行只读工具，写类操作不进审批弹窗、直接驳回并回喂模型调整计划，最终回答可产出结构化计划块）——P14 起**界面不再提供 Plan/Act 手动切换**（改由模型按系统约定在任务里自己列步骤），该档保留给 API 与 eval 使用。
- **执行沙箱**：`SandboxTier` 统一接口包住三档——Job Objects 整树击杀 + 受限 token + 环境白名单（密钥不进沙箱）为默认档；`[sandbox] mode = "appcontainer"` 为实验特性（LowBox + 能力 SID、默认断网，探测链 canary 不过自动回退受限档并在 UI/审计注明）；**档位探测链**（快速探针跑在生效档上，解析结果进程内缓存）+ 设置 → 插件 → 运行状态「沙箱档位」卡明示当前档位（`GET /api/sandbox/info`）。浏览器工具走 Chromium 自带站点隔离，不进 Windows 沙箱。
- **上下文工程**：token 预算裁剪、工具结果头尾压缩、`read_page` 分页读大文件。
- **恢复与撤销**：danger≥1 工具执行前拍影子仓库检查点；撤销只回退本任务改过的文件（绝不整树 reset）；对话流内显示「N 个文件已更改 +x −y ↩ 撤销」。
- **会话**：每会话一个文件落盘，刷新/重启自动恢复；置顶/归档/分叉/重命名/批量管理；侧栏项目树（工作区维度）；HANDOFF 交接文档（手动或上下文 ≥80% 自动）。
- **记忆**：双层存储（全局 `~/.agent` + 项目 `<workspace>/.agent/memory`）、events.log 事件流水（默认脱敏）、AGENTS.md 双层注入、滚动摘要；系统约定（`prompts/system.md`）**每轮任务实时读盘**，面板保存即生效。
- **记忆库与星云**：会话压成检索正文，EMBEDDING 角色席位走真语义向量（未配置时用本地双字哈希向量），词法+向量混合排序；前端体积渲染星云（离线烘焙 cubemap）按上下文长度驱动光点大小，支持检索高亮与相似会话。
- **Skills**：`<workspace>/skills/` 与 `~/.agent/skills/` 下的 `SKILL.md`（frontmatter：name/description）；system 提示只注入「名字+一句话」，模型按需用 `read_skill` 拉全文。
- **插件（契约 v1，二合一）**：一个目录 + 一份 manifest.json = 一个插件，面板与工具按清单声明、不分类型；MCP 保持独立层（stdio + Streamable HTTP 双 transport）；工具经 WASM 引擎（纯计算沙箱、零 host function）；面板跑在沙箱 iframe 里经 postMessage 白名单桥取数据；模型自产插件走 **draft 隔离区**（内存、权限清空、一键转正）；**第三方验签**：来源为 `user` 的插件（自装 / 工作区 / plugins 目录）要点「信任」固定内容指纹后才加载，文件被改动即停用；**钩子**四挂点（config.toml `[hooks]`，before_tool 仅否决权、超时放行）。
- **用量台账与成本红线（P21）**：每次模型调用在网关单点记账（`logs/usage/ledger.jsonl`）——token 分 prompt / completion / cache 三路、请求次数、失败与取消、延迟分位、**记账时算定冻结**的费用；**「用量」插件**（`ui-ext/usage/`，随包内置试点）在右侧面板多一个页签，显示**当前对话**的 token / 请求次数 / 费用，插件自带价格设置页（美元/人民币切换，单价 = 每百万 token，键是「渠道 host + 模型 id」——同模型名不同供应商互不混淆）；**红线一** = 全局花费上限硬闸（`[cost] spend_cap_usd`，累计达到即拒绝新模型请求，看全量台账）；**红线二** = 第三方插件来源验签；`[roles] budget` 降级渠道（单任务超 token 预算后剩余步骤改走便宜渠道）。
- **模型配置**：渠道/模型/角色三层解耦；API Key 按配置 DPAPI 加密保存；模型能力标记（工具/视觉/视频/音频）；思考档位按厂商规范适配（reasoning_content / thinking_param / effort_param 等）。
- **观测**：JSONL 六类事件（含 `kind` 子类，schema 定死）、日志按大小轮转（归档保多份，`[logging].keep_archives`）、trace 时间线面板、eval 黑盒评测（`cargo run --bin eval`，12 个任务，断言含负向事件 `no_sse_event`）。
- **界面**：Svelte 经典聊天布局 + 深/浅双主题 + 顶栏三面板（终端 / 产物与预览 / **网页**——ZCode 式内置浏览器：多标签页、地址栏、后退前进刷新，桌面壳里是**真实网页**（child webview，可直接点击/滚动/输入，还能开真 DevTools），普通浏览器打开时回落截图帧流；agent 的 browser_* 工具打开的页面同场可见）+ 右侧面板页签含**插件贡献的 dock 页签**（如「用量」：当前对话 token/请求/费用）；设置面板七分区（外观 / 模型配置 / 角色分工 / 记忆 / 技能 / MCP / 插件；插件页签底部含「运行状态」——沙箱档位卡、Git 运行时卡与沙箱自检）。
- **工程**：clippy 零告警、CI（clippy + 全量测试 + 前端构建）、单测主 bin 193 项 + eval 5 项（含沙箱 / 浏览器内核真机冒烟 / MCP / WASM / 服务自管理）、发布打包脚本（一键出可独立运行的 win64 目录与 zip）；写盘全部走**原子写**（临时文件 + rename），写入中途失败不会留下半截文件。

## 目录结构

```
agent/
├── src/                  # Rust 后端
│   ├── main.rs           # 启动（app_root 定位资源 → 配置/日志/工作区/记忆层装配）
│   ├── server.rs         # axum：静态页 + 全部 HTTP/SSE 接口 + 审批门 + 网关装配
│   ├── service.rs        # 服务自管理（start/stop/status/--install/双击智能启动）
│   ├── skills.rs         # SKILL.md 目录约定（frontmatter 解析、索引注入、读写）
│   ├── core/             # 核心循环、取消注册表（会话级）、审批规则引擎（rules.rs）
│   ├── gateway/          # 模型网关：providers 数据加载、端点/思考形态探测、降级链
│   ├── config/           # config.toml + settings.json（DPAPI 存 key）+ app_root 运行形态
│   ├── observability/    # JSONL 日志（6 类事件 + 轮转）+ trace 读取
│   ├── browser/          # P20 浏览器内核：cdp.rs（CDP WebSocket 客户端 + 浏览器发现拉起）+ 会话管理与 DOM 快照/diff
│   ├── sandbox/          # 执行沙箱：backend.rs（SandboxTier trait 三档）+ probes.rs（探针与档位探测链）
│   ├── context/          # 上下文工程：token 估算/压缩/裁剪
│   ├── recovery/         # 影子仓库检查点 + HANDOFF 交接
│   ├── session/          # 会话落盘与恢复
│   ├── tools/            # 工具系统（内置 + 并行调度 + git 发现链）
│   ├── memory/           # 记忆 v1（events.log/AGENTS.md/摘要）+ library.rs 记忆库索引
│   ├── plugin/           # contract.rs（契约 v1）+ mcp.rs + wasm.rs（工具引擎）+ ui.rs（面板）+ draft.rs（隔离区）+ trust.rs（第三方验签）
│   ├── usage/            # P21 用量台账：ledger.jsonl 记账（费用冻结）+ prices.rs 价格匹配 + 按会话/分组报表 + 花费硬闸 + 重算
│   ├── usage/            # P21 用量台账：ledger.jsonl 记账 + prices.rs 价格表 + 聚合报表 + 花费硬闸
│   ├── storage.rs        # 存储一门：内核保留区唯一落盘入口 + 插件命名空间数据
│   └── hooks.rs          # 钩子四挂点（before_tool / after_tool / session_end / budget_exceeded）
├── web/                  # Svelte + Vite 聊天界面（含 galaxy/ 体积星云渲染）
├── ui-ext/               # 插件面板（每个目录 = 一块面板，如 board/ 项目速览、trace/ 时间线、usage/ 用量）
├── desktop/              # P13 Tauri 桌面壳（脚手架，搁置）
├── evals/                # eval 评测：tasks/ 12 个任务 + mcp-http-server.mjs（http 冒烟工具）
├── skills/               # 项目级技能（每个技能一个目录 + SKILL.md）
├── plugins/              # 插件工具目录（*.wat / 目录+manifest；仓库内示例已清空，放自己的插件即可）
├── prompts/system.md     # 系统约定（每轮任务实时读盘）
├── providers.json        # 供应商预设（数据驱动）
├── docs/                 # ROADMAP / jsonl-schema / workorders/ / 交接文档
├── scripts/              # package_release.ps1（发布打包）/ bundle_git.ps1（内置 git）/ mock_provider.mjs
├── .github/workflows/    # CI：clippy -D warnings + 全量测试 + 前端构建
├── config.toml           # 服务/网关/审批/沙箱/上下文/恢复/记忆/钩子配置
└── LICENSE               # MIT
```

## 运行形态（开发 / 打包两态）

资源定位只有一条规则（`config::app_root()`）：

- **开发形态**：`cargo run` 或直接跑 `target\release\agent.exe` —— exe 同级没有 `config.toml` / `web`，
  一律回落**编译期源码目录**（改动前的老行为，开发流程零变化）。
- **打包形态**：exe 同级有 `config.toml` 或 `web/` —— 认定随包运行，`config.toml` /
  `settings.json` / `providers.json` / `prompts/` / `web/dist/` / `plugins/` / `ui-ext/` / `logs/`
  全落在 **exe 同级目录**。所以整个目录可以拷到任何地方（含没装 Rust/Node 的机器）直接双击 `start.bat`。

侧栏底部的版本行（鼠标悬停显示「运行根」）就是用来一眼确认当前读的是哪一份资源。

**来源校验**：服务只接受本机来源的请求（`Host` 必须是 `localhost` / `127.x` / `[::1]`；
带 `Origin` 时必须同源同端口）——绑定 127.0.0.1 挡不住 DNS rebinding（恶意页面把自己的域名
解析到本机就变成「同源」），这层校验补的就是它。不带 `Origin` 的命令行客户端（curl、eval
驱动器、服务自管理）照常可用；前端热更（vite 5174）由代理把 Origin 改写成主服务地址。

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
- **界面上能看到**：设置 → 插件 分区顶部有「Git 运行时」卡片（设置 → 运行时），显示当前用的是系统 Git 还是内置 Git、
  版本与路径，以及内置包是否随包携带（「重新检测」按钮手动刷新）。
- 已知差异：MinGit 不带 git-lfs / bash / 图形界面——agent 的提交、diff、撤销、检查点都不需要它们；
  用户项目若使用 LFS，需另装完整版 git。

## 打包发布（一键出可独立运行的目录）

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package_release.ps1
# 只组装不重编译：-SkipBuild -SkipFrontend / 只出目录不打 zip：-SkipZip
```

产出 `dist\agent-<版本>-win64\`（+ 同名 zip，约 43MB）：

```
agent.exe            服务二进制（start/stop/status 自管理内置）
config.toml  providers.json  prompts\  plugins\  ui-ext\
web\dist\            前端产物（由 exe 托管）
tools\git-runtime.zip 内置 MinGit（未跑 bundle_git.ps1 时缺，缺了就要系统 git）
start.bat  stop.bat  README.md  LICENSE
```

**验证过的用法**：把整个目录拷到任意位置（含没装 Rust/Node 的机器）双击 `start.bat` 即用——
exe 同级就是运行根，配置、会话、日志、插件全落在那个目录里，不碰源码树。

## CI

`.github/workflows/ci.yml`（windows-latest）三步，提交前本地等价自查：

```powershell
cargo clippy --all-targets -- -D warnings
cargo test --no-fail-fast
cd web; npm ci; npm run build
```

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
- Plan 档是接口层能力（请求级 `plan_mode`，见上）：只放行只读工具，写类操作不进审批弹窗、直接驳回并回喂模型调整计划。**界面已无手动切换入口**（P14 起模型自己在任务里列步骤），要复现/验证用 `/api/chat` 的 `plan_mode` 字段或 `cargo run --bin eval -- plan-mode-write-guard`。

## 日志与排查

- **进程日志**：`logs/agent.jsonl`（单行 JSON，六类事件）。超过 `[logging].max_mb`（默认 20MB）自动归档成
  `agent.jsonl.1`（最新）并开新文件，旧归档依次倒位为 `.2`…`.N`，保留份数走 `[logging].keep_archives`
  （默认 3，0=归档即删）；`max_mb = 0` 关闭轮转。
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
| POST | `/api/chat` | 对话主入口，SSE 流式（content / reasoning / tool_call / tool_result / approval_request / plan_proposal / notice / **reset** / error / done；`reset` = 降级链换渠道重试，前端按给的长度丢掉本步已推的半截内容） |
| POST | `/cancel?session=<id>` | 停止指定会话当前这一轮（不带 `session` = 停全部在跑任务） |
| GET | `/api/cancel?session=<id>` | 查询取消状态：带 `session` 看该会话（`cancelled`/`running`），不带看全部（`cancelled`/`active`） |
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
| GET | `/api/git/info` | git 运行时状态（可用性/版本/来源：系统·便携·内置/内置包情况；设置→运行时 卡片用） |
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
| GET / POST | `/api/mcp` | MCP server（stdio / http 双 transport）+ 工具插件状态 / 配置管理与生命周期动作 |
| GET / POST | `/api/skills` | 技能列表 / 保存·删除·读取 |
| GET | `/api/ext` | 插件统一清单（面板 + 工具 + draft，一份列表）|
| POST | `/api/ext/toggle` | 启用/停用插件（停用 = 拒新调用 + 等完）|
| POST | `/api/ext/draft` / DELETE | 模型自产插件提交 draft 隔离区 / 丢弃 |
| POST | `/api/ext/promote` | draft 一键转正（落盘持久化、来源改 user）|
| GET / PUT / DELETE | `/api/plugin/data` | 插件命名空间数据读写删（存储一门；键名白名单 ≤64 字符，值 ≤64KB）|
| GET | `/ui-ext/{name}[/{path}]` | 插件面板静态资源（入口页由 manifest 指定；路径穿越一律 404） |
| GET | `/api/version` | 版本与运行形态（`root` = 当前生效的应用根） |
| GET | `/api/sandbox/selftest` | 沙箱探针套件（真实拉起受限进程） |
| GET | `/api/sandbox/info` | 当前沙箱档位（配置档/生效档/回退原因/探测链明细；`?refresh=1` 强制重探） |
| GET/POST/DELETE | `/api/browser/*` | 网页面板后端：state（标签清单）/ new / navigate / screenshot（PNG）/ click / type / key / scroll / back / forward / reload / viewport（视口跟随面板）/ overlay（「⋯」菜单开合，壳模式开时临时藏 child webview）/ tab（关标签）——与 agent 浏览器工具共享实例 |

## MCP 双 transport

设置 → MCP → 添加 server，transport 二选一：

| transport | 配置 | 说明 |
| --- | --- | --- |
| `stdio` | 命令 + 参数（+ 可选 env） | 本地子进程，换行分隔 JSON-RPC；子进程环境是白名单重建的，主服务的密钥绝不进去 |
| `http` | url（+ 可选自定义请求头） | 远端 **Streamable HTTP**：POST JSON-RPC，响应支持 `application/json` 与 `text/event-stream`（读 `data:` 帧、按 id 取匹配帧，心跳与串扰帧跳过）；`initialize` 响应里的 `Mcp-Session-Id` 会自动回带 |

- 请求头（如 `Authorization: Bearer …`）按每行一条 `Key: Value` 填，只存在本地 `settings.json` 与内存，**不进日志**。
- 传输层失败（连不上 / 非 2xx / 解析不出）标记离线，下次调用自动重连重握手；server 自己报的协议错误照实显示。
- 本地冒烟：`node evals/mcp-http-server.mjs`（加 `--sse` 走 SSE 响应），然后按上表添加 `http://127.0.0.1:9401/mcp`。

# 插件（契约 v1，二合一）

一个插件 = 一个目录 + 一份 manifest.json，清单里**只声明「有什么」**（面板 / 工具 / 权限），不分类型。对外只留四个词：**Skill / MCP / 插件 / 沙箱**。契约全文见 [docs/plugin-contract-v1.md](docs/plugin-contract-v1.md)。

```
<应用根>/ui-ext/<名字>/           # 面板（随包/内置）
<当前工作区>/ui-ext/<名字>/       # 面板（项目自带；同名顶掉内置）
<plugins 目录>/x.wat             # 工具（旧形态：裸文件）
<plugins 目录>/x/manifest.json   # 工具（目录形态：manifest + 恰好一个 .wasm/.wat）
    manifest.json                # {"apiVersion":1,"source":"user","title":"…","entry":"index.html","ui":[],"permissions":[]}
```

- 打开：设置 → 插件 → 打开（面板停靠在右侧）；纯工具插件显示工具清单，无「打开」按钮。
- **隔离**：iframe 用 `sandbox="allow-scripts"` → 不透明源，读不到宿主 DOM、localStorage、cookie，
  也 fetch 不到 `/api`（跨源无 CORS）。唯一通道是 postMessage。
- **数据桥（白名单）**：入口页先 `postMessage({type:'agent:hello'})` 握手，宿主回
  `{type:'agent:init', payload:{version, root, theme, sessionId, title, ext}}`；之后用
  `{type:'agent:request', id, method, params}` 调用下列方法，宿主回 `{type:'agent:response', id, ok, result}`：

  | method | 权限 | 说明 |
  | --- | --- | --- |
  | `app.info` | — | 版本 / 运行根 / 主题 / 当前会话 |
  | `workspace.info` | — | 当前工作区与最近项目 |
  | `workspace.files` | — | 工作区文件相对路径列表 |
  | `git.status` | — | 分支 / 头部 / 脏文件 / 领先落后 |
  | `memory.search` | — | `{query, top_k}` 记忆库检索 |
  | `session.info` | — | 当前会话 id 与标题 |
  | `composer.insert` | composer.insert | 往输入框插文本 |
  | `notify` | notify | 注入一条系统消息 |
  | `bus.publish` | bus.publish | 插件间通信（宿主转发 `bus.message`）|
  | `workspace.write` | workspace.write | 写工作区文件（宿主弹确认；工作区内 / 512KB / 原子写）|
  | `storage.read / write / delete` | storage | **插件命名空间数据**（`plugin_data/<插件名>/`，键名白名单 ≤64 字符、值 ≤64KB；会话/设置/记忆等内核保留区不在任何插件写路径上）|
  | `usage.session` | — | **当前会话**的用量统计（token 三路/请求/费用；会话 id 由宿主注入，插件拿不到别的会话）|
  | `usage.prices` | — | 读价格表、可选渠道、币种与汇率、累计/上限 |
  | `usage.prices.save` / `usage.recompute` | usage.prices | 写价格表与币种 / 用当前价格重算全部历史账 |

- **来源与版本**：manifest `source` 三值 built_in / user / model_generated；`apiVersion` 不匹配 →
  拒载并提示升级（不做 shim；v0.x 期间不保证插件兼容）。未知字段一律忽略（只增不删 = 向后兼容）。
- **draft 隔离区**（模型自产，`source=model_generated`）：只在内存（重启即失）、权限一律清空；
  设置 → 插件 里「转正」（落盘持久化、来源改 user）或「丢弃」。
- **右侧面板 dock 插槽**：manifest 声明 `ui:[{slot:"dock",title:"用量"}]` 的启用插件，在右侧「产物/预览/网页」旁多一个页签（内置 `usage` 插件就是这么落的）；停用即页签消失。
- **插件自带设置页**：manifest `settings_entry`（如 `prices.html`）→ 设置 → 插件卡片上多一个「设置」按钮。
- **启用/停用**：停用 = 拒新调用 + 在跑调用等完（工具从总线消失、面板 404、总线不再转发）。
- **第三方验签（P21 红线二）**：manifest `source` 为 `user` 的插件（自装 ui-ext、当前工作区 ui-ext、`plugins/` 目录与裸文件；未声明 source 的自装位也按 user）**必须先被信任才加载**——设置 → 插件 →「信任」会把插件的**内容指纹**（canonical sha256）记进 `settings.json`，之后每次加载都重新算指纹比对；对不上（文件被改/被换）立即停用并提示重新信任。随包内置（`built_in`）与模型自产草稿（`model_generated`，权限已清空 + 内存态）不在此列；draft「转正」会**自动 pin**（转正即用户的信任决定）。
- 参考实现：`ui-ext/board/`（项目速览）。清单写坏不会静默：标红给原因，「打开」禁用。

# 用量台账与成本红线（P21）

每一次模型调用（主对话 / 滚动摘要 / 标题生成 / 媒体旁路 / 降级链重试）都在**网关单点**记一条账，落 `<日志目录>/usage/ledger.jsonl`：

```json
{"ts":"2026-09-23T12:00:00.000Z","session_id":"…","parent_session":null,"purpose":"main",
 "provider":"api.deepseek.com","provider_id":"deepseek","model":"deepseek-chat",
 "prompt_tokens":1234,"completion_tokens":567,"cached_tokens":1024,
 "status":"ok","latency_ms":1830,"cost_usd":0.0016}
```

- **分计量**：prompt / completion / cache 三路分开；渠道不回 usage 的调用 token 记 0，但请求与延迟照记。
- **费用冻结构**：`cost_usd` 在记账时按当时价格算定写进记录——之后改价只影响新请求，历史账一分不动（价格页有「按当前价格重算历史」的显式补救按钮）。
- **`parent_session`**：P22 子代理的父任务归属字段（v1 恒为 null，子代理落地后填值即可按父任务聚合）。
- **报表**：`GET /api/usage?group=provider|model&range=today|7d|30d|all&session=<id>`；`session` 非空 = 只算该会话（「用量」插件的「当前对话」口径），缺省 = 全量。

## 「用量」插件（随包内置试点，ui-ext/usage/）

右侧面板在「产物 / 预览 / 网页」旁多一个**用量**页签（manifest 声明 `ui:[{slot:"dock"}]`；插件停用则页签与价格设置一起消失，**台账与硬闸照常跑**——内核不受插件停用影响）。插件经只读桥 `usage.session` 拉当前会话统计，显示输入 token（含缓存命中）/ 输出 token / 请求次数 / 本次对话费用，页脚一行累计花费与上限。

**价格设置**（插件自带页 `prices.html`，设置 → 插件 → 用量 →「设置」打开）：

- 一行 = **渠道 host + 模型 id** + 三个单价（输入 / 缓存输入 / 输出），单位**每百万 token**；host 从已保存的渠道下拉带出，`*` 是通配兜底（精确匹配优先于通配）。
- **同模型 id 不同供应商是两行**——键含 host，中转与官方同名的模型永远不会搞混。
- **美元 / 人民币切换**：内部一律以美元记账（台账与上限的口径），人民币值按 `1 USD = N CNY` 汇率折算显示与录入；切币种只换展示，不动任何账。
- 价格表落 `settings.json` 的 `cost` 节（`prices` / `currency` / `usd_cny_rate`），保存后**新**请求按新价计价；要改历史账点「按当前价格重算历史」。

**红线一：全局花费上限（硬闸）**——config.toml：

```toml
[cost]
spend_cap_usd = 0.0   # 美元；累计花费达到即拒绝新的模型请求（主对话与辅助调用同一道闸），0 = 不限
```

上限看**全量**台账（与面板的「当前对话」口径分层）；启动时扫台账装入、之后随记账增量更新；触发时请求直接返回人话错误并记 `cost.cap_blocked` 审计，调高上限即恢复。

**预算降级渠道**：`[roles] budget = "<便宜渠道配置名>"`——单任务累计 token 超 `[hooks] budget_tokens` 后，该任务剩余步骤改走这个渠道（Notice 明示），防止一个任务把额度吃满。

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

- **桌面壳（P13/P20.4）**：已启用 = 内置浏览器宿主（child webview + 控制通道 9225 + CDP 9223），用户日常使用中。留口：`desktop/` 仍是开发布局（服务 exe 路径按 desktop/ 上两级解析），打包分发时改同级目录探测；壳启动后窗口会自行 resize 一次（保存的窗口 bounds 恢复时序，机制待查）。
- **MCP**：tools + `resources` + `prompts`（stdio + Streamable HTTP 双 transport 都有）。
  声明了资源的 server 会给模型挂 `list_resources` / `read_resource`；声明了提示词的挂
  `list_prompts` / `get_prompt`（服务自己已有同名工具时不覆盖）；设置→运行时 的 MCP 卡片会列出
  资源与提示词清单。**OAuth 授权流程未做**（远端服务需要 OAuth 时，可先用带 `Authorization`
  头的配置接）。
- **工具插件（WASM 引擎）**：ABI v1（alloc/describe/run，action 与 payload 分缓冲），仍是**零 host function** 的
  纯计算沙箱——工具碰不到文件/网络/UI，只能做纯函数。draft 工具字节内存编译，同尺隔离。
- **插件契约**：v1 已冻结（二合一 + apiVersion + storage 权限 + draft）——详见插件章节与
  `docs/plugin-contract-v1.md`；「扩展」一词退役，端点与设置字段名保留为兼容。
- **第三方验签**：v1 = 内容指纹 pin（不是密码学签名体系：无 CA/私钥分发，manifest 里自称的
  签名字串不采信）。信任决定是一次性的（点「信任」），指纹变了要重新点。
- **用量台账**：价格表靠手填（不抓取、不同步、不多币种，一律美元 / 1M tokens）；未命中价格的
  请求计入「未计价」不计费；花费上限是**全累计**口径（不是按日/按月），调到 0 即关闭。
- **钩子**：v1 四挂点（before_tool 仅否决权 / after_tool / session_end / budget_exceeded），
  超时与失败一律放行，JSONL `hook.*` 审计；承载形式 = config.toml `[hooks]` 里的本地命令。
- **eval 任务集**：12 个任务（hello-file / fix-json / read-only-plan / edit-existing-file / delete-file /
  multi-file-scaffold / fix-code-bug / search-and-report / plan-mode-write-guard / run-tests-and-report /
  plan-approve-execute / csv-clean）。2026-09-23 已在真实渠道（StepFun `step-5-preview`，隔离临时实例 + 独立端口）跑过一遍：**53/53 断言全过、0 驱动失败**，覆盖 025 之后全部影响模型行为的改动（web_search 入表 / 审批四档 / browser 统一工具入表）。复跑流程与两个本机坑（`NO_PROXY`、命令行 `Bearer <key>` 脱敏）见 ROADMAP「eval 真模型验证」行。
- **CI**：只在 GitHub Actions 上跑（本机没有 hook 强制）。
- **远端备份**：已推送到 GitHub 私有仓库 `https://github.com/pumu21600-boop/agent`（origin：`dev` 为主线 + 8 个历史阶段分支 + 全部标签）。
