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
| P6 恢复与续命 | git shadow repo checkpoint、会话落盘+恢复、HANDOFF 交接 | ✅ 已完成（tag v0.7-p6；P6.1 多会话+改动条 v0.7.1-p6） |
| P7 记忆 v1 + AGENTS.md | events.log + 滚动摘要、双层存储（~/.agent + 项目 .agent）、index.json | ✅ 已完成（tag v0.8-p7） |
| P8 网关补全 | 角色映射（SUMMARY/TITLE/EMBEDDING）、采样参数、限流、降级链；补齐官方网关（用户拍板：**移除第三方中转/聚合**——商汤/百炼/千帆/星火/硅基流动/Groq/OpenRouter，只留大模型厂商官方 + 本地推理；补 MiniMax/Claude/豆包/混元）；**思考参数按厂商适配**（providers.json 数据字段：reasoning_style=reasoning_content/thinking_param/effort_param/in_content（反代把 `<think>` 当正文吐，需剥离）/ thinking_budget_param / reasoning_counts_in_max_tokens——各家不同：GLM 实测思考占 max_tokens，OpenAI/DeepSeek/Claude 占；MiniMax/Claude/豆包/混元端点待真实 key 验证） | ✅ 已完成（tag v0.9-p8） |
| P8.4 网关对齐 OpenClaw | 用户拍板：**照抄 OpenClaw v2026.8.2 网关，覆盖我们自创的实现**。机制全量移植：`thinkingFormat` 词表（openai/deepseek/zai/qwen/qwen-chat-template）+ 端点归属探测（compat.rs，官方域名/local/default 三类，custom 中转认不出时用户覆盖）；reasoning_effort 档位解析器逐行移植（effort.rs：gpt-5.x 家族 supported 列表、降级链 xhigh→high、off→none 仅在模型支持时发、gpt-5.6 拒收 temperature）；响应端四字段思考提取（reasoning_content/reasoning/reasoning_text/reasoning_details，OpenRouter details 兼容）；14 种思考标签剥离（tags.rs，对齐上游 REASONING_TAG_NAMES，孤立闭合标签丢弃）；历史回放净化（replay.rs：deepseek/zai 契约保留 reasoning_content + kimi/deepseek-v4/mimo 白名单，其余剥离；gpt-5*/o 系 system→developer；qwen 端点空消息过滤）；max_completion_tokens 按 gpt-5*/o 系切换。**删自创项**（上游没有）：thinking_model 换模型（DeepSeek 改发 thinking 对象）、thinking_budget 直发、reasoning_counts_in_max_tokens、enable_thinking/thinking_budget_param 逐字段适配。providers.json 瘦身：仅豆包(zai)/混元(qwen) 带 thinking_format 兜底，其余交给端点探测。thinking_style 覆盖改新词表 + P8.3 旧值自动映射（thinking_object→zai、reasoning_effort→openai、enable_thinking→qwen）。商汤真实冒烟：on→`thinking:{type:enabled}` 思考分片正常，off→disabled 0 分片，多轮回放渠道照收。57 测试全绿 | ✅ 已完成 |
| P8.5 模型配置重构 | 施工单 011：三层解耦（渠道/模型/角色）+ 消除镜像冗余确立单一真理源 + 8大官方规范全覆盖 + 模型能力标记（工具/视觉/视频/音频）与角色专职分工路由 + 清除Key。67 测试全绿，前端完整构建（tag v0.9.5-p8.5）。**计划外交付**（tag 后）：多模态媒体转接全链路（f65e08d，对齐 DSH media-understanding 规范，详见 docs/HANDOFF_MEDIA_UNDERSTANDING.md）与审批风险分级引擎 v0（core/rules.rs，P9 内容提前落地）。**施工单 012 收尾**（2026-09-04）：恢复思考参数形态 UI 覆盖、能力标记所有权（用户标记>推导+重新推导按钮）、capabilities.tools 闭环、角色语义文案区分+fallbacks 接 UI、.attachments/ 自动进 git 排除；B1–B4 语义固化（见坑 14）。**侧边栏复刻 DSH**（41fa79c，无施工单用户直派）：项目=工作区树 + 会话置顶/归档/分叉/重命名 + 搜索/批量管理/按工作区或单列表视图 + 添加项目弹 Win 原生选文件夹 + 侧栏拖拽调宽（240–520）+ 收起按钮；交接文档按钮移至输入框上方；会话新增 pinned/archived/workspace 字段，73 测试 | ✅ 已完成（tag v0.9.6-p8.5） |
| P9 Plan 模式 + 审批规则引擎转正 | 施工单 013（2026-09-04）：**A 引擎转正**——A1 v0→v1（shell 串联命令拆分逐条判定取最高风险，堵 `cargo test && del /f x` 绕过；高危/安全名单按程序名 token 精确匹配，堵 `echo "del x"` 误伤与 `dirname` 误放；shell 内 git 复用子命令判定；write_file 敏感清单扩展 .env/密钥后缀/.agent 等；越界判定 Windows 大小写不敏感；未登记工具仍一律 Dangerous）；A2 审批判定单点化（删静态 danger_level 旧分支，唯一入口 `needs_approval_for`）；A3 会话内「同类允许」（`approval_scope`：shell 按程序名 / write_file 按目录 / git 按子命令，风险档参与 key；Dangerous 不参与会话记忆；`/api/approvals` 接 `remember`）+ A4 审批卡展示风险档位徽章（risk_label 三档配色）与判定依据 reason。**B Plan 模式**——B1 请求级 `plan_mode`（前端 Plan/Act 切换 pill，随会话持久化 localStorage.planMode_\<sid\>）；B2 `plan_block_reason` 单点拦截（Plan 档只放行 Safe 只读，Normal/Dangerous 不进审批弹窗直接驳回并回喂模型调整计划，plan.blocked JSONL；注：`find` 因 `-delete/-exec` 可写而保守判 Normal 属预期设计）；B3 最终回答解析 \`\`\`plan 围栏（目标/步骤/涉及文件，`parse_plan` 纯函数容错中英文冒号/全角字符边界/markdown 符号，plan.proposed JSONL + PlanProposal SSE）+ Plan 档 system prompt 增量（纯任务行为约束，不含身份/人设，坑 13）；B4 前端计划卡（目标/步骤/文件 + 批准按钮）——批准自动切回 Act 并下发「计划已批准」执行指令，拒绝继续对话调整。代码锚点：`core/rules.rs`（引擎+approval_scope）、`core/agent.rs`（PLAN_PROMPT/parse_plan/plan_block_reason trait）、`server.rs`（ChatGate）。**88 测试基线（73→88：rules v1 +11、parse_plan +4）**。真实冒烟（Plan 档发拆分测试计划任务）：9 个工具调用全为只读（dir/search/git status/read_file 正常放行 ✓），2 个含 `cd` 的串联命令被拦截、模型自动调整改用纯 `dir` 后继续 ✓（B2 拦截+回喂真机过）；最终回答成功产出结构化 plan_proposal（goal/steps/files 完整）✓（B3 全链路真机过）；全程零文件改动 ✓。备注：tokeness 渠道间歇 500 时可临时切商汤渠道复测（/api/configs apply，测毕切回）；批准后 Act 执行落地的端到端属普通 chat 请求（前端按钮=setPlanMode(false)+自动发消息），未真机跑以免弄脏工作区 | ✅ 已完成（tag v0.9.7-p9） |
| P10（第二个月） | 多开会话 + trace 时间线视图 + eval 体系。**全部完成**：多开会话 v1（P6.1 提前落地）+ 工作区切换（P8.2/P8.5 落地，014 盘点关项）+ trace 时间线（014：`GET /api/trace` + 侧栏「时间线」面板，任务切分/分片聚合/pair 耗时）+ **eval 体系（015）**：HTTP 黑盒驱动（`cargo run --bin eval`，主服务零改动）——临时工作区隔离 + 切工作区跑真实对话 + 断言引擎（file_exists/file_not_exists/file_contains/no_new_files[豁免服务运行时文件 .agent//AGENTS.md/.gitignore 等]/sse_event）+ 报告与 exit code；首批任务集 3 个（hello-file/fix-json/read-only-plan）。**真机 ALL PASS 9/9**（tokeness 间歇 500 时切商汤跑通，测毕切回）。测试基线：主 bin 90 + eval 3 + sandbox_hog 3（沙箱环境假阴性 3 个，干净终端全绿）；cargo test 多 target 默认 fail-fast，全量确认用 `--no-fail-fast`。代码锚点：observability/trace.rs、src/bin/eval.rs + eval_core.rs、evals/tasks/ | ✅ 已完成（tag v0.9.8-p10） |
| P10.5（计划外，用户直派） | 前端改版：**主题系统**——121 个硬编码色聚类变量化（8 级灰阶 --g0~g7 + 彩色 --tint-<hue>-<bg\|fg>），深/浅双主题 data-theme 切换 + localStorage 持久化，侧栏 ☀/🌙 快捷按钮 + 设置-外观主题卡片；**坑：Svelte 会丢弃组件样式里的 [data-theme] 选择器，须写 :global([data-theme])**。**设置面板重构为经典左侧导航**（6 分区：外观/模型配置/模型参数/角色分工/记忆/工作区，记住上次分区；工作区并入设置页，原 wsModal 保留双入口）。**记忆分区**（此前零 UI）：GET /api/memory（双层路径/events 行数/登记项目数/双层 AGENTS.md 原文）+ POST /api/memory/agents（保存即下次注入生效）；前端双层 AGENTS.md 编辑器 + 状态卡；真机闭环验证过（含修复 POST 路由误挂问题）。基线不变 | ✅ 已完成 | 
| P11 | MCP client + WASM 插件 + 双总线。**全部核心完成（016 + 017，2026-09-04）**：✅ MCP client stdio 最小闭环（016，详见 016 单与 tag v0.10.0-mcp）；✅ WASM 插件（017）——wasmtime 24 + 内置 wat 解析（零工具链依赖），单函数 ABI（alloc/describe/run，len==0 表示 0 终止串），**每次调用全新实例 = 纯计算沙箱**（零 host function，天然无文件/网络边界，trap 捕获不连坐），`wasm__<plugin>__<tool>` 桥接（本地可信目录默认 Normal，加载失败 Dangerous），plugins/hello.wat 冒烟真实模型全链路回喂 ✓；✅ 工具总线——tools::definitions 统一下发内置+mcp__+wasm__ 三源。**组件总线（UI 插槽）设计留口 018**：需要运行时动态组件加载方案（iframe 隔离 or 运行时编译），工程大风险高，工具总线 v0 已统一。MCP HTTP/SSE transport、resources/prompts 留口 | ✅ 核心完成（tag v0.11.0-p11） |
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
12. **影子仓库回滚绝不整树 reset**（P6.1 教训）：`reset --hard <旧基线>` 会把基线之后**用户自己**的提交/改动一并刷回旧快照（真实发生过：点旧检查点回滚把已提交的文档刷回 P5 状态）。restore 必须按 `diff --name-status 基线..HEAD` 逐文件回退 agent 实际改过的文件；改动统计条的基线也必须是**任务起点当场拍的快照**，不能拿影子 HEAD 凑数（会把历史漂移算进条里）。
13. **提示词保持最小 + AGENTS.md 归用户**（P7.1→P7.3 用户拍板）：system prompt 只留 `prompts/system.md`（"用中文简洁回答"）+ AGENTS.md，**不注入模型名/渠道/身份设定**——模型被问是谁会自造产品名（实测自称"khy/Kai-Code"），用户明确选择不修。**AGENTS.md 是用户给每个项目自填的设定文件，模板必须近乎空白**；我们做的是 agent 产品，不是"造这个产品"项目的自用工具——本仓库的开发知识（红线/坑/规范）只进 ROADMAP，绝不写进 AGENTS.md 或任何运行时注入物。
14. **配置语义四条定死**（施工单 012 B1–B4，前后端任何一方改动必须对照；代码锚点：settings.rs / server.rs SettingsUpdate、apply_thinking_style、find_channel）：
    - **B1 清除语义**：`0`=清除长度类（context_length/max_output_tokens）；负数=清除采样类（temperature/top_p）；空串/auto=清除思考类（thinking/thinking_style）；`clear_key=true`=清 key。P8.3「思考档位被吞」事故即源于此约定未成文。
    - **B2 key 回落链**：拉模型/发消息的 key 取值顺序 = 请求显式 key > 激活配置 DPAPI key > `DEEPSEEK_API_KEY` 环境变量；key 永不出日志、永不出 JSONL。
    - **B3 角色解析顺序**：`settings.roles.<role>` > `config.toml [roles]` > 回落主渠道；角色值按「配置名或配置 id」解析，解析失败回落主渠道并 Notice。
    - **B4 思考形态解析顺序**：`Config.thinking_style` 显式覆盖 > 端点探测（compat.rs：官方域名→native 供应商规范 / 本地→local / 其余→openai 兜底）> providers.json 预设 `thinking_format` 兜底。词表（none/openai/deepseek/zai/qwen/qwen-chat-template）**只增不改名**；新形态入词表必须附真实渠道冒烟记录。
    - 附：**能力标记所有权**（A2）——用户显式勾选 > 按模型名推导；推导值仅在配置无用户标记历史时作初值，重置走「按模型名重新推导」按钮。
