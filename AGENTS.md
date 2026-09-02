# AGENTS.md —— 项目工作记忆

> agent 每次开工自动读本文件（全局层 `~/.agent/AGENTS.md` 同读，项目层优先级更高）。
> 这是给 agent 看的标准答案：约定、命令、坑写在这里，少一次试错。

## 构建与测试

- 后端构建：`cargo build`（日常）/ `cargo build --release`（start.bat 依赖的产物）
- 测试：`cargo test`（沙箱探针会真实拉起受限进程，全套约 5 秒）
- 前端：`cd web && npm run build`（产物 web/dist 由 axum 托管，不入库）
- 重启服务：`taskkill //F //IM agent.exe` 后再启动 `target/debug/agent.exe`（exe 被占用时构建会失败）

## 约定

- 分支：`dev` 主线；每阶段 `p<N>-<短名>` 分支，验收合回 dev 打 tag `v0.<N>-p<N>`
- 提交：小步提交，中文 message，前缀标阶段（如 `P7: ...`）
- 排产与协作：见 `docs/ROADMAP.md`（施工单在 `docs/workorders/`）
- JSONL 事件类型定死 6 类，新增状态一律走 `state.kind`（schema 见 `docs/jsonl-schema.md`）

## 红线（永不违反）

- `settings.json`（DPAPI 加密 key）、`logs/`、`.agent/`、`HANDOFF.md` 永不入库
- API key 永不写入任何日志/文件/控制台
- 工具内禁止 `git push`；远端推送只能用户手动执行
- 影子仓库回滚只逐文件回退 agent 改过的内容，绝不整树 reset、绝不碰真仓库 HEAD

## 已知坑

全部记录在 `docs/ROADMAP.md` 第四节「已知坑（新对话必读）」——开工前先读一遍。
最要命的三个：本机 GNU 工具链损坏（钉死 MSVC）；Git Bash 发中文 curl 会 GBK 乱码
（写 UTF-8 文件后 `--data-binary @file`）；受限 token 必须带 logon SID。
