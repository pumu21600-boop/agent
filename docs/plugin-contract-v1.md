# 插件契约 v1（施工单 027 / ）

> 定案日期：2026-09-19（对话拍板，执行不得推翻的部分已写进施工单 027「设计定案」）。
> 配套代码：`src/plugin/contract.rs`（契约校验）、`src/plugin/ui.rs`（面板插件）、
> `src/plugin/wasm.rs`（工具引擎）、`src/plugin/draft.rs`（draft 隔离区）、`src/storage.rs`（存储一门）、
> `src/hooks.rs`（钩子四挂点）。

## 一、模型定案：二合一

**扩展与 WASM 插件统一为「插件」**：一个插件 = 一个目录 + 一份 manifest.json，
清单里只声明「有什么」（面板 / 工具 / 权限），**不贴类型标签**——一个插件可以同时有面板和工具。
（原报告的「界面型 / 能力型」分支作废；实据：board 名义界面型，实有 workspace.write /
composer.insert / notify 三项能力，分类名不副实。）

对外只留四个词：**Skill / MCP / 插件 / 沙箱**。

- Skill：知识注入，进提示词，不可执行；
- MCP：外部工具通道，公开标准协议，**不并入插件**（兼容责任在外）；
- 插件：宿主扩展，本地目录 + 清单，兼容责任归我们；
- 沙箱：执行隔离，不是扩展机制。

WASM 不再是一种「插件」，降为**插件工具的可选实现 / 沙箱轻量档**（代码即 wasm 引擎）。

## 二、manifest schema v1

```json
{
  "apiVersion": 1,
  "source": "user",
  "title": "项目文件",
  "description": "…",
  "version": "0.3.0",
  "entry": "index.html",
  "tool_file": "calc.wat",
  "ui": [{ "slot": "toolbar", "id": "board", "title": "项目文件" }],
  "permissions": ["composer.insert", "notify", "bus.publish", "workspace.write", "storage"]
}
```

| 字段 | 必填 | 语义 |
| --- | --- | --- |
| apiVersion | 否 | 缺失按 v1 读入（老目录自动迁移）；写了但不等于 1 → **拒载并提示升级**，不做 shim |
| source | 否 | 来源三值：`built_in`（随包）/ `user`（用户安装）/ `model_generated`（模型自产，走 draft）；**未声明按位置默认：只有应用根 `plugins/` 默认 built_in，其余位置（工作区 plugins、自装位）一律默认 user**（2026-10-08 补洞：豁免级来源只在位置配得上时被接受——`built_in` 仅 app 位、`model_generated` 仅 draft 隔离区）。来源现在决定三件事：删除保护（内置拒删）、draft 隔离区归属、界面来源标注 |
| title / description / version | 否 | 展示元数据；目录形态工具插件未写时用目录名 |
| entry | 否 | 面板入口页（相对插件目录，默认 index.html）；纯工具插件不写。dock/toolbar 面板插件的 entry 不再在设置→插件卡上给「打开」按钮（面板归侧边栏，用户 2026-10-03 拍板） |
| tool_file | 否 | 工具文件（.wat/.wasm，目录形态专用；裸文件形态无需清单） |
| settings_entry | 否 | 插件自带设置页（相对插件目录的文件，如 prices.html）；宿主在设置→插件卡片上渲染「设置」按钮打开它。dock 面板插件不给「设置」按钮——设置从侧边栏面板内进（如用量的价格表在面板里有入口） |
| ui | 否 | 宿主界面插槽（v1 支持 toolbar / composer / **dock**——dock = 右侧面板「产物/预览/网页」旁多一个页签，如内置「用量价格」插件） |
| popup | 否 | `true` = 设置→插件卡片点「打开」时开**居中小窗口**（模态）而非右侧大面板；表单型插件用它（用户 2026-10-08 拍板：要弹窗不要侧栏）。缺省 false = 老行为 |
| permissions | 否 | 能力白名单：`composer.insert` / `workspace.write` / `bus.publish` / `notify` / `storage` / `usage.prices`（写模型价格表与币种、触发历史重算）/ `clipboard`（写剪贴板，宿主页代写——插件 iframe 是 sandbox 不透明源，剪贴板 API 不归它使）/ `session.create`（在指定工作区新建对话并预填输入框，插件制作器用）/ `settings.feature` / `action.run`；**未声明的一律拒绝** |
| skills | 否 | 插件自带的技能目录（相对插件目录，如 `"skills"`）：每个子目录一个 SKILL.md 即一个技能，进系统提示的「可用技能」索引（模型 read_skill 可读，返回头部带技能目录绝对路径）。纯技能插件可以既不写 entry 也不写 ui。同名规则：项目/全局技能 > 工作区插件 > 内置插件；停用插件不贡献技能。技能页不列插件技能（管理在插件页），composer 的 $ 选择菜单含插件技能 |

