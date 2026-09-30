---
name: browser-use
description: 用内置浏览器打开/导航/检查/点击/输入/测试/验证网页与本地页面（含 localhost）的工作流纪律
---

# browser-use 技能（内置浏览器）

适用：打开网页、读 JS 渲染后的页面、点击/输入/填表、等待页面变化、验证本地服务界面。
纯读静态内容用 web_fetch 更快；凡是要「看到渲染后的页面」或在页面上动手，都用 `browser` 工具。

## 工具形态

一个 `browser` 工具，按 `method` 判别动作（协议对齐 ZCode browser-use）：

- 导航：`navigate`（返回快照）/ `back` / `forward` / `reload` / `wait_for`
- 观察：`snapshot` / `get_state` / `element_info` / `list`
- 操作：`click` / `fill` / `type` / `press` / `scroll` / `hover` / `select` / `check` / `drag`
- 其他：`evaluate`（隔离世界执行 JS）/ `screenshot`（刷新面板画面）/ `get_dialog` / `handle_dialog`
- 标签页：`new_tab` / `activate_tab` / `close` / `list`
- 收尾：`close_browser`（关掉整个浏览器进程，不等 10 分钟空转回收）

定位优先级：**ref（快照编号）→ text/label（语义）→ x/y（视口坐标）**。
ref 是快照时钉在元素上的编号（如 e12），跨快照稳定；DOM 变了会失效。

## 工作流纪律（按 ZCode control-browser 对齐）

1. **navigate 后直接读返回的快照**，不要再立刻 snapshot 一次——navigate 已等加载完成并附带结构。
2. **点击/输入前先 snapshot 拿最新 ref**；不要凭记忆用旧 ref。
3. **每个观察周期只做一个状态变更动作**（一次 click/fill/type/press/select/check/drag），做完再 snapshot 验证效果出现了没有——用「预期效果是否出现」判断成败，不要连发多个动作再看结果。
4. **ref 失效就换定位方式**：用 text/label 语义匹配或 x/y 坐标兜底；绝不重试同一个已失败的 ref。错误信息里的 `[ref_not_found]` 就是这个意思。
5. **等待页面用 wait_for**（selector/text/text_gone + timeout_ms），不要用固定时长的等待或反复 snapshot 碰运气。
6. **同一站点的任务在同一个标签页里做**，不要每步开新 tab；要并行看别的页面再 `new_tab`。
7. **弹窗**：页面弹了 alert/confirm/prompt 会卡住页面——`get_dialog` 看内容，`handle_dialog` 接受/取消。用户也会在右侧「网页」面板看到弹窗条并可直接处理。
8. **标签页**：`list` 看全部标签页（agent 页带 [agent] 标记、面板可见页带 [可见]）；`tab_id` 参数可操作面板里用户打开的页面；`activate_tab` 可把某页切到面板前台。
9. **用户可见**：agent 对浏览器的一切操作实时显示在右侧「网页」面板（agent 专用页带徽章）——用户看着你点，重要操作前不必额外征求确认，但破坏性操作（提交表单、下单、删除）先用 snapshot 确认目标。
10. **读页面用 snapshot**（elements + 正文双结构，带「屏外」标记）；`screenshot` 只是把画面刷到面板给用户看，读内容不要用它。
11. `evaluate` 在隔离世界执行，页面脚本无法篡改结果；但它算页面操作，会走审批。
12. 任务收尾：自己的 agent 页可以留着（复用更快），明确结束再 `close`；确认再也不用了再 `close_browser` 释放进程。
