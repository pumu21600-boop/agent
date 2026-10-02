<script>
  import { createEventDispatcher } from 'svelte';

  const dispatch = createEventDispatcher();
  export let tab = '';
  export let skills = [];
  export let loading = false;
  // 列表数据(skills)与加载(loadSkills)在 App 侧:能力菜单共用;保存/删除后回酒刷新

  let skillMsg = '';
  let skillEditor = null; // { isNew, name, description, content, source }
  // 用户 2026-10-02：新建输入框不再预填模板（正文自己写；name/description 由保存时回填校验）
  function newSkill() {
    skillEditor = { isNew: true, name: '', description: '', content: '', source: 'project' };
  }
  function editSkill(s) {
    skillEditor = {
      isNew: false,
      name: s.name,
      description: s.description || '',
      content: s.content || '',
      source: s.source || 'project',
    };
  }
  async function saveSkill() {
    if (!skillEditor) return;
    const name = skillEditor.name.trim();
    if (!name) {
      skillMsg = '技能名不能为空';
      return;
    }
    try {
      const r = await fetch('/api/skills', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          action: 'save',
          name,
          description: skillEditor.description,
          content: skillEditor.content,
        }),
      }).then((r) => r.json());
      if (r.error) {
        skillMsg = `保存失败: ${r.error}`;
        return;
      }
      skillMsg = `已保存到 ${r.dir}`;
      skillEditor = null;
      dispatch('reload');
    } catch (e) {
      skillMsg = `保存失败: ${e.message || e}`;
    }
  }
  async function deleteSkill(name) {
    try {
      const r = await fetch('/api/skills', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ action: 'delete', name }),
      }).then((r) => r.json());
      skillMsg = r.error ? `删除失败: ${r.error}` : `已删除 ${name}`;
      dispatch('reload');
    } catch (e) {
      skillMsg = `删除失败: ${e.message || e}`;
    }
  }
</script>

      {#if tab === 'skills'}
        <div class="sub-row">
          <button class="btn primary" on:click={newSkill}>新建技能</button>
          <button class="btn" on:click={() => dispatch('reload')}>刷新</button>
          {#if skillMsg}<span class="hint">{skillMsg}</span>{/if}
        </div>
        {#if skillEditor}
          <div class="field">
            <label>{skillEditor.isNew ? '新建技能' : `编辑技能：${skillEditor.name}`}{skillEditor.source === 'global' ? '（全局技能）' : ''}</label>
            <div class="sub-row">
              <input placeholder="技能名" bind:value={skillEditor.name} style="max-width:200px" disabled={skillEditor.isNew ? false : skillEditor.source === 'global'} />
              <input placeholder="一句话描述" bind:value={skillEditor.description} />
            </div>
            <textarea class="agents-editor" rows="14" spellcheck="false" bind:value={skillEditor.content}></textarea>
            <div class="sub-row">
              <button class="btn primary" on:click={saveSkill}>保存</button>
              <button class="btn" on:click={() => (skillEditor = null)}>取消</button>
            </div>
          </div>
        {/if}
        {#if loading}
          <div class="trace-empty">加载中…</div>
        {:else}
          <!-- 用户拍板（2026-09-30）：插件自带的技能不在这里列出（引导住在插件里，去插件页看），
               这页只管自己建的技能；数据不过滤——composer 的 $ 技能选择菜单仍含插件技能。
               形式对齐插件页 plug-row（2026-10-02）：图标 + 名字 + 一句话描述 + 行内操作 -->
          {#each skills.filter((s) => s.source !== 'plugin') as s (s.name)}
            <div class="plug-row">
              <div class="plug-ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M13 2 4.5 13.5H11L9.5 22 19 10h-6.5z"/></svg></div>
              <div class="plug-meta">
                <div class="plug-title">${s.name}</div>
                <div class="plug-desc">{s.description || '（无描述）'}</div>
              </div>
              <span class="plug-chip">{s.source === 'project' ? '项目级' : s.source === 'plugin' ? (s.plugin ? `插件 · ${s.plugin}` : '插件') : '全局'}</span>
              <span class="flex-sp"></span>
              <button class="btn ghost" on:click={() => editSkill(s)}>编辑</button>
              <button class="btn ghost danger" on:click={() => deleteSkill(s.name)}>删除</button>
            </div>
          {/each}
        {/if}
      {/if}


<style>
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
  .menu-empty {
    color: var(--g5);
    font-size: 12px;
    padding: 8px 10px;
  }
</style>