**契约纪律**：解析器忽略未知字段（只增不删 = 天然向后兼容）；apiVersion 不匹配即拒载 +
UI 提示升级，**永不做 shim**；README 写明 v0.x 期间不保证插件兼容。

## 三、目录形态（磁盘自动迁移，两种都认）

- 面板：`plugins/<name>/manifest.json` + 静态资源（应用根与工作区两处，工作区同名优先；
   ui-ext/ 已并入 plugins/，老 ui-ext/ 目录读入即迁）；
- 工具（旧形态）：`plugins/*.wasm|.wat`，插件名 = 文件名去扩展名；
- 工具（目录形态）：`plugins/<name>/manifest.json` + 目录内**恰好一个** .wasm/.wat。

## 四、生命周期

- **停用 = 拒新调用 + 等完**：settings.disabled_plugins（面板与工具插件共用一份名单，
   由 disabled_extensions 改名，旧名读时迁移）
  → 工具不再下发 + 调用被拒；在跑调用是独立实例、30s 内自终，自然结束即「等完」，
  超时击杀兜底。选「等完」的理由：每会话单轮不变量下不存在需要杀的长驻任务，
  为它造任务管理器不值（施工单 027 定案 5）。
- **资源域记账**：统一清单（/api/plugins）本身就是账本——每个插件的 panels / tools /
  permissions 都在清单行里；停用时整域回收（工具从总线消失、面板 404、总线拒转发）。
- **draft 隔离区**（source=model_generated）：只在内存（不入盘，**重启即失**）、
  **权限一律清空**（最低权限）、工具经 WASM 引擎内存编译（零宿主函数物理隔离）；
  一键转正（POST /api/plugins/promote）落盘持久化、来源改 user；丢弃（DELETE /api/plugins/draft）即清。
  ⚠️ 与定案的差异：定案说「仅当前会话生效」，v1 实为「到重启」（多会话同见）——
  会话级过滤要贯穿清单/面板/总线全链路，在还没有真实生产者（模型生成插件的工具链属 ）
  之前是纯成本，先做重启粒度，语义留口。

## 五、存储一门（插件侧 API）

插件永远没有 raw 文件句柄。数据 API（宿主总线 → /api/plugin/data）：

- `storage.read(key)` / `storage.write(key, value)` / `storage.delete(key)`，须声明 `storage` 权限；
- 物理位置 `<app_root>/plugin_data/<插件名>/<key>.json`（原子写）；
- 键名白名单 `^[A-Za-z0-9_-]{1,64}$`（构造上无路径穿越），值 ≤ 64KB；
- 会话 / settings / 记忆 / 检查点是**内核保留区**，不在任何插件写路径上；
  越权尝试（含键名穿越）→ 拒绝 + `storage.plugin_denied` 审计；
- 内核侧：会话与 settings 落盘收口 `storage::write_session / write_settings`（唯一入口 + 审计）。

