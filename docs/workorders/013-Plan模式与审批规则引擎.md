# 施工单 013：Plan 模式 + 审批规则引擎转正

> 2026-09-04 接手 agent 起草，供用户拍板。遵循施工单 011 修订时定的规矩：
> **本单只列范围 / 现状事实 / 验收底线 / 红线，具体实现方案由开工 agent 自拟**。

## 零、现状事实（已核实，本单的前提）

**审批规则引擎 v0（提前落地，已接线但仍是 v0）**
- 代码：`src/core/rules.rs` —— `RiskLevel`(Safe0/Normal1/Dangerous2)、
  `evaluate_risk(tool_name, args, workspace_root) -> (RiskLevel, 依据)`、
  `should_ask_approval(mode, risk) -> bool`，含 5 个单测。
- 接线：`core/agent.rs` L538 走 `gate.needs_approval_for(tool, args)` →
  `server.rs` `ChatGate::needs_approval_for`（L1368-1377）→ `rules`；
  `ask()` 里 SSE 下发 `approval_request{id,tool,args,risk,reason}`，
  JSONL 记 State 事件 `kind:"approval.requested"`。
- 前端：`App.svelte` L653-664 已接收 `risk`/`reason` 存进消息体，L1131 `decideApproval`
  回 `/api/approvals`。
- 档位归一化：`suggest→read-only`、`auto-edit|full-auto→auto`、`local→full-access`。
  现有**三处重复**：`server.rs` L840、`server.rs` L1437、前端 `App.svelte` L19-24。
- 旧判定仍在：`core/agent.rs::needs_approval(mode, danger_level)`（按工具静态
  danger_level）作为 trait 兼容分支保留 —— 与细粒度判定**两套并存**。

**v0 的已知硬伤（事实，不是定性）**
- shell 安全前缀用 `starts_with` 匹配 → `cargo test && del /f x` 现判 **Safe**（可绕过）。
- shell 高危黑名单用子串匹配 → `echo "del x"` 现判 **Dangerous**（误伤）。
- 未登记工具一律 Dangerous —— 有 MCP 计划，需留登记/默认策略口。
- `write_file` 敏感清单仅 `.git/` 与 `settings.json`；Windows 盘符/大小写差异下越界判定
  可能漏判。
- 审批决定不可复用：无「本次允许 / 本会话内同类允许」。
- reason 已下发但前端未展示 —— 用户看不出为什么被拦。

**Plan 模式**：零实现。当前核心循环是单阶段的「解析→工具→回填→判定→终止」。

**测试基线**：73 个。⚠️ 在 agent 自身运行环境里跑为 **70 通过 / 3 失败**，三个失败均为
环境假阴性、非代码缺陷：
1. `sandbox::probes::env_whitelist_trim` 与 `selftest_suite_shape`：agent 运行环境注入的
   `LSBOX_SHMEM_NAME` / `LSBOX_SHMEM_MUTEX` / `LSBOX_AUDIT_SHMEM` 三个变量被测试进程
   继承，导致「沙箱环境只剩白名单变量」断言失败；
2. `tools::tests::git_version_and_push_block`：本机 git 在
   `.workbuddy\binaries\PortableGit\...\mingw64\bin`（非系统安装路径），沙箱 PATH 重建后
   找不到 `git.exe`。
**本单开工前须在干净终端复测一次定基线**，往后只增不减。

## 一、范围（本单只做这些）

**A. 审批规则引擎转正**
- A1 判定能力补齐：必须覆盖第六节「v0 已知硬伤」列出的每一条事实场景（方案自拟）。
- A2 判定单点化：细粒度判定为唯一入口；旧 `danger_level` 分支要么删除、要么确保无调用
  点，不允许两套语义并存。
- A3 审批决定可复用：至少支持「本次允许」与「本会话内同类允许」。粒度方案自拟，但必须
  可撤销，且**不得跨会话写进 settings.json**。
