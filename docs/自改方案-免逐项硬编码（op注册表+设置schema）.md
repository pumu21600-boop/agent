# 自改方案：从「逐个 UI / 逐个字段硬编码」到「自描述注册表 + 通用入口」

> 背景：本文针对项目里「改 Agent 自身设置 / 操作自身界面」这条链（俗称「自改」）的现状与痛点，
> 给出一套**不再需要对着每个功能、每个 UI 一个一个加**的改造方案。
> 结论先行：项目里其实**已经有通用通道的雏形**，麻烦来自**两处清单仍写死在代码里**；
> 把它们升级为**数据化 / 自描述**之后，新增设置或新增界面动作只需改一处数据。

---

## 一、现状：自改链长什么样（已核实）

自改能力目前是「三件套 + 两条通道」的结构：

| 层 | 位置 | 作用 |
|---|---|---|
| 总闸 | `src/tools/mod.rs` · `selfmod_gate()` | `features.self_mod` 关闭时，所有「改自身」动作直接拒绝并给用户指路；只读动作不闸 |
| 改设置 | `src/tools/settings.rs` · 工具 `settings` + `SettingsPatch` | 以**逐字段补丁**方式读写 `settings.json` |
| 点界面 | `src/tools/ui.rs` · 工具 `ui` | 一部分是**具名方法**（`open_sidebar` / `set_theme` / `inject_css` …），一部分是**通用口** `fire_event` + `SAFE_EVENT_KINDS` 白名单 |
| 风险分级 | `src/core/rules.rs` | 按「工具名 + action/method」判定风险；红线键经 `red_line_hold` **任何档位强制弹卡** |
| 前端消费（SSE） | `web/src/App.svelte` · EventSource `/api/ui/events` 处理块 | **逐 `kind` 手写 `if / else if` 长链** |
| 前端消费（插件桥） | `web/src/App.svelte` · `postMessage` 桥 `switch (d.method)` | **逐 `method` 手写 `switch`** |

已存在的「通用化」萌芽（说明方向本来就对）：

- `ui` 工具的 `fire_event` + `SAFE_EVENT_KINDS`，代码注释明确写了「**别一个一个加就指这个**」；
- 前端已通过 `/api/ui/state` 接口**主动上报**界面状态（侧栏/右栏/终端/设置/主题），双向通道已经打通；
- `rules.rs` 对**未登记工具**默认判 `Dangerous`，是天然的「白名单兜底」模式；
- 设置工具 `action=get` 已能返回**全量设置快照** + `available_plugins` / `available_skills` 合法 id 清单。

缺的只有一件东西：**一份能被机器读取的「自描述清单」**（设置有哪些字段、界面有哪些动作、各自风险几何）。

---

## 二、痛点精确定位：「一个一个加」卡在哪

### 痛点 A：加一个**设置项** ≈ 动 5 处代码

当前设置走的是「类型化结构体 + 手写补丁」，新增一个可改字段通常要同步改：

1. `SettingsPatch` 的字段定义；
2. `apply_patch`（或等价）里的字段映射；
3. `SettingsUpdate` / 返回结构体；
4. `rules.rs::risk_settings` 的风险分级；
5. 若涉及红线，还要进 `SETTINGS_REDLINE_KEYS`。

→ 信息分散在 4~5 处，任何一处漏改都会导致「能改但不生效 / 生效但不弹卡 / 弹卡但分类错」。

### 痛点 B：加一个**界面动作** ≈ 动 3 处代码，且要写两遍

1. 后端 `ui.rs`：加白名单条目或具名方法；
2. 前端 `App.svelte`：在 SSE 的 `if/else` 链里加一个 `else if (m.kind === ...)` 分支；
3. 通常还要在前端补一个对应的状态/执行函数。

而且前端**两条入口各有一套**（SSE 事件链 + 插件 `postMessage` 桥），同一个能力往往得写两遍、维护两遍。

### 根因一句话

> 能力清单（能改什么设置、能点什么界面）目前是**编译期硬编码在代码里**的，
> 所以「扩展能力」不可避免地等于「改代码、加分支」。

