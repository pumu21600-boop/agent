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
  let ctxUsage = { tokens: 0, budget: 0, breakdown: null }; // 上下文占用（P5 圆环+明细卡）
  let ctxLen = 0; // 当前模型上下文长度（0=默认）
  let maxOut = 0; // 当前模型最大输出长度（0=默认）
  let temperature = null; // 采样温度（null=渠道默认）
  let topP = null; // 核采样（null=渠道默认）
  let thinking = ''; // 思考开关：''=渠道默认 / on / off
  let thinkingStyle = 'auto'; // 生效的思考参数形态（P9，OpenClaw thinkingFormat 词表）
  let thinkingStyleOverride = null; // 配置里的形态覆盖值（设置面板回显）
  let capabilities = { tools: true, vision: false, video: false, audio: false }; // P8.5 模型能力标记
  let capabilitiesUserSet = false; // A2：true=用户显式标记过，模型切换不再自动改写
  let roles = {}; // P8.5 角色路由映射
  // —— P8.2 工作区 ——
  let workspace = '';
  let recentWorkspaces = [];
  let wsModalOpen = false;
  let wsPath = '';
  let wsMsg = '';
  let apiTotals = { prompt: 0, cached: 0 }; // 渠道真实用量累计（平均缓存命中率）

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

  // —— P8.5 多模态媒体附件与大图预览 ——
  let pendingAttachments = []; // 待发送附件列表 [{ name, mime, data, size }]
  let fileInput; // 隐藏文件选择器引用
  let previewImageModal = null; // 放大预览的图片 dataUrl

  function fmtBytes(b) {
    if (!b) return '0B';
    if (b < 1024) return `${b}B`;
    if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)}KB`;
    return `${(b / (1024 * 1024)).toFixed(1)}MB`;
  }

  function onFileSelect(e) {
    const files = e.target.files;
    if (!files || !files.length) return;
    for (let i = 0; i < files.length; i++) {
      addAttachmentFile(files[i]);
    }
    e.target.value = '';
  }

  function addAttachmentFile(file) {
    if (file.size > 25 * 1024 * 1024) {
      alert(`文件「${file.name}」超过 25MB 上限`);
      return;
    }
    // 对图片附件进行前端等比缩放与优质压缩（最长边 1536px，符合各大视觉模型规范且规避反代请求体体积限制）
    if (file.type && file.type.startsWith('image/')) {
      const reader = new FileReader();
      reader.onload = (ev) => {
        const rawData = ev.target.result;
        const img = new Image();
        img.onload = () => {
          const maxDim = 1536;
          let w = img.width;
          let h = img.height;
          if (w > maxDim || h > maxDim) {
            if (w > h) {
              h = Math.round((h * maxDim) / w);
              w = maxDim;
            } else {
              w = Math.round((w * maxDim) / h);
              h = maxDim;
            }
          }
          const canvas = document.createElement('canvas');
          canvas.width = w;
          canvas.height = h;
          const ctx = canvas.getContext('2d');
          ctx.drawImage(img, 0, 0, w, h);
          // 转为 0.85 压缩率的 jpeg，体积极小（百KB量级）且画质清晰
          const compressedData = canvas.toDataURL('image/jpeg', 0.85);
          pendingAttachments = [
            ...pendingAttachments,
            {
              name: file.name,
              mime: 'image/jpeg',
              data: compressedData,
              size: Math.round(compressedData.length * 0.75),
            },
          ];
        };
        img.src = rawData;
      };
      reader.readAsDataURL(file);
      return;
    }

    // 非图片文件（视频/音频/其他）走标准直读
    const reader = new FileReader();
    reader.onload = (ev) => {
      let mime = file.type || '';
      if (!mime || mime === 'application/octet-stream') {
        const n = file.name.toLowerCase();
        if (n.endsWith('.mp3')) mime = 'audio/mp3';
        else if (n.endsWith('.wav')) mime = 'audio/wav';
        else if (n.endsWith('.ogg')) mime = 'audio/ogg';
        else if (n.endsWith('.m4a')) mime = 'audio/m4a';
        else if (n.endsWith('.aac')) mime = 'audio/aac';
        else if (n.endsWith('.flac')) mime = 'audio/flac';
        else if (n.endsWith('.mp4')) mime = 'video/mp4';
        else if (n.endsWith('.webm')) mime = 'video/webm';
        else if (n.endsWith('.mov')) mime = 'video/quicktime';
        else mime = 'application/octet-stream';
      }
      pendingAttachments = [
        ...pendingAttachments,
        {
          name: file.name,
          mime,
          data: ev.target.result,
          size: file.size,
        },
      ];
    };
    reader.readAsDataURL(file);
  }

  function removeAttachment(index) {
    pendingAttachments = pendingAttachments.filter((_, i) => i !== index);
  }

  function handlePaste(e) {
    const files = e.clipboardData && e.clipboardData.files;
    if (files && files.length > 0) {
      let hasMedia = false;
      for (let i = 0; i < files.length; i++) {
        const file = files[i];
        if (file.type.startsWith('image/') || file.type.startsWith('video/') || file.type.startsWith('audio/') || file.size > 0) {
          hasMedia = true;
          addAttachmentFile(file);
        }
      }
      if (hasMedia) {
        e.preventDefault();
        return;
      }
    }
    const items = e.clipboardData && e.clipboardData.items;
    if (!items) return;
    for (let i = 0; i < items.length; i++) {
      const type = items[i].type || '';
      if (type.startsWith('image/') || type.startsWith('video/') || type.startsWith('audio/') || items[i].kind === 'file') {
        const file = items[i].getAsFile();
        if (file) {
          e.preventDefault();
          addAttachmentFile(file);
        }
      }
    }
  }

  function handleDrop(e) {
    e.preventDefault();
    const files = e.dataTransfer && e.dataTransfer.files;
    if (files && files.length) {
      for (let i = 0; i < files.length; i++) {
        addAttachmentFile(files[i]);
      }
    }
  }

  // —— P6 恢复与续命 / P6.1 多会话 ——
  let sessions = []; // 会话列表（新→旧）
  let currentSessionId = null; // 当前会话（null=下次发送自动新建）
  let projectName = ''; // 工作区目录名

  onMount(async () => {
    await loadProviders();
    await loadSettings();
    await loadSessions(true);
    // 点工具栏菜单外部 → 全部收起（问题1）
    document.addEventListener('click', closeMenusOnOutside);
  });

  function closeMenusOnOutside(e) {
    if (e.target && e.target.closest && (e.target.closest('.bar-menu') || e.target.closest('.side-menu') || e.target.closest('.row-btn') || e.target.closest('.conv-more'))) return;
    modelMenuOpen = false;
    thinkMenuOpen = false;
    approvalMenuOpen = false;
    viewMenuOpen = false;
    sessMenuId = null;
    projMenuPath = null;
  }

  // —— P8.2 工作区切换 ——
  async function applyWorkspace(path) {
    wsMsg = '切换中…';
    try {
      const r = await fetch('/api/workspace', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ path }),
      });
      const d = await r.json();
      if (!r.ok || d.error) throw new Error(d.error || `HTTP ${r.status}`);
      wsMsg = '';
      wsPath = '';
      wsModalOpen = false;
      // 工作区变了：会话/消息/圆环全部重置，重新拉列表
      currentSessionId = null;
      messages = [];
      resetCtxUsage();
      await loadSettings();
      await loadSessions(true);
    } catch (e) {
      wsMsg = `切换失败: ${e.message}`;
    }
  }

  function relTime(iso) {
    if (!iso) return '';
    const ms = Date.now() - new Date(iso).getTime();
    const m = Math.floor(ms / 60000);
    if (m < 1) return '刚刚';
    if (m < 60) return `${m}分钟前`;
    const h = Math.floor(m / 60);
    if (h < 24) return `${h}小时前`;
    const d = Math.floor(h / 24);
    if (d < 30) return `${d}天前`;
    return new Date(iso).toLocaleDateString();
  }

  afterUpdate(() => {
    // 只有用户本来就钉在底部时才自动滚屏；上滑阅读时不打断
    if (chatPinned && chatBody) chatBody.scrollTop = chatBody.scrollHeight;
  });

  function onChatScroll() {
    if (!chatBody) return;
    chatPinned = chatBody.scrollTop + chatBody.clientHeight >= chatBody.scrollHeight - 60;
  }

  // —— 上下文圆环（P5）——
  $: ctxPct = ctxUsage.budget ? Math.min(100, Math.round((ctxUsage.tokens / ctxUsage.budget) * 100)) : 0;
  // 占用极少时百分比仍为 0，但弧保底 2%（实色弧可见，圆环"活"着）
  $: ctxArcPct = ctxUsage.tokens > 0 && ctxPct === 0 ? 2 : ctxPct;
  $: ctxPctText = ctxUsage.tokens > 0 && ctxPct === 0 ? '<1%' : ctxPct + '%';
  $: ctxCirc = 2 * Math.PI * 13;
  // 常态浅灰（同参考样式）；占用高时转警示色
  $: ctxColor = ctxPct >= 85 ? '#f87171' : ctxPct >= 60 ? '#facc15' : '#a9b1c2';
  $: ctxRows = (() => {
    const b = ctxUsage.breakdown || { messages: 0, tools: 0, system: 0, other: 0 };
    const sum = Math.max(1, b.messages + b.tools + b.system + b.other);
    const row = (label, v, color) => ({ label, v, color, pct: ((v / sum) * 100).toFixed(1) + '%' });
    return [
      row('消息', b.messages, '#3b82f6'),
      row('工具结果', b.tools, '#22d3ee'),
      row('系统提示词', b.system, '#a78bfa'),
      row('其他', b.other, '#6b7280'),
    ];
  })();
  function fmtTok(n) {
    if (n >= 10000) return (n / 10000).toFixed(1).replace(/\.0$/, '') + '万';
    return String(n);
  }

  // —— ZCode 式输入框：思考模式选择器（常驻）——
  // 复用设置面板的 thinking 字段（唯一数据源）：off/on=明确设置；''=渠道默认
  let thinkMenuOpen = false;
  const THINK_LABELS = { '': '默认', on: '开启', off: '关闭', low: '低档', medium: '中档', high: '高档', max: '最大' };
  // 提示按「生效形态」给（覆盖 > 端点探测），auto/无参数渠道给可操作指引
  $: thinkHint =
    thinkingStyle === 'zai'
      ? 'GLM/豆包 thinking 对象参数'
      : thinkingStyle === 'deepseek'
        ? 'DeepSeek thinking 对象参数'
        : thinkingStyle === 'qwen'
          ? 'Qwen 系 enable_thinking 布尔'
          : thinkingStyle === 'qwen-chat-template'
            ? '本地推理服务 chat_template_kwargs（Qwen3 系模板）'
            : thinkingStyle === 'openai'
              ? 'OpenAI 风格 reasoning_effort 档位'
              : thinkingStyle === 'none'
                ? '形态=不发参数，思考由渠道决定'
                : '该渠道无标准开关参数——关不掉到设置改「思考参数形态」';
  function pickThinking(v) {
    thinking = v;
    thinkMenuOpen = false;
    // 同步进设置（持久化到当前配置，刷新/切会话不丢）
    autoSave({ thinking: v }, 0);
  }

  // —— 审批档位菜单（输入栏内）——
  let approvalMenuOpen = false;
  const APPROVAL_LABELS = { 'read-only': '只读', auto: '自动', 'full-access': '完全访问' };
  function pickApproval(m) {
    approvalMode = m;
    approvalMenuOpen = false;
    localStorage.setItem('approvalMode', m);
    fetch('/api/approval-mode', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ mode: m }),
    });
  }

  function handleKey(e) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }
  function autoRows(e) {
    const el = e.target;
    el.style.height = 'auto';
    el.style.height = Math.min(el.scrollHeight, 160) + 'px';
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
    ctxLen = s.context_length || 0;
    maxOut = s.max_output_tokens || 0;
    temperature = s.temperature ?? null;
    topP = s.top_p ?? null;
    thinking = s.thinking || '';
    thinkingStyle = s.thinking_style || 'auto';
    thinkingStyleOverride = s.thinking_style_override || null;
    capabilities = s.capabilities || { tools: true, vision: false, video: false, audio: false };
    capabilitiesUserSet = s.capabilities_user_set || false;
    roles = s.roles || {};
    workspaceAliases = s.workspace_aliases || {};
    workspace = s.workspace || '';
    recentWorkspaces = s.recent_workspaces || [];
    // 圆环常驻：会话未开始时也用配置预算初始化（context_length 或全局默认 96000）
    if (!ctxUsage.budget) {
      ctxUsage = { ...ctxUsage, budget: s.context_length || 96000 };
    }
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

  // 防抖自动保存设置字段；合并挂起字段，切换配置前必须落盘到当前配置
  function autoSave(fields, delay = 500) {
    clearTimeout(autoSaveTimer);
    pendingSave = { ...(pendingSave || {}), ...fields };
    autoSaveTimer = setTimeout(async () => {
      const body = { ...pendingSave };
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
      applyState(d);
      flashSaved();
    } catch (e) {
      settingsMsg = `保存失败: ${e}`;
    }
  }

  async function clearKey() {
    await sendSettings({ clear_key: true });
    savedKeyUrl = '';
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
    const p = providers.find((x) => x.id === providerId);
    const newBase = p ? p.default_base_url : '';
    const matchesPreset = providers.some((x) => x.default_base_url && x.default_base_url === baseUrl);
    if (!baseUrl || matchesPreset) {
      baseUrl = newBase;
      autoSave({ provider_id: providerId, base_url: baseUrl });
    } else {
      autoSave({ provider_id: providerId });
    }
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

  function inferCapabilities(mId) {
    const id = (mId || '').toLowerCase();
    return {
      tools: true,
      vision: id.includes('vl') || id.includes('vision') || id.includes('gemini') || id.includes('gpt-4o') || id.includes('claude-3') || id.includes('omni') || id.includes('mimo'),
      video: id.includes('video') || id.includes('gemini-2') || id.includes('omni') || id.includes('mimo'),
      audio: id.includes('audio') || id.includes('realtime') || id.includes('whisper') || id.includes('omni') || id.includes('gemini') || id.includes('mimo'),
    };
  }

  function isChecked(m) {
    return savedModels.includes(m);
  }

  async function toggleModel(m) {
    const list = savedModels.includes(m)
      ? savedModels.filter((x) => x !== m)
      : [...savedModels, m].sort();
    savedModels = list; // 乐观更新
    let newModel = model;
    if (list.length && !list.includes(model)) {
      newModel = list[0];
      model = newModel;
    }
    // A2（施工单 012）：能力标记归用户所有，切换/勾选模型不再改写 capabilities；
    // 后端仅在该配置无用户标记历史时按新模型名推导初值
    await sendSettings({ models: list, model: newModel, api_key: apiKey.trim() || undefined });
  }

  async function selectAllModels() {
    const list = [...fetchedModels].sort();
    savedModels = list;
    let newModel = model;
    if (!list.includes(model) && list.length) {
      newModel = list[0];
      model = newModel;
    }
    // A2：同上，不随选择改写能力标记
    await sendSettings({ models: list, model: newModel });
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
    // A2：不随快选改写能力标记（后端在无用户标记时推导初值）
    await sendSettings({ model: m });
  }

  // A2 显式重置入口：按当前模型名重新推导能力标记，回到推导态
  function recomputeCapabilities() {
    autoSave({ recompute_capabilities: true }, 0);
  }

  // —— 对话 ——
  async function send() {
    const msg = input.trim();
    if ((!msg && pendingAttachments.length === 0) || streaming) return;
    streaming = true;
    controller = new AbortController();
    const effectiveMsg = msg || (pendingAttachments.length > 0 ? '请查看分析附带的媒体文件' : '');
    const sendingAttachments = [...pendingAttachments];
    pendingAttachments = [];
    const history = messages
      .filter((m) => (m.role === 'user' && m.content) || (m.role === 'assistant' && m.content && !m.streaming))
      .map((m) => ({ role: m.role, content: m.content }));
    history.push({ role: 'user', content: effectiveMsg });
    messages = [
      ...messages,
      { id: nextId(), role: 'user', content: effectiveMsg, attachments: sendingAttachments },
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
        body: JSON.stringify({
          messages: history,
          message: effectiveMsg,
          attachments: sendingAttachments.length > 0 ? sendingAttachments.map((a) => ({ name: a.name, mime: a.mime, data: a.data })) : undefined,
          model,
          base_url: baseUrl,
          approval_mode: approvalMode,
          session_id: currentSessionId || '',
        }),
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
              if (d.session) {
                currentSessionId = d.session;
              } else if (d.content) patch((m) => ({ ...m, content: m.content + d.content }));
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
                  approval: {
                    id: d.approval_request.id,
                    tool: d.approval_request.tool,
                    args: d.approval_request.args,
                    risk: d.approval_request.risk ?? 1,
                    riskLabel: d.approval_request.risk_label || '',
                    reason: d.approval_request.reason || '',
                    decided: null,
                  },
                }));
              else if (d.approval_decided)
                patch((m) =>
                  m.approval && m.approval.id === d.approval_decided.id
                    ? { ...m, approval: { ...m.approval, decided: d.approval_decided.approved } }
                    : m
                );
              else if (d.checkpoint)
                messages = [
                  ...messages,
                  { id: nextId(), role: 'cp', cp: d.checkpoint },
                ];
              else if (d.notice)
                messages = [...messages, { id: nextId(), role: 'system', content: '⚠️ ' + d.notice }];
              else if (d.title) loadSessions(true); // P8 TITLE：标题已生成，刷新会话列表
              else if (d.context_usage)
                ctxUsage = { tokens: d.context_usage.tokens, budget: d.context_usage.budget, breakdown: d.context_usage.breakdown || null };
              else if (d.api_usage) {
                apiTotals = {
                  prompt: apiTotals.prompt + (d.api_usage.prompt_tokens || 0),
                  cached: apiTotals.cached + (d.api_usage.cached_tokens || 0),
                };
              } else if (d.step_limit) {
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
      loadSessions(); // 刷新会话列表（新会话出现/标题更新）
    }
  }

  function stop() {
    fetch('/cancel', { method: 'POST' });
    if (controller) controller.abort();
  }

  // —— P6 恢复与续命 ——
  function contentText(c) {
    if (typeof c === 'string') return c;
    if (Array.isArray(c)) return c.map((p) => p.text || '').join('');
    return '';
  }

  // —— P6.1 多会话：列表 / 切换 / 新建（新建不清空历史）——
  async function loadSessions(autoPick = false) {
    try {
      const r = await fetch('/api/sessions').then((r) => r.json());
      projectName = r.project || '';
      sessions = r.sessions || [];
      // 启动时默认恢复最近一条会话
      if (autoPick && currentSessionId === null && sessions.length) {
        await loadSession(sessions[0].id);
      }
    } catch (e) {
      sessions = [];
    }
  }

  // 重置占用但保留预算（圆环常驻：预算来自设置，切会话不该归零）
  function resetCtxUsage() {
    ctxUsage = { tokens: 0, budget: ctxUsage.budget, breakdown: null };
  }

  async function loadSession(id) {
    if (streaming) return;
    try {
      const r = await fetch(`/api/session?id=${encodeURIComponent(id)}`).then((r) => r.json());
      currentSessionId = id;
      messages = rebuildFromSession(r.messages || []);
      resetCtxUsage();
    } catch (e) {
      // 加载失败不阻塞
    }
  }

  function startNewSession() {
    if (streaming) return;
    currentSessionId = null;
    messages = [];
    resetCtxUsage();
    apiTotals = { prompt: 0, cached: 0 };
  }

  async function deleteSession(id) {
    if (!confirm('删除这条会话？其余会话不受影响')) return;
    await fetch(`/api/session?id=${encodeURIComponent(id)}`, { method: 'DELETE' });
    if (id === currentSessionId) startNewSession();
    loadSessions();
  }

  // —— DSH 式侧边栏（项目=工作区；置顶/归档/分叉/搜索/批量管理/视图切换）——
  let workspaceAliases = {}; // 工作区路径 → 项目显示名（重命名项目）
  let sidebarView = localStorage.getItem('sidebarView') === 'list' ? 'list' : 'workspace'; // 按工作区 | 单列表
  let expandedFlags = { __ungrouped__: true, ...(JSON.parse(localStorage.getItem('projExpanded') || '{}') || {}) }; // path → 展开？
  let showAllFlags = {}; // path → 展开全部会话（默认每项目先显 5 条）
  let searchOpen = false;
  let searchQuery = '';
  let manageMode = false;
  let checkedIds = [];
  let sidebarCollapsed = false;
  let sidebarW = Number(localStorage.getItem('sidebarW')) || 300; // 侧栏宽度（右缘拖拽可调，240–520）
  let resizing = false;

  // 侧栏右缘拖拽调宽：按住手柄移动鼠标，松手持久化
  function startResize(e) {
    e.preventDefault();
    resizing = true;
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
    const move = (ev) => {
      sidebarW = Math.min(520, Math.max(240, Math.round(ev.clientX)));
    };
    const up = () => {
      resizing = false;
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
      localStorage.setItem('sidebarW', String(sidebarW));
      window.removeEventListener('mousemove', move);
      window.removeEventListener('mouseup', up);
    };
    window.addEventListener('mousemove', move);
    window.addEventListener('mouseup', up);
  }
  let sessMenuId = null; // 打开了操作菜单的会话 id
  let projMenuPath = null; // 打开了操作菜单的项目 path
  let viewMenuOpen = false;
  let renameModal = null; // { kind: 'session'|'project', target, value }
  const CONV_PREVIEW = 5; // 项目收起态先显 5 条，其余走「展开其余 N 个会话」

  $: if (workspace && !(workspace in expandedFlags)) {
    expandedFlags = { ...expandedFlags, [workspace]: true };
  }

  function normPath(p) {
    return (p || '').replace(/[\\/]+$/, '');
  }

  // 项目列表 = 当前工作区 + 最近工作区（去重，按新→旧）
  $: projectList = (() => {
    const list = [];
    const seen = new Set();
    if (workspace) {
      list.push({ path: workspace, active: true });
      seen.add(normPath(workspace).toLowerCase());
    }
    for (const w of recentWorkspaces) {
      if (!seen.has(normPath(w).toLowerCase())) {
        list.push({ path: w, active: false });
        seen.add(normPath(w).toLowerCase());
      }
    }
    return list.map((p) => ({
      ...p,
      name: workspaceAliases[p.path] || p.path.split(/[\\/]/).filter(Boolean).pop() || p.path,
    }));
  })();

  $: searchQ = searchQuery.trim().toLowerCase();
  $: visibleSessions = manageMode ? sessions : sessions.filter((s) => !s.archived);

  // 分组：会话按创建时工作区归项目；无工作区记录的旧会话进「未分组」
  $: sessionsByProject = (() => {
    const map = {};
    for (const s of visibleSessions) {
      if (searchQ && !(s.title || '').toLowerCase().includes(searchQ)) continue;
      const hit = projectList.find((p) => normPath(p.path).toLowerCase() === normPath(s.workspace).toLowerCase());
      const k = hit ? hit.path : '__ungrouped__';
      (map[k] = map[k] || []).push(s);
    }
    return map;
  })();

  $: ungroupedSessions = sessionsByProject['__ungrouped__'] || [];

  // 单列表视图：全部会话平铺（后端已按置顶→更新时间排序）
  $: flatSessions = visibleSessions.filter((s) => !searchQ || (s.title || '').toLowerCase().includes(searchQ));

  // 渲染行序列：单列表=纯会话流；工作区视图=项目行+缩进会话行+「展开其余」行
  $: displayRows = (() => {
    const rows = [];
    if (sidebarView === 'list') {
      for (const s of flatSessions) rows.push({ type: 'conv', s });
      if (!rows.length) rows.push({ type: 'empty' });
      return rows;
    }
    const pushGroup = (path, name, active, items) => {
      rows.push({ type: 'proj', path, name, active, count: items.length });
      const expanded = !!searchQ || !!expandedFlags[path];
      if (!expanded) return;
      const capped = searchQ
        ? items
        : items.slice(0, showAllFlags[path] ? items.length : CONV_PREVIEW);
      for (const s of capped) rows.push({ type: 'conv', s });
      if (!searchQ && items.length > CONV_PREVIEW && !showAllFlags[path]) {
        rows.push({ type: 'more', path, rest: items.length - CONV_PREVIEW });
      }
    };
    for (const p of projectList) pushGroup(p.path, p.name, p.active, sessionsByProject[p.path] || []);
    if (ungroupedSessions.length) pushGroup('__ungrouped__', '未分组', false, ungroupedSessions);
    if (!rows.length) rows.push({ type: 'empty' });
    return rows;
  })();

  function toggleProjectRow(path) {
    if (searchQ) return; // 搜索态全展开
    expandedFlags = { ...expandedFlags, [path]: !expandedFlags[path] };
    localStorage.setItem('projExpanded', JSON.stringify(expandedFlags));
  }

  async function switchProject(path) {
    if (normPath(path).toLowerCase() === normPath(workspace).toLowerCase()) return;
    if (streaming) {
      settingsMsg = '正在流式输出，稍后再切换项目';
      return;
    }
    await applyWorkspace(path);
  }

  function newSessionInProject(path, active) {
    if (active) {
      startNewSession();
    } else {
      switchProject(path).then(() => startNewSession());
    }
  }

  // 时间用 DSH 短格式：4天 / 3小时 / 刚刚
  function relShort(iso) {
    if (!iso) return '';
    const ms = Date.now() - new Date(iso).getTime();
    const m = Math.floor(ms / 60000);
    if (m < 1) return '刚刚';
    if (m < 60) return `${m}分钟`;
    const h = Math.floor(m / 60);
    if (h < 24) return `${h}小时`;
    return `${Math.floor(h / 24)}天`;
  }

  // —— 工具栏四键：搜索 / 管理 / 视图 / 添加项目 ——
  function toggleSearch() {
    searchOpen = !searchOpen;
    if (!searchOpen) searchQuery = '';
  }

  function toggleManage() {
    manageMode = !manageMode;
    checkedIds = [];
  }

  function toggleChecked(id) {
    checkedIds = checkedIds.includes(id) ? checkedIds.filter((x) => x !== id) : [...checkedIds, id];
  }

  function manageVisibleIds() {
    return sidebarView === 'list' ? flatSessions.map((s) => s.id) : displayRows.filter((r) => r.type === 'conv').map((r) => r.s.id);
  }

  async function batchSet(archived) {
    if (!checkedIds.length) return;
    await fetch('/api/sessions/batch', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ ids: checkedIds, archived }),
    });
    checkedIds = [];
    await loadSessions();
  }

  async function batchDelete() {
    if (!checkedIds.length || !confirm(`删除选中的 ${checkedIds.length} 条会话？不可恢复`)) return;
    for (const id of checkedIds) {
      await fetch(`/api/session?id=${encodeURIComponent(id)}`, { method: 'DELETE' });
    }
    if (checkedIds.includes(currentSessionId)) startNewSession();
    checkedIds = [];
    await loadSessions();
  }

  function pickView(v) {
    sidebarView = v;
    viewMenuOpen = false;
    localStorage.setItem('sidebarView', v);
  }

  // 添加项目：server 弹 Win 原生选文件夹窗口 → 选中的目录注册为项目（=切换工作区）
  async function addProject() {
    try {
      const r = await fetch('/api/workspace/picker', { method: 'POST' }).then((r) => r.json());
      if (r.canceled || !r.path) return;
      await applyWorkspace(r.path);
    } catch (e) {
      settingsMsg = `选择文件夹失败: ${e.message || e}`;
    }
  }

  async function removeProject(path, name) {
    if (normPath(path).toLowerCase() === normPath(workspace).toLowerCase()) {
      settingsMsg = '当前项目不能移除，先切换到其他项目';
      return;
    }
    if (!confirm(`从侧边栏移除项目「${name}」？\n只移出列表，不删除磁盘上的文件夹与会话。`)) return;
    projMenuPath = null;
    await fetch('/api/workspace/remove', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ path }),
    });
    await loadSettings();
  }

  // —— 会话菜单四项：置顶 / 重命名 / 分叉 / 归档 ——
  async function togglePin(s) {
    sessMenuId = null;
    await fetch('/api/sessions/meta', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id: s.id, pinned: !s.pinned }),
    });
    await loadSessions();
  }

  async function toggleArchive(s) {
    sessMenuId = null;
    await fetch('/api/sessions/meta', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id: s.id, archived: !s.archived }),
    });
    await loadSessions();
  }

  async function forkSession(id) {
    sessMenuId = null;
    try {
      const r = await fetch('/api/sessions/fork', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id }),
      }).then((r) => r.json());
      await loadSessions();
      if (r.id) await loadSession(r.id);
    } catch (e) {
      settingsMsg = `分叉失败: ${e.message || e}`;
    }
  }

  function submitRename() {
    if (!renameModal) return;
    const v = renameModal.value.trim();
    const modal = renameModal;
    renameModal = null;
    if (modal.kind === 'session') {
      if (v && v !== modal.value) {
        fetch('/api/sessions/meta', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ id: modal.target, title: v }),
        }).then(() => loadSessions());
      }
    } else {
      fetch('/api/workspace/alias', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ path: modal.target, name: v }),
      }).then(() => loadSettings());
    }
  }

  // —— P6.1 改动统计条：撤销 = 工作树回滚到任务起点基线 ——
  async function undoCheckpoint(cp) {
    if (!confirm(`撤销本次任务的改动？\n${cp.files} 个文件将恢复到任务开始前（含删除本次新建的已跟踪文件）。\nshell 副作用不可回滚。`))
      return;
    try {
      const r = await fetch('/api/checkpoints/restore', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id: cp.id }),
      });
      const d = await r.json();
      messages = [
        ...messages,
        {
          id: nextId(),
          role: 'system',
          content: d.ok ? `↩ 已撤销：${cp.files} 个文件恢复到任务开始前` : `⚠️ 撤销失败：${d.error}`,
        },
      ];
    } catch (e) {
      messages = [...messages, { id: nextId(), role: 'error', content: `[撤销请求失败] ${e}` }];
    }
  }

  function rebuildFromSession(msgs) {
    const out = [];
    for (const m of msgs) {
      if (m.role === 'user') {
        out.push({ id: nextId(), role: 'user', content: contentText(m.content) });
      } else if (m.role === 'assistant') {
        const asst = {
          id: nextId(),
          role: 'assistant',
          content: contentText(m.content),
          thinking: '',
          tools: (m.tool_calls || []).map((tc) => ({
            id: tc.id,
            name: tc.function?.name || '?',
            args: tc.function?.arguments || '',
            status: 'ok',
            preview: '',
          })),
          streaming: false,
        };
        out.push(asst);
      } else if (m.role === 'tool') {
        const asst = [...out].reverse().find(
          (x) => x.role === 'assistant' && (x.tools || []).some((t) => t.id === m.tool_call_id)
        );
        const t = asst && asst.tools.find((t) => t.id === m.tool_call_id);
        if (t) t.preview = contentText(m.content).slice(0, 300);
      }
    }
    return out;
  }

  async function makeHandoff() {
    try {
      const r = await fetch('/api/handoff', { method: 'POST' });
      const d = await r.json();
      messages = [
        ...messages,
        {
          id: nextId(),
          role: 'system',
          content: d.ok ? `📄 交接文档已生成：${d.path}` : `⚠️ HANDOFF 生成失败：${d.error}`,
        },
      ];
    } catch (e) {
      messages = [...messages, { id: nextId(), role: 'error', content: `[HANDOFF 请求失败] ${e}` }];
    }
  }

  // 审批决定：本地乐观标记 + 通知后端（后端也会回 approval_decided，幂等）。
  // remember=true → 本会话内「同类操作」不再询问（P9 A3）；后端拒绝记忆 Dangerous 档。
  async function decideApproval(id, approved, remember = false) {
    patchMessageApproval(id, approved);
    await fetch('/api/approvals', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id, approved, remember }),
    });
  }
  function patchMessageApproval(id, approved) {
    messages = messages.map((m) =>
      m.approval && m.approval.id === id ? { ...m, approval: { ...m.approval, decided: approved } } : m
    );
  }
  // 档位名兜底（与 core::rules::RiskLevel::label 对齐；正常情况 SSE 已带 risk_label）
  function riskLabelFor(risk) {
    return risk >= 2 ? '高危操作' : risk === 1 ? '常规改动' : '安全';
  }

  function nextId() {
    return ++seq;
  }
</script>

<div class="app">
  <aside class="sidebar" class:collapsed={sidebarCollapsed} class:resizing={resizing} style="width:{sidebarW}px">
    <div class="side-head">
      <div class="brand-row">
        <svg class="whale" viewBox="-0.54 -0.06 56.78 41.72" aria-hidden="true"><path fill="currentColor" d="M55.6128,3.4712c-.5953-.2917-.8517.2642-1.1998.5466-.1191.0911-.2198.2095-.3206.3188-.8701.9292-1.8867,1.5398-3.2148,1.4668-1.9417-.1094-3.5995.5012-5.065,1.9863-.3114-1.8313-1.3463-2.9248-2.9217-3.6262-.8242-.3645-1.6577-.729-2.2348-1.5217-.403-.5647-.5129-1.1934-.7144-1.813-.1283-.3735-.2565-.7563-.687-.8201-.4671-.0728-.6503.3188-.8335.647-.7327,1.3394-1.0166,2.8154-.9892,4.3096.0641,3.3621,1.4838,6.0406,4.3047,7.9449.3206.2187.403.4372.3023.7563-.1924.656-.4214,1.2937-.6228,1.9497-.1283.4192-.3207.5103-.7694.3279-1.5479-.6467-2.8852-1.6035-4.0667-2.7605-2.0058-1.9407-3.8193-4.0818-6.0815-5.7583-.5312-.3918-1.0625-.7561-1.6121-1.1025-2.3081-2.2412.3023-4.0818.9068-4.3003.6319-.2278.2198-1.0115-1.8227-1.0022-2.0425.009-3.9109.6924-6.2922,1.6035-.348.1367-.7145.2368-1.09.3188-2.1615-.4099-4.4055-.5012-6.7502-.2368-4.4147.4919-7.9408,2.5784-10.5328,6.1409C.1914,13.1289-.5413,17.9941.3563,23.0691c.9434,5.3481,3.6727,9.7761,7.8676,13.2385,4.3506,3.5896,9.3606,5.3481,15.0758,5.011,3.4713-.2004,7.5027-1.5586,10.9096-3.4904.6597-.3732,1.3061-.7636,1.9431-1.1647.0963-.0607.1892-.1283.3207-.0675.9754.4383,2.0069.6785,3.0863.6008,1.7863-.1283,3.3526-.7837,4.6372-2.0626.4152-.4122.7981-.8629,1.0271-1.4137.1737-.4155.0848-.6528-.2136-.7087-.3238-.0606-.5181.1559-.7171.3464-.4424.4257-.8792.8623-1.4054,1.1953-1.3641.8645-2.8269,1.1212-4.3692.6344.2536-.3279.6359-.3952.9702-.5365,1.9778-.8359,3.2843-2.2719,3.7624-4.3781.0413-.1826.0772-.3664.1097-.5512.1085-.6145.0798-1.2163-.1039-1.8147-.2416-.7892-.5436-1.5545-1.0509-2.2141-.7773-1.0132-1.7827-1.7146-2.9838-2.1453-.9036-.3242-1.8389-.4709-2.7974-.4189-1.0882.0589-2.1247.3549-3.0951.8451-.9579.4846-1.7778,1.1485-2.4422,1.9958-.8585,1.0932-1.3642,2.3374-1.4744,3.7265-.0861,1.0831.0563,2.1333.4752,3.1313.5739,1.3627,1.5162,2.3944,2.7931,3.1218.1209.0691.2424.1375.3678.1983.1741.084.2086.1805.1097.3582-.2705.4865-.5297.9802-.7958,1.4698-.0677.1242-.1424.2452-.2169.3656-.0592.0954-.1402.1088-.2423.0526-1.8657-1.0286-3.3232-2.4476-4.3562-4.3129-.6723-1.2134-1.0598-2.511-1.1529-3.8904-.0283-.4183-.0306-.8378-.0128-1.2565.0051-.1203-.0301-.1564-.1478-.1495-1.1135.0652-2.1912.2819-3.2096.7305-2.6173,1.1529-4.2559,3.1448-4.8387,5.9438-.1274.6097-.1745,1.2276-.1673,1.8494.0014.1196-.0364.1471-.1455.1433-.8329-.0292-1.6424-.1919-2.4255-.4637-2.5975-.9021-4.562-2.5741-5.8622-5.0172-.7471-1.4059-1.1209-2.9122-1.1355-4.5003-.0043-.4674.0214-.9332.0874-1.3958.0088-.0617-.0136-.1587.0617-.1806.0639-.0187.1187.0532.1648.0963,1.6421,1.5276,3.5543,2.6415,5.7029,3.3254.662.2114,1.3354.3726,2.0173.4882.1171.0198.1604.0601.1473.1848-.0578.5473-.0434,1.0955.0244,1.6412.0185.1485-.0164.1864-.1632.1587-1.0813-.2049-2.1094-.5612-3.0829-1.0795-2.4993-1.3291-4.2931-3.2623-5.3628-5.8955-.4405-1.0806-.6789-2.2061-.7225-3.3708-.0258-.6917.0234-1.3776.1587-2.0556.0203-.1019.053-.2012.0794-.3017.0274-.1032.0876-.1155.1663-.0487.2729.231.5589.4433.8586.6382,2.1417,1.3985,4.4712,2.4025,6.9481,3.0743.5347.145,1.0733.2707,1.6127.3947.1095.0251.2196.0469.3296.069.053.0107.0989.0034.1119-.0612.0115-.0569-.0221-.0918-.0666-.1142-.1211-.0606-.2443-.1166-.3652-.1775-1.675-.8482-3.2055-1.9111-4.5642-3.2072-1.1633-1.1104-2.1489-2.3577-2.9257-3.7649-.4816-.8734-.8476-1.7908-1.0691-2.7623-.0592-.2579-.0654-.2573.1945-.1964,1.0394.2442,2.0905.4096,3.1546.4957,1.2397.1002,2.4805.1076,3.7208.0223,2.0513-.1417,4.0542-.5761,5.9935-1.2897,1.8939-.6987,3.6617-1.6463,5.2914-2.8224.8853-.6405,1.7112-1.3523,2.4701-2.1379.0417-.0431.0801-.0902.1178-.1372.0394-.0492.0851-.0772.1446-.0374Z"/></svg>
        <span class="brand-name">Agent Local Build</span>
        <button class="icon-btn" title="收起侧栏" on:click={() => (sidebarCollapsed = true)}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="3" y="4.5" width="18" height="15" rx="3.2"/><line x1="9.8" y1="4.5" x2="9.8" y2="19.5"/></svg>
        </button>
      </div>

      <button class="new-chat" on:click={startNewSession} disabled={streaming}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="9"/><path d="M12 8v8M8 12h8" stroke-linecap="round"/></svg>
        新会话
      </button>

      <div class="section-row bar-menu">
        <span class="section-label">工作区</span>
        <span class="section-tools">
          <button class="icon-btn" class:on={searchOpen} title="搜索会话" on:click={toggleSearch}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><circle cx="11" cy="11" r="7"/><line x1="21" y1="21" x2="16.3" y2="16.3"/></svg>
          </button>
          <button class="icon-btn" class:on={manageMode} title="管理会话（批量归档）" on:click={toggleManage}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><circle cx="6.5" cy="7" r="2.2"/><line x1="11" y1="7" x2="19.5" y2="7"/><circle cx="6.5" cy="17" r="2.2"/><line x1="11" y1="17" x2="19.5" y2="17"/></svg>
          </button>
          <button class="icon-btn" class:on={viewMenuOpen} title="切换视图" on:click={() => (viewMenuOpen = !viewMenuOpen)}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><line x1="3.5" y1="6" x2="9.5" y2="6"/><line x1="14.5" y1="6" x2="20.5" y2="6"/><circle cx="12" cy="6" r="2"/><line x1="3.5" y1="12" x2="4" y2="12"/><line x1="8" y1="12" x2="20.5" y2="12"/><circle cx="6" cy="12" r="2"/><line x1="3.5" y1="18" x2="11.5" y2="18"/><line x1="16.5" y1="18" x2="20.5" y2="18"/><circle cx="14" cy="18" r="2"/></svg>
          </button>
          <button class="icon-btn" title="添加项目" on:click={addProject} disabled={streaming}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M20 11.2v5.8a2 2 0 0 1-2 2H5.5a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h3.2l1.8 2H18a2 2 0 0 1 2 2v.2"/><line x1="17" y1="2.5" x2="17" y2="8"/><line x1="14.2" y1="5.2" x2="19.8" y2="5.2"/></svg>
          </button>
        </span>
        {#if viewMenuOpen}
          <div class="side-menu view-menu">
            <button on:click={() => pickView('workspace')}>
              <span>按工作区</span>
              {#if sidebarView === 'workspace'}<svg class="sel-check" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>{/if}
            </button>
            <button on:click={() => pickView('list')}>
              <span>单列表</span>
              {#if sidebarView === 'list'}<svg class="sel-check" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg>{/if}
            </button>
          </div>
        {/if}
      </div>

      {#if searchOpen}
        <input class="side-search" bind:value={searchQuery} placeholder="搜索会话" />
      {/if}

      {#if manageMode}
        <div class="manage-bar">
          <span class="m-count">已选 {checkedIds.length} / {manageVisibleIds().length}</span>
          <button class="m-btn" on:click={() => (checkedIds = manageVisibleIds())}>全选</button>
          <button class="m-btn" disabled={!checkedIds.length} on:click={() => batchSet(true)}>归档</button>
          <button class="m-btn" disabled={!checkedIds.length} on:click={() => batchSet(false)}>取消归档</button>
          <button class="m-btn" disabled={!checkedIds.length} on:click={batchDelete}>删除</button>
          <button class="m-btn primary" on:click={toggleManage}>完成</button>
        </div>
      {/if}
    </div>

    <div class="side-scroll">
      {#each displayRows as row}
        {#if row.type === 'proj'}
          <div
            class="proj"
            class:expanded={!!searchQ || !!expandedFlags[row.path]}
            role="button"
            tabindex="0"
            title={row.path === '__ungrouped__' ? '无工作区记录的历史会话' : row.path}
            on:click={() => (row.active ? toggleProjectRow(row.path) : switchProject(row.path))}
            on:keydown={(e) => e.key === 'Enter' && (row.active ? toggleProjectRow(row.path) : switchProject(row.path))}
          >
            {#if !!searchQ || !!expandedFlags[row.path]}
              <svg viewBox="0 0 24 24"><path d="M6.5 9.2 12 14.7l5.5-5.5z" fill="currentColor"/></svg>
            {:else}
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M20 19.5a1.8 1.8 0 0 0 1.8-1.8V8.3a1.8 1.8 0 0 0-1.8-1.8h-7.4a1.8 1.8 0 0 1-1.5-.8l-.8-1.2a1.8 1.8 0 0 0-1.5-.8H4a1.8 1.8 0 0 0-1.8 1.8v12.2A1.8 1.8 0 0 0 4 19.5Z"/></svg>
            {/if}
            <span class="proj-name">{row.name}{row.count ? '' : ''}</span>
            {#if row.path !== '__ungrouped__'}
              <span class="proj-side">
                <button class="row-btn boxed" class:on={projMenuPath === row.path} title="项目操作" on:click|stopPropagation={() => (projMenuPath = projMenuPath === row.path ? null : row.path)}>
                  <svg viewBox="0 0 24 24" fill="currentColor"><circle cx="5.5" cy="12" r="1.5"/><circle cx="12" cy="12" r="1.5"/><circle cx="18.5" cy="12" r="1.5"/></svg>
                </button>
                <button class="row-btn" title="在项目内新建会话" on:click|stopPropagation={() => newSessionInProject(row.path, row.active)}>
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M12 5v14M5 12h14"/></svg>
                </button>
              </span>
            {/if}
            {#if projMenuPath === row.path}
              <div class="side-menu proj-menu">
                <button on:click|stopPropagation={() => { projMenuPath = null; renameModal = { kind: 'project', target: row.path, value: row.name }; }}>
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M16.8 3.7a2.1 2.1 0 0 1 3 3L7.3 19.2 3 20.3l1.1-4.3Z"/><path d="M13.5 20h7"/></svg>
                  重命名项目
                </button>
                <button on:click|stopPropagation={() => removeProject(row.path, row.name)}>
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6.5h18"/><path d="M8 6.5V4.8A1.3 1.3 0 0 1 9.3 3.5h5.4A1.3 1.3 0 0 1 16 4.8v1.7"/><path d="M5.5 6.5 6.4 19a2 2 0 0 0 2 1.9h7.2a2 2 0 0 0 2-1.9l.9-12.5"/></svg>
                  删除项目
                </button>
              </div>
            {/if}
          </div>
        {:else if row.type === 'conv'}
          {@const s = row.s}
          <div
            class="conv"
            class:active={s.id === currentSessionId}
            role="button"
            tabindex="0"
            on:click={() => (manageMode ? toggleChecked(s.id) : loadSession(s.id))}
            on:keydown={(e) => e.key === 'Enter' && (manageMode ? toggleChecked(s.id) : loadSession(s.id))}
          >
            {#if manageMode}
              <span class="conv-check" class:on={checkedIds.includes(s.id)}>{checkedIds.includes(s.id) ? '✓' : ''}</span>
            {:else if s.pinned}
              <svg class="conv-pin" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" transform="rotate(38 12 12)"><path d="M14.5 4.2 19.8 9.5a1 1 0 0 1-.94 1.68l-2.6-.52-3.62 3.62.55 2.87a1 1 0 0 1-1.7.89L9.9 16.4l-4.6 4.6-1.3-1.3 4.6-4.6-1.64-1.64a1 1 0 0 1 .89-1.7l2.87.55 3.62-3.62-.52-2.6a1 1 0 0 1 1.68-.94Z"/></svg>
            {/if}
            <span class="conv-title" class:archived={s.archived} title={s.title}>{s.title}</span>
            {#if manageMode && s.archived}<span class="arch-tag">已归档</span>{/if}
            {#if !manageMode}
              <button class="conv-more" title="会话操作" on:click|stopPropagation={() => (sessMenuId = sessMenuId === s.id ? null : s.id)}>
                <svg viewBox="0 0 24 24" fill="currentColor"><circle cx="5.5" cy="12" r="1.6"/><circle cx="12" cy="12" r="1.6"/><circle cx="18.5" cy="12" r="1.6"/></svg>
              </button>
            {/if}
            <span class="conv-time">{relShort(s.updated_at)}</span>
            {#if sessMenuId === s.id}
              <div class="side-menu sess-menu">
                <button on:click|stopPropagation={() => togglePin(s)}>
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M14.5 4.2 19.8 9.5a1 1 0 0 1-.94 1.68l-2.6-.52-3.62 3.62.55 2.87a1 1 0 0 1-1.7.89L9.9 16.4l-4.6 4.6-1.3-1.3 4.6-4.6-1.64-1.64a1 1 0 0 1 .89-1.7l2.87.55 3.62-3.62-.52-2.6a1 1 0 0 1 1.68-.94Z"/></svg>
                  {s.pinned ? '取消置顶' : '置顶'}
                </button>
                <button on:click|stopPropagation={() => { sessMenuId = null; renameModal = { kind: 'session', target: s.id, value: s.title }; }}>
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M16.8 3.7a2.1 2.1 0 0 1 3 3L7.3 19.2 3 20.3l1.1-4.3Z"/><path d="M13.5 20h7"/></svg>
                  重命名
                </button>
                <button on:click|stopPropagation={() => forkSession(s.id)}>
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M5 6.5h11"/><circle cx="18.3" cy="6.5" r="1.7"/><path d="M5 6.5c5.5 0 3.5 11 9.5 11"/><circle cx="18.3" cy="17.5" r="1.7"/></svg>
                  分叉会话
                </button>
                <button on:click|stopPropagation={() => toggleArchive(s)}>
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3.5" width="18" height="5" rx="1.2"/><path d="M4.8 8.5V19a1.8 1.8 0 0 0 1.8 1.8h10.8a1.8 1.8 0 0 0 1.8-1.8V8.5"/><path d="M9.8 12.2h4.4"/></svg>
                  {s.archived ? '取消归档' : '归档会话'}
                </button>
              </div>
            {/if}
          </div>
        {:else if row.type === 'more'}
          <button class="expand-more" on:click={() => (showAllFlags = { ...showAllFlags, [row.path]: true })}>
            展开其余 {row.rest} 个会话
          </button>
        {:else}
          <div class="side-empty">{searchQ ? '没有匹配的会话' : '暂无会话，点上方「新会话」开始'}</div>
        {/if}
      {/each}
    </div>

    <div class="side-foot">
      <button class="foot-btn" on:click={() => (settingsOpen = true)}>设置</button>
    </div>
    <div class="resize-handle" on:mousedown={startResize}></div>
  </aside>

  <main class="chat">
    {#if sidebarCollapsed}
      <button class="sidebar-restore" title="展开侧栏" on:click={() => (sidebarCollapsed = false)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="3" y="4.5" width="18" height="15" rx="3.2"/><line x1="9.8" y1="4.5" x2="9.8" y2="19.5"/></svg>
      </button>
    {/if}
    <header class="chat-head">
      <div>
        <div class="chat-title">对话</div>
        <div class="chat-sub">P6 恢复与续命 · 会话落盘可恢复 · 改动前自动检查点</div>
      </div>
    </header>

    <div class="chat-body" bind:this={chatBody} on:scroll={onChatScroll}>
      {#if messages.length === 0}
        <div class="empty-hint">输入消息开始对话（Enter 发送）</div>
      {/if}
      {#each messages as m (m.id)}
        {#if m.role === 'user'}
          <div class="msg user">
            <div class="bubble user-bubble">
              {#if m.attachments && m.attachments.length > 0}
                <div class="msg-attachments">
                  {#each m.attachments as a}
                    {#if a.mime && a.mime.startsWith('image/')}
                      <img
                        class="msg-att-img"
                        src={a.data}
                        alt={a.name}
                        title="点击查看原图"
                        on:click={() => (previewImageModal = a.data)}
                      />
                    {:else if a.mime && a.mime.startsWith('video/')}
                      <video class="msg-att-video" controls src={a.data}>
                        <track kind="captions" />
                      </video>
                    {:else if a.mime && a.mime.startsWith('audio/')}
                      <audio class="msg-att-audio" controls src={a.data}></audio>
                    {:else}
                      <div class="msg-att-file">📎 {a.name}</div>
                    {/if}
                  {/each}
                </div>
              {/if}
              {#if m.content}
                <div class="user-text">{m.content}</div>
              {/if}
            </div>
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
              {#if m.approval}
                <div class="approval-box" class:danger={m.approval.risk >= 2}>
                  <div class="approval-head">
                    <span class="approval-title"
                      >⚠️ 请求执行：<b>{m.approval.tool}</b>
                      <span class="approval-risk risk-{m.approval.risk}"
                        >{m.approval.riskLabel || riskLabelFor(m.approval.risk)}</span
                      ></span
                    >
                    {#if m.approval.reason}
                      <span class="approval-reason" class:danger={m.approval.risk >= 2}>{m.approval.reason}</span>
                    {/if}
                    <span class="tool-args" title={m.approval.args}>{m.approval.args}</span>
                  </div>
                  {#if m.approval.decided === null}
                    <div class="approval-actions">
                      <button class="btn" on:click={() => decideApproval(m.approval.id, true)}>允许</button>
                      {#if m.approval.risk < 2}
                        <button class="btn ghost" on:click={() => decideApproval(m.approval.id, true, true)}
                          >允许·本会话同类不再问</button
                        >
                      {/if}
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
        {:else if m.role === 'cp'}
          <div class="cp-bar">
            <span class="cp-sum">› <b>{m.cp.files}</b> 个文件已更改</span>
            <span class="cp-add">+{m.cp.additions}</span>
            <span class="cp-del">−{m.cp.deletions}</span>
            <button class="cp-undo" on:click={() => undoCheckpoint(m.cp)}>↩ 撤销</button>
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

    <div class="handoff-bar">
      <button class="handoff-btn" on:click={makeHandoff} title="把当前会话压成交接文档 HANDOFF.md（写入工作区）">交接文档</button>
    </div>

    <footer class="chat-input">
      <div class="composer">
        {#if pendingAttachments.length > 0}
          <div class="composer-attachments">
            {#each pendingAttachments as att, idx}
              <div class="att-chip" title={`${att.name} (${fmtBytes(att.size)})`}>
                {#if att.mime.startsWith('image/')}
                  <img class="att-thumb" src={att.data} alt={att.name} />
                {:else if att.mime.startsWith('video/')}
                  <span class="att-badge">🎬</span>
                {:else if att.mime.startsWith('audio/')}
                  <span class="att-badge">🎵</span>
                {:else}
                  <span class="att-badge">📎</span>
                {/if}
                <span class="att-name">{att.name}</span>
                <span class="att-size">{fmtBytes(att.size)}</span>
                <button class="att-del" on:click={() => removeAttachment(idx)} title="移除">✕</button>
              </div>
            {/each}
          </div>
        {/if}
        <textarea
          class="composer-input"
          rows="1"
          bind:value={input}
          placeholder="提出后续修改要求（支持粘贴图片截图、拖拽文件或点 📎 附件）"
          on:keydown={handleKey}
          on:input={autoRows}
          on:paste={handlePaste}
          on:dragover|preventDefault
          on:drop={handleDrop}
          disabled={streaming}
        ></textarea>
        <div class="composer-bar">
          <div class="composer-left">
            <button class="icon-btn" title="新建对话" on:click={startNewSession} disabled={streaming}>＋</button>
            <input
              type="file"
              bind:this={fileInput}
              multiple
              accept="image/*,video/*,audio/*"
              style="display:none"
              on:change={onFileSelect}
            />
            <button
              class="icon-btn"
              title="添加多模态媒体附件（图片/视频/音频）"
              on:click={() => fileInput && fileInput.click()}
              disabled={streaming}
            >
              📎
            </button>
            <div class="bar-menu">
              <button
                class="pill"
                class:danger={approvalMode === 'full-access'}
                on:click={() => (approvalMenuOpen = !approvalMenuOpen)}
                title="审批档位：控制工具执行要不要先问你"
              >
                <span class="pill-label">审批:</span> {APPROVAL_LABELS[approvalMode] || approvalMode}
                <span class="caret">▾</span>
              </button>
              {#if approvalMenuOpen}
                <div class="bar-dropdown">
                  {#each Object.entries(APPROVAL_LABELS) as [val, label]}
                    <button class="bar-item" class:active={val === approvalMode} on:click={() => pickApproval(val)}>
                      {label}
                      <span class="bar-item-hint">
                        {val === 'read-only' ? '改动要批' : val === 'auto' ? '沙箱内全自动' : '脱沙箱！'}
                      </span>
                    </button>
                  {/each}
                </div>
              {/if}
            </div>
          </div>
          <div class="composer-right">
            {#if ctxUsage.budget > 0}
              <div class="ctx-wrap">
                <svg class="ctx-ring" viewBox="0 0 30 30" width="22" height="22" title="上下文占用">
                  <circle class="ctx-track" cx="15" cy="15" r="13" />
                  <circle
                    class="ctx-arc"
                    cx="15"
                    cy="15"
                    r="13"
                    stroke={ctxColor}
                    stroke-dasharray={`${(ctxCirc * ctxArcPct) / 100} ${ctxCirc}`}
                    transform="rotate(-90 15 15)"
                  />
                </svg>
                <span class="ctx-pct" class:warn={ctxPct >= 60} class:crit={ctxPct >= 85}>{ctxPctText}</span>
                <div class="ctx-card">
                  <div class="ctx-card-head">
                    <b>上下文容量</b>
                    <span>{fmtTok(ctxUsage.tokens)}/{fmtTok(ctxUsage.budget)}（{ctxPct}%）</span>
                  </div>
                  <div class="ctx-bar"><div class="ctx-bar-fill" style:width={`${ctxPct}%`}></div></div>
                  {#each ctxRows as row}
                    <div class="ctx-row">
                      <span class="ctx-dot" style:background={row.color}></span>
                      <span class="ctx-label">{row.label}</span>
                      <span class="ctx-val">{row.pct}</span>
                    </div>
                  {/each}
                  <div class="ctx-row cache-row">
                    <span class="ctx-label">平均缓存命中率</span>
                    <span class="ctx-val">{apiTotals.prompt ? ((apiTotals.cached / apiTotals.prompt) * 100).toFixed(1) + '%' : '—'}</span>
                  </div>
                  <div class="ctx-card-foot">超过预算自动归档旧工具结果并折叠早期对话</div>
                </div>
              </div>
            {:else}
              <!-- 常驻空环：会话还没用过（无预算数据）时也显示空轨道环 -->
              <div class="ctx-wrap" title="上下文占用（发送消息后显示详情）">
                <svg class="ctx-ring" viewBox="0 0 30 30" width="22" height="22">
                  <circle class="ctx-track" cx="15" cy="15" r="13" />
                </svg>
              </div>
            {/if}
            <div class="bar-menu">
              <button class="pill model-pill" on:click={() => (modelMenuOpen = !modelMenuOpen)} title="切换模型">
                {model || '未选模型'}
                <span class="caret">▾</span>
              </button>
              {#if modelMenuOpen}
                <div class="bar-dropdown right">
                  {#if savedModels.length === 0}
                    <div class="menu-empty">暂无模型，先到设置里获取</div>
                  {/if}
                  {#each savedModels as m}
                    <button class="bar-item" class:active={m === model} on:click={() => { pickModel(m); modelMenuOpen = false; }}>
                      {m}
                    </button>
                  {/each}
                  <div class="menu-sep"></div>
                  <button class="bar-item" on:click={() => { modelMenuOpen = false; settingsOpen = true; }}>
                    管理模型…
                  </button>
                </div>
              {/if}
            </div>
            <div class="bar-menu">
              <button
                class="pill think-pill"
                class:active={thinking === 'on' || thinking === 'low' || thinking === 'medium' || thinking === 'high'}
                on:click={() => (thinkMenuOpen = !thinkMenuOpen)}
                title={thinkHint}
              >
                <span class="pill-label">思考:</span> {THINK_LABELS[thinking] || '默认'}
                <span class="caret">▾</span>
              </button>
              {#if thinkMenuOpen}
                <div class="bar-dropdown right compact">
                  <button class="bar-item" class:active={thinking === ''} on:click={() => pickThinking('')}>
                    默认
                  </button>

                  {#if thinkingStyle === 'deepseek'}
                    <!-- DeepSeek-V4：默认 -> 低 -> 中 -> 高 -> 最大 -> 关闭 -->
                    <button class="bar-item" class:active={thinking === 'low'} on:click={() => pickThinking('low')}>
                      低
                    </button>
                    <button class="bar-item" class:active={thinking === 'medium'} on:click={() => pickThinking('medium')}>
                      中
                    </button>
                    <button class="bar-item" class:active={thinking === 'high' || thinking === 'on'} on:click={() => pickThinking('high')}>
                      高
                    </button>
                    <button class="bar-item" class:active={thinking === 'max'} on:click={() => pickThinking('max')}>
                      最大
                    </button>
                    <button class="bar-item" class:active={thinking === 'off'} on:click={() => pickThinking('off')}>
                      关闭
                    </button>
                  {:else if thinkingStyle === 'openai'}
                    <!-- OpenAI (o1/o3/GPT-5)：默认 -> 低 -> 中 -> 高 -> 关闭 -->
                    <button class="bar-item" class:active={thinking === 'low'} on:click={() => pickThinking('low')}>
                      低
                    </button>
                    <button class="bar-item" class:active={thinking === 'medium' || thinking === 'on'} on:click={() => pickThinking('medium')}>
                      中
                    </button>
                    <button class="bar-item" class:active={thinking === 'high' || thinking === 'max'} on:click={() => pickThinking('high')}>
                      高
                    </button>
                    <button class="bar-item" class:active={thinking === 'off'} on:click={() => pickThinking('off')}>
                      关闭
                    </button>
                  {:else if thinkingStyle === 'none'}
                    <!-- 极简模式 / 不发思考参数 -->
                    <button class="bar-item" class:active={thinking === 'off'} on:click={() => pickThinking('off')}>
                      关闭
                    </button>
                  {:else}
                    <!-- 通义千问 (Qwen)、智谱 GLM、字节豆包、本地服务及通用渠道：官方原生开关 开启 / 关闭 -->
                    <button class="bar-item" class:active={thinking === 'on' || thinking === 'high' || thinking === 'medium' || thinking === 'low' || thinking === 'max'} on:click={() => pickThinking('on')}>
                      开启
                    </button>
                    <button class="bar-item" class:active={thinking === 'off'} on:click={() => pickThinking('off')}>
                      关闭
                    </button>
                  {/if}
                </div>
              {/if}
            </div>
            {#if streaming}
              <button class="send-btn stop" on:click={stop} title="停止">■</button>
            {:else}
              <button class="send-btn" on:click={send} disabled={!input.trim()} title="发送（Enter）">↑</button>
            {/if}
          </div>
        </div>
      </div>
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

      <div class="field">
        <label>模型参数（tokens，0=恢复默认）</label>
        <div class="sub-row">
          <input
            class="num-input"
            type="number"
            min="0"
            bind:value={ctxLen}
            title="上下文长度：决定圆环预算与历史裁剪时机"
            on:change={() => autoSave({ context_length: Number(ctxLen) || 0 }, 0)}
          />
          <input
            class="num-input"
            type="number"
            min="0"
            bind:value={maxOut}
            title="最大输出长度：随请求作为 max_tokens 发给渠道"
            on:change={() => autoSave({ max_output_tokens: Number(maxOut) || 0 }, 0)}
          />
        </div>
        <div class="param-hint">左：上下文长度　右：最大输出长度</div>
      </div>

      <div class="field">
        <label>采样参数（留空=渠道默认）</label>
        <div class="sub-row">
          <input
            class="num-input"
            type="number" step="0.1" min="0" max="2"
            placeholder="温度"
            bind:value={temperature}
            title="temperature：越低越稳定，0=确定性输出"
            on:change={() => autoSave({ temperature: temperature === null || temperature === '' ? -1 : Number(temperature) }, 0)}
          />
          <input
            class="num-input"
            type="number" step="0.05" min="0" max="1"
            placeholder="top_p"
            bind:value={topP}
            title="top_p：核采样截断"
            on:change={() => autoSave({ top_p: topP === null || topP === '' ? -1 : Number(topP) }, 0)}
          />
        </div>
        <div class="param-hint">左：temperature（-1 清除）　右：top_p（-1 清除）</div>
      </div>

      <div class="field">
        <label>思考参数形态（中转渠道探测不准时用）</label>
        <select
          bind:value={thinkingStyleOverride}
          on:change={() => autoSave({ thinking_style: thinkingStyleOverride || 'auto' }, 0)}
        >
          <option value={null}>自动（按端点探测）</option>
          <option value="none">不发任何思考参数（none）</option>
          <option value="openai">OpenAI · reasoning_effort 档位</option>
          <option value="deepseek">DeepSeek/小米 · thinking 对象</option>
          <option value="zai">智谱/豆包 · thinking 对象</option>
          <option value="qwen">千问/混元 · enable_thinking 布尔</option>
          <option value="qwen-chat-template">本地服务 · chat_template_kwargs（Qwen3 模板）</option>
        </select>
        <div class="param-hint">
          默认按域名/供应商自动探测思考协议；中转/反代认不准时（如商汤式网关）在这里显式指定，on/off 立即按该形态发参
        </div>
      </div>

      <div class="field">
        <label>模型能力标记（多模态转接与工具调用依据）</label>
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
        <div class="param-hint">
          {capabilitiesUserSet ? '已手动标记：切换/重选模型不再自动改写这些勾选' : '自动推导：随模型名变化（切换模型会按名字重推）'}
          ；若主力模型未勾选对应媒体能力（如纯文本模型），遇到图片/音视频时将自动转接给下方设置的解析专家
        </div>
        <div class="sub-row">
          <button class="btn" on:click={recomputeCapabilities} title="丢弃手动标记，按当前模型名重新推导能力标记">
            按模型名重新推导
          </button>
        </div>
      </div>

      <div class="field roles-section">
        <label>角色分工与多模态转接（专职专家席位）</label>
        <div class="roles-grid">
          <div class="role-item">
            <span class="role-name">会话摘要 (Summary)</span>
            <select
              bind:value={roles.summary}
              on:change={() => autoSave({ roles }, 0)}
            >
              <option value={null}>未配置：由主力渠道代跑摘要</option>
              {#each configs as c}
                <option value={c.id}>{configLabel(c)} · {c.model || '未选模型'}</option>
              {/each}
            </select>
          </div>
          <div class="role-item">
            <span class="role-name">标题生成 (Title)</span>
            <select
              bind:value={roles.title}
              on:change={() => autoSave({ roles }, 0)}
            >
              <option value={null}>未配置：由主力渠道代跑标题</option>
              {#each configs as c}
                <option value={c.id}>{configLabel(c)} · {c.model || '未选模型'}</option>
              {/each}
            </select>
          </div>
          <div class="role-item">
            <span class="role-name">图片理解专家 (Vision)</span>
            <select
              bind:value={roles.vision}
              on:change={() => autoSave({ roles }, 0)}
            >
              <option value={null}>未配置：主力支持图片时直连，不支持时才需要配置专家</option>
              {#each configs as c}
                <option value={c.id}>{configLabel(c)} · {c.model || '未选模型'}</option>
              {/each}
            </select>
          </div>
          <div class="role-item">
            <span class="role-name">视频理解专家 (Video)</span>
            <select
              bind:value={roles.video}
              on:change={() => autoSave({ roles }, 0)}
            >
              <option value={null}>未配置：主力支持视频时直连，不支持时才需要配置专家</option>
              {#each configs as c}
                <option value={c.id}>{configLabel(c)} · {c.model || '未选模型'}</option>
              {/each}
            </select>
          </div>
          <div class="role-item">
            <span class="role-name">音频理解专家 (Audio)</span>
            <select
              bind:value={roles.audio}
              on:change={() => autoSave({ roles }, 0)}
            >
              <option value={null}>未配置：主力支持音频时直连，不支持时跳过并注入占位提示</option>
              {#each configs as c}
                <option value={c.id}>{configLabel(c)} · {c.model || '未选模型'}</option>
              {/each}
            </select>
          </div>
          <div class="role-item">
            <span class="role-name">降级备用渠道 (Fallbacks)</span>
            <input
              placeholder="逗号分隔配置名/id；留空用 config.toml [gateway].fallback_configs"
              value={(roles.fallbacks || []).join(', ')}
              title="主渠道当轮调用失败时按序切换重试；每次切换发 Notice"
              on:change={(e) => {
                roles.fallbacks = e.target.value.split(/[,，]/).map((s) => s.trim()).filter(Boolean);
                autoSave({ roles }, 0);
              }}
            />
          </div>
        </div>
        <div class="param-hint">摘要/标题未配置=主力代跑；多模态专家仅主力不支持该媒体时启用，未配置则注入占位提示</div>
      </div>
    </div>
  </div>
{/if}

{#if wsModalOpen}
  <div class="modal-backdrop" on:click={() => (wsModalOpen = false)}>
    <div class="modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>切换工作区</b>
        <span class="hint">{wsMsg}</span>
        <button class="btn ghost" on:click={() => (wsModalOpen = false)}>关闭</button>
      </div>
      <div class="field">
        <label>当前工作区</label>
        <div class="ws-current" title={workspace}>{workspace || '（工程根）'}</div>
      </div>
      <div class="field">
        <label>本地文件夹路径（agent 的文件读写/命令执行/影子仓库都在这里，与 agent 自身目录隔离）</label>
        <div class="sub-row">
          <input bind:value={wsPath} placeholder="如 D:\projects\demo 或 C:\Users\你\test" />
          <button class="btn primary" on:click={() => applyWorkspace(wsPath)} disabled={!wsPath.trim()}>应用</button>
        </div>
      </div>
      {#if recentWorkspaces.length}
        <div class="field">
          <label>最近使用</label>
          <div class="ws-recent">
            {#each recentWorkspaces as w}
              <button class="ws-chip" class:active={w === workspace} title={w} on:click={() => applyWorkspace(w)}>
                {w.split(/[\\/]/).filter(Boolean).pop()}
              </button>
            {/each}
          </div>
        </div>
      {/if}
      <div class="param-hint">切换后：会话列表/记忆层（.agent、AGENTS.md）按新工作区重开；进行中的任务不受影响</div>
    </div>
  </div>
{/if}

{#if renameModal}
  <div class="modal-backdrop" on:click={() => (renameModal = null)}>
    <div class="modal rename-modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>{renameModal.kind === 'session' ? '重命名会话' : '重命名项目'}</b>
        <button class="btn ghost" on:click={() => (renameModal = null)}>关闭</button>
      </div>
      <div class="field">
        <input
          class="rename-input"
          bind:value={renameModal.value}
          placeholder={renameModal.kind === 'project' ? '留空恢复为文件夹名' : '会话标题'}
          on:keydown={(e) => e.key === 'Enter' && submitRename()}
        />
      </div>
      <div class="rename-actions">
        <button class="btn" on:click={() => (renameModal = null)}>取消</button>
        <button class="btn primary" on:click={submitRename}>确定</button>
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
          {@const caps = inferCapabilities(m)}
          <label class="model-item">
            <input type="checkbox" checked={isChecked(m)} on:change={() => toggleModel(m)} />
            <span class="m-name">{m}</span>
            <span class="m-caps">
              {#if caps.vision}<span class="m-tag" title="支持图片理解">图片</span>{/if}
              {#if caps.video}<span class="m-tag" title="支持视频理解">视频</span>{/if}
              {#if caps.audio}<span class="m-tag" title="支持音频理解">音频</span>{/if}
            </span>
          </label>
        {/each}
      </div>
    </div>
  </div>
{/if}

{#if previewImageModal}
  <div class="modal-backdrop" on:click={() => (previewImageModal = null)}>
    <div class="image-modal" on:click|stopPropagation>
      <img src={previewImageModal} alt="大图预览" />
      <button class="image-modal-close" on:click={() => (previewImageModal = null)}>✕</button>
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
    width: 300px;
    box-sizing: border-box; /* P9 修复：宽=sidebarW 语义对齐（渲染宽不再多出 padding+border 25px，拖拽手柄贴手指） */
    flex: none;
    background: #151517;
    border-right: 1px solid #232325;
    display: flex;
    flex-direction: column;
    padding: 14px 12px 10px;
    position: relative;
  }
  .sidebar.resizing {
    border-right-color: #3d3d41;
  }
  .resize-handle {
    position: absolute;
    top: 0;
    bottom: 0;
    right: -3px;
    width: 6px;
    cursor: col-resize;
    z-index: 20;
  }
  .resize-handle:hover {
    background: rgba(255, 255, 255, 0.08);
  }
  .sidebar.collapsed {
    display: none;
  }
  .side-head {
    flex: none;
  }
  .brand-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 2px 4px 14px;
  }
  .whale {
    width: 30px;
    height: 24px;
    color: #fff;
    flex: none;
  }
  .brand-name {
    flex: 1;
    font-size: 20px;
    font-weight: 700;
    color: #f3f4f6;
    letter-spacing: 0.2px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .icon-btn {
    width: 34px;
    height: 34px;
    border-radius: 9px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: transparent;
    border: none;
    color: #a0a3a6;
    cursor: pointer;
    flex: none;
  }
  .icon-btn:hover {
    background: #232326;
    color: #e8e9ea;
  }
  .icon-btn.on {
    background: #272729;
    color: #fff;
  }
  .icon-btn:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .icon-btn svg {
    width: 20px;
    height: 20px;
  }
  .new-chat {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 8px;
    width: 100%;
    height: 46px;
    border-radius: 23px;
    border: 1px solid #2c2c2e;
    background: transparent;
    color: #f3f4f6;
    font-size: 15px;
    cursor: pointer;
  }
  .new-chat:hover {
    background: #1d1d20;
    border-color: #3a3a3d;
  }
  .new-chat:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .new-chat svg {
    width: 19px;
    height: 19px;
  }
  .section-row {
    position: relative;
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 14px 4px 6px;
  }
  .section-label {
    color: #8a8e92;
    font-size: 14px;
    padding-left: 4px;
  }
  .section-tools {
    display: flex;
    gap: 2px;
  }
  .section-tools .icon-btn {
    width: 32px;
    height: 32px;
  }
  .side-search {
    margin: 2px 2px 8px;
    width: calc(100% - 4px);
    box-sizing: border-box;
    background: #1d1d20;
    border: 1px solid #2c2c2e;
    border-radius: 10px;
    color: #f3f4f6;
    font-size: 14px;
    padding: 9px 12px;
    outline: none;
  }
  .side-search:focus {
    border-color: #3d3d41;
  }
  .manage-bar {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 4px 2px 10px;
    flex-wrap: wrap;
  }
  .m-count {
    color: #8a8e92;
    font-size: 13px;
    margin-right: auto;
    padding-left: 4px;
  }
  .m-btn {
    background: #1d1d20;
    border: 1px solid #2c2c2e;
    color: #d6d7d9;
    font-size: 13px;
    border-radius: 8px;
    padding: 5px 10px;
    cursor: pointer;
  }
  .m-btn:hover:not(:disabled) {
    background: #272729;
  }
  .m-btn:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .m-btn.primary {
    background: #e7e9ea;
    color: #17181c;
    border-color: transparent;
    font-weight: 600;
  }
  .side-scroll {
    flex: 1;
    overflow-y: auto;
    margin: 0 -4px;
    padding: 0 4px;
  }
  .proj {
    position: relative;
    display: flex;
    align-items: center;
    gap: 10px;
    height: 48px;
    padding: 0 10px;
    border-radius: 12px;
    cursor: pointer;
    color: #eceef0;
    font-size: 15.5px;
  }
  .proj:hover {
    background: #1e1e21;
  }
  .proj.expanded {
    background: #272729;
  }
  .proj > svg {
    width: 19px;
    height: 19px;
    color: #a0a3a6;
    flex: none;
  }
  .proj-name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .proj-side {
    display: none;
    align-items: center;
    gap: 4px;
  }
  .proj:hover .proj-side,
  .proj.expanded .proj-side {
    display: flex;
  }
  .row-btn {
    width: 28px;
    height: 28px;
    border-radius: 8px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: transparent;
    border: none;
    color: #b9bcc0;
    cursor: pointer;
    flex: none;
  }
  .row-btn:hover {
    background: #333338;
    color: #fff;
  }
  .row-btn.boxed {
    border: 1.4px solid #85888c;
  }
  .row-btn.on {
    border-color: #fff;
    color: #fff;
  }
  .row-btn svg {
    width: 17px;
    height: 17px;
  }
  .conv {
    position: relative;
    display: flex;
    align-items: center;
    gap: 8px;
    height: 42px;
    padding: 0 10px 0 38px;
    border-radius: 10px;
    cursor: pointer;
    color: #bebec0;
    font-size: 14.5px;
  }
  .conv:hover {
    background: #1e1e21;
  }
  .conv.active {
    background: #272729;
    color: #f3f4f6;
  }
  .conv-pin {
    width: 15px;
    height: 15px;
    color: #7c7f83;
    flex: none;
  }
  .conv-check {
    width: 16px;
    height: 16px;
    border: 1.5px solid #4a4d52;
    border-radius: 4.5px;
    flex: none;
    display: flex;
    align-items: center;
    justify-content: center;
    font-size: 12px;
    line-height: 1;
    color: #17181c;
  }
  .conv-check.on {
    background: #e7e9ea;
    border-color: #e7e9ea;
  }
  .conv-title {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .conv-title.archived {
    opacity: 0.55;
  }
  .arch-tag {
    font-size: 11px;
    color: #8a8e92;
    border: 1px solid #333338;
    border-radius: 6px;
    padding: 1px 6px;
    flex: none;
  }
  .conv-more {
    display: none;
    width: 26px;
    height: 26px;
    border-radius: 7px;
    align-items: center;
    justify-content: center;
    background: transparent;
    border: none;
    color: #a0a3a6;
    cursor: pointer;
    flex: none;
  }
  .conv-more:hover {
    background: #333338;
    color: #fff;
  }
  .conv-more svg {
    width: 16px;
    height: 16px;
  }
  .conv:hover .conv-more {
    display: flex;
  }
  .conv:hover .conv-time {
    display: none;
  }
  .conv-time {
    font-size: 12.5px;
    color: #7f8388;
    flex: none;
  }
  .expand-more {
    display: block;
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    color: #8a8e92;
    font-size: 14px;
    padding: 10px 10px 10px 38px;
    cursor: pointer;
    border-radius: 10px;
  }
  .expand-more:hover {
    color: #c9cbce;
    background: #1e1e21;
  }
  .side-empty {
    color: #6f7276;
    font-size: 14px;
    padding: 18px 12px;
  }
  .side-menu {
    position: absolute;
    z-index: 40;
    min-width: 172px;
    background: #1b1b1e;
    border: 1px solid #2c2c2e;
    border-radius: 14px;
    padding: 6px;
    box-shadow: 0 12px 32px rgba(0, 0, 0, 0.55);
  }
  .sess-menu,
  .proj-menu {
    top: calc(100% + 4px);
    right: 6px;
  }
  .view-menu {
    top: calc(100% + 2px);
    right: 0;
  }
  .side-menu button {
    display: flex;
    align-items: center;
    gap: 10px;
    width: 100%;
    background: none;
    border: none;
    color: #f3f4f6;
    font-size: 14px;
    padding: 10px 12px;
    border-radius: 9px;
    cursor: pointer;
    text-align: left;
    white-space: nowrap;
  }
  .side-menu button:hover {
    background: #272729;
  }
  .side-menu button svg {
    width: 17px;
    height: 17px;
    color: #a0a3a6;
    flex: none;
  }
  .side-menu .sel-check {
    margin-left: auto;
    color: #fff;
  }
  .rename-modal {
    min-width: 380px;
  }
  .rename-input {
    width: 100%;
    box-sizing: border-box;
    background: #1d1d20;
    border: 1px solid #2c2c2e;
    border-radius: 10px;
    color: #f3f4f6;
    font-size: 14px;
    padding: 9px 12px;
    outline: none;
  }
  .rename-input:focus {
    border-color: #3d3d41;
  }
  .rename-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
  .sidebar-restore {
    position: absolute;
    left: 12px;
    top: 12px;
    z-index: 25;
    width: 36px;
    height: 36px;
    border-radius: 9px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: #1d1d20;
    border: 1px solid #2c2c2e;
    color: #a0a3a6;
    cursor: pointer;
  }
  .sidebar-restore:hover {
    color: #fff;
    background: #272729;
  }
  .sidebar-restore svg {
    width: 20px;
    height: 20px;
  }
  .side-foot {
    flex: none;
    display: flex;
    gap: 8px;
    padding-top: 8px;
  }
  .foot-btn {
    flex: 1;
    background: transparent;
    border: 1px solid #26262a;
    color: #8a8e92;
    font-size: 13px;
    border-radius: 10px;
    padding: 8px 0;
    cursor: pointer;
  }
  .foot-btn:hover {
    color: #d6d7d9;
    background: #1d1d20;
  }
  .handoff-bar {
    flex: none;
    display: flex;
    justify-content: flex-end;
    padding: 0 16px 4px;
  }
  .handoff-btn {
    background: transparent;
    border: none;
    color: #6f7276;
    font-size: 12px;
    padding: 3px 8px;
    border-radius: 7px;
    cursor: pointer;
  }
  .handoff-btn:hover {
    color: #c9cbce;
    background: #1e1e21;
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
  .ctx-wrap {
    position: relative;
    display: flex;
    align-items: center;
    flex-shrink: 0;
  }
  .ctx-ring {
    display: block;
    cursor: default;
  }
  .ctx-card {
    display: none;
    position: absolute;
    bottom: calc(100% + 12px);
    right: -8px;
    width: 300px;
    background: #1b1d23;
    border: 1px solid #3d4150;
    border-radius: 10px;
    padding: 12px 14px;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.5);
    z-index: 20;
    font-size: 13px;
  }
  /* 悬停桥：卡片与环之间的间隙保持 hover 不断 */
  .ctx-card::after {
    content: '';
    position: absolute;
    top: 100%;
    right: 0;
    width: 60%;
    height: 12px;
  }
  .ctx-wrap:hover .ctx-card {
    display: block;
  }
  .ctx-card-head {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    margin-bottom: 10px;
    color: #e8eaf0;
  }
  .ctx-card-head span {
    color: #9aa0ae;
    font-size: 12px;
  }
  .ctx-bar {
    height: 8px;
    border-radius: 4px;
    background: #2b2e36;
    overflow: hidden;
    margin-bottom: 12px;
  }
  .ctx-bar-fill {
    height: 100%;
    border-radius: 4px;
    background: #3b82f6;
    transition: width 0.3s;
  }
  .ctx-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 0;
    color: #cdd2dd;
  }
  .ctx-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .ctx-label {
    flex: 1;
  }
  .ctx-val {
    color: #9aa0ae;
    font-variant-numeric: tabular-nums;
  }
  .ctx-card-foot {
    margin-top: 10px;
    padding-top: 8px;
    border-top: 1px solid #2f323c;
    color: #6f7686;
    font-size: 11px;
  }
  .cache-row {
    margin-top: 8px;
    padding-top: 8px;
    border-top: 1px solid #2f323c;
  }
  .num-input {
    width: 100%;
    background: #1b1d23;
    color: #cdd2dd;
    border: 1px solid #2f323c;
    border-radius: 6px;
    padding: 6px 8px;
    font-size: 12px;
  }
  .param-hint {
    margin-top: 4px;
    color: #6f7686;
    font-size: 11px;
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
  .approval-box.danger {
    background: #2b1717;
    border-color: #8c3131;
  }
  .approval-head {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
    color: #e8d9a0;
  }
  .approval-reason {
    display: inline-block;
    padding: 1px 6px;
    border-radius: 4px;
    background: #4a3e1b;
    color: #fae17d;
    font-size: 11px;
  }
  .approval-reason.danger {
    background: #541d1d;
    color: #ff9b9b;
  }
  /* P9 A4：风险档位徽章（安全/常规改动/高危操作，与 RiskLevel::label 对齐） */
  .approval-risk {
    display: inline-block;
    padding: 1px 6px;
    border-radius: 4px;
    font-size: 11px;
    font-weight: 600;
  }
  .approval-risk.risk-0 {
    background: #1d3a28;
    color: #8fe3ae;
  }
  .approval-risk.risk-1 {
    background: #1f2c44;
    color: #9ec1ff;
  }
  .approval-risk.risk-2 {
    background: #541d1d;
    color: #ff9b9b;
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

  /* 待发送附件条 */
  .composer-attachments {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    padding: 8px 12px 4px;
    border-bottom: 1px solid #2a2d37;
  }
  .att-chip {
    display: flex;
    align-items: center;
    gap: 6px;
    background: #232630;
    border: 1px solid #363a45;
    border-radius: 6px;
    padding: 3px 8px;
    font-size: 12px;
    color: #cdd2dd;
    max-width: 240px;
  }
  .att-thumb {
    width: 22px;
    height: 22px;
    object-fit: cover;
    border-radius: 4px;
  }
  .att-badge {
    font-size: 13px;
  }
  .att-name {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 120px;
  }
  .att-size {
    font-size: 10.5px;
    color: #6f7686;
  }
  .att-del {
    background: transparent;
    border: none;
    color: #9aa0ae;
    cursor: pointer;
    font-size: 11px;
    padding: 0 2px;
    line-height: 1;
  }
  .att-del:hover {
    color: #ff7b72;
  }

  /* 气泡内的附件渲染 */
  .msg-attachments {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    margin-bottom: 6px;
  }
  .msg-att-img {
    max-width: 280px;
    max-height: 200px;
    border-radius: 6px;
    border: 1px solid rgba(255, 255, 255, 0.1);
    cursor: zoom-in;
    object-fit: contain;
    background: #000;
  }
  .msg-att-video {
    max-width: 320px;
    max-height: 220px;
    border-radius: 6px;
  }
  .msg-att-audio {
    max-width: 320px;
    height: 38px;
    border-radius: 6px;
  }
  .msg-att-file {
    font-size: 12px;
    color: #8bb2ff;
    padding: 4px 8px;
    background: rgba(255, 255, 255, 0.06);
    border-radius: 4px;
  }

  /* 图片预览模态框 */
  .image-modal {
    position: relative;
    max-width: 90vw;
    max-height: 90vh;
    display: flex;
    justify-content: center;
    align-items: center;
  }
  .image-modal img {
    max-width: 90vw;
    max-height: 90vh;
    object-fit: contain;
    border-radius: 8px;
    box-shadow: 0 10px 30px rgba(0, 0, 0, 0.6);
  }
  .image-modal-close {
    position: absolute;
    top: -12px;
    right: -12px;
    width: 28px;
    height: 28px;
    background: #2a2d37;
    border: 1px solid #484f58;
    color: #fff;
    border-radius: 50%;
    cursor: pointer;
    font-size: 14px;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .cp-bar {
    display: flex;
    align-items: center;
    gap: 10px;
    background: #1b1d23;
    border: 1px solid #2f323c;
    border-radius: 8px;
    padding: 8px 12px;
    font-size: 13px;
    color: #9aa0ae;
    max-width: 720px;
  }
  .cp-sum b {
    color: #e6e9f0;
  }
  .cp-add {
    color: #4ade80;
    font-family: monospace;
    font-size: 12px;
  }
  .cp-del {
    color: #f87171;
    font-family: monospace;
    font-size: 12px;
  }
  .cp-undo {
    margin-left: auto;
    background: transparent;
    border: none;
    color: #cdd2dd;
    font-size: 12px;
    cursor: pointer;
    padding: 2px 4px;
  }
  .cp-undo:hover {
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
  /* —— ZCode 式输入框：圆角容器 = 输入区 + 底部工具栏 —— */
  .chat-input {
    padding: 10px 16px 14px;
    border-top: 1px solid #2a2d37;
    background: #1b1d23;
  }
  .composer {
    background: #23252d;
    border: 1px solid #333742;
    border-radius: 14px;
    padding: 4px 10px 8px;
    transition: border-color 0.15s;
  }
  .composer:focus-within {
    border-color: #4a5062;
  }
  .composer-input {
    display: block;
    width: 100%;
    box-sizing: border-box;
    resize: none;
    border: none;
    outline: none;
    background: transparent;
    color: #e6e6eb;
    font-size: 14px;
    line-height: 1.5;
    padding: 8px 6px 6px;
    max-height: 160px;
    font-family: inherit;
  }
  .composer-input::placeholder {
    color: #6b707c;
  }
  .composer-input:disabled {
    opacity: 0.6;
  }
  .composer-bar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .composer-left,
  .composer-right {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }
  .icon-btn {
    width: 28px;
    height: 28px;
    border-radius: 7px;
    border: none;
    background: transparent;
    color: #9aa0ae;
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
  }
  .icon-btn:hover {
    background: #2a2d37;
    color: #e6e6eb;
  }
  .icon-btn:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .bar-menu {
    position: relative;
    min-width: 0;
  }
  .pill {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    max-width: 240px;
    padding: 4px 8px;
    border-radius: 8px;
    border: none;
    background: transparent;
    color: #b6bcc9;
    font-size: 12.5px;
    cursor: pointer;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .pill:hover,
  .pill.active {
    background: #2a2d37;
    color: #e6e6eb;
  }
  .pill.danger {
    color: #f5a623;
  }
  .pill.danger:hover {
    background: #33281a;
  }
  .pill .pill-icon {
    font-size: 12px;
  }
  .pill .caret {
    font-size: 9px;
    color: #6f7686;
  }
  .model-pill {
    font-family: Consolas, monospace;
  }
  .bar-dropdown {
    position: absolute;
    left: 0;
    bottom: calc(100% + 8px);
    min-width: 230px;
    max-height: 320px;
    overflow-y: auto;
    background: #23252d;
    border: 1px solid #3d4150;
    border-radius: 10px;
    padding: 4px;
    z-index: 40;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.5);
  }
  .bar-dropdown.right {
    left: auto;
    right: 0;
  }
  .bar-dropdown.compact {
    min-width: 90px;
  }
  .bar-dropdown.compact .bar-item {
    justify-content: center;
    padding: 6px 14px;
  }
  .bar-item {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 12px;
    width: 100%;
    text-align: left;
    padding: 7px 10px;
    background: transparent;
    border: none;
    border-radius: 7px;
    color: #e6e6eb;
    font-size: 13px;
    cursor: pointer;
    white-space: nowrap;
  }
  .bar-item:hover {
    background: #2a2d37;
  }
  .bar-item.active {
    background: #2f5fd0;
    color: #fff;
  }
  .bar-item.active .bar-item-hint {
    color: #cdd6f4;
  }
  .bar-item-hint {
    color: #6f7686;
    font-size: 11px;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  /* 上下文圆环：空色轨道 + 实色占用弧 */
  .ctx-ring .ctx-track {
    fill: none;
    stroke: #333742;
    stroke-width: 3;
  }
  .ctx-ring .ctx-arc {
    fill: none;
    stroke-width: 3;
    stroke-linecap: round;
    transition: stroke-dasharray 0.3s;
  }
  .ctx-pct {
    font-size: 11px;
    color: #9aa0ae;
    font-variant-numeric: tabular-nums;
  }
  .ctx-pct.warn {
    color: #facc15;
  }
  .ctx-pct.crit {
    color: #f87171;
  }
  .send-btn {
    width: 32px;
    height: 32px;
    border-radius: 9px;
    border: none;
    background: #2f5fd0;
    color: #fff;
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    flex-shrink: 0;
  }
  .send-btn:hover:not(:disabled) {
    background: #3d7bf5;
  }
  .send-btn:disabled {
    background: #2b2e36;
    color: #6f7686;
    cursor: default;
  }
  .send-btn.stop {
    background: #8a2f2f;
  }
  .send-btn.stop:hover {
    background: #a83a3a;
  }
  /* —— P8.2 工作区切换 —— */
  .ws-current {
    font-family: Consolas, monospace;
    font-size: 12px;
    color: #9aa0ae;
    background: #1b1d23;
    border: 1px solid #2f323c;
    border-radius: 6px;
    padding: 6px 8px;
    word-break: break-all;
  }
  .ws-recent {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
  .ws-chip {
    padding: 4px 10px;
    border-radius: 12px;
    border: 1px solid #333742;
    background: #1b1d23;
    color: #b6bcc9;
    font-size: 12px;
    cursor: pointer;
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ws-chip:hover {
    border-color: #4a5062;
    color: #e6e6eb;
  }
  .ws-chip.active {
    border-color: #2f5fd0;
    color: #cdd6f4;
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
    overflow-y: auto;
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
  .del-key {
    padding: 7px 10px;
    font-size: 12px;
    background: #4d2f2f;
    border-color: #6b3f3f;
    color: #fca5a5;
  }
  .del-key:hover {
    background: #6b3535;
  }
  .caps-row {
    display: flex;
    gap: 14px;
    flex-wrap: wrap;
    background: #1b1d23;
    border: 1px solid #2f323c;
    border-radius: 8px;
    padding: 8px 12px;
  }
  .cap-check {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 13px;
    color: #cdd2dd;
    cursor: pointer;
  }
  .cap-check input {
    cursor: pointer;
  }
  .roles-section {
    border-top: 1px solid #2f323c;
    padding-top: 12px;
    margin-top: 4px;
  }
  .roles-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
    margin-top: 4px;
  }
  .role-item {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .role-name {
    font-size: 11px;
    color: #9aa0ae;
  }
  .role-item select {
    font-size: 12px;
    padding: 6px 8px;
  }
  .m-name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .m-caps {
    display: flex;
    gap: 4px;
    font-size: 11px;
  }
  .m-tag {
    background: #2b334a;
    color: #a5b4fc;
    padding: 1px 6px;
    border-radius: 4px;
    font-family: sans-serif;
  }
</style>
