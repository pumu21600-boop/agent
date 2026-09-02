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

| event_type | 含义 | 本单状态 |
| --- | --- | --- |
| `thought` | 模型思考 / 推理内容 | 预留（核心循环接入后使用） |
| `tool_call` | 工具调用 | 预留（本单无工具） |
| `tool_result` | 工具结果 | 预留（本单无工具） |
| `model_chunk` | 模型流式分片 | 已使用 |
| `state` | 状态快照（启动、网关请求/响应、取消、流结束等） | 已使用 |
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

### 4.2 state —— 状态快照（用 `kind` 区分）

| kind | 说明 | 关键字段 |
| --- | --- | --- |
| `startup` | 进程启动 | `base_url`（真实渠道地址）、`model`、`host`、`port`、`session_id` |
| `gateway.request` | 网关请求 | `url`（真实 base_url + /chat/completions）、`body`（完整请求体） |
| `gateway.response` | 网关响应头 | `status` |
| `gateway.stream_end` | 流结束 | `chunks`（分片数）、`done` |
| `cancel` | 取消事件 | `flag`（置位）、或 `phase`（`before_request`/`streaming`） |

### 4.3 error —— 错误

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
