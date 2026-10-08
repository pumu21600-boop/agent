# agent-copy 项目整改 — 验收交接文档

> 用途：在**新对话**中粘贴本文件（或用 `@agent-copy` 带上本仓库，引用 `docs/agent-copy-验收交接文档.md`），让新的会话无缝接手验收工作。
> 项目本地路径：`C:\Users\31144\Desktop\agent-copy`（用 `@agent-copy` 引用该文件夹）
> **本文件历史**：原由用户维护于 Desktop，此前未存进项目；2026-09-29 由 AI 补写入库（本节及第四、六、七节为当日新增/更新），建议以此版为准并替换 Desktop 旧件。

---

## 一、项目背景（30 秒版）

- 项目名 `agent`，当前版本 **0.30.0**，Windows 本地优先的自主编码 Agent
- 技术栈：Rust（axum）后端 `src/` + Svelte 前端 `web/` + Tauri 桌面壳 `desktop/`
- 工程惯例：**施工单驱动**（`docs/workorders/NNN-主题.md`），每个 P 阶段在 `docs/ROADMAP.md` 的进度大表里登记一行

## 二、已完成并验收通过的工作

### 插件机制分家（用户方已改，已两轮验收 ✅）

对照原先诊断出的 7 个乱点，全部收口：

1. `ui-ext/` 旧目录已删除，代码中仅剩兼容垫片引用
2. `plugins/` 目录现在装三个真插件（board / trace / usage，各含 manifest + index.html）
3. 接口 `/api/ext` 在 Rust 和前端均零引用；新路由为 `/api/plugins`（toggle/draft/promote/trust）+ `/api/plugin/data` + 静态分发 `/plugins/{name}/`
4. 设置字段 `disabled_extensions` 已改 `disabled_plugins`，带 serde alias 读旧写新，信任键 `ui-ext:<名>` → `plugins:<名>` 迁移有测试覆盖
5. `src/plugin/` 从 7 文件收敛到 6 文件（contract/draft/trust/ui/wasm/mod），mod.rs 仅 19 行
6. MCP 已分家为顶层 `src/mcp/`（mod.rs + oauth.rs），调用点全部 `crate::mcp`，与 plugin 的唯一过境点是 `ToolBrief`
7. 三套生命周期（内置 / 用户验签 / 模型草稿）保留——属安全设计，合理

有意保留（已记入施工单 031 第四节，**不要再报为问题**）：
- 前端内部变量仍为 ext 词汇（extCurrent / extOpen / rpTab 值 `ext:xxx`）——含 localStorage 持久键，改名会破坏用户已存状态
- `ui.rs` 类型名 `UiExt`——纯内部类型名，无序列化影响
- 路由文件 `src/server/ext.rs` 已在第二轮改名为 `plugins.rs`（mod.rs 引用已同步）

配套产物：施工单 `docs/workorders/031-插件分家（MCP独立国-目录二合一-API改名）.md` 已建立，质量合格；`Cargo.toml` 版本 0.30.0 与施工单声明一致。

## 三、文档整改：6/6 验收通过（2026-09-29，两轮）

原 6 项待办已全部完成并通过两轮实测验收，完整记录见 `docs/文档整改验收报告-2026-09-29.md`。摘要：

| # | 验收项 | 判定 | 实测证据 |
|---|--------|------|----------|
| 1 | ROADMAP「补丁五」重复 + 损坏副本 | ✅ | 「补丁五」仅存第 47 行完整版（标识符完好）；第 49 行现为「 补丁四」，损坏副本已删；第 157 行为正常交叉引用 |
| 2 | 断行 + 转义引号残留 | ✅（带附注）| 已并单行（第 50 行）；字节级确认字面 `\"` 残留 = 0。**附注**：行 50 残留 1 个 0x1C 控制字符（`` `*` `` 与「通配兜底」之间），建议清理 |
| 3 | 表尾补当前版本 / 0.30.0 条目 | ✅ | 第 64 行条目四要素齐全（MCP 独立国 / 目录二合一 / 改名带迁移 / 0.30.0 + 施工单 031）|
| 4 | README 进度停在上一轮 | ✅ | 第 4 行已更新为「上一轮 完成」。附注：README 全文无「0.30.0」字样（仅 ROADMAP 第 64 行有），如需双表述可补 |
| 5 | README 目录结构按现状重写 | ✅（首轮部分通过后返修，复验通过） | 第 33 行 server/ 15 子模块与实物一致；首轮 5 处不符（plugin/ 仍列 mcp.rs、缺 src/mcp/、usage/ 与 plugins/ 重复、打包清单 plugins/ 重复）已在用户返修后全部消除 |
| 6 | App.svelte 8918 行表述对齐 | ✅ | 第 4 行已改「拆分未落地——仍 8919 行单文件」；实测 8918 个换行符 + 末尾无换行 = 8919 行，数字精确一致 |

