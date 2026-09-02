# agent —— Windows 原生类 Codex 自主编码 Agent

技术栈：Rust 后端（axum）+ Svelte 前端（Vite）+ SSE 流式。
当前进度：**P6 完成**（恢复与续命），排产见 [docs/ROADMAP.md](docs/ROADMAP.md)。

## P6 恢复与续命

- **影子仓库检查点**：danger≥1 工具（write_file/shell/git）审批通过后、执行前，把真仓库已跟踪文件
  提交进 `<workspace>/.agent/shadow`（独立 git dir，只跟踪 `git ls-files` + 排除目录 + 10MB 上限，
  pathspec 走文件防命令行超长）；失败只记日志绝不阻断工具。
- **回滚**：GET /api/checkpoints 列表、POST /api/checkpoints/restore 恢复工作树到检查点
  （`reset --hard`，含删除其后新建的已跟踪文件）；shell 副作用不可回滚，绝不碰真仓库 HEAD。
- **会话落盘**：核心循环任务开始/每步工具回填完/终止时发 `Snapshot` 事件 → `logs/session.json`
  原子写；前端启动拉 GET /api/session 重建对话（含工具卡片）；「新会话」= DELETE /api/session。
- **HANDOFF 交接**：手动（侧栏按钮 / POST /api/handoff）或自动（上下文占用 ≥
  `[recovery].handoff_token_ratio` 默认 80% 时每任务一次）生成 `<workspace>/HANDOFF.md`，
  头部强制项目名/git remote/commit/时间戳，正文含最近请求、最后回答摘要、工具计数。
- 配置：`config.toml [recovery]`（checkpoint_enabled / handoff_token_ratio）

## P5 上下文工程 v1

- **token 预算**：启发式估算（CJK≈1/字、ASCII≈1/4 字符），全 messages 超 `[context].budget_tokens`（默认 96k）才裁
- **工具结果头尾压缩**：超限保留头 60% + 尾 40%，中间标省略（JSONL `tool_result.truncated`）
- **历史裁剪**：先归档旧工具结果（保留最近 `[context].recent_tool_results_kept`=6 个），仍超按「轮」折叠——tool_calls/tool 配对永不破坏
- **read_page 工具**：大文件分页读（默认 200 行/页），模型主动翻页替代一次读全

## P4 工具第一批 + 审批三档

- 工具：read_file / search（rg，findstr 兜底）/ web_fetch（去标签出正文）/ write_file / shell / git
- shell 与 git **跑在 P3 沙箱里**：超时整树击杀、内存/进程数上限、密钥不进环境；工作目录=项目根
- 审批四档（侧栏切换，请求级可覆盖，默认 auto-edit）：
  - **建议**：读自动，写/执行要批
  - **自动编辑**：读/写自动，执行要批
  - **全自动**：全部自动，沙箱内跑
  - **本机直跑**：脱沙箱（完整用户 token + 继承父环境），需 `config.toml [agent] allow_unsandboxed = true`
    总开关（默认关，未开自动回退全自动并提示）；`unsandboxed_approval = true` 可让它保留执行审批；
    无论哪档都保留超时整树击杀与防孤儿
- 审批交互：SSE 推 `approval_request` → 卡片内「允许/拒绝」→ POST /api/approvals；120s 超时视为拒绝，停止立即中断
- 工具结果按 [agent].tool_result_max_chars 截断回填

## P3 执行沙箱（真实隔离，壳先于 shell）

- **Job Objects**：`kill-on-close` 整树必灭（孤儿零残留）、内存/进程数/CPU 上限、UI 限制（剪贴板/全局原子/桌面）
- **受限 token**：`CreateRestrictedToken` 剥光特权 + 受限 SID 白名单（用户/Everyone/Users/登录会话），普通用户权限运行、无需管理员
- **环境白名单**：子进程环境重建注入，密钥（API key 等）永不进沙箱
- 配置：`config.toml [sandbox]`（timeout_ms / memory_mb / active_processes）
- 探针验收：`cargo test` 8 项 + `GET /api/sandbox/selftest`（输出/环境/特权/工作目录/超时/树杀/内存）
- P4 的 shell / 文件写工具将统一走这套壳

## P2 核心循环（模型驱动 agentic loop）

- 模型带 `tools` 决策：返回 `tool_calls` → 本地执行 → 结果回填 → 继续，直到纯文本回答
- 内置工具 `read_file`（只读、限工作目录内、路径穿越拒绝、超长截断）
- 硬护栏：`config.toml [agent] max_steps`（默认 8）步数上限；每步与每个流式分片检查取消标志，「停止」立即中断
- system prompt 外置于 `prompts/system.md`（数据驱动）
- SSE 事件：`content` / `reasoning` / `tool_call` / `tool_result` / `step_limit`；工具卡片在聊天界面内渲染
- 多轮对话：前端携带历史 messages（user/assistant）；P6 起服务端同时落盘 `logs/session.json`，刷新/重启可恢复

## 目录结构

