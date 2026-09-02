# 施工单 009：P7 记忆 v1 + AGENTS.md

## 项目背景（自行对齐）
- 依据 `docs/ROADMAP.md`（P7）+ 总架构 §9/§10/§16：events.log + 滚动摘要、
  双层存储（~/.agent + 项目 .agent）、index.json、AGENTS.md。
- 现状（P6.1 完成 tag v0.7.1-p6）：会话已落盘可恢复、HANDOFF 机械摘录、影子仓库检查点。
  缺口：跨会话的"经验"不沉淀（每次从零开始）；P5 裁剪把旧对话直接折成占位符丢信息；
  项目约定（构建命令/坑）没有标准载体。

## 任务 A：双层存储 + index.json（src/memory/）
1. 全局层 `~/.agent/`（HOME 环境变量）；项目层 `<workspace>/.agent/memory/`（与 P6 影子仓库同根）。
2. 项目身份 key：git remote origin url 优先，无 remote 用 git 根目录路径（小写归一）。
3. `~/.agent/index.json`：`{projects:[{key,name,path,remote,last_used}]}`，打开项目登记/更新；
   读写记忆一律先经 index 定位，防串项目。
4. config `[memory]` 节：`enabled`（默认 true，总开关）。

## 任务 B：events.log 事件流水（src/memory/）
1. 项目层 `.agent/memory/events.log`（JSONL 追加，独立于 logs/agent.jsonl——那个是进程日志不入库，
   这个跟 git 仓库走是"经验"）。
2. 记录节点：task.start（用户请求摘要）、task.finish（步数/工具数/结果字数）、
   tool.call（name/danger/ok）、handoff.written、checkpoint.created。
3. 敏感脱敏：`[memory] persist_sensitive=false`（默认）时，落盘前把
   `sk-…`、`api_key`、`Bearer …` 等模式打码；true 才存原文。
4. 写入走核心循环/server 既有挂钩点，不双写不新增 event_type。

## 任务 C：滚动摘要（context/ + gateway/ + session/）
1. 触发：P5 折叠发生时（folded_messages>0）——compact_messages 需把被折叠的原始消息带出来。
2. 生成：折叠内容 + 旧摘要 → 调当前主模型压成 ≤300 字中文要点（非流式小调用，
   max_tokens 上限）；失败降级保留 P5 占位符，绝不阻断循环。
3. 注入：摘要作为 system 角色消息紧跟 msgs[0]（不进前端历史，跨任务由后端管）。
4. 滚动：摘要存进会话文件（SessionFile 加 summary 字段），下一任务带着旧摘要继续压。
5. 角色映射（SUMMARY 专用模型）留 P8，v1 用主模型。

## 任务 D：AGENTS.md（src/config/ 或 memory/）
1. 双层读取：`~/.agent/AGENTS.md`（全局）+ `<workspace>/AGENTS.md`（项目）；
   都存在则都注入（全局在前、项目在后=优先级更高），只注入一次进 system prompt。
2. 开箱即用：项目根无 AGENTS.md 时首次启动生成**空白模板**（仅标题+自填提示注释）。
   定位纠正（P7.3）：AGENTS.md 是用户给每个项目自填设定的文件，产品不预置任何内容；
   我们仓库的开发知识只进 ROADMAP，绝不写进 AGENTS.md。
3. v1 只读注入；agent 自动沉淀经验进 AGENTS.md 留口（P7.1 或 P8）。

## 验收标准（全过才算完）
1. 首次启动：~/.agent/index.json 登记本项目；AGENTS.md 模板生成并被注入（问模型"项目约定"可答出）
2. 跑一轮带工具任务 → events.log 出现 task.start/tool.call/task.finish；
   含 sk- 字样的参数落盘前已打码（persist_sensitive=false）
3. 构造超预算历史触发折叠 → 摘要消息注入（非占位符）、会话文件带 summary、
   下一任务折叠时旧摘要被合并；模型调用失败时降级为占位符不崩
4. 删掉 ~/.agent 与 .agent/memory 后重启 → 自动重建（静默原则，开箱即用）
5. cargo test 全绿（index 登记/防串、脱敏正则、折叠消息带出、摘要注入降级均有单测）

## 禁止事项
- 不做向量嵌入/语义检索（EMBEDDING 留 P8，检索需求真实出现再补）
- 不做记忆自动蒸馏进 AGENTS.md（留口）；不做跨项目记忆共享
- 不改 6 类 event_type；不动审批三档与沙箱；滚动摘要失败绝不阻断核心循环

## 交付物
- src/memory/（双层存储+index+events.log+脱敏+AGENTS.md 读取）、context 折叠带出、
  gateway 非流式摘要调用、session.summary、核心循环注入、config [memory]、单测
- 文档更新 + 合入 dev + tag v0.8-p7
