# agent —— 施工单 001：日志地基 + 网关冒烟 + 取消按钮

Windows 原生、类 Codex 的自主编码 Agent 的第一期工程。
本单范围：单会话、无历史、无记忆、无工具（取消按钮除外）。

## 目录结构

```
agent/
├── src/                  # Rust 后端
│   ├── main.rs           # 启动：配置 → 日志 → 网关 → HTTP 服务
│   ├── config.rs         # config.toml 加载 + 环境变量覆盖
│   ├── logging.rs        # JSONL logger（6 类事件，全工程统一走它）
│   ├── gateway.rs        # DeepSeek 官方 API 网关（唯一 provider，流式）
│   ├── server.rs         # axum：静态页 + /api/chat 流式 + /cancel
│   └── cancel.rs         # 全局取消标志（AtomicBool）
├── frontend/             # Svelte + Vite 冒烟页（输入框/发送/停止/流式显示）
├── docs/jsonl-schema.md  # JSONL 日志 schema（定死）
├── config.toml           # 渠道/端口/日志配置
└── scripts/mock_provider.mjs  # 无 key 冒烟用的本地模拟渠道（测试工具，非 provider）
```

## 快速启动

```powershell
cd frontend; npm install; npm run build   # 一次性构建前端
cd ..; cargo run                          # 打开 http://127.0.0.1:8321
```

## 模型设置（网页里配，持久化）

页面上方设置面板，流程：**选供应商 → 填 Base URL 和 Key → 获取模型 → 选模型 → 保存**。

- 内置 13 个供应商预设：DeepSeek / OpenAI / 商汤 / 通义 / 智谱 / Kimi / 百度千帆 / 星火 / SiliconFlow / Groq / xAI / Ollama / **OpenAI 兼容（自定义兜底）**。
- 每个预设自带默认 Base URL（可改）；**不带模型清单**——点「获取模型」实时从渠道拉取（OpenAI 兼容的 `GET /models`），列表下拉选择，也能手动填。
- 用中转渠道：选对应供应商（如 DeepSeek），把 Base URL 改成中转地址、Key 填中转 key，其余不用管。思考模式（reasoning_content）自动解析显示。
- 设置持久化到 `settings.json`；**API Key 用 Windows DPAPI 加密存储**（本机本用户才能解，文件里是密文），重新打开网页/重启服务都还在，无需重配。
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
