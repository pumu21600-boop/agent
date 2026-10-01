# 上下文 Token 统计问题总结

> 项目：`agent-copy`（Rust 工程）
> 范围：上下文工程的 token 估算、裁剪与用量统计链路

## 统计链路

```
src/context/mod.rs（估算 + 裁剪）
   → src/core/agent/mod.rs（预算判定 / 圆环）
   → src/gateway/accounting.rs（用量台账）
   → src/server/chat/mod.rs（事件 / 落盘）
   → src/session/mod.rs（SessionUsage）
   → web/src/App.svelte（圆环 / 明细卡）
```

## 主要问题

### 1. 双口径分裂（最核心）

- 裁剪与交接判定用启发式估算 `estimate_messages`（`src/context/mod.rs`），而圆环显示优先用渠道实报 `last_real_tokens + last_completion_tokens`（`src/core/agent/mod.rs:410-414`）。
- 后果：圆环按实报走、裁剪按估算走，二者会不一致——可能出现圆环还没满就已裁剪，或圆环已满却迟迟不裁剪。

### 2. `tools_schema` 不计入裁剪预算

- 工具 Schema 只在圆环 breakdown 里补上（`src/core/agent/mod.rs:406`），`compact_messages` 判定超预算时完全不含它。
- 代码注释自述差额约 1884（估算 1866 vs 实际 4750），真实 prompt 明显大于裁剪所见的量级，裁剪时机被系统性推迟。

### 3. 圆环数值滞后一步、且裁剪后不回落

- `ContextUsage` 在每步模型调用**之前**发出（`src/core/agent/mod.rs:415`），用的是**上一轮**实报值，本步新增要等下一次才反映。
- 上下文被裁剪变小后 `last_real_tokens` 未重置，圆环仍显示裁剪前的旧大数。

### 4. 同一快照两个来源写法不一致

- 事件推送的 `tokens` 用核心循环算出的 `used`，而落盘时 `src/server/chat/mod.rs:725` 又按 `prompt_tokens + completion` 覆盖同一字段，口径不统一。

### 5. 估算遗漏字段

- `estimate_message`（`src/context/mod.rs:35-46`）只统计 `content` 与 `tool_calls`，不含随消息持久化的 `reasoning_content`（见 `src/core/agent/mod.rs` 落 `final_msg`）及 `role` / `name`。思考型模型的真实占用被系统性低估。

### 6. 摘要失败导致早期对话永久丢失

- 折叠后只留占位符 `[早期对话已省略 N 条消息]`（`src/context/mod.rs:216`）；若 `summarize_folded` 返回 `None`（`src/core/agent/mod.rs:391-394`），被折叠的原文不会回填，早期上下文永久丢失。

### 7. 默认预算高于常见模型窗口

- `default_budget_tokens() = 96_000`（`src/config/mod.rs:347`），`config.toml` 同为 96000，前端兜底也硬编码 96000（`web/src/App.svelte:571`）。
- 模型未配 `context_length` 时圆环按 96000 计算，但很多渠道（常见 64K 窗口）会远早于此就报超长。

### 8. 裁剪达标不强求

- 折到保护区（最近 N 个工具结果）后即停止，`tokens_after` 可能仍大于预算（`src/context/mod.rs:317` 测试注释也承认“本身可能超预算，此时不强求达标”）。

## 建议方向

- 统一契约：估算用于裁剪、实报仅用于展示；或在裁剪入口也加上 `tools_schema`。
- 裁剪后重置 `last_real_tokens`，避免圆环回落滞后。
- `estimate_message` 补上 `reasoning_content` 等字段。
- 默认预算按当前模型窗口动态取值，而非固定 96000。

## 复核结论（2026-10-01，逐条对码核实）

| # | 定性 | 处理 |
|---|---|---|
| 1 | 双口径并存属实，但「估算管裁剪、实报管展示」是合理分工（实报无法回放历史，估算无法精确），非缺陷 | 不改；#2/#5 修后两口径差距已收窄 |
| 2 | **实锤**。工具定义 ~3.3k tokens 随请求发送却不占裁剪预算，裁剪被系统性推迟 | **已修**：`compact_messages` 增 `tools_schema_tokens` 参数，before/after/折叠比较全计入（带新单测） |
| 3a | 滞后属实但有限：每步响应后 `api_usage` 即刷新（prompt+completion），只有工具执行窗口内短暂滞后；静止时圆环精确 | 不改（窗口期引入混合口径反而更乱） |
| 3b | **实锤**。裁剪后 `last_real_tokens` 不重置，圆环停在旧大数 | **已修**：`stats.compacted` 时清零实报值，回落到按当前消息的估算 |
| 4 | 表述不准：落盘只发生在 ApiUsage 臂，快照 tokens 单一来源；但 ContextUsage 臂确有冗余写入 | **已顺手修**：ContextUsage 臂只更分类占比，不再写 tokens |
| 5 | **实锤**（范围有限）：reasoning 随消息持久化，DeepSeek 契约白名单渠道会回放进 prompt，估算漏算；对剥离渠道是保守高估（与估算器取向一致） | **按用户拍板回退（2026-10-01）**：思考内容一律**不计入**——回放默认剥离、不进下一次请求，占用的只是渠道当次临时窗口；圆环的输出部分也改用「留存增量」（正文+工具调用参数），渠道 completion 含推理 token 不能直接用 |
| 6 | 属实但属边缘权衡：摘要调用失败（辅助渠道故障）时折叠原文不回填；原文仍在 agent.jsonl 请求日志里（轮转保留）可追溯 | 暂不改；可选方向=失败时把折叠原文写进 HANDOFF.md 附件，待拍板 |
| 7 | 风险成立但当前不咬人：默认 96000 只在模型未配 `context_length` 时生效，现有配置全部显式配了窗口 | 待拍板：把默认降到 65536（保守方向）还是引入按供应商的窗口表 |
| 8 | 设计如此（测试注释自述）：保护区（最近 N 个工具结果+最后一轮）本身超预算时不强求，否则会把「正在用的上下文」裁掉 | 不改 |

