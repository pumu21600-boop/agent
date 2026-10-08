# 施工单 015：eval 评测体系（收官单）

> 2026-09-04 起草。用户指示「直接开始」，方案自拟；方向性取舍列于零节，供事后复核。

## 零、方向性取舍（本单的前提假设）

1. **HTTP 黑盒驱动**：eval 是独立驱动器（`cargo run --bin eval`），对运行中的服务
   发真实对话（/api/chat SSE），**主服务代码零改动**（仅新增 src/eval.rs 模块 +
   src/bin/eval.rs 壳 + evals/tasks/*.json）。服务保持黑盒，eval 才测得出真实行为。
2. **真实渠道跑**：单任务成本 = 一次真实对话；任务集首批 3 个轻任务。渠道不可用时
   eval 报告明确标注失败原因（不静默）。
3. **隔离与恢复**：每任务在系统临时目录建独立工作区（预置 setup_files）→
   POST /api/workspace 切过去 → 跑 → 断言 → 恢复原工作区。settings.json 最近列表
   会滚入临时路径（8 条上限自动淘汰，可接受）。
4. **审批档位**：任务默认 auto 档（工作区内写静默放行），headless 下无人点审批卡，
   任务设计不得触发 Dangerous（触发了就等 120s 超时拒绝，算任务失败）。

## 一、范围

**A. eval 核心模块**（src/eval.rs，可单测）
- 任务定义加载：`evals/tasks/*.json`——字段 name/prompt/plan_mode/approval_mode/
  timeout_secs/setup_files/assertions。
- 断言引擎（首批 5 种，留口扩展）：`file_exists` / `file_not_exists` /
  `file_contains`(needle) / `no_new_files`（对比 setup 后快照）/ `sse_event`（流中出现
  某顶层事件，如 done/plan_proposal）。
- SSE 驱动器：POST /api/chat 收流（data: 行解析），记录出现过的顶层事件类型与
  done/error 终态，带整体超时。

**B. CLI**（src/bin/eval.rs）
- 用法：`cargo run --bin eval [-- 任务名 ...] [--base http://127.0.0.1:8321] [--keep]`
- 流程：扫任务 → 逐任务（临时工作区+setup → 切工作区 → 跑 → 断言 → 收集）→
  恢复原工作区 → 报告（每任务断言明细 + 耗时）→ exit code（全过 0 / 有失败 1）。

**C. 首批任务集**（evals/tasks/）
1. `hello-file`：创建包含指定内容的文件 → file_exists + file_contains。
2. `fix-json`：预置坏 JSON，修复成合法 JSON → file_contains。
3. `read-only-plan`：Plan 档只读调研并输出计划 → sse_event:plan_proposal（零写操作由
   Plan 档机制保证）。

## 二、验收底线

1. `cargo test` 基线 90 只增不减（eval 断言引擎补单测）。
2. 真机跑 3 任务全过（渠道间歇 500 时可切商汤，测毕切回）。
3. eval 结束后服务工作区恢复原值；主服务代码零改动（git diff 核对）。
4. 改前端不需要（本单无前端改动）。

## 三、红线

1. 主服务零改动：只增 src/eval.rs、src/bin/eval.rs、evals/。
2. JSONL/AGENTS.md/settings 语义不动（坑 1/13/14）；evals/ 目录入库（任务集是资产）。
3. 小步提交，中文 message，前缀 ` ...`。
