# 插件契约 v1（施工单 027 / P18）

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
  "title": "项目速览",
  "description": "…",
  "version": "0.3.0",
  "entry": "index.html",
  "tool_file": "calc.wat",
  "ui": [{ "slot": "toolbar", "id": "board", "title": "项目速览" }],
  "permissions": ["composer.insert", "notify", "bus.publish", "workspace.write", "storage"]
}
```

| 字段 | 必填 | 语义 |
| --- | --- | --- |
| apiVersion | 否 | 缺失按 v1 读入（老目录自动迁移）；写了但不等于 1 → **拒载并提示升级**，不做 shim |
| source | 否 | 来源三值：`built_in`（随包）/ `user`（用户安装）/ `model_generated`（模型自产，走 draft）；**未声明按位置默认：只有应用根 `ui-ext/` 默认 built_in，其余位置（工作区 ui-ext、plugins/ 自装位）一律默认 user**（P21：不声明不能成为绕过第三方验签的后门） |
| title / description / version | 否 | 展示元数据；目录形态工具插件未写时用目录名 |
| entry | 否 | 面板入口页（相对插件目录，默认 index.html）；纯工具插件不写 |
| tool_file | 否 | 工具文件（.wat/.wasm，目录形态专用；裸文件形态无需清单） |
| settings_entry | 否 | 插件自带设置页（相对插件目录的文件，如 prices.html）；宿主在设置→插件卡片上渲染「设置」按钮打开它 |
| ui | 否 | 宿主界面插槽（v1 支持 toolbar / composer / **dock**——dock = 右侧面板「产物/预览/网页」旁多一个页签，如内置「用量」插件） |
| permissions | 否 | 能力白名单：`composer.insert` / `workspace.write` / `bus.publish` / `notify` / `storage` / `usage.prices`（写模型价格表与币种、触发历史重算）；**未声明的一律拒绝** |

**契约纪律**：解析器忽略未知字段（只增不删 = 天然向后兼容）；apiVersion 不匹配即拒载 +
UI 提示升级，**永不做 shim**；README 写明 v0.x 期间不保证插件兼容。

## 三、目录形态（磁盘自动迁移，两种都认）

- 面板：`ui-ext/<name>/manifest.json` + 静态资源（应用根与工作区两处，工作区同名优先）；
- 工具（旧形态）：`plugins/*.wasm|.wat`，插件名 = 文件名去扩展名；
- 工具（目录形态）：`plugins/<name>/manifest.json` + 目录内**恰好一个** .wasm/.wat。

## 四、生命周期

- **停用 = 拒新调用 + 等完**：settings.disabled_extensions（ui-ext 与工具插件共用一份名单）
  → 工具不再下发 + 调用被拒；在跑调用是独立实例、30s 内自终，自然结束即「等完」，
  超时击杀兜底。选「等完」的理由：每会话单轮不变量下不存在需要杀的长驻任务，
  为它造任务管理器不值（施工单 027 定案 5）。
- **资源域记账**：统一清单（/api/ext）本身就是账本——每个插件的 panels / tools /
  permissions 都在清单行里；停用时整域回收（工具从总线消失、面板 404、总线拒转发）。
- **draft 隔离区**（source=model_generated）：只在内存（不入盘，**重启即失**）、
  **权限一律清空**（最低权限）、工具经 WASM 引擎内存编译（零宿主函数物理隔离）；
  一键转正（/api/ext/promote）落盘持久化、来源改 user；丢弃（DELETE /api/ext/draft）即清。
  ⚠️ 与定案的差异：定案说「仅当前会话生效」，v1 实为「到重启」（多会话同见）——
  会话级过滤要贯穿清单/面板/总线全链路，在还没有真实生产者（模型生成插件的工具链属 P22）
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

## 五之二、成本台账（P21 插件化：ui-ext/usage 试点）

内核（台账 / 计价 / 花费硬闸）**永不插件化**——计量挂在网关唯一出入口、闸门要真能挡住模型请求。插件只拿两层：

- **dock 面板**：manifest `ui:[{slot:"dock",title:"用量"}]` → 右侧面板多一个页签；停用插件页签即消失（台账照记）。
- **数据桥**：
  - `usage.session`（只读，不占权限）→ **当前会话**的用量统计（token 三路 / 请求 / 费用；会话 id 由宿主注入，插件看不到别的会话）；
  - `usage.prices`（只读）→ 价格表 + 可选渠道清单 + 币种汇率 + 累计/上限；
  - `usage.prices.save` / `usage.recompute`（写，须声明 `usage.prices`）→ 存价格表与币种 / 用当前价格重算全部历史账。
- 价格表是**内核配置**（settings.json 的 `cost` 节），插件只是编辑器：停用插件不影响计价与硬闸。
- 金额一律**美元**规范存储；人民币是展示层（`cost.currency` + `usd_cny_rate` 折算），切币种不动账。

## 六、钩子 v1

config.toml `[hooks]`，四个挂点，承载形式 = 本地命令（stdin 收 JSON 事件、stdout 回 JSON）：

| 挂点 | 触发时机 | 决定权 |
| --- | --- | --- |
| before_tool | 每次工具执行前 | **仅否决权**：stdout `{"decision":"deny","reason":"…"}` = 拦下并回喂模型；放行 ≠ 免审批（审批门单点不动） |
| after_tool | 工具执行后 | 通知（观测 / 格式化等后续动作） |
| session_end | 任务结束（含取消/出错） | 通知（沉淀记忆等收尾） |
| budget_exceeded | 单任务累计 token 超 budget_tokens（每次任务最多一次） | 通知 + **降级已落地（P21）**：配了 `[roles] budget` 渠道时，该任务剩余步骤改走它 |

语义四条：4 挂点 / 仅否决 / **超时（timeout_ms）与拉起失败一律放行**（坏钩子不卡主循环）/
JSONL 审计（hook.* 事件只增）。

```toml
[hooks]
before_tool = "C:\\hooks\\block-dangerous.cmd"   # deny 示例见本文件
session_end = "C:\\hooks\\save-memory.cmd"
budget_tokens = 500000
timeout_ms = 5000
```

## 七、内置插件试点选型（P18 选型，P19 落地）

| 试点 | 形态 | 结论 |
| --- | --- | --- |
| trace 时间线 | 纯面板（entry + 只读桥） | 现有字段**够用**；P19 拆 App.svelte 时把 trace 面板拆成独立组件顺势插件化 |
| web_search | 纯工具 | 现有字段**够用**：以目录形态落 plugins/（或内置注册表直挂）即可，无需新契约字段 |
| 用量台账（P21 插件化） | dock 面板 + 插件自带设置页 | **要加两口**：`ui` 插槽新值 `dock`（右侧面板页签）+ manifest `settings_entry`（插件设置页，宿主在插件卡上渲染「设置」按钮）；数据走新增桥 `usage.session` / `usage.prices`（写要 `usage.prices` 权限）。内核（台账/计价/硬闸）不插件化 |

试点的意义：自己吃自己的狗粮——内置功能装得进插件壳，契约就够结实；装不进就说明契约
有缺陷，开源前赶紧改。**执法者（审批门 / 存储 / 并发控制）永不插件化**（裁判不下场）。

> **清理记录（P20.1，用户指示）**：web_search 工具插件试点已撤（WASM 零宿主函数无网络，
> 被调用只能返回说明文字，真搜索由内置 web_search 工具承担）；同批撤下 hello.wat 冒烟
> 插件与 evals/mcp-mini-server.mjs 示例 MCP。trace 面板保留（时间线功能载体）。

> **P19 真机坑（面板插件必读）**：宿主在面板 iframe 的 `load` 事件之后才登记该 frame，
> 之前的 postMessage 会被宿主按「未知来源」丢弃。因此**初始拉数据必须等宿主推来的
> `agent:init` 再发起**（见 ui-ext/trace/index.html），脚本一加载就调用只读桥会一直超时。

## 八、第三方来源验签（P21 红线二，施工单 030）

**诉求**：第三方插件要能验明来源再跑。真实密码学签名体系（CA / 私钥分发 / 吊销列表）
工程面超出一个阶段的容量，且 manifest 里自称的签名字串在无密钥前提下不构成任何保证
（复制一份就能伪造）——故采用 **pin digest 模型**（SSH 主机密钥同款思路，零密钥分发）：

- **内容指纹**：canonical digest = sha256(排序后的 `相对路径\n` + sha256(文件内容) + `\n` 序列)；
  目录形态与裸文件形态各一把尺子（`digest_dir` / `digest_file`），域前缀换算法必须换（历史 pin 全失效重 pin）。
- **规则**：`source` 为 `user` 的插件（工作区 ui-ext、应用根 ui-ext 自装、plugins/ 目录与裸文件）
  **必须先被 pin 才能加载**——未 pin → 清单里显式「未受信任」+ 拒载（工具不进注册表、面板不服务资产）；
  内容被改 → 指纹不符 → 拒载 + 「内容已变，需重新信任」。
- **pin 表**：settings.json `trusted_plugins{ "<kind>:<name>": digest }`（kind = `ui-ext` / `plugins`）；
  设置 → 插件 →「信任 / 取消信任」（`POST /api/ext/trust`）即写即生效并重扫注册表。
- **豁免两类**：`built_in`（随包内置，位置即信任）、`model_generated`（draft 隔离区：
  内存态 + 权限清空）；**draft「转正」自动 pin**（转正是用户显式动作 = 信任决定，
  见 `draft::promote` 落盘后由 `trust::pin_from_manifest_paths` 回填）。
- **审计**：`plugin.trust_pinned` / `plugin.trust_blocked` / `plugin.trust_unpinned` 只增。
- 不做的事：不做签名 CA、不校验 manifest 自称的 `signature` 字段、不联网验签。

## 九、术语对照（退役表）

| 旧 | 新 |
| --- | --- |
| 扩展 / UI 扩展 | 插件（面板） |
| WASM 插件 | 工具插件 / 插件工具（WASM 引擎） |
| 界面型 / 能力型 | （废除，不再分型） |

兼容保留（避免无谓破坏）：端点 `/api/ext`、`/ui-ext/` 路径、设置字段 `disabled_extensions`
名字不变；术语退役以用户可见文案与文档为准。
