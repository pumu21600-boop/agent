<script>
  // 定时任务的时间选择器：常用频率全部点选（单次/每天/每周/每 N 分钟/每月），
  // 不提供手写 cron——避免用户写表达式出格式错。
  // 对外 bind:cron + bind:onceTs：周期模式产出 cron，单次模式产出时间戳（cron 留空）。
  // 外部传入的值会反向解析回界面字段；settings.json 里手改出的不认识的表达式
  // 走 legacy 兜底：原样保留不覆盖，界面上提示用户重新选择。
  import { createEventDispatcher } from 'svelte';
  import Select from './Select.svelte';

  export let cron = '';
  export let onceTs = 0;
  const dispatch = createEventDispatcher();

  let mode = 'daily'; // once | daily | weekly | interval | monthly | legacy（仅兜底，不在选项里）
  let time = '09:00';
  let onceDate = ''; // YYYY-MM-DD
  let onceTime = '09:00';
  let weekdays = []; // 0=周日 1-6=周一..周六
  let intervalMin = 15;
  let monthDay = 1;

  const MODES = [
    { value: 'once', label: '单次', hint: '跑一次就停' },
    { value: 'daily', label: '每天' },
    { value: 'weekly', label: '每周' },
    { value: 'interval', label: '每 N 分钟' },
    { value: 'monthly', label: '每月' },
  ];
  const WD = [
    { v: 1, l: '一' },
    { v: 2, l: '二' },
    { v: 3, l: '三' },
    { v: 4, l: '四' },
    { v: 5, l: '五' },
    { v: 6, l: '六' },
    { v: 0, l: '日' },
  ];
  const TIME_RE = /^([01]\d|2[0-3]):[0-5]\d$/;

  function computeOnceEpoch(date, t) {
    if (!date || !TIME_RE.test(t || '')) return 0;
    const ms = new Date(`${date}T${t}`).getTime();
    return Number.isFinite(ms) ? Math.floor(ms / 1000) : 0;
  }
  // 模板里的提示要用响应式变量——直接调函数 Svelte 看不到内部依赖，提示不会跟着消失
  $: onceEpochVal = computeOnceEpoch(onceDate, onceTime);

  function setOnceFields(ts) {
    const d = new Date(ts * 1000);
    const p = (n) => String(n).padStart(2, '0');
    onceDate = `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
    onceTime = `${p(d.getHours())}:${p(d.getMinutes())}`;
  }

  function buildCron() {
    const m = TIME_RE.exec(time || '');
    if (!m) return '';
    const hh = Number(m[1]);
    const mm = Number(m[2]);
    if (mode === 'interval') {
      const n = Number(intervalMin);
      return n >= 1 && n <= 59 ? `*/${n} * * * *` : '';
    }
    if (mode === 'daily') return `${mm} ${hh} * * *`;
    if (mode === 'monthly') {
      const d = Number(monthDay);
      return d >= 1 && d <= 31 ? `${mm} ${hh} ${d} * *` : '';
    }
    if (mode === 'weekly') {
      return weekdays.length
        ? `${mm} ${hh} * * ${[...weekdays].map(Number).sort((a, b) => a - b).join(',')}`
        : '';
    }
    return '';
  }

  function parseCron(c) {
    const f = (c || '').trim().split(/\s+/);
    if (f.length !== 5) return false;
    const [mf, hf, domf, monf, dowf] = f;
    const iv = /^\*\/(\d+)$/.exec(mf);
    if (iv && hf === '*' && domf === '*' && monf === '*' && dowf === '*') {
      const n = Number(iv[1]);
      if (n >= 1 && n <= 59) {
        mode = 'interval';
        intervalMin = n;
        return true;
      }
      return false;
    }
    if (!/^\d{1,2}$/.test(mf) || !/^\d{1,2}$/.test(hf)) return false;
    const mm = Number(mf);
    const hh = Number(hf);
    if (mm > 59 || hh > 23) return false;
    const t = `${String(hh).padStart(2, '0')}:${String(mm).padStart(2, '0')}`;
    if (domf === '*' && monf === '*' && dowf === '*') {
      mode = 'daily';
      time = t;
      return true;
    }
    if (monf === '*' && domf === '*' && dowf !== '*') {
      const parts = dowf.split(',');
      if (!parts.every((p) => /^\d$/.test(p))) return false;
      const days = [...new Set(parts.map((p) => (Number(p) === 7 ? 0 : Number(p))))];
      if (!days.every((d) => d >= 0 && d <= 6)) return false;
      mode = 'weekly';
      weekdays = days;
      time = t;
      return true;
    }
    if (monf === '*' && dowf === '*' && /^\d{1,2}$/.test(domf)) {
      const d = Number(domf);
      if (d >= 1 && d <= 31) {
        mode = 'monthly';
        monthDay = d;
        time = t;
        return true;
      }
    }
    return false;
  }

  let lastCron = null;
  let lastOnce = null;
  // 外部值（编辑老任务/新建空值）→ 反解析进界面字段；自己产出的回流不重解析
  $: {
    const incCron = cron == null ? '' : String(cron);
    const incOnce = Number(onceTs) || 0;
    if (incCron !== lastCron || incOnce !== lastOnce) {
      lastCron = incCron;
      lastOnce = incOnce;
      if (incOnce > 0) {
        mode = 'once';
        setOnceFields(incOnce);
      } else if (incCron && parseCron(incCron)) {
        // 已解析成对应模式
      } else if (!incCron) {
        emit(); // 新建没有初始值：给出缺省选择（每天 09:00）并回写
      } else {
        mode = 'legacy'; // 认不出的旧表达式：原样保留，等用户重新选
      }
    }
  }

  function emit() {
    if (mode === 'once') {
      lastOnce = computeOnceEpoch(onceDate, onceTime);
      lastCron = '';
    } else {
      lastOnce = 0;
      lastCron = buildCron();
    }
    cron = lastCron;
    onceTs = lastOnce;
    dispatch('change', lastOnce || lastCron);
  }

  function toggleWd(v) {
    weekdays = weekdays.includes(v) ? weekdays.filter((x) => x !== v) : [...weekdays, v];
    emit();
  }
</script>

<div class="cron-picker">
  <div class="cp-row">
    <Select bind:value={mode} title="重复频率" options={MODES} on:change={emit} />
    {#if mode === 'once'}
      <input type="date" bind:value={onceDate} on:input={emit} />
      <input type="time" bind:value={onceTime} on:input={emit} />
    {:else if mode === 'daily'}
      <input type="time" bind:value={time} on:input={emit} />
    {:else if mode === 'weekly'}
      <div class="wd-row">
        {#each WD as w (w.v)}
          <button type="button" class="wd" class:on={weekdays.includes(w.v)} on:click={() => toggleWd(w.v)}>{w.l}</button>
        {/each}
      </div>
      <input type="time" bind:value={time} on:input={emit} />
    {:else if mode === 'interval'}
      <span class="lab">每</span>
      <input type="number" min="1" max="59" bind:value={intervalMin} on:input={emit} class="cp-num" />
      <span class="lab">分钟</span>
    {:else if mode === 'monthly'}
      <span class="lab">每月</span>
      <input type="number" min="1" max="31" bind:value={monthDay} on:input={emit} class="cp-num" />
      <span class="lab">日</span>
      <input type="time" bind:value={time} on:input={emit} />
    {/if}
  </div>
  {#if mode === 'legacy'}
    <div class="cp-hint">这条任务的时间是旧格式（见下方人话），重新选择后会覆盖它</div>
  {:else if mode === 'once' && !onceEpochVal}
    <div class="cp-hint">选一个将来的日期和时间</div>
  {:else if mode === 'weekly' && !weekdays.length}
    <div class="cp-hint">勾选要运行的星期几</div>
  {:else if mode === 'interval' && !(Number(intervalMin) >= 1 && Number(intervalMin) <= 59)}
    <div class="cp-hint">分钟数要填 1–59</div>
  {:else if mode === 'monthly' && !(Number(monthDay) >= 1 && Number(monthDay) <= 31)}
    <div class="cp-hint">日期要填 1–31</div>
  {:else if mode !== 'legacy' && mode !== 'once' && !TIME_RE.test(time || '')}
    <div class="cp-hint">选个时间</div>
  {/if}
</div>

<style>
  .cp-row {
    display: flex;
    gap: 10px;
    align-items: center;
    flex-wrap: wrap;
    flex: 1 1 auto;
    min-width: 0;
  }
  .cron-picker :global(.csel-wrap) {
    width: 150px;
    flex: none;
  }
  .lab {
    color: var(--g6);
    font-size: 13px;
    white-space: nowrap;
  }
  .cp-num {
    max-width: 90px;
  }
  input[type='time'],
  input[type='date'] {
    max-width: 150px;
  }
  .wd-row {
    display: flex;
    gap: 4px;
  }
  .wd {
    min-width: 30px;
    padding: 5px 7px;
    border-radius: 7px;
    border: 1px solid var(--g3);
    background: var(--g1);
    color: var(--g6);
    font-size: 12.5px;
    font-family: inherit;
    cursor: pointer;
  }
  .wd:hover {
    border-color: var(--g4);
  }
  .wd.on {
    border-color: var(--brand);
    color: var(--brand);
    font-weight: 600;
  }
  .cp-hint {
    margin-top: 4px;
    color: var(--tint-orange-fg);
    font-size: 12px;
  }
</style>
