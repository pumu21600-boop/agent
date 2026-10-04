<script>
  import Select from '../Select.svelte';
  import ScheduleCronPicker from '../ScheduleCronPicker.svelte';

  export let tab = '';
  export let workspace = '';
  export let recentWorkspaces = [];

  // —— P22-B 定时任务（cron 到点唤醒无头会话；结果走通知列表）——
  let schedulesData = [];
  let scheduleNotices = [];
  let schedMsg = '';
  // P27 通用确认弹窗（桌面壳 WebView2 不实现 window.confirm，同 App 侧 ConfirmModal）
  let confirmModal = null;
  function confirmAsk(text) {
    return new Promise((resolve) => {
      confirmModal = { text, resolve };
    });
  }
  let schedLoading = false;
  // 编辑器：null = 关；isNew = 新建（id 空）。档位/高危策略语义同审批四档
  let schedEditor = null;
  // 高级选项（会话/审批/超时）默认折叠；编辑老任务时若与默认不同则自动展开，不藏已配置的东西
  let schedAdvOpen = false;
  const SCHED_ADV_DEFAULT = { session_mode: 'new', session_id: '', approval_mode: 'auto', on_approval: 'deny', timeout_secs: 1800 };
  // cron 人话预览（输入即查，250ms 防抖；解析失败由界面提示）
  let schedCronText = '';
  let schedCronTimer = null;
  let schedCronPending = false;
  const SCHED_MODES = [
    { value: 'read-only', label: '只读（写/执行逐次询问）' },
    { value: 'sandbox', label: '沙箱（只在沙箱执行）' },
    { value: 'auto', label: '自动（可动工作区）' },
    { value: 'full-access', label: '完全访问（需总开关）' },
  ];
  function schedAdvNonDefault(t) {
    const n = (v, d) => (v == null || v === '' ? d : v);
    return (
      n(t.session_mode, 'new') !== 'new' ||
      !!(t.session_id && t.session_id.trim()) ||
      n(t.approval_mode, 'auto') !== 'auto' ||
      n(t.on_approval, 'deny') !== 'deny' ||
      (Number(t.timeout_secs) || 1800) !== 1800
    );
  }
  // 工作区：默认跟随触发时的当前工作区；下拉里给最近项目（去重）
  function wsLabel(w) {
    return String(w).split(/[\\/]/).filter(Boolean).pop() || w;
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
    try {
      const r = await fetch('/api/schedules').then((r) => r.json());
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
      isNew: true,
    };
    previewCron();
  }
  function editSchedule(t) {
    schedMsg = '';
    schedAdvOpen = schedAdvNonDefault(t);
    // 超时回显为空 + 占位符，不显示突兀的 0；保存时留空按 1800 发（见 saveSchedule）
    schedEditor = { ...t, isNew: false, timeout_secs: Number(t.timeout_secs) || '', memory_days: t.memory_days || '', budget_tokens: t.budget_tokens ? Number(t.budget_tokens) : '' };
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
      setTimeout(loadSchedules, 1500);
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
            <div class="sub-row">
              <span class="hint">在哪儿完成</span>
              <Select
                value={schedEditor.workspace || ''}
                title="工作区"
                options={wsOptions()}
                on:change={(e) => (schedEditor.workspace = e.detail)}
              />
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
        {/if}
        {#if schedLoading && !schedulesData.length}
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
                {#if t.budget_tokens}<span class="mcp-risk" title={`本任务上下文上限 ${Number(t.budget_tokens).toLocaleString()} tokens`}>⛨ {Number(t.budget_tokens).toLocaleString()} tokens</span>{/if}
                <span class="flex-sp"></span>
                <button class="btn ghost" disabled={runningIds[t.id]} on:click={() => runScheduleNow(t.id)}>{runningIds[t.id] ? '触发中…' : '立即运行'}</button>
                <button class="btn ghost" on:click={() => editSchedule(t)}>编辑</button>
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
          {:else}
            <div class="menu-empty">还没有定时任务。到点会唤醒 agent 跑一轮，结果出现在下面的完成记录里。</div>
          {/each}
        {/if}
        {#if scheduleNotices.length}
          <div class="field"><label>完成记录</label></div>
          {#each scheduleNotices as n (n.ts + n.task_id)}
            <div class="mcp-cmd">{n.ts} · {n.title}{n.detail ? `：${n.detail}` : ''}</div>
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