---

## 三、方案：把「白名单 + 逐项分支」升级为「注册表 + 自描述清单 + 通用入口」

核心思路：**让界面自己告诉后端「我能干什么」**，模型再问界面「你能干什么」，
而不是由开发者替界面写死「它会干什么」。

### 方案 1：设置改为「schema + 通用路径补丁」

不再为每个字段写 Rust 分支，改为**按路径打补丁**：

```
action=set  { path: "features.galaxy", value: true }
action=set  { path: "roles.embedding", value: "xxx" }
```

落点：通用地把 `path` 写到 `Settings` 结构体上（JSON 指针 / 点号路径均可）。

配套一张**字段元数据表**（Rust 常量、或与结构体字段同源的注解 / 旁挂一份 JSON），每个字段声明：

| 元数据 | 说明 |
|---|---|
| `path` | 设置路径，如 `features.self_mod`、`roles.embedding` |
| `type` / `enum` | 值类型、可选枚举 |
| `default` | 默认值 |
| `label` | 人类可读名（也能喂给模型） |
| `risk` | 风险档：`normal` / `red_line` |
| `desc` | 说明，用于生成工具描述与 UI |

好处：

- 加一个设置项 = **结构体加字段 + 元数据加一行**，不再动 5 处；
- `risk_settings` 与红线表**退化为一次查表**（查不到默认危险 / 或默认弹卡），红线清单不再维护两份；
- 新增接口 `GET /api/settings/schema`：模型先读 schema 就知道「**所有能改的设置长什么样、哪些是红线**」，
  天然满足「能改所有设置」的目标；
- `settings` 工具的描述文本可由 schema **自动生成**，不再手写长句。

### 方案 2：界面动作改为「op 注册表 + 自描述清单」

前端把已有函数（如 `startNewSession`、`rpOpenExtByName`、`applyThemeMode`、`applyCustomCss` …）
登记进**一张 `ops` 表**，每项带：`name`、`args` 参数说明、`title`、`risk`（safe / normal / destructive）、
以及（给插件用的）`permission`。

后端不再发 `{ kind: "xxx" }` 让前端 `else-if` 挨个认，
而是发**统一入口** `{ op: "panel.open", args: {...} }`，前端查表执行：

```js
const op = UI_OPS[m.op];
if (op) op.run(m.args);
```

再进一步：前端通过**已有的** `/api/ui/state` 通道，把这张 `ops` 清单**上报给后端**。于是：

- 后端可**动态**地把「当前可点的动作清单 + 参数 + 风险」喂给模型 —— 这就是「**点所有页面、点所有按钮**」的正解：
  用「应用自报家门」代替枚举硬编码；
- 后端无需再维护 `SAFE_EVENT_KINDS` 这类**编译期常量**，改为读前端清单；
- 加界面能力 = **前端注册表加一行**（本就要写函数），不再改后端白名单 + 前端长 `if` 链。

### 方案 3：风险分级也变成数据

把方案 1、2 里每个设置字段 / 每个 op 自带的 `risk` 标注作为**事实源**，
`rules.rs` 从「一堆 `match` 」退化为「**一次查表**」：

- 查得到 → 用表里的 `risk`；
- 查不到 → 沿用现有「未登记 = Dangerous」兜底；
- 红线 = 表里 `red_line = true` 的标记。

**安全不变量一点不丢**（见第五节）。

### 方案 4：两条前端入口合一

SSE 事件链 与 插件 `postMessage` 桥 共用同一张 `ops` 表与同一个通用入口。
插件权限（`composer.insert` / `workspace.write` / `bus.publish` / `notify` / `storage` / `clipboard` …）
作为**条目上的声明**，不再维护两套 `switch`。

---

## 四、落地顺序建议（小步、可验证）

1. **最小一步（可先验证效果）**：把前端 SSE 的 `if/else` 长链抽成一张 `UI_OPS` 注册表 + 通用入口。
   行为不变，纯重构，风险最低。
