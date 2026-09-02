# JSONL 日志 Schema（施工单 001 / 任务 A）

## 1. 文件位置

- 默认：`logs/agent.jsonl`（相对工程根目录）
- 可通过 `config.toml` 的 `[logging]` 节改 `dir` / `file`
- 追加写（append-only），进程每次启动生成新的 `session_id`，旧内容不清理

## 2. 记录格式（每条一行）

每条记录是一个 JSON 对象，**固定 4 个顶层字段**，之后是 `payload`：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `ts` | string | RFC3339 UTC 毫秒时间戳，如 `2026-09-02T07:04:12.345Z` |
| `session_id` | string | UUIDv4，每次进程启动生成一个（单会话：重启即换，无残留） |
| `event_type` | string | 6 类之一，见下 |
| `payload` | object | 事件负载，子结构随 `event_type` 变化 |

### 写盘保证

- 单行一条：序列化为一行 JSON + `\n`，**一次 write_all 写完**，立即 `flush()`
- 不允许多线程交错写入（内部 Mutex），不产生半行
- 新增事件一律通过 `src/logging.rs` 的 `logging::event()` 写入

## 3. event_type 枚举（定死 6 类）

| event_type | 含义 | 状态 |
| --- | --- | --- |
| `thought` | 模型思考 / 推理内容（完整段落） | 已使用（P2） |
| `tool_call` | 工具调用 | 已使用（P2） |
| `tool_result` | 工具结果 | 已使用（P2） |
| `model_chunk` | 模型流式分片 | 已使用 |
| `state` | 状态快照（启动、网关请求/响应、循环、取消等） | 已使用 |
| `error` | 错误 | 已使用 |

## 4. payload 子结构

### 4.1 model_chunk —— 流式分片

```json
{
  "raw": "data 行原始 JSON 字符串",
  "content": "本次分片增量文本",
  "reasoning_content": "推理内容（DeepSeek R1 类模型才有，可缺省）",
  "finish_reason": "stop（最后一个分片才有，可缺省）"
}
```

### 4.2 tool_call / tool_result —— 工具调用与结果（P2 核心循环）

```json
{ "step": 0, "id": "call-1", "name": "read_file", "arguments": "{\"path\":\"README.md\"}" }
{ "step": 0, "id": "call-1", "name": "read_file", "ok": true, "chars": 1234, "truncated": false, "preview": "前 300 字符…" }
```

### 4.3 thought —— 完整思考段落（P2 起）

```json
{ "step": 0, "text": "该轮模型返回的完整 reasoning_content" }
```

### 4.4 state —— 状态快照（用 `kind` 区分）

| kind | 说明 | 关键字段 |
| --- | --- | --- |
| `startup` | 进程启动 | `base_url`（真实渠道地址）、`model`、`host`、`port`、`session_id` |
| `gateway.request` | 网关请求 | `url`（真实 base_url + /chat/completions）、`body`（完整请求体：model + messages 历史 + tools，不含 key） |
| `gateway.response` | 网关响应头 | `status` |
| `gateway.stream_end` | 流结束 | `chunks`（分片数）、`done`、`tool_calls`（本轮聚合工具调用数） |
| `loop.finish` | 核心循环终止（模型给出纯文本） | `step`、`content_chars` |
| `settings.updated` | 设置保存 | `provider_id`、`model` |
| `cancel` | 取消事件 | `flag`（置位）、或 `phase`（`before_request`/`streaming`/`tool`/`loop`） |
| `sandbox.spawn` | 沙箱/本机直跑拉起进程（P3/P4） | `pid`、`app`、`working_dir`、`restricted`（false=本机直跑）、`timeout_ms`、`memory_mb`、`active_processes` |
| `sandbox.exit` | 沙箱进程树收割（P3） | `pid`、`exit_code`、`timed_out`、`stdout_chars`、`stderr_chars` |
| `sandbox.selftest` | 沙箱探针套件结果（P3） | `ok` |
| `gateway.client_rebuild` | 网关 HTTP 客户端热重建（网络自愈） | `note` |
| `settings.channel_changed` | 配置渠道地址变更，模型清单作废 | `base_url` |
| `approval.requested` | 工具调用待审批（P4） | `id`、`tool` |
| `approval.decided` | 审批决定（P4） | `id`、`approved` |
| `context.compacted` | 上下文超预算触发裁剪（P5） | `step`、`archived_tools`、`folded_messages`、`tokens_before`、`tokens_after` |
| `checkpoint.created` | 影子仓库检查点已提交（P6） | `id`、`ts`、`reason`（工具名+参数头） |
| `checkpoint.restored` | 工作树回滚到检查点（P6） | `id` |
| `checkpoint.skipped` | 检查点失败/停用，不阻断工具（P6） | `reason` |
| `session.saved` | 会话消息落盘（P6，每步/终止时） | `messages`（条数） |
| `session.cleared` | 新会话清盘（P6） | 无 |
| `handoff.written` | 交接文档已生成（P6） | `path`、`trigger`（auto/manual）、`context_pct`（auto 时） |

### 4.5 error —— 错误

```json
{ "message": "错误描述", "status": 401, "body": "渠道返回体（仅网关错误）" }
```

## 5. 示例

```json
{"ts":"2026-09-02T07:04:12.345Z","session_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","event_type":"state","payload":{"kind":"startup","base_url":"https://api.deepseek.com","model":"deepseek-chat","host":"127.0.0.1","port":8321}}
{"ts":"2026-09-02T07:04:15.102Z","session_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","event_type":"state","payload":{"kind":"gateway.request","url":"https://api.deepseek.com/chat/completions","body":{"model":"deepseek-chat","messages":[{"role":"user","content":"你好"}],"stream":true}}}
{"ts":"2026-09-02T07:04:15.402Z","session_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","event_type":"model_chunk","payload":{"raw":"{\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}","content":"你好"}}
{"ts":"2026-09-02T07:04:16.001Z","session_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","event_type":"error","payload":{"message":"渠道返回 401 Unauthorized: ..."}}
```

## 6. 红线

- **API key 永不写入 JSONL / 任何文件 / 控制台**（只存在于环境变量与内存中的 Authorization 头）
- 事件类型只允许上述 6 类，不新增
- 每行必须完整，验收可用 `tail -1` / 逐行 JSON 解析校验
