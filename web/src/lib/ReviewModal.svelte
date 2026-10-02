<script>
  // 插件准入审查报告（用户 2026-10-02 拍板「审核要有一个可视化界面，还要简洁」）：
  // static 审查（后端 review.rs）结论可视化：一眼结论、发现分组、规则说人话。
  // 弹窗由 App.svelte 持有：{ ext, report }；信任被拒（pin 前 blocker）时自动带上报告弹开。
  import { createEventDispatcher } from 'svelte';
  const dispatch = createEventDispatcher();

  export let ext = null; // 插件清单项（{ name, title, origin, trusted, trust_key }）
  export let report = null; // 审核报告（后端 review::Report）
  export let busy = false; // 重新审核中

  // rule → 人话短标签（rule 本身是稳定 id，界面上不裸露）。
  const RULE_LABEL = {
    'review.dir_unreadable': '目录',
    'review.size_files': '体积',
    'review.size_bytes': '体积',
    'review.file_too_big': '体积',
    'review.manifest_missing': '清单',
    'review.manifest_json': '清单',
    'review.manifest_apiversion': '清单',
    'review.manifest_entry_missing': '清单',
    'review.permission_unknown': '权限',
    'review.extension_unknown': '文件',
    'review.network_external': '外联',
    'review.dynamic_exec': '动态执行',
    'review.dynamic_import': '动态执行',
    'review.dynamic_import_remote': '动态执行',
    'review.host_api_direct': '宿主 API',
    'review.host_dom_touch': '宿主 DOM',
    'review.minified_code': '混淆',
  };

  $: blockers = (report?.findings || []).filter((f) => f.severity === 'blocker');
  $: warns = (report?.findings || []).filter((f) => f.severity !== 'blocker');
  $: canTrust = report && report.verdict !== 'blocked' && ext && ext.origin === 'user' && !ext.trusted;

  function fmtBytes(n) {
    if (!n && n !== 0) return '—';
    if (n < 1024) return `${n} 字节`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(n < 10240 ? 1 : 0)} KB`;
    return `${(n / 1024 / 1024).toFixed(1)} MB`;
  }
  function where(f) {
    return f.line ? `${f.file}:${f.line}` : f.file;
  }
</script>

{#if ext && report}
  <div class="modal-backdrop" on:click={() => dispatch('close')}>
    <div class="modal review-modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>审查报告 · {ext.title || ext.name}</b>
        <span class="hint">{report.verdict === 'blocked' ? '有阻断项，装不进来' : canTrust ? '可以信任并固定指纹' : '审查通过'}</span>
        <button class="btn ghost" on:click={() => dispatch('close')}>关闭</button>
      </div>

      <!-- 结论横幅：一眼结论 + 体量 + 指纹状态 -->
      <div class="rv-verdict" class:bad={report.verdict === 'blocked'} class:warn={report.verdict !== 'blocked' && warns.length}>
        <span class="rv-mark">{report.verdict === 'blocked' ? '✕' : '✓'}</span>
        <div class="rv-verdict-text">
          <div class="rv-verdict-title">
            {report.verdict === 'blocked' ? '审核未通过' : '审核通过'}{report.verdict !== 'blocked' && warns.length ? `（${warns.length} 条警告）` : ''}
          </div>
          <div class="rv-verdict-sub">
            扫了 {report.files_scanned} 个文件 / {fmtBytes(report.bytes_scanned)}{report.digest ? ' · 指纹已绑定' : ''}
          </div>
        </div>
      </div>

      <!-- 发现分组：阻断在上，警告在下；行内不摊原则，只看「哪条规则、哪个文件、什么事」 -->
      <div class="rv-body">
        {#if blockers.length}
          <div class="rv-group">
            <div class="rv-group-label bad">阻断 {blockers.length}</div>
            {#each blockers as f}
              <div class="rv-finding bad">
                <span class="rv-rule bad">{RULE_LABEL[f.rule] || f.rule}</span>
                <span class="rv-where">{where(f)}</span>
                <span class="rv-msg">{f.message}</span>
              </div>
            {/each}
          </div>
        {/if}
        {#if warns.length}
          <div class="rv-group">
            <div class="rv-group-label warn">警告 {warns.length}</div>
            {#each warns as f}
              <div class="rv-finding warn">
                <span class="rv-rule warn">{RULE_LABEL[f.rule] || f.rule}</span>
                <span class="rv-where">{where(f)}</span>
                <span class="rv-msg">{f.message}</span>
              </div>
            {/each}
          </div>
        {/if}
        {#if !blockers.length && !warns.length}
          <div class="rv-clean">没有发现问题：清单合规、没有外联、没有动态执行、体积正常。</div>
        {/if}
      </div>

      <div class="rv-note">
        静态审查挡「来路不明、不合规范、要外联」；信任后按内容指纹绑定，文件一改就要重新审核再信任。
      </div>

      <div class="rv-actions">
        {#if busy}<span class="rv-busy">审查中…</span>{/if}
        <button class="btn" on:click={() => dispatch('review')} disabled={busy}>重新审查</button>
        {#if canTrust}
          <button class="btn primary" on:click={() => dispatch('trust')}>信任并固定指纹</button>
        {/if}
        {#if report.verdict === 'blocked'}
          <span class="rv-hint">修掉阻断项再来，或者别装。</span>
        {/if}
      </div>
    </div>
  </div>
{/if}

<style>
  .review-modal {
    width: 620px;
    gap: 10px;
  }
  .rv-verdict {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 12px 14px;
    border-radius: 10px;
    background: var(--tint-green-bg);
    color: var(--tint-green-fg);
  }
  .rv-verdict.bad {
    background: var(--tint-red-bg);
    color: var(--tint-red-fg);
  }
  .rv-verdict.warn {
    background: var(--tint-yellow-bg);
    color: var(--tint-orange-fg);
  }
  .rv-mark {
    flex: none;
    width: 34px;
    height: 34px;
    border-radius: 50%;
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 18px;
    font-weight: 700;
    border: 1.5px solid currentColor;
  }
  .rv-verdict-title {
    font-weight: 700;
    font-size: 15px;
  }
  .rv-verdict-sub {
    font-size: 12px;
    opacity: 0.85;
    margin-top: 1px;
  }
  .rv-body {
    max-height: 46vh;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .rv-group-label {
    font-size: 12px;
    font-weight: 650;
    margin-bottom: 4px;
    color: var(--g6);
  }
  .rv-group-label.bad {
    color: var(--tint-red-fg);
  }
  .rv-group-label.warn {
    color: var(--tint-orange-fg);
  }
  .rv-finding {
    /* 两行结构：规则+文件地点一行、说明一行（flex 行内挤压踩过，块级堆叠最稳） */
    padding: 7px 10px;
    border-radius: 8px;
    background: var(--g1);
    border: 1px solid var(--g3);
    margin-bottom: 4px;
  }
  .rv-finding.bad {
    border-color: var(--tint-red-fg);
  }
  .rv-finding.warn {
    border-color: var(--tint-orange-fg);
  }
  .rv-rule {
    display: inline-block;
    font-size: 11px;
    line-height: 1;
    padding: 3px 7px;
    border-radius: 999px;
    margin-right: 8px;
    vertical-align: 1px;
    background: var(--veil);
    color: var(--g6);
  }
  .rv-rule.bad {
    background: var(--tint-red-bg);
    color: var(--tint-red-fg);
  }
  .rv-rule.warn {
    background: var(--tint-orange-bg);
    color: var(--tint-orange-fg);
  }
  .rv-where {
    font-size: 12px;
    color: var(--g6);
  }
  .rv-msg {
    display: block;
    font-size: 12.5px;
    color: var(--g7);
    line-height: 1.55;
    margin-top: 3px;
    word-break: break-all;
  }
  .rv-clean {
    font-size: 13px;
    color: var(--g6);
    padding: 10px 2px;
  }
  .rv-note {
    font-size: 12px;
    color: var(--g5);
    line-height: 1.5;
  }
  .rv-actions {
    display: flex;
    align-items: center;
    gap: 8px;
    justify-content: flex-end;
  }
  .rv-hint {
    font-size: 12px;
    color: var(--tint-red-fg);
  }
  .rv-busy {
    font-size: 12px;
    color: var(--g5);
    margin-right: auto;
  }
</style>
