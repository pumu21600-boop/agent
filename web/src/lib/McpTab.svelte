<script>
  import Select from '../Select.svelte';
  import { createEventDispatcher } from 'svelte';

  const dispatch = createEventDispatcher();
  export let tab = '';
  export let servers = [];
  // 动作统一回 App 执行(那边握着 loadMcp 与 settingsMsg);parseHeaders 是表单本地助手
  function mcpAction(action, name, server) { dispatch('action', { action, name, server }); }

  // 021：transport 二选一。stdio=本地子进程；http=远端 MCP（Streamable HTTP）
  // P24：http 再加认证方式（无/自定义头、OAuth 授权码、OAuth 客户端凭据）
  let mcpForm = {
    name: '',
    transport: 'stdio',
    command: '',
    args: '',
    url: '',
    headers: '',
    oauth: 'none',
    oauth_client_id: '',
    oauth_client_secret: '',
    oauth_scope: '',
  };
  // 表单里的 headers 写法：每行 `Key: Value`（值里可以有冒号，按第一个冒号切）
  function parseHeaders(text) {
    const out = {};
    for (const line of (text || '').split('\n')) {
      const t = line.trim();
      if (!t) continue;
      const i = t.indexOf(':');
      if (i > 0) out[t.slice(0, i).trim()] = t.slice(i + 1).trim();
    }
    return out;
  }
  function addMcpServer() {
    const isHttp = mcpForm.transport === 'http' || mcpForm.transport === 'sse';
    const args = mcpForm.args.trim() ? mcpForm.args.trim().split(/\s+/) : [];
    // P24：选了 OAuth 就带 oauth 配置（授权码可留空 client_id 走动态注册）
    const oauth =
      isHttp && mcpForm.oauth !== 'none'
        ? {
            type: mcpForm.oauth,
            client_id: mcpForm.oauth_client_id.trim(),
            client_secret: mcpForm.oauth_client_secret.trim(),
            scope: mcpForm.oauth_scope.trim(),
          }
        : undefined;
    const srv = isHttp
      ? {
          name: mcpForm.name.trim(),
          transport: mcpForm.transport,
          url: mcpForm.url.trim(),
          headers: parseHeaders(mcpForm.headers),
          ...(oauth ? { oauth } : {}),
        }
      : {
          name: mcpForm.name.trim(),
          transport: 'stdio',
          command: mcpForm.command.trim(),
          args,
        };
    mcpAction('save', null, null, srv);
    mcpForm = {
      name: '',
      transport: mcpForm.transport,
      command: '',
      args: '',
      url: '',
      headers: '',
      oauth: 'none',
      oauth_client_id: '',
      oauth_client_secret: '',
      oauth_scope: '',
    };
  }
