<script>
  import Select from '../Select.svelte';
  import ScheduleCronPicker from '../ScheduleCronPicker.svelte';
  import { createEventDispatcher, onDestroy } from 'svelte';

  // 「去查看」：交给宿主 App 打开对应会话（组件内拿不到会话切换逻辑）
  const dispatch = createEventDispatcher();

  export let tab = '';
  export let workspace = '';
  export let recentWorkspaces = [];

  // —— 定时任务（cron 到点唤醒无头会话；结果走通知列表）——
  let schedulesData = [];
  let scheduleNotices = [];
  // 用户 2026-10-05 实锤：完成记录改成小字一行，右边给个 × 关掉——原来整行按钮太占地方，
  // 「去查看」任务卡片上已有。× 关掉必须落盘：壳重启只重载 WebView（pawch-agent.exe 服务常驻），
  // 设置弹窗每次重开也重建组件——只做内存过滤的话，叉掉的刷新一下就回来。
  let dismissedNotices = {};
  $: visibleNotices = scheduleNotices.filter((n) => !dismissedNotices[n.ts + n.task_id]);
  function dismissNotice(key) {
    // 本地先滤掉（即时消失），再调后端落盘；失败不吞，给人话反馈
    dismissedNotices = { ...dismissedNotices, [key]: true };
    fetch(`/api/schedules/notice?id=${encodeURIComponent(key)}`, { method: 'DELETE' })
      .then((r) => r.json())
      .then((r) => {
        if (r.error) schedMsg = `关掉失败：${r.error}`;
      })
      .catch((e) => {
        schedMsg = `关掉失败：${e.message || e}`;
      });
  }
  let schedMsg = '';
  // 通用确认弹窗（桌面壳 WebView2 不实现 window.confirm，同 App 侧 ConfirmModal）
  let confirmModal = null;
  function confirmAsk(text) {
    return new Promise((resolve) => {
      confirmModal = { text, resolve };
    });
  }
  let schedLoading = false;
  // 渠道配置候选（执行模型选择；GET /api/settings 的 configs，含 id/label/model/models）
  let schedConfigs = [];
  // 编辑器：null = 关；isNew = 新建（id 空）。档位/高危策略语义同审批四档
  let schedEditor = null;
  // 高级选项（会话/审批/超时）默认折叠；编辑老任务时若与默认不同则自动展开，不藏已配置的东西
  let schedAdvOpen = false;
  const SCHED_ADV_DEFAULT = { session_mode: 'new', session_id: '', approval_mode: 'auto', on_approval: 'deny', timeout_secs: 1800 };
  // cron 人话预览（输入即查，250ms 防抖；解析失败由界面提示）
  let schedCronText = '';
  let schedCronTimer = null;
  let schedReloadTimer = null;
  // 2026-10-08 修：编辑中/点了「立即运行」后立刻关设置弹窗，组件已销毁但两个
  // 定时器到点仍会对已销毁实例发 fetch 并写状态（Svelte4 不报错，纯浪费）。
  onDestroy(() => {
    clearTimeout(schedCronTimer);
    clearTimeout(schedReloadTimer);
  });
  let schedCronPending = false;
  // 三档（2026-10-05 用户实锤纠正：sandbox 档 10-03 就取消了，这里一直挂着四档旧表；
  // 与输入框上方的全局档位菜单同一套口径，跟随全局语义）
  const SCHED_MODES = [
    { value: 'read-only', label: '只读（写/执行逐次询问）' },
    { value: 'auto', label: '自动（可动工作区，高危先问）' },
    { value: 'full-access', label: '完全访问（全免审，本机直跑）' },
  ];
  function schedAdvNonDefault(t) {
    const n = (v, d) => (v == null || v === '' ? d : v);
    return (
      n(t.session_mode, 'new') !== 'new' ||
      !!(t.session_id && t.session_id.trim()) ||
      n(t.approval_mode, 'auto') !== 'auto' ||
      n(t.on_approval, 'deny') !== 'deny' ||
      !!(t.config_id && t.config_id.trim()) ||
      !!(t.model && t.model.trim()) ||
      (Number(t.timeout_secs) || 1800) !== 1800
    );
  }
  // 工作区：默认跟随触发时的当前工作区；下拉里给最近项目（去重）
  function wsLabel(w) {
    return String(w).split(/[\\/]/).filter(Boolean).pop() || w;
  }
  // 2026-10-08 修：options={fn()} 在模板里不建依赖（Svelte4 只跟踪表达式直接引用
  // 的变量，函数体里读的 schedConfigs/workspace/recentWorkspaces 换了不重算）——
  // 真机实锤：列表还在加载时开编辑器，供应商下拉永远只有「跟随当前激活配置」一项。
  // 改走响应式声明：$: 语句跟踪函数体依赖，源数据一换选项就重算。
  $: configOpts = configOptions();
  $: wsOpts = wsOptions();
  function configOptions() {
    const opts = [{ value: '', label: '跟随当前激活配置' }];
    for (const c of schedConfigs) {
      opts.push({ value: c.id, label: c.name }); // 只显示供应商名；模型是下一个下拉的事（用户 2026-10-05 实锤）
    }
    return opts;
  }
  // configId 由模板直接传参：Svelte4 只跟踪表达式里直接引用的变量，包进函数体
  // 读 schedEditor.config_id 不建依赖（toolFold 同款坑，真机实锤过），选供应商时
  // 模型下拉不会重算——2026-10-05 用户实锤「选了 bai 要出 bai 的三个模型」。
  function modelOptions(configId) {
    const cfg = schedConfigs.find((c) => c.id === (configId || ''));
    const opts = [{ value: '', label: '默认（该配置的模型）' }];
    for (const m of cfg?.models || []) opts.push({ value: m, label: m });
    // 配置当前模型不在清单里（手填的）也给一个可选项
    if (cfg?.model && !(cfg.models || []).includes(cfg.model)) {
      opts.push({ value: cfg.model, label: `${cfg.model}（当前）` });
    }
    return opts;
  }
  function wsOptions() {
    const seen = new Set();
    const opts = [{ value: '', label: '跟随当前工作区' }];
    for (const w of [workspace, ...recentWorkspaces]) {
      if (w && !seen.has(w)) {
        seen.add(w);
        opts.push({ value: w, label: wsLabel(w) });
      }
    }
    return opts;
  }
  function previewCron() {
    clearTimeout(schedCronTimer);
    schedCronTimer = setTimeout(async () => {
      if (!schedEditor) { schedCronText = ''; schedCronPending = false; return; }
      const cron = (schedEditor.cron || '').trim();
      if (!cron) { schedCronText = ''; schedCronPending = false; return; }
      schedCronPending = true;
      try {
        const r = await fetch(`/api/schedules/cron-text?cron=${encodeURIComponent(cron)}`).then((r) => r.json());
        schedCronText = r.ok ? r.text : '';
      } catch {
        schedCronText = '';
      } finally {
        schedCronPending = false;
      }
    }, 250);
  }
  async function loadSchedules() {
    schedLoading = true;
    // 只清上一次加载留下的「读取失败」：操作反馈（已保存/触发失败…）留到下一次操作，
    // 不被加载抹掉。2026-10-08 修：这行原在 try/catch 之后——catch 刚设的错误
    // 在同一同步块里又被无条件擦掉，「服务没起时读取失败」永远看不见（死代码）。
    if (schedMsg.startsWith('读取失败')) schedMsg = '';
    try {
      const [r, st] = await Promise.all([
        fetch('/api/schedules').then((r) => r.json()),
        fetch('/api/settings', { cache: 'no-store' }).then((r) => r.json()).catch(() => ({})),
      ]);
      schedConfigs = (st.configs || []).map((c) => ({
        id: c.id,
        name: c.name || c.provider_id || c.id.slice(0, 8),
        model: c.model || '',
        models: c.models || [],
        context_length: c.context_length || 0,
      }));
      schedulesData = r.schedules || [];
      scheduleNotices = r.notices || [];
    } catch (e) {
      schedMsg = `读取失败: ${e.message || e}`;
    } finally {
      schedLoading = false;
    }
    // 只清自己留下的读取失败：操作反馈（已保存/触发失败…）留到下一次操作，不被加载抹掉
    if (schedMsg.startsWith('读取失败')) schedMsg = '';
  }
  function newSchedule() {
    schedMsg = '';
    schedAdvOpen = false;
    // 输入框一律留空不预填：cron/回顾天数/超时都有占位提示，留空走默认（天数缺省 3、超时缺省 1800）
    schedEditor = {
      id: '',
      name: '',
      kind: 'message',
      cron: '',
      once_ts: 0,
      message: '',
      memory_days: '',
      session_mode: 'new',
      session_id: '',
      workspace: '',
      approval_mode: 'auto',
      on_approval: 'deny',
      enabled: true,
      timeout_secs: '',
      budget_tokens: '',
      config_id: '',
      model: '',
      isNew: true,
    };
    previewCron();
  }
  function editSchedule(t) {
    schedMsg = '';
    schedAdvOpen = schedAdvNonDefault(t);
    // 超时回显为空 + 占位符，不显示突兀的 0；保存时留空按 1800 发（见 saveSchedule）
    schedEditor = { ...t, isNew: false, timeout_secs: Number(t.timeout_secs) || '', memory_days: t.memory_days || '', budget_tokens: t.budget_tokens ? Number(t.budget_tokens) : '', config_id: t.config_id || '', model: t.model || '' };
    previewCron();
  }
  async function saveSchedule() {
    const e = schedEditor;
    if (!e) return;
    // body 先组装：fetch 调用点往后 16 行内必须出现 .json()（scripts/check_fetch_json.mjs 硬体检）
    const body = {
      id: e.id || '',
      name: e.name,
      kind: e.kind || 'message',
      cron: e.cron,
      once_ts: Number(e.once_ts) || 0,
      message: e.message,
      memory_days: Number(e.memory_days) || 3,
      session_mode: e.session_mode,
      session_id: e.session_id,
      workspace: e.workspace || '',
      approval_mode: e.approval_mode,
      on_approval: e.on_approval,
      enabled: e.enabled !== false,
      // 留空 = 1800（与服务端 DEFAULT_TIMEOUT_SECS 对齐，显式发值不靠「0 走默认」隐式契约，F10）
      timeout_secs: Number(e.timeout_secs) || 1800,
      // 上下文上限（tokens）：留空=0=跟全局/渠道口径，填了本任务硬覆盖
      budget_tokens: Number(e.budget_tokens) || 0,
      config_id: e.config_id || '',
      model: e.model || '',
    };
    // 单次模式没选时间（或周期模式没产出 cron）：本地拦下，不发请求
    if (!String(body.cron || '').trim() && !body.once_ts) {
      schedMsg = '还没选执行时间';
      return;
    }
    // 单次时间已过：本地直接提示，不等服务端校验（已跑完停用的任务编辑不受限，与服务端口径一致）
    if (body.once_ts && body.enabled && body.once_ts <= Math.floor(Date.now() / 1000)) {
      schedMsg = '单次任务的时间要设在将来';
      return;
    }
    try {
      const r = await fetch('/api/schedules', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      }).then((r) => r.json());
      if (r.ok) {
        schedEditor = null;
        await loadSchedules();
        schedMsg = '已保存'; // 放在加载之后：loadSchedules 会清提示，顺序反了就被抹掉
      } else {
        schedMsg = `保存失败: ${r.error}`;
        // 编辑器里挂着的任务已从列表消失（被删除/外部清掉）：关掉编辑器，说人话
        if (String(r.error || '').includes('任务不存在')) {
          schedEditor = null;
          schedMsg = '这条任务已不在列表里（可能已被删除），请重新创建';
        }
      }
    } catch (err) {
      schedMsg = `保存失败: ${err.message || err}`;
    }
  }
  async function deleteSchedule(id) {
    if (!(await confirmAsk('删除这条定时任务？（已跑出的会话与记录保留）'))) return;
    try {
      const r = await fetch(`/api/schedules?id=${encodeURIComponent(id)}`, { method: 'DELETE' }).then((r) => r.json());
      if (r.ok) {
        await loadSchedules();
        schedMsg = '已删除';
      } else {
        schedMsg = `删除失败: ${r.error}`;
      }
    } catch (e) {
      schedMsg = `删除失败: ${e.message || e}`;
    }
  }
  // 「立即运行」防连点（F6）：跑着的任务按钮置灰，响应回来才恢复
  let runningIds = {};
  async function runScheduleNow(id) {
    if (runningIds[id]) return;
    runningIds = { ...runningIds, [id]: true };
    try {
      const r = await fetch(`/api/schedules/run?id=${encodeURIComponent(id)}`, { method: 'POST' }).then((r) => r.json());
      schedMsg = r.ok ? '已触发，跑完会出现在下面的完成记录里' : `触发失败: ${r.error}`;
      clearTimeout(schedReloadTimer);
      schedReloadTimer = setTimeout(loadSchedules, 1500);
    } catch (e) {
      schedMsg = `触发失败: ${e.message || e}`;
    } finally {
      runningIds = { ...runningIds, [id]: false };
    }
  }
  function schedNextText(t) {
    if (t.enabled === false) return '已停用';
    return t.next_run_ts ? fmtTs(t.next_run_ts) : '无法计算';
  }
  // unix 秒 → 本地时间字符串（定时任务的运行时间展示）
  function fmtTs(ts) {
    if (!ts) return '—';
    const d = new Date(ts * 1000);
    const p = (n) => String(n).padStart(2, '0');
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
  }
  // 组件化回归修复（审计 02）：响应式声明必须在 <script> 顶层——原先缩进在 fmtTs
  // 函数体内被 Svelte 当普通 JS 标签编译，切页签永远不会自动拉列表
  $: if (tab === 'schedule') loadSchedules();
