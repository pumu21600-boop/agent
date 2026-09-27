<script>
  // 定时任务的 cron 选择器：常用频率全部点选（每天/每周/每 N 分钟/每月），
  // 只留「自定义」一个口子手写 cron——避免用户手写表达式出格式错。
  // 对外只暴露 bind:cron；外部传入的 cron 会反向解析回界面字段，
  // 认不出的形状（范围/多时段等）自动落进自定义模式原样保留。
  import { createEventDispatcher } from 'svelte';
  import Select from './Select.svelte';

  export let cron = '';
  const dispatch = createEventDispatcher();

  let mode = 'daily'; // daily | weekly | interval | monthly | custom
  let time = '09:00';
  let weekdays = []; // 0=周日 1-6=周一..周六
  let intervalMin = 15;
  let monthDay = 1;
  let raw = '';

  const MODES = [
    { value: 'daily', label: '每天' },
    { value: 'weekly', label: '每周' },
    { value: 'interval', label: '每 N 分钟' },
    { value: 'monthly', label: '每月' },
    { value: 'custom', label: '自定义 cron' },
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

  function buildCron() {
    if (mode === 'custom') return (raw || '').trim();
    if (mode === 'interval') {
      const n = Number(intervalMin);
      return n >= 1 && n <= 59 ? `*/${n} * * * *` : '';
    }
    const m = /^(\d{1,2}):(\d{2})$/.exec(time || '');
    if (!m) return '';
    const hh = Number(m[1]);
    const mm = Number(m[2]);
    if (hh > 23 || mm > 59) return '';
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

  let last = null;
  // 外部 cron（编辑老任务/新建空值）→ 反解析进界面字段；自己生成的值回流时不重解析
  $: {
    const incoming = cron == null ? '' : String(cron);
    if (incoming !== last) {
      if (!incoming) {
        emit(); // 新建没有初始值：给出缺省选择（每天 09:00）并回写
      } else if (parseCron(incoming)) {
        last = incoming;
      } else {
        mode = 'custom';
        raw = incoming;
        last = incoming;
      }
    }
  }

  function emit() {
    last = buildCron();
    cron = last;
    dispatch('change', last);
  }

  function toggleWd(v) {
    weekdays = weekdays.includes(v) ? weekdays.filter((x) => x !== v) : [...weekdays, v];
    emit();
  }
</script>

<div class="cron-picker">
  <div class="cp-row">
    <Select bind:value={mode} title="重复频率" options={MODES} on:change={emit} />
    {#if mode === 'daily'}
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
    {:else}
      <input placeholder="cron：分 时 日 月 周" bind:value={raw} on:input={emit} style="max-width:220px" />
    {/if}
  </div>
  {#if mode === 'weekly' && !weekdays.length}
    <div class="cp-hint">勾选要运行的星期几</div>
  {:else if mode === 'interval' && !(Number(intervalMin) >= 1 && Number(intervalMin) <= 59)}
    <div class="cp-hint">分钟数要填 1–59</div>
  {:else if mode === 'monthly' && !(Number(monthDay) >= 1 && Number(monthDay) <= 31)}
    <div class="cp-hint">日期要填 1–31</div>
  {:else if mode !== 'custom' && !/^([01]\d|2[0-3]):[0-5]\d$/.test(time || '')}
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
  input[type='time'] {
    max-width: 130px;
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
