<script>
  // 工作区切换弹窗（自 App.svelte 按视图拆出）：路径输入自持，应用/关闭经事件回宿主。
  export let open = false;
  export let msg = '';
  export let workspace = '';
  export let recent = [];
  import { createEventDispatcher } from 'svelte';
  const dispatch = createEventDispatcher();
  let path = '';
  function apply() {
    if (!path.trim()) return;
    dispatch('apply', path);
    path = ''; // 与原 applyWorkspace 内的清输入行为一致
  }
  function applyRecent(w) {
    dispatch('apply', w);
  }
  function close() {
    dispatch('close');
  }
</script>

{#if open}
  <div class="modal-backdrop" on:click={close}>
    <div class="modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>切换工作区</b>
        <span class="hint">{msg}</span>
        <button class="btn ghost" on:click={close}>关闭</button>
      </div>
      <div class="field">
        <label>当前工作区</label>
        <div class="ws-current" title={workspace}>{workspace || '（工程根）'}</div>
      </div>
      <div class="field">
        <label>本地文件夹路径（agent 的文件读写/命令执行/影子仓库都在这里，与 agent 自身目录隔离）</label>
        <div class="sub-row">
          <input bind:value={path} placeholder="如 D:\projects\demo 或 C:\Users\你\test" />
          <button class="btn primary" on:click={apply} disabled={!path.trim()}>应用</button>
        </div>
      </div>
      {#if recent.length}
        <div class="field">
          <label>最近使用</label>
          <div class="ws-recent">
            {#each recent as w}
              <button class="ws-chip" class:active={w === workspace} title={w} on:click={() => applyRecent(w)}>
                {w.split(/[\\/]/).filter(Boolean).pop()}
              </button>
            {/each}
          </div>
        </div>
      {/if}
      <div class="param-hint">切换后会话列表与约定按新工作区重开；进行中的任务不受影响</div>
    </div>
  </div>
{/if}

<style>
  .ws-current {
    font-family: Consolas, monospace;
    font-size: 12px;
    color: var(--g6);
    background: var(--g1);
    border: 1px solid var(--g2);
    border-radius: 6px;
    padding: 6px 8px;
    word-break: break-all;
  }
  .ws-recent {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .ws-chip {
    padding: 4px 10px;
    border-radius: 12px;
    border: 1px solid var(--g3);
    background: var(--g1);
    color: var(--g6);
    font-size: 12px;
    cursor: pointer;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ws-chip:hover {
    border-color: var(--g4);
    color: var(--g7);
  }
  .ws-chip.active {
    border-color: var(--g5);
    color: var(--g7);
  }
</style>