</script>

      {#if tab === 'schedule'}
        <div class="sub-row">
          <button class="btn primary" on:click={newSchedule}>新建任务</button>
          <button class="btn" on:click={loadSchedules}>刷新</button>
          <span class="hint">{schedMsg}</span>
        </div>
        {#if schedEditor}
          <div class="field">
            <label>{schedEditor.isNew ? '新建定时任务' : `编辑：${schedEditor.name}`}</label>
            <div class="sub-row">
              <input placeholder="名称" bind:value={schedEditor.name} style="max-width:220px" />
              <Select
                value={schedEditor.kind || 'message'}
                title="任务类型"
                options={[
                  { value: 'message', label: '固定消息' },
                  { value: 'memory', label: '记忆提醒（回顾最近对话）' },
                ]}
                on:change={(e) => (schedEditor.kind = e.detail)}
              />
              <label class="mcp-risk-label"><input type="checkbox" bind:checked={schedEditor.enabled} /> 启用</label>
            </div>
            <div class="sub-row">
              <ScheduleCronPicker bind:cron={schedEditor.cron} bind:onceTs={schedEditor.once_ts} on:change={previewCron} />
              {#if (schedEditor.kind || 'message') === 'memory'}
                <input placeholder="回顾天数" type="number" bind:value={schedEditor.memory_days} style="max-width:110px" />
              {/if}
              {#if schedCronText}
                <span class="hint">{schedCronText}</span>
              {:else if !schedCronPending && (schedEditor.cron || '').trim()}
                <span class="hint" style="color: var(--tint-orange-fg)">没认出来，示例 0 9 * * * = 每天 09:00</span>
              {/if}
            </div>
            {#if (schedEditor.kind || 'message') === 'message'}
              <textarea
                class="agents-editor"
                rows="4"
                spellcheck="false"
                placeholder="到点时唤醒 agent 的消息"
                bind:value={schedEditor.message}
              ></textarea>
            {:else}
              <div class="mcp-cmd">到点先检索最近 {schedEditor.memory_days || 3} 天的对话，让 agent 找出要跟进的事项</div>
            {/if}
            <div class="field">
              <label>工作区选择</label>
              <Select
                value={schedEditor.workspace || ''}
                title="工作区"
                options={wsOpts}
                on:change={(e) => (schedEditor.workspace = e.detail)}
              />
            </div>
            <div class="field">
              <label>模型</label>
              <div class="sub-row">
                <Select
                  value={schedEditor.config_id || ''}
                  title="供应商（执行配置）"
                  options={configOpts}
                  on:change={(e) => {
                    schedEditor.config_id = e.detail;
                    schedEditor.model = ''; // 换配置清掉模型覆盖，回落到该配置默认
                  }}
                />
                <Select
                  value={schedEditor.model || ''}
                  title="模型（默认=该配置的模型）"
                  options={modelOptions(schedEditor.config_id || '')}
                  on:change={(e) => (schedEditor.model = e.detail)}
                />
              </div>
            </div>
            <div class="sub-row">
              <button class="btn ghost" on:click={() => (schedAdvOpen = !schedAdvOpen)}>{schedAdvOpen ? '收起高级选项' : '高级选项'}</button>
            </div>
            {#if schedAdvOpen}
              <div class="sub-row">
                <Select
                  value={schedEditor.session_mode}
                  title="会话模式"
                  options={[
                    { value: 'new', label: '每轮新会话' },
                    { value: 'fixed', label: '固定会话' },
                  ]}
                  on:change={(e) => (schedEditor.session_mode = e.detail)}
                />
                {#if schedEditor.session_mode === 'fixed'}
                  <input placeholder="会话 id" bind:value={schedEditor.session_id} style="max-width:260px" />
                {/if}
              </div>
              <div class="sub-row">
                <Select
                  value={schedEditor.approval_mode}
                  title="审批档位"
                  options={SCHED_MODES}
                  on:change={(e) => (schedEditor.approval_mode = e.detail)}
                />
                <Select
                  value={schedEditor.on_approval}
                  title="高危操作"
                  options={[
                    { value: 'deny', label: '高危：拒绝' },
                    { value: 'approve', label: '高危：放行' },
                  ]}
                  on:change={(e) => (schedEditor.on_approval = e.detail)}
                />
                <input placeholder="超时（秒）" type="number" bind:value={schedEditor.timeout_secs} style="max-width:110px" />
                <input
                  placeholder="上下文上限"
                  type="number"
                  bind:value={schedEditor.budget_tokens}
                  style="max-width:150px"
                  title="本任务的上下文 token 预算。留空=跟全局设置或渠道的 context_length；要读更多历史、不想被过早裁剪就填大值。超过模型真实窗口会报错，按你渠道的量力填。"
                />
              </div>
            {/if}
            <div class="sub-row">
              <button class="btn primary" on:click={saveSchedule}>保存</button>
              <button class="btn" on:click={() => (schedEditor = null)}>取消</button>
            </div>
          </div>
        <!-- 与技能页同款：编辑器开着只留编辑器，列表让位（用户 2026-10-05 实锤回归：
             编辑时该任务还挂在下面） -->
        {:else if schedLoading && !schedulesData.length}
          <div class="trace-empty">加载中…</div>
        {:else}
          {#each schedulesData as t (t.id)}
            <div class="mcp-card">
              <div class="mcp-head">
                <span class="dot" class:haskey={t.enabled !== false}></span>
                <b>{t.name}</b>
                {#if t.kind === 'memory'}<span class="mcp-risk">记忆 · {t.memory_days || 3} 天</span>{/if}
                <span class="mcp-status" title={t.cron}>{t.cron_text}</span>
                {#if t.workspace}<span class="mcp-risk" title={t.workspace}>@ {wsLabel(t.workspace)}</span>
                {:else if workspace}<span class="mcp-risk" title={`${workspace}（跟随当前工作区）`}>@ {wsLabel(workspace)}</span>{/if}
                {#if t.config_id}<span class="mcp-risk" title={`用配置 ${schedConfigs.find((c) => c.id === t.config_id)?.name || t.config_id.slice(0, 8)} 跑${t.model ? `（${t.model}）` : ''}`}>◆ {schedConfigs.find((c) => c.id === t.config_id)?.name || t.config_id.slice(0, 8)}{t.model ? ` · ${t.model}` : ''}</span>{/if}
                {#if t.budget_tokens}<span class="mcp-risk" title={`本任务上下文上限 ${Number(t.budget_tokens).toLocaleString()} tokens`}>⛨ {Number(t.budget_tokens).toLocaleString()} tokens</span>{/if}
                <span class="flex-sp"></span>
                <button class="btn ghost" disabled={runningIds[t.id]} on:click={() => runScheduleNow(t.id)}>{runningIds[t.id] ? '触发中…' : '立即运行'}</button>
                <button class="btn ghost" on:click={() => editSchedule(t)}>编辑</button>
                {#if t.last_session_id}<button class="btn ghost" on:click={() => dispatch('open-session', { id: t.last_session_id })}>去查看</button>{/if}
                <button class="btn ghost danger" on:click={() => deleteSchedule(t.id)}>删除</button>
              </div>
              {#if t.kind === 'memory'}
                <div class="mcp-cmd">到点回顾最近 {t.memory_days || 3} 天的对话，找出要跟进的事项</div>
              {:else}
                <div class="mcp-cmd">{t.message}</div>
              {/if}
              <div class="mcp-cmd">
                下次运行 {schedNextText(t)}{t.last_status
                  ? ` · 上次${t.last_status === 'done' ? '完成' : t.last_status === 'timeout' ? '超时' : t.last_status === 'cancelled' ? '取消' : '出错'}`
                  : ''}
              </div>
              {#if t.last_error}
                <div class="mcp-cmd" style="color: var(--tint-orange-fg)">{t.last_error}</div>
              {/if}
            </div>
          {/each}
        {/if}
        {#if visibleNotices.length}
          <div class="field"><label>完成记录</label></div>
          {#each visibleNotices as n (n.ts + n.task_id)}
            <div class="mcp-cmd sched-notice">
              <span class="sched-notice-text">{n.ts} · {n.title}{n.detail ? `：${n.detail}` : ''}</span>
              <button class="notice-x" title="关掉这条记录" on:click={() => dismissNotice(n.ts + n.task_id)}>×</button>
            </div>
          {/each}
        {/if}
      {/if}

      {#if confirmModal}
        <div class="modal-backdrop" on:click={() => { const r = confirmModal.resolve; confirmModal = null; r(false); }}>
          <div class="modal confirm-modal" on:click|stopPropagation>
            <div class="modal-head"><b>确认</b></div>
            <div class="confirm-text">{confirmModal.text}</div>
            <div class="confirm-actions">
              <button class="btn" on:click={() => { const r = confirmModal.resolve; confirmModal = null; r(false); }}>取消</button>
              <button class="btn primary" on:click={() => { const r = confirmModal.resolve; confirmModal = null; r(true); }}>确定</button>
            </div>
          </div>
        </div>
      {/if}

<style>
  .confirm-modal {
    min-width: 380px;
    max-width: 520px;
  }
  .confirm-text {
    white-space: pre-line;
    font-size: 14px;
    line-height: 1.6;
    color: var(--g6);
  }
  .confirm-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 16px;
  }
  .menu-empty {
    color: var(--g5);
    font-size: 12px;
    padding: 8px 10px;
  }
  .agents-editor {
    width: 100%;
    box-sizing: border-box;
    font-family: Consolas, 'Courier New', monospace;
    font-size: 12.5px;
    line-height: 1.7;
    background: var(--g1);
    color: var(--g7);
    border: 1px solid var(--g3);
    border-radius: 8px;
    padding: 12px 14px;
    resize: vertical;
  }
  .agents-editor:focus {
    outline: none;
    border-color: var(--g4);
  }
  .mcp-card {
    border: 1px solid var(--g3);
    border-radius: 10px;
    padding: 10px 12px;
  }
  .mcp-head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    color: var(--g7);
  }
  .mcp-status {
    font-size: 11px;
    color: var(--g5);
  }
  .mcp-risk {
    font-size: 11px;
    color: var(--tint-yellow-fg);
    background: var(--tint-yellow-bg);
    border-radius: 4px;
    padding: 1px 6px;
  }
  .mcp-cmd {
    font-family: Consolas, monospace;
    font-size: 11px;
    color: var(--g5);
    margin: 6px 0;
    word-break: break-all;
  }
  .mcp-risk-label {
    font-size: 12px;
    color: var(--g6);
  }
  /* 完成记录：小字一行 + 行尾 ×（关掉这条）。不放大字号、不加按钮——
     「去查看」任务卡片上已有，这里只做记录本体 */
  .sched-notice {
    display: flex;
    align-items: baseline;
    gap: 8px;
    margin: 2px 0;
  }
  .sched-notice-text {
    flex: 1;
    min-width: 0;
  }
  .notice-x {
    flex: none;
    border: none;
    background: none;
    color: var(--g5);
    font-size: 13px;
    line-height: 1;
    padding: 2px 5px;
    cursor: pointer;
    border-radius: 4px;
  }
  .notice-x:hover {
    color: var(--g7);
    background: var(--g2);
  }
  .dot {
    display: inline-block;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    margin-right: 6px;
    background: var(--g4);
    vertical-align: middle;
  }
  .dot.haskey {
    background: var(--tint-green-fg);
  }
</style>