## 第三轮：对齐 DSH token-meter 的占用口径（2026-10-01 落地）

对照本机 DSH 实现（`@deepseek-ai/dsh-token-meter`、`dsh-compaction-basic`、客户端 `ContextMeter`）
逐条对齐。DSH 的核心是**一个测量源 + 一个锚点**：`measure()` 回放会话日志给出确定性的表面定价，
提供方实报只在「规范 envelope 完全一致」时复用；占用显示 = `projectedTokens`（样本 + 样本之后
表面的**带符号**位移），压缩读 `measure()`，UI 数字不参与任何决策。

| 前两轮问题 | 本轮处理 |
|---|---|
| ① 折叠后 system 提示词被复制一份（`kept` 未排除下标 0；实测输出 `[system, 占位, system, …]`，请求多发一份、统计算两遍、还随快照写进会话文件） | **已修**：`kept` 只收下标 ≥1；回归测试 `fold_keeps_exactly_one_system_message` |
| ② 圆环漏算整块工具结果（旧口径 `last_real + added(正文+工具参数)`——盯着圆环等模型读工具结果的那段时间显示偏小，要等下次响应才跳） | **已修**：换成 DSH 的「实报锚点 + 带符号增量」（`context::projected_tokens`）：工具结果/追发消息自动补上，裁剪当场回落，不再需要清零实报值 |
| ③ 无头/定时/子代理会话不落用量快照 → 切回去圆环是空的 | **已修**：`SessionUsage::apply_context/apply_api` 收成一处共用，headless 事件臂同样落盘 |
| ⑤ 圆环/构成/交接三个分母各说各话 | **已修（展示口径）**：圆环 = projected/window；构成行按圆环百分比分摊（加总 = 圆环，DSH 同一根环的画法）；卡片补「最近一次实报」行与口径说明。交接阈值仍走估算口径（未动） |
| ⑥ real 路径增量与估算器的思考口径不一致（回放渠道来回跳） | **已消除**：增量不再单独算，统一由 `estimate_*` 的 `count_reasoning` 决定 |
| ⑦ display_only 气泡（通知/错误）被本地估算计入，却不发给模型 | **已修**：`estimate_message`/`estimate_breakdown` 对 display_only 计价 0——模型看不到的不计价 |
| ④ 预算 = 整个模型窗口，没给输出留余量 | **未动（下一步）**：DSH 阈值 `floor(min(W×0.8, W−O−B))`，O = 生效输出上限、B = 65536 余量；逐字保留预算还必须在阈值之下（配置不合法直接拒载） |
| ⑧ 折叠达标不强求 / 摘要不参与裁剪 | 未动。DSH 的差异主要在阈值公式与「保留预算 < 阈值」的校验，随 ④ 一起做 |

### 会话用量快照字段（旧文件自动升级，serde alias）

| 字段 | 语义 |
|---|---|
| `projected` | 圆环分子：**下一次请求**的预估占用 = 实报锚点 + 自锚点以来的带符号增量 |
| `pressure` | 最近一次渠道实报的 prompt（锚点，展示「最近一次实报」用） |
| `anchor_est` | 与 `projected` 配对的本地估价（DSH 的 `sampledSurfaceTokens`）：跨轮沿用锚点靠这一对，新起一轮不会退回估算 |
| `window` | 圆环分母（模型 `context_length` 或全局 `[context].budget_tokens`） |
| `tokens` / `budget` | 旧字段名，读取时按 alias 收下（→ projected / window），写出只用新名 |

### 对齐依据（本机 DSH 实现，只读核对）

- `dsh-token-meter/lib/index.js:641-683`：`totalTokens = baseline + surfaceDeltaTokens`，锚点 envelope 不等就整体重估，增量可为负
- `dsh-token-meter/lib/index.js:509-513`：`projectedTokens = max(0, pressureTokens + surfaceTokens − sampledSurfaceTokens)`（样本在 assistant 消息入 surface **之前**打戳，见 `:456-466`）
- `dsh-client-ui-conversation/lib/client.js:16912-16920`：UI 占用 = `projectedTokens ?? pressureTokens` ÷ `contextWindow`；`:17031-17040` 构成段按圆环百分比分摊
- `dsh-token-meter/lib/types/estimate.js`：固定启发式（4 字符/token、块开销 4、角色开销 4），文档明说「构成行只当近似构成，绝不当总量」
- `dsh-compaction-basic/README.zh.md:62-69`：阈值 = `floor(min(W × thresholdRatio, W − O − headroomTokens))`，`retainRatio` 必须小于阈值
