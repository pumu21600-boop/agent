# 接口一览

agent 服务的全部 HTTP / SSE 端点。服务只接受本机来源的请求（`Host` 必须是
`localhost` / `127.x` / `[::1]`；带 `Origin` 时必须同源同端口）——绑定 127.0.0.1 挡不住
DNS rebinding，这层校验补的就是它。不带 `Origin` 的命令行客户端（curl、eval 驱动器、
服务自管理）照常可用。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/` | Svelte 页面（未构建时内置页） |
| POST | `/api/chat` | 对话主入口，SSE 流式（content / reasoning / tool_call / tool_result / approval_request / plan_proposal / notice / **reset** / error / done；`reset` = 降级链换渠道重试，前端按给的长度丢掉本步已推的半截内容） |
| POST | `/cancel?session=<id>` | 停止指定会话当前这一轮（不带 `session` = 停全部在跑任务） |
| GET | `/api/cancel?session=<id>` | 查询取消状态 |
| POST | `/api/headless/run` | 无头会话：程序化唤醒一条会话跑完整轮，返回结构化结果（session_id/status/answer/tool_calls/用量/notices）；`status` = done·cancelled·timeout·error。无人值守时命中审批的动作按 `on_approval` 处理（缺省 deny 拒绝并回喂模型，`approve` 才放行）；会话在跑返回 409。CLI 等价入口：`agent.exe run <消息> [--session <id>] [--model <m>] [--mode <档位>] [--timeout <秒>] [--approve] [--json]` |
| GET / POST / DELETE | `/api/schedules` | 定时任务：清单（含下次运行时间/cron 人话/最近完成通知）/ 新建改（id 空=新建）/ 删除 |
| POST | `/api/schedules/run?id=` | 立即运行一次（不打乱排期；结果进完成记录） |
| GET | `/api/health` | 健康检查 |
| POST | `/api/approvals` | 审批决定（允许/拒绝 + 本会话同类允许） |
| POST | `/api/approval-mode` | 切换审批档（对进行中的请求即时生效） |
| POST | `/api/shell` | 终端面板：沙箱内跑一条用户命令 |
| GET | `/api/settings` / PUT | 设置读取 / 保存 |
| GET | `/api/providers` | 供应商预设列表 |
| POST | `/api/models` | 按当前渠道拉取模型清单 |
| POST | `/api/configs` | 配置管理：new / delete / apply（切换激活） |
| GET | `/api/sessions` | 会话列表（新→旧，含项目名/置顶/归档） |
| GET / DELETE | `/api/session?id=` | 恢复 / 删除单条会话 |
| POST | `/api/sessions/new` | 新建空会话并立即落盘 |
| POST | `/api/sessions/open` | 打开会话（工作区不同时先切换） |
| POST | `/api/sessions/meta` | 置顶 / 归档 / 重命名 |
| POST | `/api/sessions/fork` | 分叉会话（复制消息/摘要/工作区） |
| POST | `/api/sessions/batch` | 批量归档 / 取消归档 / 置顶 |
| GET | `/api/changes?baseline=` | 本任务相对基线的改动文件清单（产物标签） |
| GET | `/api/checkpoints` | 影子仓库检查点列表（≤20） |
| POST | `/api/checkpoints/restore` | 撤销：逐文件回退检查点之后 agent 改过的文件 |
| GET | `/api/git/status` | 工作区 git 概览（分支/头部/脏文件/领先落后） |
| GET | `/api/git/info` | git 运行时状态（可用性/版本/来源；设置→宿主能力 卡片用）|
| POST | `/api/git/commit` | 用户手动提交（按选中路径 stage + commit） |
| GET | `/api/trace?limit=N` | 最近 N 个任务的事件流（时间线数据源） |
| GET | `/api/workspace` | 当前工作区 + 最近项目列表 |
| POST | `/api/workspace` | 切换工作区 |
| GET | `/api/workspace/files` | 工作区文件相对路径列表（「@ 添加上下文」数据源） |
| GET | `/api/workspace/file?path=` | 读工作区文本文件（预览，沿用 read_file 越界防护） |
| POST | `/api/workspace/picker` | 弹 Win 原生选文件夹窗口 |
| POST | `/api/workspace/remove` | 从项目列表移除（不删目录） |
| POST | `/api/workspace/alias` | 项目重命名（别名，空名=清除） |
| GET | `/api/memory` | 记忆层状态 + 三类约定文件原文（system/global/project） |
| POST | `/api/memory/agents` | 保存约定文件（system 保存即生效） |
| GET | `/api/memory/library` | 记忆库光点数据（每次调用做有界增量同步） |
| POST | `/api/memory/library/reindex` | 强制重建向量 |
| POST | `/api/memory/search` | 检索（`query` 或 `like=<会话id>`） |
| GET / POST | `/api/mcp` | MCP server（stdio / http / sse 三 transport）+ 工具插件状态 / 配置管理与生命周期动作 |
| GET / POST | `/api/skills` | 技能列表 / 保存·删除·读取 |
| GET | `/api/plugins` | 插件统一清单（面板 + 工具 + draft，一份列表）|
| POST | `/api/plugins/create` | 新建插件脚手架（工作区 plugins/，落盘即加载）|
| POST | `/api/plugins/remove` | 删除插件（只删 user 来源，内置拒删；清停用名单残留）|
| POST | `/api/plugins/toggle` | 启用/停用插件（停用 = 拒新调用 + 等完）|
| POST | `/api/plugins/draft` / DELETE | 模型自产插件提交 draft 隔离区 / 丢弃 |
| POST | `/api/plugins/promote` | draft 一键转正（落盘持久化、来源改 user）|
| GET / PUT / DELETE | `/api/plugin/data` | 插件命名空间数据读写删（存储一门；键名白名单 ≤64 字符，值 ≤64KB）|
| GET | `/plugins/{name}[/{path}]` | 插件面板静态资源（入口页由 manifest 指定；路径穿越一律 404） |
| GET | `/api/version` | 版本与运行形态（`root` = 当前生效的应用根） |
| GET | `/api/sandbox/selftest` | 沙箱探针套件（真实拉起受限进程） |
| GET | `/api/sandbox/info` | 当前沙箱档位（配置档/生效档/回退原因/探测链明细；`?refresh=1` 强制重探） |
| GET/POST/DELETE | `/api/browser/*` | 网页面板后端：state（标签清单）/ new / navigate / screenshot（PNG）/ click / type / key / scroll / back / forward / reload / viewport（视口跟随面板）/ overlay（「⋯」菜单开合，壳模式开时临时藏 child webview）/ tab（关标签）——与 agent 浏览器工具共享实例 |
