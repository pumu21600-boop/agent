<script>
  export let tab = '';
  // 插件分区的数据与动作都在 App 侧：extList/extMsg 与右侧栏 dock、"/"菜单、组件总线
  // 共用一份，动作函数（开关/转正/丢弃/删除/新建）改的也是那份根状态，这里只做界面半边
  export let extMsg = '';
  export let builtInExts = [];
  export let thirdPartyExts = [];
  export let extInfo = { app_dir: '', workspace_dir: '' };
  export let loadExt = () => {};
  export let createExt = () => {};
  export let cancelCreateExt = () => {};
  export let extCreateOpen = false;
  export let extCreateForm = { name: '', description: '' };
  export let openExt = () => {};
  export let openExtPage = () => {};
  export let isDockExt = () => false;
  export let extTip = () => '';
  export let extIcon = () => '';
  export let toggleExt = () => {};
  export let promoteExt = () => {};
  export let discardExt = () => {};
  export let removeExt = () => {};
  // 宿主能力两卡：git 运行时 / 沙箱自检（2026-10-06 重整后不是插件，留在设置页）
  export let gitRuntime = null;
  export let gitSourceLabel = () => '';
  export let loadGitRuntime = () => {};
  export let sandboxInfo = null;
  export let sandboxProbe = null;
  export let sandboxProbeBusy = false;
  export let runSandboxProbe = () => {};
  export let sandboxProbeTip = () => '';
  export const GIT_INSTALL_HINT = '装一个 Git for Windows（https://git-scm.com/downloads）后重启本软件即可恢复';
</script>

