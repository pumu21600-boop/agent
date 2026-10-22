# JSONL 日志 Schema（施工单 001 / 任务 A）

## 1. 文件位置

- 默认：`logs/agent.jsonl`（相对工程根目录）
- 可通过 `config.toml` 的 `[logging]` 节改 `dir` / `file` / `max_mb` / `keep_archives`
- 追加写（append-only），进程每次启动生成新的 `session_id`
- **轮转**（2026-09-14 补，多份归档）：单文件超过 `[logging].max_mb`（默认 20MB）就归档成
  `agent.jsonl.1`（最新）再开新文件，旧归档依次倒位成 `.2`…`.N`，保留份数走 `[logging].keep_archives`
  （默认 3，0 = 归档即删）；启动时检查一次，运行中每 500 条复查一次，
  轮转本身记一条 `state / log.rotated`；`max_mb = 0` 关闭轮转

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
- 新增事件一律通过 `src/observability/mod.rs` 的 `observability::event()` 写入

## 3. event_type 枚举（定死 6 类）

| event_type | 含义 | 状态 |
| --- | --- | --- |
| `thought` | 模型思考 / 推理内容（完整段落）| 已使用 |
| `tool_call` | 工具调用 | 已使用 |
| `tool_result` | 工具结果 | 已使用 |
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

### 4.2 tool_call / tool_result —— 工具调用与结果（核心循环）

```json
{ "step": 0, "id": "call-1", "name": "read_file", "arguments": "{\"path\":\"README.md\"}" }
{ "step": 0, "id": "call-1", "name": "read_file", "ok": true, "chars": 1234, "truncated": false, "preview": "前 300 字符…" }
```

### 4.3 thought —— 完整思考段落（起）

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
| `sandbox.spawn` | 沙箱/本机直跑拉起进程（/）| `pid`、`app`、`working_dir`、`restricted`（false=本机直跑）、`timeout_ms`、`memory_mb`、`active_processes` |
| `sandbox.exit` | 沙箱进程树收割 | `pid`、`exit_code`、`timed_out`、`stdout_chars`、`stderr_chars` |
| `sandbox.selftest` | 沙箱探针套件结果 | `ok` |
| `sandbox.tier.resolved` | 档位探测链解析完成 | `effective`、`ok`（探测链是否全绿）|
| `sandbox.tier.fallback` | 请求档不可用自动回退（仅 AppContainer→受限 一种）| `requested`、`effective`、`reason` |
| `gateway.client_rebuild` | 网关 HTTP 客户端热重建（网络自愈） | `note` |
| `settings.channel_changed` | 配置渠道地址变更，模型清单作废 | `base_url` |
| `approval.requested` | 工具调用待审批 | `id`、`tool` |
| `approval.decided` | 审批决定 | `id`、`approved` |
| `context.compacted` | 上下文超预算触发裁剪 | `step`、`archived_tools`、`folded_messages`、`tokens_before`、`tokens_after` |
| `checkpoint.created` | 影子仓库检查点已提交 | `id`、`ts`、`reason`（任务起点/工具名+参数头/任务结束快照）|
| `checkpoint.restored` | 撤销：回退检查点之后 agent 改过的文件 | `id` |
| `checkpoint.skipped` | 检查点失败/停用，不阻断工具 | `reason` |
| `checkpoint.summary` | 任务改动统计条 | `baseline`、`files`、`additions`、`deletions` |
| `session.saved` | 会话消息落盘（每步/终止时）| `session`（会话 id）、`messages`（条数）|
| `session.cleared` | 删除单条会话 | `session` |
| `handoff.written` | 交接文档已生成 | `path`、`trigger`（auto/manual）、`context_pct`（auto 时）|
| `memory.summarized` | 滚动摘要已生成并注入 | `step`、`chars` |
| `memory.summary_skipped` | 摘要调用失败，降级保留占位符 | `step`、`note` |
| `model.truncated` | 模型输出被 max_tokens 截断，SSE 已提示用户 | `step`、`max_output_tokens` |
| `gateway.retry_429` | 渠道 429 退避重试中 | `attempt`、`wait_ms` |
| `task.start` | 任务开始（trace 的任务切分锚点）| — |
| `gateway.error` | 渠道返回非 2xx | `status`、`body` |
| `gateway.throttle` | 本地限流排队（`max_rpm`）| `wait_ms` |
| `gateway.fallback` | 降级链切换渠道 | `step`、`from`、`to`、`base_url` |
| `config.changed` | 配置新建/删除/切换 | `action`（new/delete/apply）、`provider_id`、`model` |
| `approval.mode` | 审批档位切换（进行中请求即时生效）| `mode` |
| `plan.mode` | 本轮任务处于 Plan 档 | `mode`（plan）、`step` |
| `plan.blocked` | Plan 档拦截非只读工具并回喂模型 | `step`、`tool`、`reason` |
| `plan.proposed` | 最终回答解析出结构化计划 | `step`、`steps`、`files` |
| `tools.parallel` | 同轮多个只读工具并行执行 | `step`、`count` |
| `write.conflict` | 同轮写同一路径，后者跳过并回喂 | `step`、`tool`、`arguments` |
| `sandbox.appcontainer.warn` | AppContainer 属性装配失败退化的提示 | `message` |
| `memory.agents_saved` | 约定文件保存（scope=system/global/project）| `scope`、`path` |
| `memory.reindex` | 记忆库强制重建向量 | — |
| `memory.search` | 记忆库检索 | `query`、`hits`、`mode` |
| `memory.similar` | 「相似会话」检索 | `id`、`hits` |
| `session.created` | 新建空会话立即落盘（09-14 补） | `session`、`workspace` |
| `session.titled` | 会话标题自动生成 | `title` |
| `workspace.switched` | 切换工作区 | `from`、`to` |
| `workspace.removed` | 从项目列表移除工作区（不删目录） | `path` |
| `workspace.aliased` | 项目重命名（空名=清除别名） | `path`、`name` |
| `git.committed` | 界面手动提交 | `id`、`files`、`manual` |
| `attachments.gitignored` | 附件目录自动写入工作区 .gitignore | `path` |
| `media.error` | 多模态理解调用失败（DSH 转接规范） | `modality`、`model`、`url`、`error` |
| `log.rotated` | 日志超限归档成 `<file>.1`（2026-09-14 补；多份倒位）| `bytes`、`limit_bytes`、`keep` |
| `steer` | 「立即发送（引导）」的消息并入了正在跑的一轮（025/027）：核心循环每个决策点取一次；没赶上本轮、在收尾续跑一轮的记 `phase: "followup"` | `count`、`step`（并入时所在步）；续跑时 `phase`、`session` |
| `chat.steered` | 同会话运行中再发消息 → 进队列（027） | `session`、`mode`（`queue`=排队等本轮结束 / `immediate`=立即并进本轮）、`pending`（积压条数） |
| `chat.rejected` | 同会话运行中再发消息被拒（025：仅带附件的「立即发送」不支持） | `session`、`reason`（`session_busy_attachments`） |

