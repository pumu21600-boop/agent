<script>
  import Select from '../Select.svelte';

  export let tab = '';
  // 功能桥接（角色路由）：features.roles 关掉时整个分区收起（App 侧也会把页签切走）
  export let rolesShown = true;
  export let configs = [];
  export let configLabel = () => '';
  // roles 本体在 App 侧（applyState 灌、后端 settings.roles），这里改完整体回传
  export let roles = {};
  export let autoSave = () => {};
</script>

{#if tab === 'roles' && rolesShown}
  <div class="field roles-section">
    <label>功能桥接</label>
    <div class="roles-grid">
      <div class="role-item">
        <span class="role-name">会话摘要</span>
        <Select
          value={roles.summary ?? null}
          title="会话摘要角色"
          options={[
            { value: null, label: '未配置：由主力渠道代跑摘要' },
            ...configs.map((c) => ({ value: c.id, label: `${configLabel(c)} · ${c.model || '未选模型'}` })),
          ]}
          on:change={(e) => {
            roles = { ...roles, summary: e.detail };
            autoSave({ roles }, 0);
          }}
        />
      </div>
      <div class="role-item">
        <span class="role-name">标题生成</span>
        <Select
          value={roles.title ?? null}
          title="标题生成角色"
          options={[
            { value: null, label: '未配置：由主力渠道代跑标题' },
            ...configs.map((c) => ({ value: c.id, label: `${configLabel(c)} · ${c.model || '未选模型'}` })),
          ]}
          on:change={(e) => {
            roles = { ...roles, title: e.detail };
            autoSave({ roles }, 0);
          }}
        />
      </div>
      <div class="role-item">
        <span class="role-name">向量嵌入</span>
        <Select
          value={roles.embedding ?? null}
          title="向量嵌入角色（记忆库语义检索用）"
          options={[
            { value: null, label: '未配置：记忆库用本地哈希向量（离线可用）' },
            ...configs.map((c) => ({ value: c.id, label: `${configLabel(c)} · ${c.model || '未选模型'}` })),
          ]}
          on:change={(e) => {
            roles = { ...roles, embedding: e.detail };
            autoSave({ roles }, 0);
          }}
        />
      </div>
      <div class="role-item">
        <span class="role-name">图片理解</span>
        <Select
          value={roles.vision ?? null}
          title="图片理解专家"
          options={[
            { value: null, label: '未配置' },
            ...configs
              .filter((c) => c.id === roles.vision || c.capabilities?.vision)
              .map((c) => ({ value: c.id, label: `${configLabel(c)} · ${c.model || '未选模型'}${c.capabilities?.vision ? '' : '（未勾「图片解析」）'}` })),
          ]}
          on:change={(e) => {
            roles = { ...roles, vision: e.detail };
            autoSave({ roles }, 0);
          }}
        />
      </div>
      <div class="role-item">
        <span class="role-name">视频理解</span>
        <Select
          value={roles.video ?? null}
          title="视频理解专家"
          options={[
            { value: null, label: '未配置' },
            ...configs
              .filter((c) => c.id === roles.video || c.capabilities?.video)
              .map((c) => ({ value: c.id, label: `${configLabel(c)} · ${c.model || '未选模型'}${c.capabilities?.video ? '' : '（未勾「视频解析」）'}` })),
          ]}
          on:change={(e) => {
            roles = { ...roles, video: e.detail };
            autoSave({ roles }, 0);
          }}
        />
      </div>
      <div class="role-item">
        <span class="role-name">音频理解</span>
        <Select
          value={roles.audio ?? null}
          title="音频理解专家"
          options={[
            { value: null, label: '未配置' },
            ...configs
              .filter((c) => c.id === roles.audio || c.capabilities?.audio)
              .map((c) => ({ value: c.id, label: `${configLabel(c)} · ${c.model || '未选模型'}${c.capabilities?.audio ? '' : '（未勾「音频解析」）'}` })),
          ]}
          on:change={(e) => {
            roles = { ...roles, audio: e.detail };
            autoSave({ roles }, 0);
          }}
        />
      </div>
    </div>
  </div>
{/if}

<style>
  .roles-section {
    border-top: 1px solid var(--g2);
    padding-top: 12px;
    margin-top: 4px;
  }
  .roles-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 16px 18px;
    margin-top: 8px;
  }
  .role-item {
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .role-name {
    font-size: 11px;
    color: var(--g6);
  }
  /* 自绘下拉（Select 组件）在各容器里的尺寸：组件内部样式管外观，这里只调整体布局 */
  .role-item :global(.csel) {
    font-size: 12px;
    padding: 6px 8px;
  }
</style>
