<script>
  // 获取到的模型清单弹窗（自 App.svelte 按视图拆出）：勾选状态由宿主的已存模型表决定。
  // 能力徽章（图片/视频/音频）随「按模型名智能推导」机制一并退役——
  // 聚合渠道上名字与真实能力对不上，预测比没有更糟；能力以设置里手动勾选为准。
  export let open = false;
  export let models = [];
  export let saved = []; // 已加入配置的模型名（checked 判定）
  import { createEventDispatcher } from 'svelte';
  const dispatch = createEventDispatcher();
  // checked 必须内联 saved.includes(m)：套函数读闭包变量不进 each 的脏值依赖，
  // saved 变了（全选/全不选）勾选框不重渲染，按钮就像失效
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
          <label class="model-item">
            <input type="checkbox" checked={saved.includes(m)} on:change={() => dispatch('toggle', m)} />
            <span class="m-name">{m}</span>
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
</style>
