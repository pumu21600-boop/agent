# 031 ·  插件分家（MCP 独立国 / ui-ext 并入 plugins / API 改名）

> **事后补记（2026-09-29）**：本单无排期来源——用户直派「不用建施工单，直接开始做」（会话原话），施工在先、本单事后补记归档。
> 状态：已完成（本地提交 `bbf0b8a` + 本单收尾提交；**未推送**——非整阶段施工单只本地提交）。
> 版本：0.30.0；测试 255 过 / clippy 零告警 / 前端构建过。

## 一、背景与用户指令

用户原话截图：「插件 skillmcp，乱成一锅粥了，明明是独立的东西」+「不用建施工单，直接开始做」。施工单 027 的「二合一」只收了逻辑层（contract.rs 已是二合一契约、`UiExt` 已是统一形状、`/api/ext` 已合清单），**皮没跟上**：目录还叫 `ui-ext/`、接口还叫 `/api/ext`、设置字段还叫 `disabled_extensions`、信任键还分 `ui-ext:` / `plugins:` 两套、MCP 的 `mcp*.rs` 物理上住在 `src/plugin/` 里。

## 二、交付（实际落地）

### ① MCP 搬独立国
- `src/plugin/{mcp.rs,mcp_oauth.rs}` → `git mv` 成 `src/mcp/{mod.rs,oauth.rs}`（保历史）。
- 全仓 20 处 `crate::plugin::mcp*` 引用改 `crate::mcp::*`（main.rs / core/rules.rs / server/{ext,workspace,settings}.rs / tools/mod.rs 五面）。
- 唯一过境点 = 共享工具总线类型 `ToolBrief`（仍住 `crate::plugin`，mcp 侧 `use` 引入）。
- MCP 自己的配置（settings `mcp_servers`）、OAuth（oauth 子模块 + `mcp_tokens.json`）、`mcp__` 工具前缀语义**零改动**；`src/mcp/mod.rs` 头注明独立国边界。

### ② 目录二合一
- 根 `ui-ext/` → `git mv plugins/`（board/trace/usage 三面板搬家，git status R 实锤 7 文件）。
- 两个扫描器互容忍半边缺席：`wasm::load_dir_plugin` 0 个 .wasm/.wat 不再报「目录里没有工具文件」；`ui::load_one` 无 entry/ui 声明不再报「入口不是文件」（entry 留空、前端按空隐藏「打开」——守卫已有 `e.entry` 判断）。
- 新增 `ui::scan_all`：面板半边 + 工具半边合成一份清单（同名一条，工具并入 tools、错误取非空半边），六处响应全走它（`serve_plugin` 静态半边仍用 `ui::scan`——只有面板半边需要文件服务）。

### ③ 转正一次写盘
- `draft::promote` 旧「面板半边落 ui-ext/、工具半边落 plugins/ 各写一份 manifest」收成 `plugins/<name>/` 单目录单次写盘（manifest + 全部 files 一轮落盘，tool_file 才 rescan）；死代码 `write_all` 删除。

### ④ 改名带迁移
- settings 字段 `disabled_extensions` → `disabled_plugins`（serde alias 读旧写新）。
- 信任键统一 `plugins:<name>`：`trust::resolve` 双 kind 分支收拢单 kind（目录形态工作区优先 → 应用根 → 裸 .wat/.wasm 兜底）；`pin_from_manifest_paths` 单一 kind；`settings::load` 读入时迁移旧键（目标键已存在不覆盖），迁移抽纯函数 `migrate_legacy_trust_keys`。
- `/api/ext` → `/api/plugins` 六路由 + handler 改名（list_plugins / toggle_plugin / plugin_entry / plugin_asset / serve_plugin）+ JSON 响应键 `extensions`→`plugins` 六处 + 事件 kind `plugin.toggle`；静态 `/ui-ext/*` → `/plugins/*`。
- 前端最小可断言改动：fetch `'/api/ext'`→`'/api/plugins'` 六处、`r.extensions`→`r.plugins` 九 token；**内部标识符不动**（两次读取清单不一致，盲改风险 > 收益）。
- 收尾改名：`src/server/ext.rs` → `src/server/plugins.rs`（本单补记时做，`mod plugins;` + `use plugins::*;` 两行，cargo check 零告警）。

## 三、验证

- `cargo test`：254→255 全过（新增 `backward_compat_plugins_rename_migration`：alias 读入 + `ui-ext:` 键迁移 + 已有新键不覆盖 + 落盘只写新名，四断言）；clippy 零告警；前端 vite 构建过。
- 真机冒烟（隔离临时目录 + 7799 端口，**不碰**用户在跑的 2688 实例）：`/api/plugins` 200 返回合并清单（board ok=true origin=built_in）、`/plugins/board/index.html` 200、`/api/mcp` 200、toggle 走新键、旧 settings.json 落盘实锤迁移（`ui-ext:board`→`plugins:board`）。
- **环境坑（教训入库）**：`cargo test` 只刷测试靶不刷普通 bin——首次冒烟用旧 `agent.exe` 全 404，`cargo build` 后重跑才过。**冒烟前必须 `cargo build`。**

## 四、遗留（有意不动的尾巴）

- 前端内部标识符仍是 ext 词汇（`extCurrent` / `extOpen`、页签键 `'ext:'` / rpTab `'ext:xxx'`）——含 localStorage 持久键，改名需带迁移逻辑，评审建议不动，采纳。
- `ui.rs` 类型名 `UiExt` 未改（纯内部类型名，无序列化影响，低价值）。
- ROADMAP.md 对应条目已补录（同日收尾）：评审证实文件有损伤（「补丁五」残副本、README 漂移三处）后修复——删 49 行残副本、补 /// 四行（日期取 git 真数据）、README 进度行接当前版本、目录树 server.rs→server/（15 子模块）、「App.svelte 拆分」不实声明改为如实（仍 8919 行单文件）。
