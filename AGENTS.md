# AGENTS.md —— 本仓库工作事实

> agent 每次任务开始时自动读本文件（全局层 `~/.agent/AGENTS.md` 同读，项目层优先）。
> 这里只记录在这个仓库里工作需要知道的事实。

## 构建与测试

- 后端：`cargo build`（日常）/ `cargo build --release`（start.bat 用这个产物）
- 测试：`cargo test`（沙箱探针会真实拉起受限进程，全套约 5 秒）
- 前端：`cd web && npm run build`（产物 web/dist 不入库）
- 重启服务：先 `taskkill //F //IM agent.exe`，再启动 `target/debug/agent.exe`
  （exe 被占用时 cargo build 会直接失败）

## 环境事实

- Rust 工具链钉死 MSVC（`rust-toolchain.toml`）：本机 GNU 的 dlltool 损坏，编不过 `windows-sys`
- Git Bash 里发中文 curl 会 GBK 乱码：先写 UTF-8 文件，再 `--data-binary @file`
- 沙箱受限 token 必须带 logon SID，否则 .NET 子进程起手 0xC0000142

## 红线

- `settings.json`（DPAPI 加密 key）、`logs/`、`.agent/`、`HANDOFF.md` 不入库
- API key 不写入任何日志、文件、控制台
- 工具内禁止 `git push`
- 影子仓库回滚只逐文件回退 agent 改过的内容，不整树 reset、不碰真仓库 HEAD

## 其它

开发流程、阶段排产、跨会话接力的已知坑全在 `docs/ROADMAP.md`——那是给开发会话看的，与本文件无关。