{#if tab === 'ext'}
  {#if extMsg}<div class="mcp-cmd">{extMsg}</div>{/if}

  <!-- 用户 2026-10-05：「新建插件」与技能/MCP 同款——表单默认收起，点开才占地方 -->
  <div class="sub-row">
    <button class="btn primary" on:click={() => (extCreateOpen = true)}>新建插件</button>
    <button class="btn" on:click={() => loadExt()}>刷新</button>
  </div>
  {#if extCreateOpen}
    <div class="field">
      <label>新建插件（工作区）</label>
      <div class="sub-row">
        <input placeholder="名称（字母数字-_）" bind:value={extCreateForm.name} style="max-width:160px" />
        <input placeholder="一句话描述" bind:value={extCreateForm.description} />
        <button class="btn primary" on:click={createExt} disabled={!extCreateForm.name.trim()}>创建</button>
        <button class="btn" on:click={cancelCreateExt}>取消</button>
      </div>
    </div>
  {/if}

  <!-- 权限放开轮（用户 2026-10-02 拍板）：内置能力已全部插件化——Git/沙箱/功能桥接/
     css-ui工具/记忆库/断线取消 现在是 plugins/ 下的真插件（功能总开关 = 插件开关，
     host 侧 toggle_plugin 同步写 settings.features），此行起全是插件，没有特例行。
     2026-10-06 重整：git-panel / sandbox-panel 移出插件位（能力留在宿主），5 个纯
     技能插件降级成全局技能（住技能页），此行只剩 9 个插件 -->
  <div class="group-label">插件 {builtInExts.length}</div>

  {#each builtInExts as e}
    <div class="plug-row" title={extTip(e)}>
      <div class="plug-ico">
        {#if extIcon(e.name)}<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">{@html extIcon(e.name)}</svg>{:else}{(e.title || e.name || '?').slice(0, 1).toUpperCase()}{/if}
      </div>
      <div class="plug-meta">
        <div class="plug-title">{e.title}</div>
        <div class="plug-desc">{e.description || e.dir}</div>
      </div>
      {#if e.draft}<span class="plug-chip warn">草稿</span>{/if}
      {#if e.origin === 'model_generated'}<span class="plug-chip">模型自产</span>{/if}
      {#if e.source === 'workspace'}<span class="plug-chip" title={e.shadowed ? '同名内置插件被工作区这份覆盖' : ''}>{e.shadowed ? '覆盖内置' : '工作区'}</span>{/if}
      {#if !e.ok}<span class="plug-chip warn">加载失败</span>{/if}
      <span class="flex-sp"></span>
      {#if e.draft}
        <button class="btn ghost" on:click={() => promoteExt(e)}>转正</button>
        <button class="btn ghost" on:click={() => discardExt(e)}>丢弃</button>
      {:else}
        {#if e.settings_entry && !isDockExt(e)}<button class="btn ghost" on:click={() => openExtPage(e, e.settings_entry)} disabled={!e.ok || e.enabled === false}>设置</button>{/if}
        {#if e.entry && !isDockExt(e)}<button class="btn ghost" on:click={() => openExt(e)} disabled={!e.ok || e.enabled === false}>打开</button>{/if}
        {#if e.origin === 'user'}
          <button class="btn ghost danger" on:click={() => removeExt(e)} title="删除插件目录（不可恢复）">删除</button>
        {/if}
        <button class="switch" class:on={e.enabled !== false} role="switch" aria-checked={e.enabled !== false} aria-label="{e.title} 启用/停用" on:click={() => toggleExt(e)}><span class="knob"></span></button>
      {/if}
    </div>
    {#if e.error}<div class="plug-sub">{e.error}</div>{/if}
  {/each}

  {#if thirdPartyExts.length}
    <div class="group-label">第三方 {thirdPartyExts.length}</div>
    {#each thirdPartyExts as e}
      <div class="plug-row" title={extTip(e)}>
        <div class="plug-ico">
          {#if extIcon(e.name)}<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">{@html extIcon(e.name)}</svg>{:else}{(e.title || e.name || '?').slice(0, 1).toUpperCase()}{/if}
        </div>
        <div class="plug-meta">
          <div class="plug-title">{e.title}</div>
          <div class="plug-desc">{e.description || e.dir}</div>
        </div>
        {#if e.draft}<span class="plug-chip warn">草稿</span>{/if}
        {#if e.origin === 'model_generated'}<span class="plug-chip">模型自产</span>{/if}
        {#if e.source === 'workspace'}<span class="plug-chip" title={e.shadowed ? '同名内置插件被工作区这份覆盖' : ''}>{e.shadowed ? '覆盖内置' : '工作区'}</span>{/if}
        {#if !e.ok}<span class="plug-chip warn">加载失败</span>{/if}
        <span class="flex-sp"></span>
        {#if e.draft}
          <button class="btn ghost" on:click={() => promoteExt(e)}>转正</button>
          <button class="btn ghost" on:click={() => discardExt(e)}>丢弃</button>
        {:else}
          {#if e.settings_entry && !isDockExt(e)}<button class="btn ghost" on:click={() => openExtPage(e, e.settings_entry)} disabled={!e.ok || e.enabled === false}>设置</button>{/if}
          {#if e.entry && !isDockExt(e)}<button class="btn ghost" on:click={() => openExt(e)} disabled={!e.ok || e.enabled === false}>打开</button>{/if}
          {#if e.origin === 'user'}
            <button class="btn ghost danger" on:click={() => removeExt(e)} title="删除插件目录，不可恢复">删除</button>
          {/if}
          <button class="switch" class:on={e.enabled !== false} role="switch" aria-checked={e.enabled !== false} aria-label="{e.title} 启用/停用" on:click={() => toggleExt(e)}><span class="knob"></span></button>
        {/if}
      </div>
      {#if e.error}<div class="plug-sub">{e.error}</div>{/if}
    {/each}
  {/if}
  <!-- 宿主能力（2026-10-06 重整后不再是插件，留在设置页）：Git 运行时状态卡片从
     git-panel 插件分区搬来（哪个 git 在生效 + 重新检测）；沙箱自检同理 -->
  <div class="group-label">宿主能力</div>
  <div class="plug-row" title="git.exe 发现链：环境变量 PAWCH_AGENT_GIT_PATH（旧名 AGENT_GIT_PATH 兼容一个版本）→ PATH → 注册表 → 常见安装位置 → exe 同级 tools/git → 内置 zip 懒解压">
    <div class="plug-ico">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="6" cy="6" r="2.4"/><circle cx="6" cy="18" r="2.4"/><circle cx="18" cy="8" r="2.4"/><path d="M6 8.4v7.2M18 10.4c0 3-2.5 3.6-6 3.6"/></svg>
    </div>
    <div class="plug-meta">
      <div class="plug-title">Git 运行时</div>
      {#if gitRuntime && gitRuntime.ok}
        <div class="plug-desc">{gitSourceLabel(gitRuntime.source)}{gitRuntime.version ? ' ' + gitRuntime.version : ''} · {gitRuntime.path}</div>
      {:else if gitRuntime}
        <!-- 缺 git 不许静默：这条就是安装指引 -->
        <div class="plug-desc warn-text">没检测到 git，撤销/检查点/改动/审查用不了。{GIT_INSTALL_HINT}。</div>
      {:else}
        <div class="plug-desc">读取中…</div>
      {/if}
    </div>
    <span class="flex-sp"></span>
    <button class="btn ghost" disabled={!gitRuntime} on:click={() => loadGitRuntime(true)} title="重新跑一遍 git.exe 发现链">重新检测</button>
  </div>
  <div class="plug-row" title="沙箱执行机制归内核（Job Objects 整树击杀 + 受限 token）；这里看生效档位、跑自检">
    <div class="plug-ico">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3l7 3v5c0 4.5-3 8.4-7 10-4-1.6-7-5.5-7-10V6z"/><path d="M9.2 12l2 2 3.6-3.8"/></svg>
    </div>
    <div class="plug-meta">
      <div class="plug-title">沙箱自检</div>
      {#if sandboxInfo}
        <div class="plug-desc">生效档位：{sandboxInfo.label || sandboxInfo.effective}{sandboxInfo.fallback_reason ? `（${sandboxInfo.fallback_reason}）` : ''}</div>
      {:else}
        <div class="plug-desc">读取中…</div>
      {/if}
    </div>
    <span class="flex-sp"></span>
    <button class="btn ghost" disabled={sandboxProbeBusy} on:click={runSandboxProbe} title="真拉起受限进程跑探针套件，验证隔离机制（Job Objects / 受限 token）">{sandboxProbeBusy ? '检测中…' : '检测'}</button>
    {#if sandboxProbe}
      {#if sandboxProbe.error}
        <span class="plug-chip warn" title={sandboxProbe.error}>检测失败</span>
      {:else}
        <span class="plug-chip" class:ok={sandboxProbe.ok} class:warn={!sandboxProbe.ok} title={sandboxProbeTip(sandboxProbe)}>{sandboxProbe.ok ? '通过' : '有失败'}</span>
      {/if}
    {/if}
  </div>
  <div class="param-hint">目录：内置 {extInfo.app_dir || '（读取中）'} ｜ 工作区 {extInfo.workspace_dir || '（读取中）'}</div>
{/if}

<style>
  .mcp-cmd {
    font-family: Consolas, monospace;
    font-size: 11px;
    color: var(--g5);
    margin: 6px 0;
    word-break: break-all;
  }
  /* 插件行描述里的警示色（宿主能力行缺 git 提示用，与 plug-chip.warn 同一只色） */
  .plug-desc.warn-text {
    color: var(--tint-orange-fg);
  }
</style>