### 4.x 新增 kind（施工单 027，只增）

| `kind` | 说明 | 关键字段 |
| --- | --- | --- |
| `storage.write` | 内核保留区落盘经存储一门（session/settings） | `area` |
| `storage.plugin_write` | 插件命名空间写入成功 | `plugin`、`key`、`bytes` |
| `storage.plugin_denied` | 插件越权/超限被拒（保留区不可达） | `plugin`、`key`、`reason` |
| `storage.plugin_delete` | 插件命名空间删除 | `plugin`、`key`、`found` |
| `hook.before_tool` | 钩子前置挂点结果（deny/allow/timeout_pass/spawn_error_pass） | `tool`、`decision`、`reason?`、`timeout_ms?` |
| `hook.blocked` | 钩子 deny 拦下工具调用并回喂模型 | `step`、`tool`、`reason` |
| `hook.after_tool` | 工具执行后通知挂点已跑 | `ok` |
| `hook.session_end` | 任务结束挂点已跑 | `session`、`ok` |
| `hook.budget_exceeded` | 单任务累计 token 超预算通知 | `tokens`、`budget`（通知载荷） |
| `browser.launch` | 内置浏览器已拉起 | `exe`、`port`（CDP 调试端口）|
| `browser.stale` | 浏览器会话失活（进程退出/连接断开），已清理待重拉 | — |
| `browser.navigate` | 页面导航（经统一 browser 工具的 navigate 方法）| `url`、`tab` |
| `browser.click` | 页面点击完成 | `target`（语义定位描述）、`x`、`y`、`tab` |
| `browser.type` | 页面输入完成（ type/fill 均记 type）| `target`、`chars`、`tab` |
| `browser.close` | 浏览器关闭（工具/空转/idle 收割）| `reason`（tool/idle/dead）|
| `browser.tab.new` | 新标签页 | `id`、`agent`、`url` |
| `browser.tab.close` | 关标签页 | `id` |
| `browser.tab.suspend` | 后台标签页挂起 frozen（驻留）| `id` |
| `browser.tab.restore` | 挂起标签页解冻恢复 | `id` |
| `browser.dialog` | 页面弹出 JS 弹窗 | `id`、`type` |
| `browser.tab.new` | 新开标签页（agent 工作页或面板「+」）| `id`、`agent`、`url` |
| `browser.tab.close` | 关闭标签页 | `id` |
| `plugin.draft_submitted` | 模型自产插件进入 draft 隔离区 | `plugin`、`files`、`tools` |
| `plugin.draft_promoted` | draft 一键转正落盘 | `plugin`、`paths` |
| `plugin.draft_discarded` | draft 丢弃 | `plugin` |

### 4.x 新增 kind（施工单 030，只增）