</script>

      {#if tab === 'mcp'}
        {#each servers as sv}
          <div class="plug-row">
            <div class="plug-ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="4" y="4" width="16" height="7" rx="2"/><rect x="4" y="13" width="16" height="7" rx="2"/><circle cx="8" cy="7.5" r="0.9" fill="currentColor" stroke="none"/><circle cx="8" cy="16.5" r="0.9" fill="currentColor" stroke="none"/></svg></div>
            <div class="plug-meta">
              <div class="plug-title">{sv.name}</div>
              <div class="plug-desc">
                {sv.transport === 'stdio' ? `${sv.command} ${(sv.args || []).join(' ')}` : `${sv.transport.toUpperCase()} · ${sv.url}`}
              </div>
            </div>
            <span class="plug-chip" class:warn={!sv.online}>{sv.online ? '在线' : '离线'}</span>
            {#if !sv.confirmed}<span class="plug-chip warn">待确认</span>{/if}
            <span class="flex-sp"></span>
            <button class="btn ghost" on:click={() => mcpAction('refresh', sv.name)}>重连</button>
            <button class="btn ghost danger" on:click={() => mcpAction('remove', sv.name)}>删除</button>
            <button class="switch" class:on={sv.enabled} role="switch" aria-checked={sv.enabled} aria-label="{sv.name} 启用/停用" on:click={() => mcpAction('toggle', sv.name)}><span class="knob"></span></button>
          </div>
          {#if sv.oauth}
            <div class="sub-row mcp-oauth">
              <span class="mcp-status">
                {#if sv.oauth_status?.status === 'pending'}授权中…
                {:else if sv.oauth_status?.status === 'error'}授权失败
                {:else if sv.oauth_status?.oauth?.state === 'authorized'}已授权
                {:else if sv.oauth_status?.oauth?.state === 'expiring'}即将过期（自动续）
                {:else}未授权{/if}
              </span>
              <button class="btn ghost" on:click={() => mcpAction('oauth_authorize', sv.name)}>
                {sv.oauth_status?.oauth?.state === 'none' || !sv.oauth_status?.oauth ? '授权' : '重新授权'}
              </button>
              {#if sv.oauth_status?.oauth && sv.oauth_status.oauth.state !== 'none'}
                <button class="btn ghost danger" on:click={() => mcpAction('oauth_revoke', sv.name)}>撤销</button>
              {/if}
              {#if sv.oauth_status?.status === 'pending' && sv.oauth_status?.authorization_url}
                <a href={sv.oauth_status.authorization_url} target="_blank" rel="noreferrer">点这里完成授权</a>
              {/if}
              {#if sv.oauth_status?.status === 'error'}
                <span class="mcp-status" style="color: var(--tint-red-fg, #e5484d)">{sv.oauth_status.message}</span>
              {/if}
            </div>
          {/if}
          {#if !sv.confirmed}
            <!-- 首次接入确认：工具清单过目后启用（审批跟随全局档位，2026-10-04 去掉每服务档） -->
            <div class="mcp-confirm">
              {#if sv.tools.length}
                <div class="mcp-tools-list">
                  <b>发现 {sv.tools.length} 个工具，请确认后启用：</b>
                  {#each sv.tools as t}
                    <div class="mcp-tool-item"><code>{t.name}</code> {t.desc}</div>
                  {/each}
                </div>
                <div class="sub-row">
                  <button class="btn primary" on:click={() => mcpAction('confirm', sv.name)}>确认启用</button>
                </div>
              {:else}
                <div class="sub-row mcp-oauth">
                  <span class="mcp-status">未发现工具</span>
                  <button class="btn ghost" on:click={() => mcpAction('refresh', sv.name)}>连接并获取工具清单</button>
                </div>
              {/if}
            </div>
          {:else}
            <div class="mcp-tools-list">
              {#each sv.tools as t}
                <div class="mcp-tool-item"><code>mcp__{sv.name}__{t.name}</code> {t.desc}</div>
              {/each}
              {#if !sv.tools.length}<span class="mcp-status">未发现工具——点「重连」尝试</span>{/if}
              {#if sv.resources && sv.resources.length}
                <div class="mcp-tool-item"><b>资源 {sv.resources.length}</b>（模型可用 list_resources / read_resource 读取）</div>
                {#each sv.resources.slice(0, 8) as r}
                  <div class="mcp-tool-item"><code>{r}</code></div>
                {/each}
              {/if}
              {#if sv.prompts && sv.prompts.length}
                <div class="mcp-tool-item"><b>提示词模板 {sv.prompts.length}</b>（模型可用 list_prompts / get_prompt 取用）</div>
                {#each sv.prompts.slice(0, 8) as p}
                  <div class="mcp-tool-item"><code>{p}</code></div>
                {/each}
              {/if}
            </div>
          {/if}
        {/each}
        <div class="field">
          <label>添加 MCP server</label>
          <div class="sub-row">
            <input placeholder="名称（字母数字-_）" bind:value={mcpForm.name} style="max-width:160px" />
            <Select
              value={mcpForm.transport}
              title="transport"
              options={[
                { value: 'stdio', label: 'stdio（本地子进程）' },
                { value: 'http', label: 'http（远端 MCP）' },
                { value: 'sse', label: 'sse（远端 MCP·SSE 长连接）' },
              ]}
              on:change={(e) => (mcpForm.transport = e.detail)}
            />
            {#if mcpForm.transport === 'http' || mcpForm.transport === 'sse'}
              <input placeholder="https://host/mcp" bind:value={mcpForm.url} />
            {:else}
              <input placeholder="命令，如 node / npx" bind:value={mcpForm.command} />
              <input placeholder="参数（空格分隔）" bind:value={mcpForm.args} />
            {/if}
            <button
              class="btn primary"
              on:click={addMcpServer}
              disabled={!mcpForm.name.trim() ||
                (mcpForm.transport === 'stdio' ? !mcpForm.command.trim() : !mcpForm.url.trim())}>添加</button
            >
          </div>
          {#if mcpForm.transport === 'http' || mcpForm.transport === 'sse'}
            <div class="sub-row">
              <textarea
                placeholder={'自定义请求头，每行一条：\nAuthorization: Bearer xxx'}
                bind:value={mcpForm.headers}
                rows="2"
              ></textarea>
            </div>
            <div class="sub-row">
              <Select
                value={mcpForm.oauth}
                title="认证方式"
                options={[
                  { value: 'none', label: '无 / 自定义请求头' },
                  { value: 'authorization_code', label: 'OAuth 授权码（浏览器登录）' },
                  { value: 'client_credentials', label: 'OAuth 客户端凭据' },
                ]}
                on:change={(e) => (mcpForm.oauth = e.detail)}
              />
            </div>
            {#if mcpForm.oauth !== 'none'}
              <div class="sub-row">
                <input placeholder="client_id（留空=动态注册）" bind:value={mcpForm.oauth_client_id} style="max-width:220px" />
                {#if mcpForm.oauth === 'client_credentials'}
                  <input placeholder="client_secret" type="password" bind:value={mcpForm.oauth_client_secret} style="max-width:220px" />
                {/if}
                <input placeholder="scope（可选）" bind:value={mcpForm.oauth_scope} style="max-width:160px" />
              </div>
            {/if}
          {/if}
        </div>
      {/if}

<style>
  .mcp-oauth {
    margin: -2px 4px 8px 46px;
  }
  .mcp-status {
    font-size: 11px;
    color: var(--g5);
  }
  .mcp-tools-list {
    font-size: 12px;
    color: var(--g6);
    display: flex;
    flex-direction: column;
    gap: 3px;
    margin: 6px 0 8px 46px;
  }
  .mcp-tool-item code {
    background: var(--g2);
    border: 1px solid var(--g3);
    border-radius: 4px;
    padding: 0 4px;
    margin-right: 6px;
    font-size: 11px;
  }
  .mcp-confirm {
    margin-top: 6px;
  }
</style>