- A4 依据可见：审批卡展示风险档位 + 判定依据（reason），用户能看懂为什么被拦。

**B. Plan 模式（Plan / Act 两档）**
- B1 模式切换入口，状态随会话持久化。
- B2 Plan 档只放行只读类工具；写/执行类**不进审批流程**，直接驳回并要求模型调整计划。
- B3 计划产物结构化（步骤 + 涉及文件），落 SSE 事件与 JSONL，前端渲染计划卡片。
- B4 用户批准后转 Act 执行；拒绝则回到 Plan。

**C. 明确不在本单范围**
- 不改审批三档的档位名与语义（read-only / auto / full-access 及归一化映射保持不变）。
- 不做规则的用户自定义 DSL、项目级规则文件。
- 不动沙箱机制与 local 档脱沙箱语义。
- 不做 MCP（属 ）。

## 二、验收底线（全过才算收尾）

1. 干净终端 `cargo test` 全绿，测试总数只增不减（基线 73；上述 3 个环境假阴性不计为
   本单失败，但须在报告里复测说明）。
2. 规则引擎新增单测，每条硬伤一个可指名断言：串联命令绕过、黑名单误伤、路径大小写
   越界、未登记工具默认策略、write_file 敏感清单扩展项。
3. 手工回归（本机冒烟 http://127.0.0.1:8321）：
   - auto 档：`cargo test && del /f x` **被拦**（当前会误放行）；
   - auto 档：`echo "del x"` **不被拦**（当前会误拦）；
   - read-only 档写文件仍要审批；full-access 档语义不变；
   - Plan 档下发「重构某模块」：全程零文件改动（影子仓库 diff 为空），计划卡片渲染出
     步骤 + 涉及文件；批准后改动按计划落地；Plan 档下只读工具照常可用；
   - 审批卡显示风险档位与判定依据。
4. 「本会话内同类允许」生效，新开会话后恢复询问。
5. 改前端必须 `cd web && npm run build` + 起服务冒烟。
6. 小步提交，中文 message，前缀 ` ...`。

## 三、禁止事项 / 红线（踩过坑的，别再踩）

1. **6 类 JSONL 事件 schema 定死**：计划事件只能作为既有 State 事件的 kind 扩展
   （照 `approval.requested` 的写法），禁止新增事件类型或改现有字段名。
2. **审批判定必须单点**：禁止一处走细粒度、另一处走静态 danger_level。
3. **settings.json 永不入库**；新增持久化字段必须向后兼容（旧 settings 能正常加载）。
4. **档位归一化只减不增**：现有三处重复，能合并则合并，不能合并就保持现状并在代码注释
   写清锚点，禁止再加第四处。
5. **沙箱红线**：Plan 模式不得绕过沙箱；local 档仍是「`config.toml [agent]
   allow_unsandboxed = true` 才脱沙箱，否则回退 full-auto 并 SSE 提示」（坑 7）。
6. **不写 AGENTS.md**（坑 13）；本仓库开发知识只进 ROADMAP 与施工单。
7. **restore/checkpoint 绝不整树 reset**（坑 12）：回归中若触发回滚，只回退 agent
   实际改过的文件。
8. **枚举/取值只增不改名**（沿用坑 14 B4 精神）：`RiskLevel` 三档与档位名的既有取值
   不得重命名或改数值。
9. **提示词不注入模型身份/人设**（坑 13）：Plan 模式的提示词增量同样遵守。
10. 环境备忘：本机 GNU 工具链损坏（MSVC only，别改 rust-toolchain.toml）；key 走 DPAPI，
    永不出日志、永不出 JSONL。

## 四、归位项

- G1 ROADMAP 对应行标记完成，补 tag。
- G2 HANDOFF.md 刷新（本单交付 + 遗留项）。
- G3 若新增规则表文件/模块，在 ROADMAP 记下代码锚点。
