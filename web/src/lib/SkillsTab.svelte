<script>
  import { createEventDispatcher } from 'svelte';

  const dispatch = createEventDispatcher();
  export let tab = '';
  export let skills = [];
  export let loading = false;
  // 列表数据(skills)与加载(loadSkills)在 App 侧:能力菜单共用;保存/删除后回酒刷新

  let skillMsg = '';
  let skillEditor = null; // { isNew, name, description, content, source }
  function newSkill() {
    skillEditor = {
      isNew: true,
      name: '',
      description: '',
      content: '---\nname: \ndescription: \n---\n\n1. 步骤一\n2. 步骤二\n',
      source: 'project',
    };
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
        <div class="param-hint">消息里写 $技能名 指定</div>
        <div class="sub-row">
          <button class="btn primary" on:click={newSkill}>新建技能</button>
          <button class="btn" on:click={() => dispatch('reload')}>刷新</button>
          <span class="hint">{skillMsg}</span>
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
          {#each skills as s (s.name)}
            <div class="mcp-card">
              <div class="mcp-head">
                <b>${s.name}</b>
                <span class="mcp-status">{s.source === 'project' ? '项目级' : '全局'}</span>
                <span class="flex-sp"></span>
                <button class="btn ghost" on:click={() => editSkill(s)}>编辑</button>
                <button class="btn ghost danger" on:click={() => deleteSkill(s.name)}>删除</button>
              </div>
              <div class="mcp-cmd">{s.description || '（无描述）'}</div>
            </div>
          {:else}
            <div class="menu-empty">还没有技能：点「新建技能」，或在 skills/ 目录放 &lt;名字&gt;/SKILL.md</div>
          {/each}
        {/if}
      {/if}


<style>
  .menu-empty {
    color: var(--g5);
    font-size: 12px;
    padding: 8px 10px;
  }
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
  .mcp-card {
    border: 1px solid var(--g3);
    border-radius: 10px;
    padding: 10px 12px;
  }
  .mcp-head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    color: var(--g7);
  }
  .mcp-status {
    font-size: 11px;
    color: var(--g5);
  }
  .mcp-cmd {
    font-family: Consolas, monospace;
    font-size: 11px;
    color: var(--g5);
    margin: 6px 0;
    word-break: break-all;
  }
</style>
