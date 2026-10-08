# 施工单 017：WASM 插件 + 组件总线（收官单）

> 2026-09-04 起草并完成（用户授权连续推进）。总架构 §7：WASM 保深度定制、
> 自带沙箱即边界、trap 不连坐；组件总线挂 Svelte 插槽。

## 一、范围与实现

**A. WASM 插件宿主**（src/plugin/wasm.rs，wasmtime 24 + 内置 wat 解析零工具链依赖）
- 单函数 ABI：插件导出 `memory` + `alloc(size)` + `describe() -> i64` +
  `run(action_ptr, action_len, payload_ptr, payload_len) -> i64`（高 32 位 ptr / 低 32 位 len；
  len==0 表示 0 终止字符串——WAT 手写友好）。
- describe 返回 {"tools":[{name,description,schema}]}；run 返回 {"text":"..."}（或 {"error"}）。
- 隔离模型：**每次调用全新 Store/Instance**，v0 零 host function = 纯计算沙箱
  （天然无文件/网络边界问题）；trap 一律捕获为 Err 不连坐；30s 超时。
- 信任模型：plugins/ 目录文件为用户主动放置（本地可信）→ 工具默认 Normal；
  加载失败/未知 → Dangerous（与 MCP 防线对齐）。

**B. 工具桥接**：`wasm__<plugin>__<tool>`（与 mcp__ 对齐）；definitions 动态追加、
execute 前缀分发、rules wasm__ 分支。

**C. 冒烟**：plugins/hello.wat（WAT 手写，data 段 0 终止字符串）——加载/发现 ✓、
真实模型调用 wasm__hello__hello 全链路回喂 ✓（切商汤渠道）。

**D. 组件总线（设计留口，未实现）**：v0 的工具总线已由 tools::definitions 统一下发
（内置 + mcp__ + wasm__ 三源）；UI 组件总线（插件提供 Svelte 组件/面板）需要运行时
动态加载方案（iframe 隔离或运行时编译），工程量大风险高，**留 018 拍板**；已在
plugin/mod.rs 与 ROADMAP 记录设计约束。

## 二、不在本单

前端动态组件加载（组件总线实现）、WASM host function（文件/网络能力授予）、
组件模型（component model）升级。

## 三、验收结果

- cargo test 基线不变（主 bin 90 + eval 3）；npm run build 通过。
- 真机：hello.wat 加载/发现 ✓；真实模型调用 wasm__hello__hello 全链路回喂 ✓。
- trap 不连坐由结构保证（每次全新实例 + trap 捕获为 Err）。
- 已知限制（留口记录）：每次调用重新实例化（性能损耗可接受，有状态插件不支持）；
  无 host function（文件/网络类插件做不了，属沙箱边界设计而非缺陷）。
