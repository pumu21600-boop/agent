<script>
  import { onMount, afterUpdate } from 'svelte';

  // —— 设置 ——
  let providers = [];
  let providerId = 'deepseek';
  let baseUrl = '';
  let model = '';
  let apiKey = '';
  let hasKey = false;
  let keyHint = '';
  let savedModels = [];
  let settingsMsg = '';
  let settingsOpen = false;
  let modelMenuOpen = false;
  let keyScopes = {}; // 归一化 base_url → 已存 key 尾号

  // 当前 Base URL 对应的已存 key 尾号（实时）
  $: keyScopeUrl = (baseUrl || '').trim().replace(/\/+$/, '').toLowerCase();
  $: savedKeyHint = keyScopeUrl ? keyScopes[keyScopeUrl] || '' : '';

  // —— 配置方案 ——
  let profiles = [];
  let profileSel = '';
  let profileName = '';

  // —— 获取模型弹窗 ——
  let modelModalOpen = false;
  let fetchedModels = [];
  let picked = new Set();

  // —— 对话 ——
  // 消息骨架：{id, role: 'user'|'assistant'|'error'|'system', content, thinking, streaming}
  // thought / tool 事件（P2 核心循环）将作为新 role 并入此结构。
  let messages = [];
  let input = '';
  let streaming = false;
  let controller = null;
  let seq = 0;
  let chatBody;

  onMount(async () => {
    await loadProviders();
    await loadSettings();
  });

  afterUpdate(() => {
    if (chatBody) chatBody.scrollTop = chatBody.scrollHeight;
  });

  async function loadProviders() {
    const r = await fetch('/api/providers').then((r) => r.json());
    providers = r.providers || [];
  }

  async function loadSettings() {
    const s = await fetch('/api/settings').then((r) => r.json());
    providerId = s.provider_id || 'deepseek';
    baseUrl = s.base_url || '';
    model = s.model || '';
    savedModels = s.models || [];
    hasKey = !!s.has_key;
    keyHint = s.key_hint || '';
    keyScopes = {};
    for (const sc of s.key_scopes || []) keyScopes[sc.url] = sc.hint;
    profiles = s.profiles || [];
    profileSel = s.active_profile_id || '';
    if (!baseUrl) applyDefault();
  }

  async function manageProfiles(body) {
    const r = await fetch('/api/profiles', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    const d = await r.json();
    if (d.error) {
      settingsMsg = `方案操作失败: ${d.error}`;
      return;
    }
    await loadSettings(); // 重拉一次：字段、key 尾号、方案全部同步
  }

  function saveAsProfile() {
    const name = profileName.trim();
    if (!name) {
      settingsMsg = '先填方案名称';
      return;
    }
    manageProfiles({ action: 'save', name });
    profileName = '';
    settingsMsg = '已另存为方案';
  }

  function overwriteProfile() {
    const name = profileName.trim();
    if (!name) {
      settingsMsg = '先填方案名称';
      return;
    }
    manageProfiles({ action: 'save', id: profileSel, name });
    settingsMsg = '已覆盖方案';
  }

  function applyProfile() {
    if (!profileSel) return;
    manageProfiles({ action: 'apply', id: profileSel });
    settingsMsg = '已切换配置方案';
  }

  function deleteProfile() {
    if (!profileSel) return;
    manageProfiles({ action: 'delete', id: profileSel });
    settingsMsg = '已删除方案（当前配置保持不变）';
  }

  function applyDefault() {
    const p = providers.find((x) => x.id === providerId);
    if (p) {
      if (!baseUrl) baseUrl = p.default_base_url || '';
      if (!model) model = p.default_model || '';
    }
  }

  function onProviderChange() {
    const p = providers.find((x) => x.id === providerId);
    if (p) {
      baseUrl = p.default_base_url || '';
      model = p.default_model || '';
    }
  }

  async function pickModel(m) {
    model = m;
    modelMenuOpen = false;
    await fetch('/api/settings', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ model: m }),
    });
  }

  function nextId() {
    return ++seq;
  }

  // —— 获取模型弹窗 ——
  async function fetchModels() {
    settingsMsg = '获取模型中…';
    try {
      const r = await fetch('/api/models', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ base_url: baseUrl, api_key: apiKey }),
      });
      const d = await r.json();
      if (!r.ok) throw new Error(d.error || `HTTP ${r.status}`);
      fetchedModels = d.models || [];
      picked = new Set(fetchedModels.filter((m) => savedModels.includes(m)));
      modelModalOpen = true;
      settingsMsg = '';
    } catch (e) {
      settingsMsg = `失败: ${e.message}`;
    }
  }

  function toggleModel(m) {
    if (picked.has(m)) picked.delete(m);
    else picked.add(m);
    picked = picked;
  }

  function selectAll() {
    picked = new Set(fetchedModels);
  }

  function selectNone() {
    picked = new Set();
  }

  async function saveModels() {
    const list = [...picked].sort();
    if (!list.includes(model) && list.length) model = list[0];
    const r = await fetch('/api/settings', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        provider_id: providerId,
        base_url: baseUrl,
        model,
        models: list,
        api_key: apiKey.trim(),
      }),
    });
    const d = await r.json();
    if (d.error) {
      settingsMsg = `保存失败: ${d.error}`;
      return;
    }
    savedModels = list;
    hasKey = !!d.has_key;
    keyHint = d.key_hint || '';
    apiKey = '';
    modelModalOpen = false;
    settingsMsg = `已保存 ${list.length} 个模型`;
  }

  async function saveSettings() {
    const r = await fetch('/api/settings', {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        provider_id: providerId,
        base_url: baseUrl,
        model,
        models: savedModels,
        api_key: apiKey.trim(),
      }),
    });
    const d = await r.json();
    if (d.error) {
      settingsMsg = `保存失败: ${d.error}`;
      return;
    }
    apiKey = '';
    await loadSettings(); // 刷新 key 尾号提示（按当前 URL）
    settingsMsg = '已保存';
  }

  // —— 对话 ——
  async function send() {
    const msg = input.trim();
    if (!msg || streaming) return;
    streaming = true;
    controller = new AbortController();
    // 历史消息（P2：服务端无状态，前端带 user/assistant 历史；刷新即清，P6 才落盘）
    const history = messages
      .filter((m) => (m.role === 'user' && m.content) || (m.role === 'assistant' && m.content && !m.streaming))
      .map((m) => ({ role: m.role, content: m.content }));
    history.push({ role: 'user', content: msg });
    messages = [
      ...messages,
      { id: nextId(), role: 'user', content: msg },
      { id: nextId(), role: 'assistant', content: '', thinking: '', tools: [], streaming: true },
    ];
    input = '';
    const asstId = seq;
    const patch = (fn) => {
      messages = messages.map((m) => (m.id === asstId ? fn({ ...m }) : m));
    };
    try {
      const res = await fetch('/api/chat', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        // 带上当前选择的模型/base_url：下拉框改了立即生效，不必先点保存
        body: JSON.stringify({ messages: history, model, base_url: baseUrl }),
        signal: controller.signal,
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      let buf = '';
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        buf += decoder.decode(value, { stream: true });
        let idx;
        while ((idx = buf.indexOf('\n\n')) !== -1) {
          const raw = buf.slice(0, idx);
          buf = buf.slice(idx + 2);
          for (const line of raw.split('\n')) {
            if (!line.startsWith('data: ')) continue;
            try {
              const d = JSON.parse(line.slice(6));
              if (d.content) patch((m) => ({ ...m, content: m.content + d.content }));
              else if (d.reasoning) patch((m) => ({ ...m, thinking: m.thinking + d.reasoning }));
              else if (d.tool_call)
                patch((m) => ({
                  ...m,
                  tools: [...(m.tools || []), { name: d.tool_call.name, args: d.tool_call.args, status: 'running' }],
                }));
              else if (d.tool_result)
                patch((m) => {
                  const tools = [...(m.tools || [])];
                  for (let i = tools.length - 1; i >= 0; i--) {
                    if (tools[i].name === d.tool_result.name && tools[i].status === 'running') {
                      tools[i] = { ...tools[i], status: d.tool_result.ok ? 'ok' : 'fail', preview: d.tool_result.preview };
                      break;
                    }
                  }
                  return { ...m, tools };
                });
              else if (d.step_limit) {
                patch((m) => ({ ...m, streaming: false }));
                messages = [...messages, { id: nextId(), role: 'error', content: `已达步数上限 ${d.step_limit}，循环终止` }];
              } else if (d.error) {
                patch((m) => ({ ...m, streaming: false }));
                messages = [...messages, { id: nextId(), role: 'error', content: d.error }];
              } else if (d.cancelled) {
                patch((m) => ({ ...m, streaming: false }));
                messages = [...messages, { id: nextId(), role: 'system', content: '已停止' }];
              } else if (d.done) {
                patch((m) => ({ ...m, streaming: false }));
              }
            } catch (e) {
              // 忽略坏行
            }
          }
        }
      }
    } catch (e) {
      if (e.name !== 'AbortError') {
        messages = [...messages, { id: nextId(), role: 'error', content: `[连接错误] ${e}` }];
      }
      patch((m) => ({ ...m, streaming: false }));
    } finally {
      streaming = false;
      controller = null;
    }
  }

  function stop() {
    fetch('/cancel', { method: 'POST' });
    if (controller) controller.abort();
  }

  function onKey(e) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }
</script>

<div class="app">
  <aside class="sidebar">
    <div class="brand">Agent</div>
    <div class="side-foot">
      <button class="btn ghost" on:click={() => (settingsOpen = true)}>设置</button>
    </div>
  </aside>

  <main class="chat">
    <header class="chat-head">
      <div>
        <div class="chat-title">对话</div>
        <div class="chat-sub">P2 核心循环 · 历史不跨刷新（P6 落盘）</div>
      </div>
    </header>

    <div class="chat-body" bind:this={chatBody}>
      {#if messages.length === 0}
        <div class="empty-hint">输入消息开始对话（Enter 发送）</div>
      {/if}
      {#each messages as m (m.id)}
        {#if m.role === 'user'}
          <div class="msg user">
            <div class="bubble user-bubble">{m.content}</div>
          </div>
        {:else if m.role === 'assistant'}
          <div class="msg assistant">
            <div class="bubble asst-bubble">
              {#if m.thinking}
                <details class="thinking">
                  <summary>思考过程</summary>
                  <div class="thinking-body">{m.thinking}</div>
                </details>
              {/if}
              {#if m.tools && m.tools.length}
                {#each m.tools as t}
                  <div class="tool-card">
                    <div class="tool-head">
                      <span class="tool-badge {t.status}">{t.status === 'running' ? '执行中' : t.status === 'ok' ? '完成' : '失败'}</span>
                      <span class="tool-name">{t.name}</span>
                      <span class="tool-args" title={t.args}>{t.args}</span>
                    </div>
                    {#if t.preview}
                      <pre class="tool-preview">{t.preview}</pre>
                    {/if}
                  </div>
                {/each}
              {/if}
              {#if m.content}
                <div class="content">{m.content}</div>
              {:else if m.streaming}
                <div class="typing">▍</div>
              {/if}
              {#if m.streaming && m.content}<span class="cursor">▍</span>{/if}
            </div>
          </div>
        {:else if m.role === 'error'}
          <div class="msg system">
            <div class="bubble error-bubble">{m.content}</div>
          </div>
        {:else}
          <div class="msg system">
            <div class="sys-text">{m.content}</div>
          </div>
        {/if}
      {/each}
    </div>

    <footer class="chat-input">
      <div class="model-pick">
        <button class="btn" on:click={() => (modelMenuOpen = !modelMenuOpen)}>
          模型：{model || '未选'} ▾
        </button>
        {#if modelMenuOpen}
          <div class="model-menu">
            {#if savedModels.length === 0}
              <div class="menu-empty">暂无已保存模型</div>
            {/if}
            {#each savedModels as m}
              <button class="menu-item" class:active={m === model} on:click={() => pickModel(m)}>
                {m}
              </button>
            {/each}
            <div class="menu-sep"></div>
            <button class="menu-item" on:click={() => { modelMenuOpen = false; settingsOpen = true; }}>
              管理模型…
            </button>
          </div>
        {/if}
      </div>
      <input
        bind:value={input}
        placeholder="输入消息，Enter 发送"
        on:keydown={onKey}
        disabled={streaming}
      />
      {#if streaming}
        <button class="btn danger" on:click={stop}>停止</button>
      {:else}
        <button class="btn primary" on:click={send}>发送</button>
      {/if}
    </footer>
  </main>
</div>

{#if settingsOpen}
  <div class="modal-backdrop" on:click={() => (settingsOpen = false)}>
    <div class="modal settings-modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>模型设置</b>
        <button class="btn ghost" on:click={() => (settingsOpen = false)}>关闭</button>
      </div>
      <div class="field">
        <label>供应商</label>
        <select bind:value={providerId} on:change={onProviderChange}>
          {#each providers as p}
            <option value={p.id}>{p.name}</option>
          {/each}
        </select>
      </div>
      <div class="field">
        <label>Base URL</label>
        <input bind:value={baseUrl} placeholder="留空用预设默认" />
      </div>
      <div class="field">
        <label>API Key（按 Base URL 独立保存，换地址互不串用）</label>
        <input
          bind:value={apiKey}
          type="password"
          placeholder={savedKeyHint
            ? `该地址已保存(…${savedKeyHint})，留空不修改`
            : '此地址还没有 key，填写后保存'}
        />
      </div>
      <div class="field">
        <label>模型</label>
        <select bind:value={model}>
          {#if !savedModels.includes(model) && model}
            <option value={model}>{model}</option>
          {/if}
          {#each savedModels as m}
            <option value={m}>{m}</option>
          {/each}
          {#if !model && !savedModels.length}
            <option value="">（先获取模型）</option>
          {/if}
        </select>
      </div>
      <div class="field">
        <label>配置方案（保存多套供应商+模型组合，一键切换；key 按供应商独立保存，不受方案影响）</label>
        <div class="sub-row">
          <select bind:value={profileSel}>
            <option value="">（当前配置未关联方案）</option>
            {#each profiles as p}
              <option value={p.id}>{p.name}</option>
            {/each}
          </select>
          <button class="btn" on:click={applyProfile} disabled={!profileSel}>应用</button>
          <button class="btn" on:click={overwriteProfile} disabled={!profileSel}>覆盖</button>
          <button class="btn" on:click={deleteProfile} disabled={!profileSel}>删除</button>
        </div>
        <div class="sub-row">
          <input bind:value={profileName} placeholder="方案名称，如：商汤主力 / DS官方" />
          <button class="btn primary" on:click={saveAsProfile}>另存当前为方案</button>
        </div>
      </div>
      <div class="modal-foot">
        <span class="msg">{settingsMsg}</span>
        <button class="btn" on:click={fetchModels}>获取模型</button>
        <button class="btn primary" on:click={saveSettings}>保存</button>
      </div>
    </div>
  </div>
{/if}

{#if modelModalOpen}
  <div class="modal-backdrop" on:click={() => (modelModalOpen = false)}>
    <div class="modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>获取到的模型（{fetchedModels.length} 个）</b>
        <div class="modal-actions">
          <button class="btn" on:click={selectAll}>全选</button>
          <button class="btn" on:click={selectNone}>全不选</button>
          <button class="btn primary" on:click={saveModels}>保存</button>
          <button class="btn ghost" on:click={() => (modelModalOpen = false)}>关闭</button>
        </div>
      </div>
      <div class="model-list">
        {#if fetchedModels.length === 0}
          <div class="empty">渠道未返回任何模型</div>
        {/if}
        {#each fetchedModels as m}
          <label class="model-item">
            <input type="checkbox" checked={picked.has(m)} on:change={() => toggleModel(m)} />
            {m}
          </label>
        {/each}
      </div>
    </div>
  </div>
{/if}

<style>
  :global(body) {
    margin: 0;
    background: #17181c;
    color: #e6e6eb;
    font-family: 'Segoe UI', 'Microsoft YaHei', sans-serif;
  }
  :global(#app) {
    height: 100vh;
  }
  .app {
    display: flex;
    height: 100vh;
  }
  .sidebar {
    width: 220px;
    background: #1d1f26;
    border-right: 1px solid #2a2d37;
    display: flex;
    flex-direction: column;
    padding: 14px 12px;
    gap: 16px;
  }
  .brand {
    font-size: 18px;
    font-weight: 600;
    letter-spacing: 1px;
  }
  .side-status {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .side-label {
    font-size: 11px;
    color: #8b8f9c;
  }
  .side-value {
    font-size: 13px;
  }
  .side-sub {
    font-size: 12px;
    color: #8b8f9c;
    word-break: break-all;
  }
  .side-foot {
    margin-top: auto;
  }
  .chat {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .chat-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 10px 16px;
    border-bottom: 1px solid #2a2d37;
  }
  .chat-title {
    font-size: 14px;
    font-weight: 600;
  }
  .chat-sub {
    font-size: 11px;
    color: #8b8f9c;
  }
  .chat-body {
    flex: 1;
    overflow-y: auto;
    padding: 16px;
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .empty-hint {
    margin: auto;
    color: #5c606b;
    font-size: 13px;
  }
  .msg {
    display: flex;
  }
  .msg.user {
    justify-content: flex-end;
  }
  .msg.system {
    justify-content: center;
  }
  .bubble {
    max-width: 72%;
    padding: 8px 12px;
    border-radius: 10px;
    font-size: 14px;
    line-height: 1.55;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .user-bubble {
    background: #2f5fd0;
    color: #fff;
    border-bottom-right-radius: 3px;
  }
  .asst-bubble {
    background: #23252d;
    border: 1px solid #2f323c;
    border-bottom-left-radius: 3px;
  }
  .error-bubble {
    background: #4a2a2a;
    border: 1px solid #7a3c3c;
    color: #ffb4b4;
  }
  .sys-text {
    font-size: 12px;
    color: #6b707c;
  }
  .thinking {
    margin-bottom: 8px;
  }
  .thinking summary {
    cursor: pointer;
    font-size: 12px;
    color: #9aa0ae;
    user-select: none;
  }
  .thinking-body {
    margin-top: 6px;
    padding: 8px;
    background: #1b1d23;
    border-left: 2px solid #3d4150;
    font-size: 13px;
    color: #9aa0ae;
    white-space: pre-wrap;
  }
  .tool-card {
    margin-bottom: 8px;
    background: #1b1d23;
    border: 1px solid #2f323c;
    border-radius: 8px;
    padding: 6px 8px;
  }
  .tool-head {
    display: flex;
    gap: 8px;
    align-items: center;
    font-size: 12px;
  }
  .tool-badge {
    padding: 1px 6px;
    border-radius: 4px;
    font-size: 11px;
  }
  .tool-badge.running {
    background: #3a3f4d;
    color: #cdd2dd;
  }
  .tool-badge.ok {
    background: #234d2c;
    color: #9fdcae;
  }
  .tool-badge.fail {
    background: #4d2323;
    color: #e0a3a3;
  }
  .tool-name {
    font-family: Consolas, monospace;
    color: #cdd2dd;
  }
  .tool-args {
    color: #6b707c;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 320px;
  }
  .tool-preview {
    margin: 6px 0 0;
    padding: 6px 8px;
    background: #17181c;
    border-radius: 6px;
    font-size: 12px;
    color: #9aa0ae;
    white-space: pre-wrap;
    max-height: 160px;
    overflow-y: auto;
  }
  .cursor {
    animation: blink 1s step-start infinite;
  }
  @keyframes blink {
    50% {
      opacity: 0;
    }
  }
  .typing {
    color: #9aa0ae;
  }
  .chat-input {
    display: flex;
    gap: 8px;
    align-items: center;
    padding: 12px 16px;
    border-top: 1px solid #2a2d37;
    background: #1b1d23;
  }
  .model-pick {
    position: relative;
  }
  .model-menu {
    position: absolute;
    left: 0;
    bottom: calc(100% + 6px);
    min-width: 240px;
    max-height: 320px;
    overflow-y: auto;
    background: #23252d;
    border: 1px solid #333742;
    border-radius: 8px;
    padding: 4px;
    z-index: 30;
    display: flex;
    flex-direction: column;
  }
  .menu-item {
    display: block;
    width: 100%;
    text-align: left;
    padding: 7px 10px;
    background: transparent;
    border: none;
    border-radius: 6px;
    color: #e6e6eb;
    font-family: Consolas, monospace;
    font-size: 13px;
    cursor: pointer;
  }
  .menu-item:hover {
    background: #2a2d37;
  }
  .menu-item.active {
    background: #2f5fd0;
    color: #fff;
  }
  .menu-sep {
    height: 1px;
    background: #2f323c;
    margin: 4px 2px;
  }
  .menu-empty {
    color: #6b707c;
    font-size: 12px;
    padding: 8px 10px;
  }
  .chat-input input {
    flex: 1;
    padding: 9px 12px;
    border-radius: 8px;
    border: 1px solid #333742;
    background: #23252d;
    color: #e6e6eb;
    font-size: 14px;
  }
  .btn {
    padding: 7px 14px;
    border-radius: 8px;
    border: 1px solid #333742;
    background: #2a2d37;
    color: #e6e6eb;
    font-size: 13px;
    cursor: pointer;
  }
  .btn:hover {
    background: #333742;
  }
  .btn.primary {
    background: #2f5fd0;
    border-color: #2f5fd0;
  }
  .btn.danger {
    background: #a33b3b;
    border-color: #a33b3b;
  }
  .btn.ghost {
    background: transparent;
  }
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.55);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 20;
  }
  .modal {
    background: #23252d;
    border: 1px solid #333742;
    border-radius: 10px;
    width: 520px;
    max-height: 85vh;
    display: flex;
    flex-direction: column;
    padding: 16px;
    gap: 12px;
  }
  .modal-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  .modal-actions {
    display: flex;
    gap: 6px;
  }
  .modal-foot {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .modal-foot .msg {
    flex: 1;
    font-size: 12px;
    color: #8b8f9c;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .field label {
    font-size: 12px;
    color: #9aa0ae;
  }
  .field input,
  .field select {
    padding: 8px 10px;
    border-radius: 8px;
    border: 1px solid #333742;
    background: #1b1d23;
    color: #e6e6eb;
    font-size: 13px;
  }
  .sub-row {
    display: flex;
    gap: 6px;
    align-items: center;
    flex-wrap: wrap;
  }
  .sub-row input {
    flex: 1;
    min-width: 140px;
    padding: 8px 10px;
    border-radius: 8px;
    border: 1px solid #333742;
    background: #1b1d23;
    color: #e6e6eb;
    font-size: 13px;
  }
  .model-list {
    overflow-y: auto;
    border: 1px solid #2f323c;
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
  .empty {
    color: #6b707c;
    padding: 12px;
    text-align: center;
  }
</style>
