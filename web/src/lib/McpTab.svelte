<script>
  import Select from '../Select.svelte';
  import { createEventDispatcher } from 'svelte';

  const dispatch = createEventDispatcher();
  export let tab = '';
  export let servers = [];
  // 动作统一回 App 执行(那边握着 loadMcp 与 settingsMsg);parseHeaders 是表单本地助手
  function mcpAction(action, name, risk, server) { dispatch('action', { action, name, risk, server }); }

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
  let mcpRiskChoice = 'normal';
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
        <div class="param-hint">工具调用名：mcp__服务__工具</div>
        {#each servers as sv}
          <div class="field mcp-card">
            <div class="mcp-head">
              <span class="dot" class:haskey={sv.online}></span>
              <b>{sv.name}</b>
              <span class="mcp-status">{sv.online ? '在线' : '离线'}</span>
              <span class="mcp-risk">{sv.risk === 'dangerous' ? '高危档' : '常规档'}{sv.confirmed ? '' : ' · 待确认'}</span>
              <span class="flex-sp"></span>
              <button class="btn ghost" on:click={() => mcpAction('toggle', sv.name)}>{sv.enabled ? '停用' : '启用'}</button>
              <button class="btn ghost" on:click={() => mcpAction('refresh', sv.name)}>重连</button>
              <button class="btn ghost danger" on:click={() => mcpAction('remove', sv.name)}>删除</button>
            </div>
            <div class="mcp-cmd">
              {sv.transport === 'stdio'
                ? `${sv.command} ${(sv.args || []).join(' ')}`
                : `${sv.transport.toUpperCase()} · ${sv.url}`}
            </div>
            {#if sv.oauth}
              <div class="sub-row">
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
              </div>
              {#if sv.oauth_status?.status === 'pending' && sv.oauth_status?.authorization_url}
                <div class="param-hint">
                  浏览器没自动打开？<a href={sv.oauth_status.authorization_url} target="_blank" rel="noreferrer">点这里完成授权</a>
                </div>
              {/if}
              {#if sv.oauth_status?.status === 'error'}
                <div class="param-hint" style="color: var(--tint-red-fg, #e5484d)">{sv.oauth_status.message}</div>
              {/if}
            {/if}
            {#if !sv.confirmed}
              <div class="mcp-confirm">
                {#if sv.tools.length}
                  <div class="mcp-tools-list">
                    <b>发现 {sv.tools.length} 个工具，请确认后启用：</b>
                    {#each sv.tools as t}
                      <div class="mcp-tool-item"><code>{t.name}</code> {t.desc}</div>
                    {/each}
                  </div>
                  <div class="sub-row">
                    <label class="mcp-risk-label">审批档：</label>
                    <Select
                      value={mcpRiskChoice}
                      title="MCP 调用审批档"
                      options={[
                        { value: 'normal', label: '常规（auto 档自动放行）' },
                        { value: 'dangerous', label: '高危（每次调用都弹窗）' },
                      ]}
                      on:change={(e) => (mcpRiskChoice = e.detail)}
                    />
                    <button class="btn primary" on:click={() => mcpAction('confirm', sv.name, mcpRiskChoice)}>确认启用</button>
                  </div>
                {:else}
                  <button class="btn primary" on:click={() => mcpAction('refresh', sv.name)}>连接并获取工具清单</button>
                {/if}
              </div>
            {:else}
              <div class="mcp-tools-list">
                {#each sv.tools as t}
                  <div class="mcp-tool-item"><code>mcp__{sv.name}__{t.name}</code> {t.desc}</div>
                {/each}
                {#if !sv.tools.length}<span class="hint">（未发现工具——点「重连」尝试）</span>{/if}
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
          </div>
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
              <div class="param-hint">
                授权码流程会打开浏览器登录（动态注册自动办 client_id）；token 自动续期，过期重授权即可。
              </div>
            {/if}
          {:else}
            <div class="param-hint">例：npx -y @modelcontextprotocol/server-filesystem &lt;目录&gt;</div>
          {/if}
        </div>
      {/if}

<style>
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
  .mcp-risk {
    font-size: 11px;
    color: var(--tint-yellow-fg);
    background: var(--tint-yellow-bg);
    border-radius: 4px;
    padding: 1px 6px;
  }
  .mcp-cmd {
    font-family: Consolas, monospace;
    font-size: 11px;
    color: var(--g5);
    margin: 6px 0;
    word-break: break-all;
  }
  .mcp-tools-list {
    font-size: 12px;
    color: var(--g6);
    display: flex;
    flex-direction: column;
    gap: 3px;
    margin: 6px 0;
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
  .mcp-risk-label {
    font-size: 12px;
    color: var(--g6);
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
</style>
