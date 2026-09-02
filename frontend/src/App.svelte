<script>
  import { onMount } from 'svelte';

  // —— 模型设置 ——
  let providers = [];
  let providerId = 'deepseek';
  let baseUrl = '';
  let model = '';
  let apiKey = '';
  let hasKey = false;
  let savedModels = [];
  let settingsMsg = '';

  // —— 获取模型弹窗 ——
  let modalOpen = false;
  let fetchedModels = [];
  let picked = new Set();

  // —— 对话 ——
  let message = '';
  let output = '';
  let streaming = false;
  let controller = null;

  onMount(async () => {
    await loadProviders();
    await loadSettings();
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
    if (!baseUrl) applyDefault();
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

  // —— 弹窗：获取模型 → 多选 → 全选 → 保存 ——
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
      // 默认勾选已保存过的；没有已保存则全不勾
      picked = new Set(fetchedModels.filter((m) => savedModels.includes(m)));
      modalOpen = true;
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
    apiKey = '';
    modalOpen = false;
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
    hasKey = !!d.has_key;
    apiKey = '';
    settingsMsg = '已保存';
  }

  async function send() {
    const msg = message.trim();
    if (!msg || streaming) return;
    streaming = true;
    controller = new AbortController();
    output += `\n[你] ${msg}\n[模型] `;
    message = '';
    try {
      const res = await fetch('/api/chat', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        // 带上当前选择的模型/base_url：下拉框改了立即生效，不必先点保存
        body: JSON.stringify({ message: msg, model, base_url: baseUrl }),
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
              if (d.content) output += d.content;
              else if (d.reasoning) output += d.reasoning;
              else if (d.error) output += `\n[错误] ${d.error}`;
              else if (d.cancelled) output += '\n[已停止]';
            } catch (e) {
              // 忽略坏行
            }
          }
        }
      }
    } catch (e) {
      if (e.name !== 'AbortError') output += `\n[连接错误] ${e}`;
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

<main>
  <h3>Agent</h3>

  <section class="settings">
    <div class="row">
      <label>
        供应商
        <select bind:value={providerId} on:change={onProviderChange}>
          {#each providers as p}
            <option value={p.id}>{p.name}</option>
          {/each}
        </select>
      </label>
      <label>
        Base URL
        <input bind:value={baseUrl} placeholder="留空用预设默认" />
      </label>
    </div>
    <div class="row">
      <label>
        API Key
        <input
          bind:value={apiKey}
          type="password"
          placeholder={hasKey ? '已保存（留空不修改）' : 'sk-...'}
        />
      </label>
      <label>
        模型
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
      </label>
      <button on:click={fetchModels}>获取模型</button>
      <button on:click={saveSettings}>保存</button>
    </div>
    <div class="msg">{settingsMsg}</div>
  </section>

  <section class="chat">
    <div class="row">
      <input bind:value={message} placeholder="输入消息，Enter 发送" on:keydown={onKey} />
      <button on:click={send} disabled={streaming}>发送</button>
      <button on:click={stop} disabled={!streaming}>停止</button>
    </div>
    <pre>{output}</pre>
  </section>

  {#if modalOpen}
    <div class="modal-backdrop" on:click={() => (modalOpen = false)}>
      <div class="modal" on:click|stopPropagation>
        <div class="modal-head">
          <b>获取到的模型（{fetchedModels.length} 个）</b>
          <div class="modal-actions">
            <button on:click={selectAll}>全选</button>
            <button on:click={selectNone}>全不选</button>
            <button on:click={saveModels}>保存</button>
            <button on:click={() => (modalOpen = false)}>关闭</button>
          </div>
        </div>
        <div class="modal-list">
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
</main>

<style>
  main {
    font-family: sans-serif;
    max-width: 900px;
    margin: 24px auto;
  }
  .settings {
    border: 1px solid #ccc;
    padding: 10px;
    margin-bottom: 12px;
  }
  .row {
    display: flex;
    gap: 8px;
    margin-bottom: 8px;
    flex-wrap: wrap;
    align-items: center;
  }
  label {
    font-size: 13px;
  }
  input,
  select {
    padding: 4px;
  }
  .settings input {
    width: 260px;
  }
  .chat input {
    flex: 1;
    padding: 6px;
  }
  .msg {
    font-size: 12px;
    color: #666;
    min-height: 16px;
  }
  pre {
    white-space: pre-wrap;
    border: 1px solid #ccc;
    min-height: 200px;
    padding: 8px;
  }
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.4);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 10;
  }
  .modal {
    background: #fff;
    border-radius: 6px;
    width: 560px;
    max-height: 80vh;
    display: flex;
    flex-direction: column;
    padding: 12px;
  }
  .modal-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 8px;
  }
  .modal-actions {
    display: flex;
    gap: 6px;
  }
  .modal-list {
    overflow-y: auto;
    border: 1px solid #eee;
    padding: 6px;
  }
  .model-item {
    display: flex;
    gap: 6px;
    align-items: center;
    padding: 3px 0;
    font-family: monospace;
    font-size: 13px;
  }
  .empty {
    color: #999;
    padding: 12px;
    text-align: center;
  }
</style>