> 一句话纪律：**插件可以写自己名下的格子，碰不到昨天存下来的东西。**

## 五之二、成本台账（插件化：plugins/usage 试点，原 ui-ext/usage）

内核（台账 / 计价 / 花费硬闸）**永不插件化**——计量挂在网关唯一出入口、闸门要真能挡住模型请求。插件只拿两层：

- **dock 面板**：manifest `ui:[{slot:"dock",title:"用量"}]` → 右侧面板多一个页签；停用插件页签即消失（台账照记）。
- **数据桥**：
  - `usage.session`（只读，不占权限）→ **当前会话**的用量统计（token 三路 / 请求 / 费用；会话 id 由宿主注入，插件看不到别的会话）；
  - `usage.prices`（只读）→ 价格表 + 可选渠道清单 + 币种汇率 + 累计/上限；
  - `usage.prices.save` / `usage.recompute`（写，须声明 `usage.prices`）→ 存价格表与币种 / 用当前价格重算全部历史账。
- 价格表是**内核配置**（settings.json 的 `cost` 节），插件只是编辑器：停用插件不影响计价与硬闸。
- 金额一律**美元**规范存储；人民币是展示层（`cost.currency` + `usd_cny_rate` 折算），切币种不动账。

## 五之三、项目文件（board 面板，2026-10-01 三轮反馈后定稿）

工作区文件**树**面板：文件夹折叠成一行、箭头点了转向才按需展开一层（`workspace.dir` 一次
只列一个目录的直接子项，不整树铺开）；顶部路径面包屑可点（点到哪，浏览根到哪）；单击只
选中、**双击**才在新标签页预览；右键复制路径/文件名/绝对路径、插入输入框。相关桥：

- `workspace.dir`（只读，不占权限）→ 列工作区某目录直接子项（`{path}` 缺省=工作区根；
  返回 `{dirs, files, truncated}`，跳过 .git/node_modules/target 等噪音目录，每类上限 500）；
- `workspace.file`（只读，不占权限）→ 读工作区文本文件内容（`{path}`，沿用 read_file 的越界
  防护与 200KB 头尾压缩，读不到以 reject 回带人话）；
- `clipboard.write`（写，须声明 `clipboard`）→ 宿主页代写剪贴板（`{text}`）。插件 iframe 是
  sandbox 不透明源，`navigator.clipboard` 与 `execCommand` 都不归它使，复制一律由宿主页执行；
  用户右键触发，未声明权限的插件一律拒。

宿主侧文件预览标签（从「项目文件」双击或「产物」点击打开）：

- `GET /api/workspace/raw?path=` 直出原始字节（图片预览用，工作区边界同 read_file 硬拦，上限 8MB）；
- 文本可**编辑保存**（`POST /api/workspace/file`，512KB 上限、原子写）；**内容被 200KB 头尾压缩
  过的文件不给编辑**（`……[中间省略` 标记，保存会把省略段写没——宁可不让改也不许写坏文件）。

## 五之四、右侧边栏统一标签页（2026-10-01 用户拍板）

顶栏只留「终端 + 侧边面板」两个按钮；产物/网页/插件面板（时间线/项目文件/用量）/文件预览
全在右侧边栏开**标签页**——标签可关（× 在激活标签上），尾部 `+` 加面板，关光显示
「打开标签页」选择页。侧栏只收编宿主三面板插件 trace/board/usage；其余插件哪怕 manifest
声明 `ui.slot = "dock"`（内部把旧 `toolbar` 视同面板并入）也不进侧栏——`+` 菜单与选择页
只出内置产物/git/网页加这三件（用户 2026-10-03 拍板：装的插件别全列出来，六个够了）。
每标签一份 iframe 常驻不重载（切走切回状态不丢）。

- `preview.open`（只读，不占权限）→ 宿主在右侧边栏新开该文件的预览标签（`{path}`）；
  项目文件点击文件即走它，「产物」列表点击同样开标签（不再是固定的「预览」页签）。
