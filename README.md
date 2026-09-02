# agent —— Windows 原生类 Codex 自主编码 Agent

技术栈：Rust 后端（axum）+ Svelte 前端（Vite）+ SSE 流式。
当前进度：P1 完成（结构模块化 + 聊天 UI 骨架），排产见 [docs/ROADMAP.md](docs/ROADMAP.md)。

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

## 模型设置（侧栏「模型设置」或右上角「设置」弹窗）

流程：**选供应商 → 填 Base URL 和 Key → 获取模型（弹窗多选/全选/保存）→ 选模型 → 保存**。

- 内置 13 个供应商预设：DeepSeek / OpenAI / 商汤 / 通义 / 智谱 / Kimi / 百度千帆 / 星火 / SiliconFlow / Groq / xAI / Ollama / **OpenAI 兼容（自定义兜底）**。
- 每个预设自带默认 Base URL（可改）；**不带模型清单**——点「获取模型」实时从渠道拉取（OpenAI 兼容的 `GET /models`），列表下拉选择，也能手动填。
- 用中转渠道：选对应供应商（如 DeepSeek），把 Base URL 改成中转地址、Key 填中转 key，其余不用管。思考模式（reasoning_content）自动解析显示。
- 设置持久化到 `settings.json`；**API Key 用 Windows DPAPI 加密存储、按供应商分别保存**（本机本用户才能解，文件里是密文），重新打开网页/重启服务都还在，无需重配。
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
