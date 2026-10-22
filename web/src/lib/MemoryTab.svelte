<script>
  import { createEventDispatcher } from 'svelte';

  const dispatch = createEventDispatcher();
  export let tab = '';
  // 星云入口显隐（App 侧 features.galaxy；关掉只藏入口，记忆功能本体不受影响）
  export let showGalaxy = true;
  // 「打开记忆库」:关设置 + 开星云(两个状态都在 App 侧)
  const openGalaxy = () => dispatch('openGalaxy');

  // —— 记忆分区（约定文件编辑 + 自动记忆）——
  // 系统约定 prompts/system.md、全局约定 ~/.pawch-agent/AGENTS.md（旧名 ~/.agent 兼容一个版本）、项目约定 工作区根 AGENTS.md、
  // 自动记忆 <app_root>/.pawch-agent/profile.md（agent 也会写，行级增改；用户可自由编辑）。
  // 文本框始终显示文件当前内容：进入分区/切 chips/保存后都回读一次盘。
  let memData = null;
  let memTab = 'system';
  let memText = '';
  let memSaving = false;
  let memMsg = '';
  // 有未保存的编辑：后台回读不得覆盖输入框（否则刚写的约定会被静默清空/回退成旧内容）
  let memDirty = false;
  const MEM_SCOPES = {
    system: { text: 'system_prompt', path: 'system_path', label: '系统约定' },
    global: { text: 'agents_global', path: 'global_agents_path', label: '全局约定' },
    project: { text: 'agents_project', path: 'project_agents_path', label: '项目约定' },
    profile: { text: 'agents_profile', path: 'profile_path', label: '自动记忆' },
  };
  function memTextOf(r, tab) {
    const k = (MEM_SCOPES[tab] || MEM_SCOPES.project).text;
    return (r && r[k]) || '';
  }
  // 当前约定文件路径。必须是响应式变量：模板里直接调用函数不会随 memTab 重算（Svelte 看不到函数体依赖）
  $: memPath = (memData && MEM_SCOPES[memTab] ? memData[MEM_SCOPES[memTab].path] : '') || '';
  async function loadMemory() {
    try {
      // 带时间戳绕开浏览器缓存：显示的一定是盘上当下的内容
      const r = await fetch(`/api/memory?t=${Date.now()}`).then((r) => r.json());
      memData = r;
      if (!memDirty) memText = memTextOf(r, memTab);
    } catch (e) {
      // 2026-10-08 修：原来只在首次加载（memData 空）时报错——首屏成功之后所有
      // 回读失败被静默吞掉，框里留着旧内容，用户以为盘上就是这内容，继续编辑保存
      // 会把外部改动覆盖掉。回读失败一律亮出来（保存前/换 tab 都会回读）。
      memMsg = `读取失败: ${e.message || e}`;
    }
  }
  function pickMemTab(t) {
    if (!memData || memTab === t) return;
    memTab = t;
    memText = memTextOf(memData, t); // 先用上一份快照，随即回读盘上真容
    memDirty = false; // 换 tab = 看那个文件的内容，放弃当前未保存编辑
    memMsg = '';
    loadMemory();
  }
  async function saveAgents() {
    memSaving = true;
    memMsg = '';
    try {
      const r = await fetch('/api/memory/agents', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ scope: memTab, content: memText }),
      }).then((r) => r.json());
      if (r.ok) {
        memDirty = false; // 先清脏标记再回读，框里保留的就是文件真实内容
        await loadMemory();
        memMsg = `已保存到 ${r.path}`;
      } else {
        memMsg = `保存失败: ${r.error}`;
      }
    } catch (e) {
      memMsg = `保存失败: ${e.message || e}`;
    } finally {
      memSaving = false;
    }
  }
  // 打开设置面板 / 切到记忆分区时都回读文件（含直接打开面板、持久化分区的情形）
  $: if (tab === 'memory') loadMemory();
</script>

      {#if tab === 'memory'}
        {#if memData}
          {#if memData.agents_conflict}
            <div class="mem-conflict">{memData.agents_conflict}</div>
          {/if}
          <div class="field">
            <label>记忆存储（自动记录）</label>
            <div class="mem-stats">
              <div>· 约定目录 <code>{memData.user_global_dir}</code> · 登记目录 <code>{memData.install_state_dir}</code> · {memData.projects_registered} 个项目</div>
              <div>· 项目目录 <code>{memData.project_dir}</code> · events.log {memData.events_lines} 行</div>
            </div>
          </div>
          {#if showGalaxy}
          <div class="field">
            <label>记忆库</label>
            <div class="sub-row">
              <button class="btn" on:click={openGalaxy}>打开记忆库</button>
            </div>
          </div>
          {/if}
          <div class="field">
            <label>约定文件</label>
            <div class="chips">
              <button class="chip" class:active={memTab === 'system'} on:click={() => pickMemTab('system')}>系统约定</button>
              <button class="chip" class:active={memTab === 'global'} on:click={() => pickMemTab('global')}>全局约定</button>
              <button class="chip" class:active={memTab === 'project'} on:click={() => pickMemTab('project')}>项目约定</button>
              <button class="chip" class:active={memTab === 'profile'} on:click={() => pickMemTab('profile')}>自动记忆</button>
            </div>
            {#if memTab === 'profile'}
              <div class="mem-cap" class:warn={memData.profile_chars > (memData.profile_max_chars || 2500) * 4 / 5}>
                自动记忆 {memData.profile_chars}/{memData.profile_max_chars || 2500} 字符（理想 ≤{memData.profile_ideal_chars || 1200} · 预警线 {((memData.profile_max_chars || 2500) * 4 / 5)}）
              </div>
            {/if}
            <textarea class="agents-editor" rows="12" spellcheck="false" bind:value={memText} on:input={() => (memDirty = true)}></textarea>
            <div class="sub-row">
              <button class="btn" on:click={saveAgents} disabled={memSaving}>{memSaving ? '保存中…' : '保存'}</button>
              <span class="hint">{memMsg}</span>
            </div>
            <div class="param-hint">文件：<code>{memPath}</code></div>
          </div>
        {:else}
          <div class="trace-empty">{memMsg || '加载中…'}</div>
        {/if}
      {/if}

<style>
  .mem-conflict {
    font-size: 12.5px;
    line-height: 1.6;
    color: var(--tint-orange-fg);
    background: var(--tint-orange-bg);
    border: 1px solid var(--tint-orange-fg);
    border-radius: 8px;
    padding: 9px 12px;
    margin-bottom: 14px;
    word-break: break-all;
  }
  .mem-cap {
    font-size: 11.5px;
    color: var(--g5);
    margin: 2px 0 10px;
  }
  .mem-cap.warn {
    color: var(--tint-orange-fg);
  }
  .mem-stats {
    font-size: 12px;
    color: var(--g6);
    display: flex;
    flex-direction: column;
    gap: 7px;
  }
  .mem-stats code {
    font-size: 11px;
    color: var(--g5);
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
  .chips {
    display: flex;
    gap: 9px;
    flex-wrap: wrap;
  }
  .chip {
    padding: 7px 13px;
    border-radius: 14px;
    border: 1px solid var(--g3);
    background: var(--g1);
    color: var(--g6);
    font-size: 12px;
    cursor: pointer;
  }
  .chip.active {
    background: var(--g3);
    border-color: var(--g5);
    color: var(--g7);
  }
</style>
