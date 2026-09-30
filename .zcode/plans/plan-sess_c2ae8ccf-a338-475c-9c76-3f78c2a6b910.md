## 内置功能插件化 · 第一批(功能桥接 / css-ui注入 / 星云入口)

**通用机制**:settings.json 新增顶层 `"features"` 对象(`{roles, css_tool, galaxy}`,bool,缺省全 true,旧配置无此字段行为不变),走既有 PUT /api/settings 保存链。插件页「内置」组新增三条功能行(复用 .plug-row/拨动开关),拨动即 autoSave({features})。以后加功能 = 一个布尔 + 一行。

### 1. 功能桥接(原「角色分工」改名 + 开关)
- 改名:设置页签、聊天媒体占位提示文案(设置→角色分工→设置→功能桥接),全仓清扫用户可见处。
- off(后端):4 个消费点加 `features.roles` 闸——turn.rs 渠道组装链(摘要/标题/降级)、subagent.rs 子代理渠道、memory.rs 嵌入(关→本地哈希向量)、chat/mod.rs 多模态(关→占位文本);config.toml [roles] 一并不再生效,全部走现成「未配置→回主渠道」退化路径。
- off(前端):隐藏页签,正停在该页签则自动切走;已配数据保留,重开即恢复。

### 2. css-ui注入(外观页签的微调引擎 + 手写 CSS)
- off = 隐藏「界面微调模式」按钮 + CSS 注入器整块(含导入/导出/清空),右击微调短路;**已调好的样式继续生效**(界面不回弹);正在微调时强制退出。localStorage 数据保留。

### 3. 星云入口
- off = 隐藏侧栏星云按钮 + 记忆页签里的星云入口(MemoryTab 加 showGalaxy prop),星云弹层开着则关闭。**记忆功能本体零影响**。

### 插件页「内置」组排序
Git、沙箱、功能桥接、css-ui注入、星云、项目速览、时间线、用量。

### 验证
cargo test + clippy 零告警(features 缺省 serde 单测)+ npm build;Tabbit 真机目检:三条功能行、开关落盘往返、各功能关/开显隐正确(验完拨回开启);本地提交不推送,待你验收。

### 触点
src/config/settings.rs、src/server/settings.rs、src/server/chat/turn.rs、src/server/subagent.rs、src/server/memory.rs、src/server/chat/mod.rs(四点闸+文案)、web/src/App.svelte、web/src/lib/MemoryTab.svelte。预计后端 ~60 行、前端 ~100 行,复用既有退化路径,低风险。