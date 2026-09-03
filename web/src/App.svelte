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
    if (e.target && e.target.closest && e.target.closest('.bar-menu')) return;
    modelMenuOpen = false;
    thinkMenuOpen = false;
    approvalMenuOpen = false;
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
    roles = s.roles || {};
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
    let caps = capabilities;
    if (list.length && !list.includes(model)) {
      newModel = list[0];
      model = newModel;
      caps = inferCapabilities(newModel);
      capabilities = caps;
    }
    await sendSettings({ models: list, model: newModel, capabilities: caps, api_key: apiKey.trim() || undefined });
  }

  async function selectAllModels() {
    const list = [...fetchedModels].sort();
    savedModels = list;
    let newModel = model;
    let caps = capabilities;
    if (!list.includes(model) && list.length) {
      newModel = list[0];
      model = newModel;
      caps = inferCapabilities(newModel);
      capabilities = caps;
    }
    await sendSettings({ models: list, model: newModel, capabilities: caps });
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
    const caps = inferCapabilities(m);
    capabilities = caps;
    await sendSettings({ model: m, capabilities: caps });
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
</script>

<div class="app">
  <aside class="sidebar">
    <div class="brand">Agent</div>
    <div class="side-project">
      <label class="side-label">工作区</label>
      <div class="proj-name" title={workspace}>📁 {projectName || 'agent'}</div>
      <button class="btn ghost ws-switch" on:click={() => { wsModalOpen = true; wsPath = ''; wsMsg = ''; }} disabled={streaming}>
        切换工作区…
      </button>
      <button class="new-chat" on:click={startNewSession} disabled={streaming}>＋ 新建对话</button>
      <div class="sess-list">
        {#each sessions as s (s.id)}
          <div
            class="sess-item"
            class:active={s.id === currentSessionId}
            role="button"
            tabindex="0"
            on:click={() => loadSession(s.id)}
            on:keydown={(e) => e.key === 'Enter' && loadSession(s.id)}
          >
            <span class="sess-title" title={s.title}>{s.title}</span>
            <span class="sess-side">
              <button class="sess-del" title="删除会话" on:click|stopPropagation={() => deleteSession(s.id)}>✕</button>
              <span class="sess-time">{relTime(s.updated_at)}</span>
            </span>
          </div>
        {/each}
      </div>
      <button class="btn ghost handoff-btn" on:click={makeHandoff}>生成 HANDOFF</button>
    </div>
    <div class="side-foot">
      <button class="btn ghost" on:click={() => (settingsOpen = true)}>设置</button>
    </div>
  </aside>

  <main class="chat">
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
                    <span class="approval-title">⚠️ 请求执行：<b>{m.approval.tool}</b></span>
                    {#if m.approval.reason}
                      <span class="approval-reason" class:danger={m.approval.risk >= 2}>{m.approval.reason}</span>
                    {/if}
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
          若主力模型未勾选对应媒体能力（如纯文本模型），遇到图片/音视频时将自动转接给下方设置的解析专家
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
              <option value={null}>（默认跟随当前主力渠道）</option>
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
              <option value={null}>（默认跟随当前主力渠道）</option>
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
              <option value={null}>（默认跟随当前主力渠道）</option>
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
              <option value={null}>（默认跟随当前主力渠道）</option>
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
              <option value={null}>（未配置，跳过并提示占位）</option>
              {#each configs as c}
                <option value={c.id}>{configLabel(c)} · {c.model || '未选模型'}</option>
              {/each}
            </select>
          </div>
        </div>
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
  .side-project {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 14px 12px 0;
    min-height: 0;
  }
  .proj-name {
    font-size: 13px;
    color: #e6e9f0;
    font-weight: 600;
  }
  .new-chat {
    background: #2f6feb;
    border: none;
    color: #fff;
    border-radius: 6px;
    padding: 6px;
    font-size: 12px;
    cursor: pointer;
  }
  .new-chat:hover {
    background: #3d7bf5;
  }
  .new-chat:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .sess-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
    overflow-y: auto;
    min-height: 0;
  }
  .sess-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 6px;
    padding: 6px 8px;
    border-radius: 6px;
    cursor: pointer;
    font-size: 12px;
    color: #cdd2dd;
  }
  .sess-item:hover {
    background: #262932;
  }
  .sess-item.active {
    background: #2f323c;
  }
  .sess-title {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .sess-side {
    display: flex;
    align-items: center;
    gap: 4px;
    flex-shrink: 0;
  }
  .sess-time {
    color: #9aa0ae;
    font-size: 11px;
  }
  .sess-del {
    background: transparent;
    border: none;
    color: #6b7280;
    cursor: pointer;
    font-size: 10px;
    padding: 0 2px;
    display: none;
  }
  .sess-item:hover .sess-del {
    display: inline;
  }
  .sess-del:hover {
    color: #ff7b72;
  }
  .handoff-btn {
    font-size: 12px;
    padding: 5px 4px;
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
  .ws-switch {
    width: 100%;
    margin: 6px 0 4px;
    font-size: 12px;
    padding: 5px 8px;
  }
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
