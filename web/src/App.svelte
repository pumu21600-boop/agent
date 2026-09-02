<script>
  import { onMount, afterUpdate } from 'svelte';

  // —— 配置（改动即自动保存即时生效）——
  let providers = [];
  let configs = [];
  let activeConfigId = '';
  let providerId = 'deepseek';
  let baseUrl = '';
  let model = '';
  let apiKey = '';
  let configName = '';
  let savedModels = [];
  let savedKeyUrl = '';   // 当前激活配置的已存 key 尾号
  let settingsMsg = '';
  let settingsOpen = false;
  let autoSaveTimer = null;
  let pendingSave = null; // autoSave 挂起的字段（切换配置前 flush 用）
  let approvalMode = (() => {
    // Codex 式三档；旧档名自动迁移
    const map = { suggest: 'read-only', 'auto-edit': 'auto', 'full-auto': 'auto', local: 'full-access' };
    const saved = localStorage.getItem('approvalMode');
    return map[saved] || saved || 'auto';
  })(); // 审批三档：read-only / auto / full-access
  let chatPinned = true; // 是否钉在对话底部（决定是否自动滚屏）
  let ctxUsage = { tokens: 0, budget: 0 }; // 上下文占用（P5 圆环）

  // —— 获取模型弹窗 ——
  let modelModalOpen = false;
  let fetchedModels = [];

  // —— 模型快选（输入栏）——
  let modelMenuOpen = false;

  // —— 对话 ——
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
    // 只有用户本来就钉在底部时才自动滚屏；上滑阅读时不打断
    if (chatPinned && chatBody) chatBody.scrollTop = chatBody.scrollHeight;
  });

  function onChatScroll() {
    if (!chatBody) return;
    chatPinned = chatBody.scrollTop + chatBody.clientHeight >= chatBody.scrollHeight - 60;
  }

  function scrollToBottom() {
    if (!chatBody) return;
    chatPinned = true;
    chatBody.scrollTo({ top: chatBody.scrollHeight, behavior: 'smooth' });
  }

  function norm(url) {
    return (url || '').trim().replace(/\/+$/, '').toLowerCase();
  }

  async function loadProviders() {
    const r = await fetch('/api/providers').then((r) => r.json());
    providers = r.providers || [];
  }

  async function loadSettings() {
    const s = await fetch('/api/settings').then((r) => r.json());
    applyState(s);
  }

  function applyState(s) {
    providerId = s.provider_id || 'deepseek';
    baseUrl = s.base_url || '';
    model = s.model || '';
    configName = s.name || '';
    savedModels = s.models || [];
    configs = s.configs || [];
    activeConfigId = s.active_config_id || '';
    // key 按配置独占：hint 直接用当前激活配置的，与其他配置无关
    savedKeyUrl = s.key_hint || '';
  }

  // 当前激活配置是否已有 key（随配置切换刷新，不随地址输入实时变化）
  $: typingHint = savedKeyUrl;

  function flashSaved() {
    settingsMsg = '已自动保存 ' + new Date().toTimeString().slice(0, 8);
  }

  // 防抖自动保存设置字段；挂起的字段先记下来，切换配置前必须落盘到当前配置
  function autoSave(fields, delay = 500) {
    clearTimeout(autoSaveTimer);
    pendingSave = fields;
    autoSaveTimer = setTimeout(async () => {
      const body = { ...fields };
      if (apiKey.trim()) {
        body.api_key = apiKey.trim();
      }
      pendingSave = null;
      await sendSettings(body);
    }, delay);
  }

  // 把挂起的防抖保存立即落盘（落给此刻的激活配置，防止串到切换后的配置）
  async function flushPendingSave() {
    clearTimeout(autoSaveTimer);
    if (!pendingSave) return;
    const body = { ...pendingSave };
    pendingSave = null;
    if (apiKey.trim()) body.api_key = apiKey.trim();
    await sendSettings(body);
  }

  async function sendSettings(body) {
    try {
      const r = await fetch('/api/settings', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      const d = await r.json();
      if (d.error) {
        settingsMsg = `保存失败: ${d.error}`;
        return;
      }
      apiKey = '';
      await loadSettings();
      flashSaved();
    } catch (e) {
      settingsMsg = `保存失败: ${e}`;
    }
  }

  // —— 配置切换 / 新建 / 删除 ——
  async function manageConfig(action, id) {
    // 先把挂起的字段改动落给当前配置，再切——防抖的尾巴不能甩到新配置头上
    await flushPendingSave();
    const r = await fetch('/api/configs', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ action, id }),
    });
    const d = await r.json();
    if (d.error) {
      settingsMsg = `操作失败: ${d.error}`;
      return;
    }
    applyState(d);
    flashSaved();
  }

  function newConfig() {
    manageConfig('new');
  }

  function deleteConfig() {
    if (configs.length <= 1) {
      settingsMsg = '至少保留一套配置';
      return;
    }
    manageConfig('delete', activeConfigId);
  }

  function configLabel(c) {
    return c.name || '未命名';
  }

  function onProviderChange() {
    providerId = providerId; // bind 已更新
    autoSave({ provider_id: providerId });
  }

  // —— 获取模型 ——
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
      modelModalOpen = true;
      settingsMsg = '';
    } catch (e) {
      settingsMsg = `获取模型失败: ${e.message}`;
    }
  }

  function isChecked(m) {
    return savedModels.includes(m);
  }

  async function toggleModel(m) {
    const list = savedModels.includes(m)
      ? savedModels.filter((x) => x !== m)
      : [...savedModels, m].sort();
    savedModels = list; // 乐观更新
    if (list.length && !list.includes(model)) model = list[0];
    await sendSettings({ models: list, model, api_key: apiKey.trim() || undefined });
  }

  async function selectAllModels() {
    const list = [...fetchedModels].sort();
    savedModels = list;
    if (!list.includes(model) && list.length) model = list[0];
    await sendSettings({ models: list, model });
  }

  async function selectNoModels() {
    savedModels = [];
    model = '';
    await sendSettings({ models: [], model: '' });
  }

  // —— 模型快选 ——
  async function pickModel(m) {
    model = m;
    modelMenuOpen = false;
    await sendSettings({ model: m });
  }

  // —— 对话 ——
  async function send() {
    const msg = input.trim();
    if (!msg || streaming) return;
    streaming = true;
    controller = new AbortController();
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
        body: JSON.stringify({ messages: history, model, base_url: baseUrl, approval_mode: approvalMode }),
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
              else if (d.approval_request)
                patch((m) => ({
                  ...m,
                  approval: { id: d.approval_request.id, tool: d.approval_request.tool, args: d.approval_request.args, decided: null },
                }));
              else if (d.approval_decided)
                patch((m) =>
                  m.approval && m.approval.id === d.approval_decided.id
                    ? { ...m, approval: { ...m.approval, decided: d.approval_decided.approved } }
                    : m
                );
              else if (d.notice)
                messages = [...messages, { id: nextId(), role: 'system', content: '⚠️ ' + d.notice }];
              else if (d.context_usage)
                ctxUsage = { tokens: d.context_usage.tokens, budget: d.context_usage.budget };
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
      // 停止后把还挂着的审批框标为失效，避免界面停在「等待决定」
      messages = messages.map((m) =>
        m.approval && m.approval.decided === null ? { ...m, approval: { ...m.approval, decided: false } } : m
      );
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

  // 审批决定：本地乐观标记 + 通知后端（后端也会回 approval_decided，幂等）
  async function decideApproval(id, approved) {
    patchMessageApproval(id, approved);
    await fetch('/api/approvals', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id, approved }),
    });
  }
  function patchMessageApproval(id, approved) {
    messages = messages.map((m) =>
      m.approval && m.approval.id === id ? { ...m, approval: { ...m.approval, decided: approved } } : m
    );
  }

  function nextId() {
    return ++seq;
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
    <div class="side-approval">
      <label class="side-label">审批档位</label>
      <select
        class:danger={approvalMode === 'full-access'}
        bind:value={approvalMode}
        on:change={() => {
          localStorage.setItem('approvalMode', approvalMode);
          // 即时同步到后端：进行中的请求下一次工具调用就按新档位走
          fetch('/api/approval-mode', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ mode: approvalMode }),
          });
        }}
      >
        <option value="read-only">只读（改动要批）</option>
        <option value="auto">自动（沙箱内全自动）</option>
        <option value="full-access">完全访问（脱沙箱！）</option>
      </select>
    </div>
    <div class="side-foot">
      <button class="btn ghost" on:click={() => (settingsOpen = true)}>设置</button>
    </div>
  </aside>

  <main class="chat">
    <header class="chat-head">
      <div>
        <div class="chat-title">对话</div>
        <div class="chat-sub">P5 上下文工程 · 历史不跨刷新（P6 落盘）</div>
      </div>
      {#if ctxUsage.budget > 0}
        {@const pct = Math.min(100, Math.round((ctxUsage.tokens / ctxUsage.budget) * 100))}
        {@const r = 15.9}
        {@const circ = 2 * Math.PI * r}
        {@const color = pct >= 85 ? '#f87171' : pct >= 60 ? '#facc15' : '#4ade80'}
        <div
          class="ctx-ring"
          title={`上下文占用约 ${ctxUsage.tokens} / ${ctxUsage.budget} tokens（估算值），超过预算自动裁剪历史`}
        >
          <svg viewBox="0 0 36 36" width="42" height="42">
            <circle cx="18" cy="18" {r} fill="none" stroke="#2f323c" stroke-width="3.6" />
            <circle
              cx="18"
              cy="18"
              {r}
              fill="none"
              stroke={color}
              stroke-width="3.6"
              stroke-linecap="round"
              stroke-dasharray={`${(circ * pct) / 100} ${circ}`}
              transform="rotate(-90 18 18)"
            />
          </svg>
          <span class="ctx-pct" style:color={color}>{pct}%</span>
        </div>
      {/if}
    </header>

    <div class="chat-body" bind:this={chatBody} on:scroll={onChatScroll}>
      {#if messages.length === 0}
        <div class="empty-hint">输入消息开始对话（Enter 发送）</div>
      {/if}
      {#each messages as m (m.id)}
        {#if m.role === 'user'}
          <div class="msg user"><div class="bubble user-bubble">{m.content}</div></div>
        {:else if m.role === 'assistant'}
          <div class="msg assistant">
            <div class="bubble asst-bubble">
              {#if m.thinking}
                <details class="thinking">
                  <summary>思考过程</summary>
                  <div class="thinking-body">{m.thinking}</div>
                </details>
              {/if}
              {#if m.approval}
                <div class="approval-box">
                  <div class="approval-head">
                    ⚠️ 请求执行：<b>{m.approval.tool}</b>
                    <span class="tool-args" title={m.approval.args}>{m.approval.args}</span>
                  </div>
                  {#if m.approval.decided === null}
                    <div class="approval-actions">
                      <button class="btn" on:click={() => decideApproval(m.approval.id, true)}>允许</button>
                      <button class="btn ghost" on:click={() => decideApproval(m.approval.id, false)}>拒绝</button>
                      <span class="approval-hint">等你的决定，120 秒不选自动拒绝</span>
                    </div>
                  {:else}
                    <div class="approval-state">{m.approval.decided ? '✓ 已允许' : '✕ 已拒绝'}</div>
                  {/if}
                </div>
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
          <div class="msg system"><div class="bubble error-bubble">{m.content}</div></div>
        {:else}
          <div class="msg system"><div class="sys-text">{m.content}</div></div>
        {/if}
      {/each}
    </div>

    {#if !chatPinned}
      <button class="jump-bottom" on:click={scrollToBottom} title="回到底部">↓ 最新消息</button>
    {/if}

    <footer class="chat-input">
      <div class="model-pick">
        <button class="btn" on:click={() => (modelMenuOpen = !modelMenuOpen)}>
          模型：{model || '未选'} ▾
        </button>
        {#if modelMenuOpen}
          <div class="model-menu">
            {#if savedModels.length === 0}
              <div class="menu-empty">暂无模型，先到设置里获取</div>
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
    <div class="modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>模型设置</b>
        <span class="hint">{settingsMsg}</span>
        <button class="btn ghost" on:click={() => (settingsOpen = false)}>关闭</button>
      </div>

      <div class="field">
        <label>配置（点一下切换，改动自动保存即时生效）</label>
        <div class="chips">
          {#each configs as c}
            <button
              class="chip"
              class:active={c.id === activeConfigId}
              title={configLabel(c) + ' · ' + (c.model || '未选模型') + (c.has_key ? ' · 有key' : ' · 无key')}
              on:click={() => manageConfig('apply', c.id)}
            >
              <span class="dot" class:haskey={c.has_key}></span>{configLabel(c)}
            </button>
          {/each}
          <button class="chip add" on:click={newConfig}>＋ 新建配置</button>
          <button class="chip del" on:click={deleteConfig}>✕ 删除当前</button>
        </div>
      </div>

      <div class="field">
        <label>配置名称</label>
        <input
          bind:value={configName}
          placeholder="给这套配置起个名，如：商汤主力 / tokeness 免费"
          on:input={() => autoSave({ name: configName })}
        />
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
        <input
          bind:value={baseUrl}
          placeholder="留空用预设默认"
          on:input={() => autoSave({ base_url: baseUrl })}
        />
      </div>

      <div class="field">
        <label>API Key（按配置独立保存）</label>
        <input
          bind:value={apiKey}
          type="password"
          placeholder={typingHint
            ? `本配置已保存(…${typingHint})，重新输入即覆盖`
            : '此配置还没有 key，填入即自动保存'}
          on:blur={() => apiKey.trim() && autoSave({})}
          on:keydown={(e) => e.key === 'Enter' && apiKey.trim() && autoSave({})}
        />
      </div>

      <div class="field">
        <label>模型</label>
        <div class="sub-row">
          <select
            bind:value={model}
            on:change={() => autoSave({ model }, 0)}
          >
            {#if !savedModels.includes(model) && model}
              <option value={model}>{model}</option>
            {/if}
            {#each savedModels as m}
              <option value={m}>{m}</option>
            {/each}
            {#if !model && !savedModels.length}
              <option value="">（点右侧获取模型）</option>
            {/if}
          </select>
          <button class="btn primary" on:click={fetchModels}>获取模型</button>
        </div>
      </div>
    </div>
  </div>
{/if}

{#if modelModalOpen}
  <div class="modal-backdrop" on:click={() => (modelModalOpen = false)}>
    <div class="modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>获取到的模型（{fetchedModels.length} 个，勾选即加入）</b>
        <div class="modal-actions">
          <button class="btn" on:click={selectAllModels}>全选</button>
          <button class="btn" on:click={selectNoModels}>全不选</button>
          <button class="btn ghost" on:click={() => (modelModalOpen = false)}>完成</button>
        </div>
      </div>
      <div class="model-list">
        {#if fetchedModels.length === 0}
          <div class="empty">渠道未返回任何模型</div>
        {/if}
        {#each fetchedModels as m}
          <label class="model-item">
            <input type="checkbox" checked={isChecked(m)} on:change={() => toggleModel(m)} />
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
    width: 200px;
    background: #1d1f26;
    border-right: 1px solid #2a2d37;
    display: flex;
    flex-direction: column;
    padding: 14px 12px;
  }
  .brand {
    font-size: 18px;
    font-weight: 600;
    letter-spacing: 1px;
  }
  .side-foot {
    margin-top: auto;
  }
  .chat {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-width: 0;
    position: relative;
  }
  .jump-bottom {
    position: absolute;
    bottom: 78px;
    left: 50%;
    transform: translateX(-50%);
    z-index: 6;
    padding: 6px 14px;
    border-radius: 16px;
    border: 1px solid #3d4150;
    background: #1b1d23;
    color: #cdd2dd;
    font-size: 12px;
    cursor: pointer;
    box-shadow: 0 4px 12px rgba(0, 0, 0, 0.4);
  }
  .jump-bottom:hover {
    border-color: #5a6070;
  }
  .ctx-ring {
    position: relative;
    width: 42px;
    height: 42px;
    margin-left: auto;
  }
  .ctx-pct {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 10px;
    font-weight: 600;
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
  .approval-box {
    margin-bottom: 8px;
    background: #241f14;
    border: 1px solid #6b5a24;
    border-radius: 8px;
    padding: 8px;
    font-size: 13px;
  }
  .approval-head {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
    color: #e8d9a0;
  }
  .approval-actions {
    display: flex;
    gap: 8px;
    margin-top: 8px;
  }
  .approval-state {
    margin-top: 6px;
    color: #9aa0ae;
    font-size: 12px;
  }
  .approval-hint {
    color: #6f7686;
    font-size: 11px;
    align-self: center;
  }
  .side-approval {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 0 12px;
  }
  .side-label {
    font-size: 11px;
    color: #9aa0ae;
  }
  .side-approval select {
    background: #1b1d23;
    color: #cdd2dd;
    border: 1px solid #2f323c;
    border-radius: 6px;
    padding: 4px 6px;
    font-size: 12px;
  }
  .side-approval select.danger {
    border-color: #b3403a;
    color: #ff7b72;
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
  .chat-input input {
    flex: 1;
    padding: 9px 12px;
    border-radius: 8px;
    border: 1px solid #333742;
    background: #23252d;
    color: #e6e6eb;
    font-size: 14px;
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
    width: 560px;
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
    gap: 10px;
  }
  .modal-head .hint {
    flex: 1;
    text-align: right;
    font-size: 12px;
    color: #8b8f9c;
  }
  .modal-actions {
    display: flex;
    gap: 6px;
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
  .chips {
    display: flex;
    gap: 6px;
    flex-wrap: wrap;
  }
  .chip {
    padding: 5px 10px;
    border-radius: 14px;
    border: 1px solid #333742;
    background: #1b1d23;
    color: #b8bdc9;
    font-size: 12px;
    cursor: pointer;
  }
  .chip.active {
    background: #2f5fd0;
    border-color: #2f5fd0;
    color: #fff;
  }
  .dot {
    display: inline-block;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    margin-right: 6px;
    background: #5c606b;
    vertical-align: middle;
  }
  .dot.haskey {
    background: #57c467;
  }
  .chip.add {
    color: #9fdcae;
    border-color: #2f4d3a;
  }
  .chip.del {
    color: #e0a3a3;
    border-color: #4d2f2f;
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
