# 排产路线图 + 协作约定（agent 长期项目）

> 依据：`C:\Users\大脚\Downloads\agent总架构.md`（v3）。纪律：**窄而刚**——每阶段只交付底线能力，其余留口。
> 本文件是跨对话的接力物：新对话开工前先读本文件 + git log 对齐。

## 一、阶段状态总览

| 阶段 | 交付底线 | 状态 |
|---|---|---|
| P0 地基 | JSONL 日志定死、网关单渠道冒烟、取消标志、供应商预设 + 设置持久化 | ✅ 已完成（tag v0.1-p0） |
| P1 结构 + UI 骨架 | 目录按总架构 17 节模块化；providers 数据化；key 按 provider 存；聊天经典布局 + 消息事件渲染骨架 | ✅ 已完成（tag v0.2-p1） |
| P2 核心循环 v1 | 任务解析→调工具→回填→判定→终止；步数/超时上限；thought/tool_call/tool_result/model_chunk 进日志和 UI；取消贯通；配 1 个安全工具（文件读） | ✅ 已完成（tag v0.3-p2） |
| P3 执行沙箱 v1 | Job Objects + 受限 token（壳先于 shell） | ✅ 已完成（tag v0.4-p3） |
| P4 工具第一批 + 审批三档 | shell（入沙箱）、文件写、ripgrep、web 抓取、git；注册表；suggest/auto-edit/full-auto | ✅ 已完成（tag v0.5-p4） |
| P5 上下文工程 | 工具结果截断/压缩/分页、token 预算 | ✅ 已完成（tag v0.6-p5） |
| P6 恢复与续命 | git shadow repo checkpoint、会话落盘+恢复、HANDOFF 交接 | ⏳ |
| P7 记忆 v1 + AGENTS.md | events.log + 滚动摘要、双层存储（~/.agent + 项目 .agent）、index.json | ⏳ |
| P8 网关补全 | 角色映射（SUMMARY/TITLE/EMBEDDING）、采样参数、限流、降级链；补 OpenRouter/Gemini/LMStudio/MNN/llama.cpp | ⏳ |
| P9 Plan 模式 + 审批规则引擎 | 留口转正式 | ⏳ |
| P10（第二个月） | 多开会话 + trace 时间线视图 + eval 体系 | ⏳ |
| P11 | MCP client + WASM 插件 + 双总线 | ⏳ |
| P12 | AppContainer + 网络出向限制；并行工具调用 + 写冲突检测 | ⏳ |
| P13 | Tauri 桌面封装 | ⏳ |

## 二、对话（施工单）协作约定

1. **一阶段一个对话**：用户发"施工单 NNN"开局，单内含目标/验收标准/禁止事项；本对话只做该单，超出边界不碰。
2. **新对话开场动作**：读本文件 + `git log --oneline` 对齐进度；不重新发明既有约定（JSONL 6 类事件 schema、settings.json 结构、DPAPI 存 key 均已定死）。
3. **阶段收尾动作**：更新本表状态 → 提交 → 打 tag →（用户提供远端后）推送。

## 三、Git 约定

- 分支：`dev` 为主线；每阶段开 `p<N>-<短名>` 分支，验收后合回 `dev` 并打 tag（`v0.<N>-p<N>`）。`master` 保持干净（或后续由用户定发布策略）。
- 提交：小步提交，一条功能点一条；中文 message，前缀标阶段（如 `P1: ...`）。
- 推送：每个阶段验收通过、合并完成后推一次；远端地址由用户提供（建议私有仓库）。
- 永不入库：`settings.json`（含 DPAPI 加密 key）、`logs/`、`target/`、`frontend/dist/`、`frontend/node_modules/`（已 gitignore）。

## 四、已知坑（新对话必读）

1. **本机 GNU 工具链损坏**：dlltool CreateProcess 失败，项目已用 `rust-toolchain.toml` 钉死 MSVC（本机有 VS 2022 Community），不要改回 gnu。
2. **Git Bash 发中文 curl**：`-d '{"message":"你好"}'` 会 GBK 乱码导致后端 JSON 解析失败，必须写 UTF-8 文件后 `--data-binary @file`。
3. **改代码后重启服务**：`taskkill //F //IM agent.exe` 再 `cargo run`，否则 exe 被占用。
4. **商汤中转 401 code 16 Forbidden** = key 无效；key 走 DPAPI，页面显示尾 4 位（key_hint）供自查，完整 key 永不出内存/日志。
5. **换模型立即生效**：/api/chat 已支持请求级 model/base_url 覆盖，前端发送时携带当前下拉框值。
6. **key 按配置独占（2026-09-02 起）**：v2.4 起 key_cipher 存进每套配置，同地址不同配置各有各的 key。此前按 base_url 共享导致「新建配置显示别的配置 key 尾 4 位、输新 key 会覆盖别的配置」。旧 settings.json 的 keys 映射在 load 时自动迁移（仅作迁移源，运行时不再读写）。
7. **本机直跑（local 档）红线**：脱沙箱=完整用户 token + 继承父环境（含 API key），仅当 `config.toml [agent] allow_unsandboxed = true` 才生效，未开时自动回退 full-auto 并 SSE 提示；无论哪档都保留超时整树击杀。审计看 JSONL `sandbox.spawn` 的 `restricted` 字段。
8. **受限 token 必须带 logon SID**：窗口站/桌面的 ACL 按登录会话 SID 授权，受限 SID 白名单缺了它，powershell 等 .NET 进程起手 0xC0000142 DLL 初始化失败。
9. **SeChangeNotifyPrivilege 剥不掉**：`CreateRestrictedToken(DISABLE_MAX_PRIVILEGE)` 只把特权置禁用不删名字，且系统固定保留 SeChangeNotifyPrivilege（遍历绕过，无害）；探针按名字白名单断言，别用 whoami 输出数个数。
10. **沙箱内存上限探针别用 powershell**：受限 token 下 .NET 启动即 0xC0000142；且 PS 里 `New-Object byte[]` 分配失败会当非终止错误吞掉继续跑，结论不可信。用原生 hog（src/bin/sandbox_hog.rs）做差分。
11. **Git Bash PATH 污染对照组**：测试进程 PATH 里的 Git usr/bin 会让 `cmd /C whoami` 解析到 GNU whoami；沙箱内没事（环境是白名单重建的），不经沙箱的对照组要写绝对路径。
