# agent —— Windows 原生类 Codex 自主编码 Agent

技术栈：Rust 后端（axum）+ Svelte 前端（Vite）+ SSE 流式。
当前进度：**P2 完成**（核心循环 v1 + 工具调用闭环），排产见 [docs/ROADMAP.md](docs/ROADMAP.md)。

## P2 核心循环（模型驱动 agentic loop）

- 模型带 `tools` 决策：返回 `tool_calls` → 本地执行 → 结果回填 → 继续，直到纯文本回答
- 内置工具 `read_file`（只读、限工作目录内、路径穿越拒绝、超长截断）
- 硬护栏：`config.toml [agent] max_steps`（默认 8）步数上限；每步与每个流式分片检查取消标志，「停止」立即中断
- system prompt 外置于 `prompts/system.md`（数据驱动）
- SSE 事件：`content` / `reasoning` / `tool_call` / `tool_result` / `step_limit`；工具卡片在聊天界面内渲染
- 多轮对话：前端携带历史 messages（user/assistant），服务端无状态；刷新即清空（P6 会话落盘前如此）

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
│   └── tools/ sandbox/ plugin/ memory/ session/ recovery/ context/  # 占位，后续单实现
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