2. **打通自描述**：前端通过 `/api/ui/state` 上报 `ops` 清单；后端据此生成 `ui` 工具的动态动作清单，
   用「读清单」替换 `SAFE_EVENT_KINDS` 硬编码常量。
3. **设置 schema 化**：抽字段元数据表 + 通用路径补丁 + `GET /api/settings/schema`；
   把 `risk_settings` / 红线表改为查表。
4. **两条入口合一** + 把插件权限并入 op 条目声明。
5. **分级查表化**：`rules.rs` 收口为「查表 + 兜底」。

每一步都可独立提交、独立真机验证，且不改变对外语义。

---

## 五、必须保留的安全底线（别被「通用化」一起抹掉）

以下约束是现状**刻意设计**的，改造时必须完整保留：

- **`self_mod` 总闸**：`features.self_mod` 关闭时，所有自改动作一律拒绝并指路，只读不闸。
- **红线键（2026-10-08 校正）**：2026-10-04 放开权限轮后 `red_line_hold` 只剩
  `("ui","undo_checkpoint")`（撤销检查点）一项任何档位必问；渠道四件套
  （`base_url`/`model`/`provider_id`/`active_config_id`）已降为按审批档位判级，
  `api_key` 与切工作区改为 out_of_scope 直接拒。**红线只剩审批档位 + API key**
  （用户 2026-10-04 拍板）。
- **破坏类操作继续走具名路径**：例如删除会话（`delete_session`）当前**刻意没有**塞进通用 `fire_event` 白名单，
  就是为了让 `rules` 能**按参数**判级、并在工具层完成确认。通用化后，这类操作应在 op 元数据里标 `risk: destructive`
  并保留「按参数判级 + 必问」的语义，而不是简单并进一条宽松的通用通道。
- **范围外保留范围外**：`api_key` / 价格表 仍不进模型可改范围；`config.toml` 改文件仍需重启且同样必问。（2026-10-08 注：随验签机制移除，「信任插件」一项已无对应字段，自然作废。）
- **审批档位本身（2026-10-08 校正）**：档位已并三档（read-only / auto / full-access，
  sandbox 档 2026-10-03 取消、旧值自动迁移 read-only）；agent 经 `set_config` 改
  `config.toml` 的 `default_approval` 有通道但**必经用户确认**（rules.rs 判 Dangerous），
  不是「任何通道都没有」。

---

## 六、方案对比一览

| 维度 | 现状 | 改造后 |
|---|---|---|
| 加一个设置项 | 改约 5 处代码 | 字段 + 元数据 1 行 |
| 加一个界面动作 | 后端白名单 + 前端长 `if` 链（×2 入口） | 前端注册表 1 行 |
| 能力清单来源 | 编译期硬编码 | 运行时自描述（前端上报） |
| 风险分级 | 分散 `match` | 数据标注 + 查表兜底 |
| 模型「点所有按钮」 | 无法枚举 | 读动态 op 清单即可 |
| 前端两条入口 | 两套 `switch` | 共用一张表 |

---

## 七、关键代码锚点（核实依据）

- `src/tools/mod.rs` — `selfmod_gate()` / 工具描述
- `src/tools/settings.rs` — `SettingsPatch`、`SettingsUpdate`、`available_plugins`
- `src/tools/ui.rs` — 具名方法、`fire_event`、`SAFE_EVENT_KINDS`
- `src/core/rules.rs` — 风险分级、`red_line_hold`、未登记默认 Dangerous
- `web/src/App.svelte` — `EventSource('/api/ui/events')` 处理块（逐 `kind` `if/else`）、
  插件 `postMessage` 桥（逐 `method` `switch`）、`/api/ui/state` 上报
- `src/storage.rs` — 单一落盘入口（方案 1 的通用补丁落盘应复用）
- `docs/plugin-contract-v1.md` — 插件契约 v1（权限声明模型，方案 4 复用）

---

> 一句话总结：
> 把「白名单 + 逐项分支」升级为「**op / 设置注册表 + 自描述清单 + 通用入口**」，
> 让模型改成**问界面「你能干什么」**，而不是开发者替界面写死「它会干什么」。