| `kind` | 说明 | 关键字段 |
| --- | --- | --- |
| `usage.ledger_ready` | 用量台账就绪（启动扫盘后） | `records`、`spend_usd`、`unpriced` |
| `usage.ledger_rotated` | 台账超 32MB 归档成 `ledger.jsonl.1` | `limit_bytes` |
| `usage.ledger_init_failed` | 台账目录初始化失败（启动时） | `dir`、`error` |
| `cost.cap_blocked` | 全局花费上限硬闸拦下一次模型请求 | `session`、`purpose`、`model`、`spend_usd`、`cap_usd` |
| `cost.downgraded` | 任务超 token 预算，剩余步骤改走降级渠道 | `step`、`from`、`to` |
| `usage.prices_saved` | 价格表/币种已保存（「用量价格」插件设置页） | `rows`、`currency` |
| `usage.ledger_recomputed` | 用当前价格重算全部历史账 | `records`、`spend_usd`、`unpriced` |

> 台账本身**不是** JSONL 事件流：每次模型调用的完整计量（token 三路 / 状态 / 延迟 / 费用）
> 落独立的 `<日志目录>/usage/ledger.jsonl`，字段见 README「用量台账与成本红线」。

### 4.x 新增 kind（无头会话 / 定时任务 / 子代理，只增）

| `kind` | 说明 | 关键字段 |
| --- | --- | --- |
| `headless.run` | 无头会话被程序化唤醒（/api/headless/run 或 pawch-agent.exe run） | `session`、`purpose`（cli/schedule/subagent…）、`mode`、`auto_approve`、`timeout_secs` |
| `headless.done` | 无头会话整轮结束 | `session`、`status`（done/cancelled/timeout/error）、`purpose`、`duration_ms`、`requests`、`prompt_tokens` |
| `headless.rejected` | 唤醒被拒（会话正在运行中，409） | `session`、`reason` |
| `headless.timeout` | 整轮超时已取消（任务在后台收尾） | `session`、`secs` |
| `headless.approval_auto` | 无人值守时命中审批，按 on_approval 策略自动决定 | `session`、`tool`、`approved` |
| `headless.leftover` | 收尾时队列里还有没跑完的追加消息（超过 3 轮续跑上限） | `session`、`count` |
| `schedule.fired` | 定时任务到点触发一次唤醒 | `task`、`name`、`trigger`（auto/manual） |
| `schedule.done` | 定时任务这一轮结束（状态已写回任务） | `task`、`name`、`trigger` |
| `schedule.saved` | 定时任务新建/修改已存 settings.json | `task`、`name`、`new` |
| `schedule.deleted` | 定时任务已删除 | `task` |
| `subagent.spawned` | 子代理已派发（独立会话起跑） | `parent`、`child`、`model`、`label`、`task_chars` |
| `subagent.done` | 子代理这一轮结束（结果已回注父任务） | `parent`、`child`、`status`、`tokens`、`duration_ms` |
| `subagent.queued` | 并发度满员，派发进等待（200ms 粒度重试） | `parent`、`running`、`per_parent` |
| `subagent.rejected` | 派发被拒（嵌套派发 / 并行未启用 / 父任务已取消） | `parent`、`reason` |
| `subagent.cascade_cancel` | 父任务被停 → 级联取消它的活跃子代理 | `parent`、`count` |

### 4.5 error —— 错误

```json
{ "message": "错误描述", "status": 401, "body": "渠道返回体（仅网关错误）" }
```

## 5. 示例

```json
{"ts":"2026-09-02T07:04:12.345Z","session_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","event_type":"state","payload":{"kind":"startup","base_url":"https://api.deepseek.com","model":"deepseek-chat","host":"127.0.0.1","port":5173}}
{"ts":"2026-09-02T07:04:15.102Z","session_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","event_type":"state","payload":{"kind":"gateway.request","url":"https://api.deepseek.com/chat/completions","body":{"model":"deepseek-chat","messages":[{"role":"user","content":"你好"}],"stream":true}}}
{"ts":"2026-09-02T07:04:15.402Z","session_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","event_type":"model_chunk","payload":{"raw":"{\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}","content":"你好"}}
{"ts":"2026-09-02T07:04:16.001Z","session_id":"f47ac10b-58cc-4372-a567-0e02b2c3d479","event_type":"error","payload":{"message":"渠道返回 401 Unauthorized: ..."}}
```

## 6. 红线

- **API key 永不写入 JSONL / 任何文件 / 控制台**（只存在于环境变量与内存中的 Authorization 头）
- 事件类型只允许上述 6 类，不新增
- `state` 的 `kind` 只增不改名；新增 kind 必须同步更新本文件
- 每行必须完整，验收可用 `tail -1` / 逐行 JSON 解析校验

## 7. 项目记忆 events.log（区别于本文件）

`<workspace>/.agent/memory/events.log` 是**记忆层**事件流水（跟 git 仓库走），
与本文件的进程日志（`logs/agent.jsonl`，不入库）是两回事：
每行 `{ts, session, kind, ...}`，kind ∈ task.start / tool.call / task.finish /
handoff.written / checkpoint.created；默认脱敏（sk-/Bearer/api_key 打码）。
