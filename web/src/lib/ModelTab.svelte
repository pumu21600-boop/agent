<script>
  import Select from '../Select.svelte';

  export let tab = '';
  // 模型配置分区的状态本体在 App 侧（模型快选菜单/上下文圆环/发送路径都读同一份），
  // 这里只做设置分区的界面半边：输入框全部 bind 回根，保存动作调根的 autoSave
  export let providers = [];
  export let configs = [];
  export let activeConfigId = '';
  export let configLabel = () => '';
  export let manageConfig = () => {};
  export let newConfig = () => {};
  export let deleteConfig = () => {};
  export let providerId = '';
  export let onProviderChange = () => {};
  export let baseUrl = '';
  export let apiKey = '';
  export let typingHint = '';
  export let clearKey = () => {};
  export let configName = '';
  export let model = '';
  export let savedModels = [];
  export let fetchModels = () => {};
  export let ctxLen = 0;
  export let maxOut = 0;
  export let temperature = null;
  export let topP = null;
  export let capabilities = { tools: true, vision: false, video: false, audio: false };
  export let autoSave = () => {};
</script>

{#if tab === 'model'}
  <div class="field">
    <label>配置</label>
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
      <button class="chip add" on:click={newConfig}>新建配置</button>
      <button class="chip del" on:click={deleteConfig}>删除当前</button>
    </div>
  </div>

  <div class="field">
    <label>配置名称</label>
    <input
      data-field="configName"
      bind:value={configName}
      placeholder="给这套配置起个名，如：商汤主力 / tokeness 免费"
      on:input={() => autoSave({ name: configName })}
    />
  </div>

  <div class="field">
    <label>供应商</label>
    <Select
      value={providerId}
      title="供应商预设"
      options={[
        ...providers.map((p) => ({ value: p.id, label: p.name })),
        // 老渠道可能还挂在已下架的预设 id 上：回显为「旧预设」，不主动改它
        ...(providerId && !providers.some((p) => p.id === providerId)
          ? [{ value: providerId, label: providerId + '（旧预设）' }]
          : []),
      ]}
      on:change={(e) => {
        providerId = e.detail;
        onProviderChange();
      }}
    />
  </div>

  <div class="field">
    <label>Base URL</label>
    <input
      data-field="baseUrl"
      bind:value={baseUrl}
      placeholder="留空用预设默认"
      on:input={() => autoSave({ base_url: baseUrl })}
    />
  </div>

  <div class="field">
    <label>API Key</label>
    <div class="sub-row">
      <input
        bind:value={apiKey}
        type="password"
        style="flex: 1;"
        placeholder={typingHint
          ? `本配置已保存(…${typingHint})，重新输入即覆盖`
          : '此配置还没有 key，填入即自动保存'}
        on:blur={() => apiKey.trim() && autoSave({})}
        on:keydown={(e) => e.key === 'Enter' && apiKey.trim() && autoSave({})}
      />
      {#if typingHint}
        <button class="btn del-key" on:click={clearKey} title="清除此配置保存的 API Key">清除 Key</button>
      {/if}
    </div>
  </div>

  <div class="field">
    <label>模型</label>
    <div class="sub-row">
      <Select
        value={model}
        placeholder="（点右侧获取模型）"
        title="当前模型"
        options={[
          ...(model && !savedModels.includes(model) ? [{ value: model, label: model }] : []),
          ...savedModels.map((m) => ({ value: m, label: m })),
        ]}
        on:change={(e) => {
          model = e.detail;
          autoSave({ model }, 0);
        }}
      />
      <button class="btn primary" on:click={fetchModels}>获取模型</button>
    </div>
  </div>

  <div class="field">
    <label>模型参数（0=恢复默认）</label>
    <div class="sub-row">
      <input
        class="num-input"
        type="number"
        min="0"
        data-field="ctxLen"
        bind:value={ctxLen}
        title="上下文长度：决定圆环预算与历史裁剪时机"
        on:change={() => autoSave({ context_length: Number(ctxLen) || 0 }, 0)}
      />
      <input
        class="num-input"
        type="number"
        min="0"
        data-field="maxOut"
        bind:value={maxOut}
        title="最大输出长度：随请求作为 max_tokens 发给渠道。思考开启时自动加思考余量（配置值+max(配置值,8192)）——上游把思考和正文算进同一个额度，不加余量正文会被思考挤断；端点拒收会自动回退原值"
        on:change={() => autoSave({ max_output_tokens: Number(maxOut) || 0 }, 0)}
      />
    </div>
  </div>

  <div class="field">
    <label>采样参数（留空=渠道默认）</label>
    <div class="sub-row">
      <input
        class="num-input"
        type="number" step="0.1" min="0" max="2"
        placeholder="温度"
        data-field="temperature"
        bind:value={temperature}
        title="temperature：越低越稳定，0=确定性输出"
        on:change={() => autoSave({ temperature: temperature === null || temperature === '' ? -1 : Number(temperature) }, 0)}
      />
      <input
        class="num-input"
        type="number" step="0.05" min="0" max="1"
        placeholder="top_p"
        data-field="topP"
        bind:value={topP}
        title="top_p：核采样截断"
        on:change={() => autoSave({ top_p: topP === null || topP === '' ? -1 : Number(topP) }, 0)}
      />
    </div>
    <div class="param-hint">-1 = 清除该项</div>
  </div>

  <div class="field">
    <label>模型能力标记</label>
    <div class="caps-row">
      <label class="cap-check">
        <input
          type="checkbox"
          bind:checked={capabilities.tools}
          on:change={() => autoSave({ capabilities }, 0)}
        />
        工具调用
      </label>
      <label class="cap-check">
        <input
          type="checkbox"
          bind:checked={capabilities.vision}
          on:change={() => autoSave({ capabilities }, 0)}
        />
        图片解析
      </label>
      <label class="cap-check">
        <input
          type="checkbox"
          bind:checked={capabilities.video}
          on:change={() => autoSave({ capabilities }, 0)}
        />
        视频解析
      </label>
      <label class="cap-check">
        <input
          type="checkbox"
          bind:checked={capabilities.audio}
          on:change={() => autoSave({ capabilities }, 0)}
        />
        音频解析
      </label>
    </div>
  </div>
{/if}

<style>
  .chips {
    display: flex;
    gap: 9px;
    flex-wrap: wrap;
  }
  .chip {
    padding: 7px 13px;
    border-radius: 14px;
    border: 1px solid var(--g3);
    background: var(--g1);
    color: var(--g6);
    font-size: 12px;
    cursor: pointer;
  }
  .chip.active {
    background: var(--g3);
    border-color: var(--g5);
    color: var(--g7);
  }
  .dot {
    display: inline-block;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    margin-right: 6px;
    background: var(--g4);
    vertical-align: middle;
  }
  .dot.haskey {
    background: var(--tint-green-fg);
  }
  /* 设置面板统一黑白：彩色只留给「已存 Key」的绿点（.dot.haskey） */
  .chip.add {
    color: var(--g6);
    border-color: var(--g3);
  }
  .chip.add:hover {
    color: var(--g7);
    border-color: var(--g5);
  }
  .chip.del {
    color: var(--g6);
    border-color: var(--g3);
  }
  .chip.del:hover {
    color: var(--g7);
    border-color: var(--g5);
  }
  .del-key {
    padding: 7px 10px;
    font-size: 12px;
    background: var(--g2);
    border-color: var(--g4);
    color: var(--g7);
  }
  .del-key:hover {
    background: var(--g3);
    border-color: var(--g5);
  }
  .caps-row {
    display: flex;
    gap: 14px;
    flex-wrap: wrap;
    background: var(--g1);
    border: 1px solid var(--g2);
    border-radius: 8px;
    padding: 8px 12px;
  }
  .cap-check {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    color: var(--g6);
    cursor: pointer;
  }
  .cap-check input {
    cursor: pointer;
  }
  .num-input {
    width: 100%;
    background: var(--g1);
    color: var(--g7);
    border: 1px solid var(--g2);
    border-radius: 6px;
    padding: 6px 8px;
    font-size: 12px;
  }
  /* 自绘下拉（Select 组件）在各容器里的尺寸：组件内部样式管外观，这里只调整体布局 */
  .field > :global(.csel-wrap) {
    width: 100%;
  }
  .sub-row > :global(.csel-wrap) {
    flex: 1 1 auto;
    width: auto;
  }
</style>
