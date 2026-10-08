<script>
  // 通用确认弹窗：桌面壳（Tauri WebView2）不实现 window.confirm/alert——
  // 原生调用静默无操作（wry 0.55 无 ScriptDialogOpening 处理，WebView2 默认
  // AreDefaultDialogDialogsEnabled=false），浏览器里又各有各的样式。全部确认/提示
  // 统一走应用内弹窗：壳内外行为一致，样式随设计系统。
  // modal 由宿主持有：{ text, okOnly, resolve }；resolve 由宿主在 answer 事件里调。
  export let modal = null;
  import { createEventDispatcher } from 'svelte';
  const dispatch = createEventDispatcher();
  function answer(ok) {
    dispatch('answer', ok);
  }
  function onKey(e) {
    if (e.key === 'Enter') answer(true);
    else if (e.key === 'Escape') answer(modal && modal.okOnly ? true : false);
  }
</script>

<svelte:window on:keydown={onKey} />

{#if modal}
  <div class="modal-backdrop" on:click={() => answer(modal.okOnly ? true : false)}>
    <div class="modal confirm-modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>{modal.okOnly ? '提示' : '确认'}</b>
      </div>
      <div class="confirm-text">{modal.text}</div>
      <div class="confirm-actions">
        {#if !modal.okOnly}
          <button class="btn" on:click={() => answer(false)}>取消</button>
        {/if}
        <button class="btn primary" on:click={() => answer(true)}>{modal.okOnly ? '知道了' : '确定'}</button>
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
</style>
