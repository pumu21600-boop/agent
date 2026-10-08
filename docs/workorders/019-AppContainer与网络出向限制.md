# 施工单 019：AppContainer 沙箱模式 + 网络出向限制

> 2026-09-04 起草并完成（用户授权连续推进）。受限 token 沙箱的升级路径。

## 一、实现

**A. AppContainer 模式**（src/sandbox/appcontainer.rs 原语 + proc.rs 启动分支）
- config.toml `[sandbox] mode = "restricted"（默认）| "appcontainer"`；`allow_network`（默认 false）。
- LowBox token 由 CreateProcess 的 STARTUPINFOEX + SECURITY_CAPABILITIES 属性创建
  （公开 API，无需 NtCreateLowBoxToken）。
- profile：CreateAppContainerProfile 幂等重建；能力 = allow_network ? [INTERNET_CLIENT S-1-15-3-3] : NULL。
- 工作区目录 + 用户 TEMP 目录自动 icacls 授 AppContainer SID 的 (OI)(CI)F ACE。
- Job 保险丝照用（超时整树击杀）；白名单环境照用。

**B. 真机踩坑记录（三条，全部已修）**
1. `DeriveAppContainerSidFromAppContainerName` 是纯数学运算，profile 不存在也返回成功
   ——CreateProcess 报错误 2。对策：ensure_sid 总是重建 profile（幂等删除+创建）。
2. profile 名含「.」→ E_INVALIDARG（0x80070057）。只能字母数字-_（agent_local_sandbox）。
3. capabilities **空数组（count=0 + 非 NULL 指针）→ E_INVALIDARG**。无网络能力时必须传
   NULL 数组。另：RtlConvertSidToStringSidW 的链接库不可靠——SID 转字符串手写（SID 内存
   布局解析，零 FFI）。

## 二、真机验证

- 探针测试 `sandbox::probes::tests::appcontainer_smoke`（#[ignore] 手工跑）：
  AppContainer（断网/联网两分支）下 cmd /c echo 成功回显 ✓（exit=0）。
- config 默认切回 restricted；启用方式：mode = "appcontainer"。
- 已知限制（留口）：AppContainer 下 node/git/python 等工具链兼容性未逐一验证
  （cmd/echo 已验）；profile 删除重建会清 AppContainer 临时数据（无持久依赖可接受）。

## 三、不在本单

组件总线 UI 插槽（018 已设计留口）、MCP HTTP/SSE transport、resources/prompts。
