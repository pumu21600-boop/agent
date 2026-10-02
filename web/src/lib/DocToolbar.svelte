<script>
  // 文档类统一工具条（用户 2026-10-02 第五轮，对齐 PDF 查看器那套组件）：
  // 缩放（缩小/百分比/放大/重置）、打印。docx/xlsx/pptx/odf 预览共用。
  // 打印对 iframe（docx 沙箱 iframe / xlsx-pptx 渲染区）由调用方经 on:print 执行。
  import { createEventDispatcher } from 'svelte';
  const dispatch = createEventDispatcher();
  export let zoom = 100; // 百分比
  export let label = ''; // 附加信息（如「第 1/3 页」之类）
  function setZoom(z) {
    zoom = Math.max(50, Math.min(200, z));
    dispatch('zoom', zoom);
  }
</script>

<div class="doc-toolbar">
  <svg class="doc-tb-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 6h16M4 12h10M4 18h13"/></svg>
  <span class="flex-sp"></span>
  <button class="doc-tb-btn" title="缩小" on:click={() => setZoom(zoom - 10)}>−</button>
  <span class="doc-tb-zoom">{zoom}%</span>
  <button class="doc-tb-btn" title="放大" on:click={() => setZoom(zoom + 10)}>＋</button>
  <button class="doc-tb-btn" title="恢复原始大小" on:click={() => setZoom(100)}>
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 12a9 9 0 1 0 3-6.7"/><path d="M3 4v4h4"/></svg>
  </button>
  {#if label}<span class="doc-tb-label">{label}</span>{/if}
  <button class="doc-tb-btn" title="打印" on:click={() => dispatch('print')}>
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M6 9V3h12v6"/><rect x="3" y="9" width="18" height="8" rx="1.5"/><path d="M7 17h10v4H7z"/></svg>
  </button>
</div>

<style>
  .doc-toolbar {
    flex: none;
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 6px 10px;
    border-bottom: 1px solid var(--g3);
    background: var(--g1);
  }
  .doc-tb-ico {
    width: 15px;
    height: 15px;
    color: var(--g5);
  }
  .doc-tb-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    min-width: 24px;
    height: 24px;
    padding: 0 5px;
    border: none;
    border-radius: 6px;
    background: transparent;
    color: var(--g6);
    font-size: 13px;
    cursor: pointer;
  }
  .doc-tb-btn:hover {
    background: var(--g3);
    color: var(--g7);
  }
  .doc-tb-btn svg {
    width: 15px;
    height: 15px;
  }
  .doc-tb-zoom {
    font-size: 11.5px;
    color: var(--g5);
    min-width: 40px;
    text-align: center;
  }
  .doc-tb-label {
    font-size: 11.5px;
    color: var(--g5);
  }
</style>