```
agent/
├── src/                  # Rust 后端（按总架构 §17 模块化）
│   ├── main.rs           # 启动
│   ├── server.rs         # axum：静态页 + 流式接口 + 取消
│   ├── core/             # 核心循环（P2）、取消标志
│   ├── gateway/          # 模型网关 + providers 数据加载
│   ├── config/           # config.toml + settings 持久化（DPAPI 存 key）
│   ├── observability/    # JSONL 日志（6 类事件）
│   ├── sandbox/          # 执行沙箱（P3：Job Objects + 受限 token + 探针）
│   ├── context/          # 上下文工程（P5：token 估算/压缩/裁剪）
│   ├── recovery/         # 恢复与续命（P6：影子仓库检查点 + HANDOFF）
│   ├── session/          # 会话管理（P6：落盘 + 恢复）
│   └── tools/ plugin/ memory/  # 工具系统（P4）/ MCP+插件（占位）/ 记忆（P7 占位）
├── web/                  # Svelte + Vite 聊天界面（侧栏 + 对话 + 设置弹窗）
├── providers.json        # 供应商预设（数据驱动）
├── docs/                 # jsonl-schema.md / ROADMAP.md / workorders/
├── config.toml           # 服务/日志配置
└── scripts/mock_provider.mjs  # 无 key 冒烟用的本地模拟渠道（测试工具，非 provider）
```

## 快速启动

**日常使用（脱离终端，推荐）**：双击 `start.bat` 后台启动（隐藏窗口，关终端不影响），双击 `stop.bat` 停止。

**开发模式**：

```powershell
cd web; npm install; npm run build   # 一次性构建前端
cd ..; cargo build --release         # 构建 release 二进制（start.bat 依赖）
cargo run                            # 前台调试运行（关终端即停）
```

打开 http://127.0.0.1:8321 。终态方案是 P13 的 Tauri 桌面封装，之后不再依赖任何终端。

## 模型设置（侧栏「设置」弹窗，改动自动保存即时生效）

- **配置 chips**：每个配置可起名（输入框「配置名称」，改动即存），chip 只显示名称＋key 状态圆点（绿=已存 key），点一下即切换；「＋ 新建配置」自动编号命名（可再改）；「✕ 删除当前」至少保留一套。
- 字段改完**自动保存并立刻生效**（无"保存/应用"按钮）：供应商下拉、Base URL 输入（防抖）、API Key（失焦/回车即存）、模型下拉、「获取模型」弹窗勾选即加入/取消即移除。
- **API Key 按 Base URL 独立加密保存**（DPAPI）：同一供应商换地址=独立一格 key，互不串；弹窗实时提示"该地址已保存(…尾号)"或"此地址还没有 key"。
- 用中转渠道：新建一套配置，选对应供应商、Base URL 填中转地址、填入该渠道 key、获取并勾选模型即可；思考模式（reasoning_content）自动解析显示。
- 设置持久化到 `settings.json`；key 密文不出现明文，重新打开网页/重启服务都还在。
- Key 也支持环境变量 `DEEPSEEK_API_KEY` 兜底（未保存 key 时使用）。key 永不写日志。

## 查日志

JSONL 在 `logs/agent.jsonl`（追加写、单行一条、落盘即完整）：

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
| GET | `/` | Svelte 冒烟页（未构建时内置页） |
| POST | `/api/chat` | 单轮对话，SSE 流式返回 `{"content"}` / `{"done":true}` / `{"cancelled":true}` / `{"error"}` |
| POST | `/cancel` | 置位全局取消标志 |
| GET | `/api/cancel` | 查询取消标志（`{"cancelled":bool}`） |
| GET | `/api/health` | 健康检查 |
| GET | `/api/session` | 恢复落盘会话（`{id, updated_at, messages}`） |
| DELETE | `/api/session` | 新会话（清盘） |
| POST | `/api/handoff` | 手动生成交接文档 HANDOFF.md |
| GET | `/api/checkpoints` | 影子仓库检查点列表（新→旧，≤20） |
| POST | `/api/checkpoints/restore` | 工作树回滚到指定检查点（body: `{id}`） |

## 无 key 冒烟（可选）

本地起一个模拟 DeepSeek SSE 渠道，验证流式 + 取消 + 日志全链路：

```powershell
node scripts/mock_provider.mjs          # 终端 1：127.0.0.1:9399
$env:DEEPSEEK_BASE_URL = "http://127.0.0.1:9399"
$env:DEEPSEEK_API_KEY = "sk-test"
cargo run                                # 终端 2：127.0.0.1:8321
```

## 工具链说明

- 本机默认 Rust 工具链是 `stable-x86_64-pc-windows-gnu`，其 dlltool 在本机无法工作（CreateProcess 失败），无法编译 `windows-sys` 等依赖。
- 项目已通过 `rust-toolchain.toml` 固定使用 `stable-x86_64-pc-windows-msvc`（本机已装 VS 2022 Community），进入本目录 `cargo` 会自动切换，无需手动操作。
