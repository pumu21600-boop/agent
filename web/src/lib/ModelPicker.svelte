<script>
  // 获取到的模型清单弹窗（P19 自 App.svelte 按视图拆出）：勾选状态由宿主的已存模型表决定。
  export let open = false;
  export let models = [];
  export let saved = []; // 已加入配置的模型名（checked 判定）
  import { createEventDispatcher } from 'svelte';
  const dispatch = createEventDispatcher();
  import { inferCapabilities } from './format.js';

  function isChecked(m) {
    return saved.includes(m);
  }
</script>

{#if open}
  <div class="modal-backdrop" on:click={() => dispatch('close')}>
    <div class="modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>获取到的模型（{models.length} 个，勾选即加入）</b>
        <div class="modal-actions">
          <button class="btn" on:click={() => dispatch('selectall')}>全选</button>
          <button class="btn" on:click={() => dispatch('selectnone')}>全不选</button>
          <button class="btn ghost" on:click={() => dispatch('close')}>完成</button>
        </div>
      </div>
      <div class="model-list">
        {#if models.length === 0}
          <div class="empty">渠道未返回任何模型</div>
        {/if}
        {#each models as m}
          {@const caps = inferCapabilities(m)}
          <label class="model-item">
            <input type="checkbox" checked={isChecked(m)} on:change={() => dispatch('toggle', m)} />
            <span class="m-name">{m}</span>
            <span class="m-caps">
              {#if caps.vision}<span class="m-tag" title="支持图片理解">图片</span>{/if}
              {#if caps.video}<span class="m-tag" title="支持视频理解">视频</span>{/if}
              {#if caps.audio}<span class="m-tag" title="支持音频理解">音频</span>{/if}
            </span>
          </label>
        {/each}
      </div>
    </div>
  </div>
{/if}

<style>
  .model-list {
    overflow-y: auto;
    border: 1px solid var(--g2);
    border-radius: 8px;
    padding: 6px;
    max-height: 50vh;
  }
  .model-item {
    display: flex;
    gap: 8px;
    align-items: center;
    padding: 4px 6px;
    font-family: Consolas, monospace;
    font-size: 13px;
    cursor: pointer;
  }
  .m-name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .m-caps {
    display: flex;
    gap: 4px;
    font-size: 11px;
  }
  .m-tag {
    background: var(--g2);
    color: var(--g6);
    padding: 1px 6px;
    border-radius: 4px;
    font-family: sans-serif;
  }
</style>
