# 施工单 014：trace 时间线视图 + 工作区切换缺口盘点

> 2026-09-04 起草。用户拍板「直接开始」（不走先提案后动工流程），方案自拟。
> eval 体系涉及任务集设计方法论，**不在本单**，留施工单 015 单独拍板。

## 零、现状事实（已核实）

- JSONL 固定 4 字段：`ts / session_id / event_type / payload`，6 类事件（observability/mod.rs）。
  `session_id` 是**进程级**的（main.rs 启动生成，重启即换）——业务对话会话 id 不进 JSONL。
- 任务边界：只有 `state kind:"loop.finish"`（agent.rs 结束时），**没有 task.start**——
  本单补记（State kind 扩展，照 plan.blocked 写法，属合法「只增」）。
- `model_chunk` 是流式分片（gateway/mod.rs:711，payload 为原始 SSE chunk），单任务可上千条
  ——时间线必须聚合展示。
- 日志现状：logs/agent.jsonl 约 31k 行 / 18.6MB，逐行解析 Rust 可承受（<100ms 级），
  先做全量读取+尾部截断，性能不够再优化（记录在案）。
- 工作区切换：已落地 `POST /api/workspace`（校验→换根→记忆层热重开→持久化+最近 8 条）
  + 启动恢复 + 侧栏「切换工作区」弹窗；又加「添加项目」（rfd 原生选文件夹，同链路）。
  ROADMAP 对应行的「工作区切换」大概率只剩盘点关项。

## 一、范围

**A. trace 时间线视图**
- A1 后端 `GET /api/trace?limit=<N>`（默认 5）：读 logs JSONL，按任务切分（task.start，
  旧日志兜底用「tool_call step==0」），每任务内连续 `model_chunk` 聚合（count/首尾 ts/字符量），
  返回最近 N 个任务（倒序）。payload 原样透传，不改写不删字段。
- A2 run_task 开始补 `state kind:"task.start"` 事件。
- A3 前端时间线面板：侧栏工具区入口；任务列表（开始时间/时长/事件数）；展开后垂直时间线
  （相对秒 + 事件类型徽章 + 工具名/参数摘要；tool_call↔tool_result 配对显示耗时）。

**B. 工作区切换缺口盘点**（对照既定交付底线，能关则关，缺什么补什么）

**C. 明确不在本单**
- eval 体系（施工单 015）。
- 不做跨会话 trace 关联（JSONL 无业务会话 id，进程级粒度足够调试用）。
- 不做 trace 的写端改造（6 类 schema 定死，红线 1）。

## 二、验收底线

1. `cargo test` 基线 88 只增不减；`cd web && npm run build` 通过。
2. 冒烟：跑一个真实任务后，时间线面板能看到该任务的事件流（类型/耗时/摘要），
   model_chunk 聚合成单条而非逐分片。
3. /api/trace 对空日志/损坏行容错（跳过坏行不 500）。

## 三、红线

1. JSONL 6 类 schema 定死：只读解析，写端仅新增 `task.start` 一个 State kind。
2. logs/ 只读，永不写、永不入库。
3. 提示词/AGENTS.md 不动（坑 13）。
4. 小步提交，中文 message，前缀 ` ...`。
