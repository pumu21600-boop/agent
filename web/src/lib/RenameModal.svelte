<script>
  // 重命名弹窗（P19 自 App.svelte 按视图拆出）：kind=session 会话 / project 项目。
  // modal 对象由宿主持有（{ kind, value }），输入直接改 value（对象按引用共享）。
  export let modal = null;
  import { createEventDispatcher } from 'svelte';
  const dispatch = createEventDispatcher();
  function submit() {
    dispatch('submit');
  }
  function close() {
    dispatch('close');
  }
</script>

{#if modal}
  <div class="modal-backdrop" on:click={close}>
    <div class="modal rename-modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>{modal.kind === 'session' ? '重命名会话' : '重命名项目'}</b>
        <button class="btn ghost" on:click={close}>关闭</button>
      </div>
      <div class="field">
        <input
          class="rename-input"
          bind:value={modal.value}
          placeholder={modal.kind === 'project' ? '留空恢复为文件夹名' : '会话标题'}
          on:keydown={(e) => e.key === 'Enter' && submit()}
        />
      </div>
      <div class="rename-actions">
        <button class="btn" on:click={close}>取消</button>
        <button class="btn primary" on:click={submit}>确定</button>
      </div>
    </div>
  </div>
{/if}

<style>
  .rename-modal {
    min-width: 380px;
  }
  .rename-input {
    width: 100%;
    box-sizing: border-box;
    background: var(--g1);
    border: 1px solid var(--g2);
    border-radius: 10px;
    color: var(--g7);
    font-size: 14px;
    padding: 9px 12px;
    outline: none;
  }
  .rename-input:focus {
    border-color: var(--g3);
  }
  .rename-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
</style>