## 四、剩余问题清单（已诊断、未整改，按优先级）

以下数据均为 **2026-09-29 实测**（验收方式：文件清单、行号、内容抽查、字节级校验）。原始诊断见 `docs/agent-项目优化报告.md`，与本节一致。

### 配置层未收敛

- `src/config/settings.rs` 共 **953 行**，仍挂 **3 个 serde alias 垫片**：
  - 第 276 行 `#[serde(default, alias = "active_profile_id")]`（`active_config_id` 读旧字段）
  - 第 280 行 `#[serde(default, alias = "profiles")]`（`configs` 读旧字段）
  - 第 305 行 `#[serde(default, alias = "disabled_extensions", skip_serializing_if = "Vec::is_empty")]`（`disabled_plugins` 读旧写新，加的）
  - 迁移测试在约第 936–943 行
- **运行时配置散在 5 处**：仓库内有 `config.toml`、`settings.json`、`providers.json`；`mcp_tokens.json` 与 `plugin_data/` 为运行时生成，不在仓库（文档"5 个文件"指这 5 处配置面）

### 巨石文件

按行数降序（2026-09-29 实测，与诊断一致）：

| 文件 | 行数 | 参照拆分范式 |
|------|------|--------------|
| `web/src/App.svelte` | 8918（8918 换行符 + 末尾无换行 = 8919 行）| server.rs 拆分 |
| `src/browser/mod.rs` | 3636 | 同上 |
| `src/tools/mod.rs` | 1808 | 同上 |
| `src/gateway/mod.rs` | 1440 | 同上 |
| `src/server/chat.rs` | 1288 | 同上 |
| `src/core/agent.rs` | 1210 | 同上 |
| `src/config/settings.rs` | 953 | 同上 |

项目里已有成熟拆分范式（server/ 的拆分，现 15 个子模块），可参照执行。

### 小项

1. **evals 两处分离**：任务定义在 `evals/`（实测 **12 个任务 json** + `mcp-http-server.mjs`；此前交接文档写"11 个"已过时），运行器却在 `src/bin/eval/`（`main.rs` + `eval_core.rs`）
2. **`src/` 裸文件与模块目录混杂**：`src/` 下裸文件为 `fsx.rs / hooks.rs / skills.rs / storage.rs / service.rs`（另有 `main.rs` 属正常入口），与 20 个模块目录混杂，无统一约定

## 五、工作约定（沿用此前会话的默契）

- 用户方负责动手改，AI 负责诊断和验收——一个一个来，改完一项验收一项
- 验收只对照实测证据（文件清单、行数、内容抽查），不接受"改了"的口头声明
- 编译/测试无法在 AI 侧沙箱验证（无 MSVC 工具链），涉及构建的结论需用户本地跑 `cargo build && cargo test` 确认
- ROADMAP.md 曾出现过读取内容互相矛盾的损伤史（见施工单 031 说明），编辑该文件务必整行操作、勿断行、勿动其他行

## 六、验收与施工产物索引（2026-09-29 时点）

- `docs/文档整改验收报告-2026-09-29.md` —— 六项验收完整记录（含复验记录节）
- `docs/workorders/031-插件分家（MCP独立国-目录二合一-API改名）.md` —— 施工单（最新）
- `docs/agent-项目优化报告.md` —— // 原始诊断
- `docs/ROADMAP.md` —— 进度大表（当前登记至 0.30.0，第 64 行）

## 七、下一步建议

按优先级可推进配置层收敛（清 3 个 serde alias 垫片 + 统一配置面）或巨石文件拆分（参照既有范式）。动手前先立施工单（下一编号 **032**），施工单中写明验收标准，改完一项验收一项。