- `trace.recent` 现在**只返回当前工作区**的任务：任务的聊天会话归属取 `task.start` 的
  `session`（新日志）或任务内 `session.saved` 的 `session`（旧日志），工作区→会话映射
  由服务端从会话文件算。注意日志顶层 `session_id` 是**进程级**日志会话（进程重启即换），
  与工作区无关，别拿它归属（踩过：按它过滤 = 时间线全空）。

> **宿主侧回传纪律（2026-09-24 补，所有桥方法通用）**：`agent:response` 的 `result` 必须是
> **解析后的普通 JSON 对象**。`fetch` 的 `Response` 过不了 `postMessage` 的结构化克隆，
> 原样回传会让插件侧只看到 `Failed to execute 'postMessage' on 'Window': Response object
> could not be cloned.`（面板整个瘫掉）；当成数据用更阴——`r.path` / `r.extensions` 之类的
> 字段在裸 Response 上恒 `undefined`，界面静默显示空值、一个报错都没有。只读桥统一走
> `apiJson(path, opts, missingMsg)`（顺带把「旧版服务没这个接口」翻成人话），写完跑
> `node scripts/check_fetch_json.mjs` 体检（判据与白名单见 ROADMAP 坑 20）。

## 六、钩子 v1

config.toml `[hooks]`，四个挂点，承载形式 = 本地命令（stdin 收 JSON 事件、stdout 回 JSON）：

| 挂点 | 触发时机 | 决定权 |
| --- | --- | --- |
| before_tool | 每次工具执行前 | **仅否决权**：stdout `{"decision":"deny","reason":"…"}` = 拦下并回喂模型；放行 ≠ 免审批（审批门单点不动） |
| after_tool | 工具执行后 | 通知（观测 / 格式化等后续动作） |
| session_end | 任务结束（含取消/出错） | 通知（沉淀记忆等收尾） |
| budget_exceeded | 单任务累计 token 超 budget_tokens（每次任务最多一次）| 通知 + **降级已落地**：配了 `[roles] budget` 渠道时，该任务剩余步骤改走它 |

语义四条：4 挂点 / 仅否决 / **超时（timeout_ms）与拉起失败一律放行**（坏钩子不卡主循环）/
JSONL 审计（hook.* 事件只增）。

```toml
[hooks]
before_tool = "C:\\hooks\\block-dangerous.cmd"   # deny 示例见本文件
session_end = "C:\\hooks\\save-memory.cmd"
budget_tokens = 500000
timeout_ms = 5000
```

## 七、内置插件试点选型（选型与落地）

| 试点 | 形态 | 结论 |
| --- | --- | --- |
| trace 时间线 | 纯面板（entry + 只读桥）| 现有字段**够用**；拆 App.svelte 时把 trace 面板拆成独立组件顺势插件化 |
| web_search | 纯工具 | 现有字段**够用**：以目录形态落 plugins/（或内置注册表直挂）即可，无需新契约字段 |
| 用量台账（插件化）| dock 面板 + 插件自带设置页 | **要加两口**：`ui` 插槽新值 `dock`（右侧面板页签）+ manifest `settings_entry`（插件设置页，宿主在插件卡上渲染「设置」按钮；2026-10-03 起 dock 插件不再给「设置」按钮，价格表从侧边栏用量面板进）；数据走新增桥 `usage.session` / `usage.prices`（写要 `usage.prices` 权限）。内核（台账/计价/硬闸）不插件化 |
| 插件制作器（plugin-maker） | 纯面板（manifest `popup:true`，设置→插件「打开」弹居中小窗口）| 2026-10-08：**表单即窗口**——插件 UI 没有宿主模态原语，由宿主按 manifest `popup` 字段把插件页放进居中小窗口（模态 iframe），不开侧面板（用户拍板：要弹窗不要侧栏）。窗口里：需求输入 + 工作区选择 + 确定；确定后走新桥 `session.create`（须声明 `session.create` 权限）：有会话在跑则拒 → 目标工作区非当前则宿主切过去（没切成就不建会话）→ 建对话 → 给输入框预填「用户需求 + 插件契约规范 + 安装验证引导」并**关掉小窗**，用户直接落进新对话，**不代发**。提示词模板内联在面板页（`plugins/plugin-maker/index.html`），改模板不用重编译宿主；工作区下拉直接用只读桥 `workspace.info`（回 `{current, recent}`） |

