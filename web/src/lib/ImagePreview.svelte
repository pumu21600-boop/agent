<script>
  // 大图预览（自 App.svelte 按视图拆出）：src 为空即不渲染，点击遮罩/关闭按钮回传 close
  export let src = null;
  import { createEventDispatcher } from 'svelte';
  const dispatch = createEventDispatcher();
</script>

{#if src}
  <div class="backdrop" on:click={() => dispatch('close')}>
    <div class="image-modal" on:click|stopPropagation>
      <img {src} alt="大图预览" />
      <button class="image-modal-close" on:click={() => dispatch('close')} aria-label="关闭"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18"/></svg></button>
    </div>
  </div>
{/if}

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: var(--overlay);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 20;
  }
  .image-modal {
    position: relative;
    max-width: 90vw;
    max-height: 90vh;
    display: flex;
    justify-content: center;
    align-items: center;
  }
  .image-modal img {
    max-width: 90vw;
    max-height: 90vh;
    object-fit: contain;
    border-radius: 8px;
    box-shadow: 0 10px 30px var(--overlay);
  }
  .image-modal-close {
    position: absolute;
    top: -12px;
    right: -12px;
    width: 28px;
    height: 28px;
    background: var(--g2);
    border: 1px solid var(--g4);
    color: var(--g7);
    border-radius: 50%;
    cursor: pointer;
    font-size: 14px;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .image-modal-close svg {
    width: 13px;
    height: 13px;
    display: block;
  }
</style>