试点的意义：自己吃自己的狗粮——内置功能装得进插件壳，契约就够结实；装不进就说明契约
有缺陷，开源前赶紧改。**执法者（审批门 / 存储 / 并发控制）永不插件化**（裁判不下场）。

> **清理记录（用户指示）**：web_search 工具插件试点已撤（WASM 零宿主函数无网络，
> 被调用只能返回说明文字，真搜索由内置 web_search 工具承担）；同批撤下 hello.wat 冒烟
> 插件与 evals/mcp-mini-server.mjs 示例 MCP。trace 面板保留（时间线功能载体）。

> ** 真机坑（面板插件必读）**：宿主在面板 iframe 的 `load` 事件之后才登记该 frame，
> 之前的 postMessage 会被宿主按「未知来源」丢弃。因此**初始拉数据必须等宿主推来的
> `agent:init` 再发起**（见 plugins/trace/index.html），脚本一加载就调用只读桥会一直超时。

## 八、准入机制的现状（2026-10-08：审核与验签移除，生态起来再重建）

2026-10-02 曾按应用商店模型加过两道门：**静态审核**（`plugin/review.rs`：manifest 合规 /
外联白名单 / 动态执行 / 体积，blocker 拒载）与 **pin 验签**（`plugin/trust.rs`：内容指纹
绑定，改动即停载）。2026-10-08 用户拍板**两道都移除**（理由：插件制作器把「提示词写规范」
的路走通后，生成插件的规范性由提示词保证；沙箱隔离——面板 iframe 不透明源 + WASM 零宿主
函数——保证单个插件崩了不拖垮整体；两道门在生态还没起来时只是安装摩擦）。

**当前状态**：

- 落盘即加载：写进 `plugins/`（应用根或工作区）的目录形态插件直接进清单，没有 pin /
  审核前置门；`/api/plugins/trust`、`/api/plugins/review` 端点与 plugin 工具的
  `review`/`unpin` action 一并移除，plugin 工具只剩 `install`（draft 提交 → 转正落位）
  与 `remove`（删 user 来源，内置拒删）。
- 仍然有效的边界：manifest 契约校验（apiVersion / entry 存在性 / 路径不出目录 /
  权限白名单）、停用语义（拒新调用 + 等完）、draft 隔离区（权限清空 + 内存态 + 重启即失）、
  `source` 三值语义（删除保护 / draft 归属 / 来源标注）。
- 重建时的入口点：git 历史里 `src/plugin/review.rs` 与 `src/plugin/trust.rs` 的删除提交
  （2026-10-08 轮）保留了完整实现与测试，照搬回去再按新形态接线即可。

## 九、术语对照（退役表）

| 旧 | 新 |
| --- | --- |
| 扩展 / UI 扩展 | 插件（面板） |
| WASM 插件 | 工具插件 / 插件工具（WASM 引擎） |
| 界面型 / 能力型 | （废除，不再分型） |

兼容说明（2026-10-08 校正）：端点已改名 `/api/plugins`（promote/draft 同）、
静态分发 `/plugins/*`、设置字段 `disabled_plugins`——旧名字（`/api/ext`、`/ui-ext/`、
`disabled_extensions`、`ui-ext:` pin 键、`trusted_plugins` 设置字段）只作**读入迁移源**
或由 serde 忽略未知字段自然兼容，不再作为现行接口保留。
