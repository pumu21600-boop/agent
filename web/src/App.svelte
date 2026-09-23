<script>
  import { onMount, afterUpdate, tick } from 'svelte';
  import Select from './Select.svelte';
  import Galaxy from './Galaxy.svelte';
  import ImagePreview from './lib/ImagePreview.svelte';
  import RenameModal from './lib/RenameModal.svelte';
  import WorkspaceModal from './lib/WorkspaceModal.svelte';
  import ModelPicker from './lib/ModelPicker.svelte';
  import { fmtBytes, relTime, fmtTok, norm, configLabel, inferCapabilities, gitStatusLabel } from './lib/format.js';

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
    // 四档（P19）；旧档名自动迁移
    const map = { suggest: 'read-only', 'auto-edit': 'auto', 'full-auto': 'auto', local: 'full-access' };
    const saved = localStorage.getItem('approvalMode');
    return map[saved] || saved || 'auto';
  })(); // 审批四档：read-only / sandbox / auto / full-access
  // M1：每会话各自的档位（切会话时圆点显示该会话真实在跑的档，而不是全局最后一次选的）
  let approvalBySession = {};
  $: approvalEffective = approvalBySession[currentSessionId || '@new'] || approvalMode;
  let chatPinned = true; // 是否钉在对话底部（决定是否自动滚屏）
  let ctxUsage = { tokens: 0, budget: 0, breakdown: null }; // 上下文占用（P5 圆环+明细卡）
  let ctxLen = 0; // 当前模型上下文长度（0=默认）
  let maxOut = 0; // 当前模型最大输出长度（0=默认）
  let temperature = null; // 采样温度（null=渠道默认）
  let topP = null; // 核采样（null=渠道默认）
  let thinking = ''; // 思考开关：''=渠道默认 / on / off
  let thinkingStyle = 'auto'; // 生效的思考参数形态（P9，OpenClaw thinkingFormat 词表）
  let capabilities = { tools: true, vision: false, video: false, audio: false }; // P8.5 模型能力标记（纯手动勾选）
  let roles = {}; // P8.5 角色路由映射
  // —— P8.2 工作区 ——
  let workspace = '';
  let recentWorkspaces = [];
  let wsModalOpen = false;
  let wsMsg = '';
  let apiTotals = { prompt: 0, cached: 0 }; // 渠道真实用量累计（平均缓存命中率）
  // 027：用量按会话记账 —— key → { ctx, api }。后台会话的流写各自账本，
  // 切回会话时从账本恢复（跑完的会话不再发事件，不记账就会显示 0）。
  let usageBySession = {};

  // —— 获取模型弹窗 ——
  let modelModalOpen = false;
  let fetchedModels = [];

  // —— 模型快选（输入栏）——
  let modelMenuOpen = false;
  let menuExpandedCfg = ''; // 模型快选菜单里展开的配置组（默认展开当前配置）
  // —— ZCode 式加号菜单：@ 上下文 / 能力 / 技能 ——
  let plusMenuOpen = false;
  let abilityMenuOpen = false;
  let skillMenuOpen = false;
  let ctxPickerOpen = false;
  let ctxFiles = [];
  let ctxLoading = false;
  let ctxQuery = '';
  let composerInputEl;
  const ABILITY_TEMPLATES = [
    { cmd: '解释', text: '请解释这段代码的作用与关键逻辑：\n' },
    { cmd: '审查', text: '请审查以下改动，指出 bug、风险与改进建议：\n' },
    { cmd: '测试', text: '请为下面的功能设计测试用例并写出代码：\n' },
    { cmd: '优化', text: '请优化以下代码的性能与可读性，给出改写版本：\n' },
    { cmd: '总结', text: '请总结以下内容的要点：\n' },
  ];

  // —— 对话（027：按会话隔离）——
  // 每个会话各有自己的消息缓冲与运行流：A 会话在跑不影响切到 B 会话发消息，
  // A 的流式内容也只写回 A 的缓冲（切走再切回不丢、不串台）。
  let messages = []; // 当前显示会话的消息（sessionMsgs 的镜像）
  let sessionMsgs = {}; // key（会话 id 或 '@new'）→ 消息数组
  let streams = {}; // key → { asstId, controller } 正在跑的那条流
  let outbox = {}; // key → [{ id, text, atts }] 排队待发（本轮结束后自动按序发出）
  let streaming = false; // 当前显示会话是否在跑（syncCur 维护）
  let controller = null; // 当前显示会话的 AbortController（停止按钮用）
  let input = '';
  let seq = 0;
  let chatBody;
  // 本页发过的「立即发送」内容：服务端 steer 事件按序配对，避免自己发的那条被重复补气泡
  let localSteers = [];

  const curKey = () => currentSessionId || '@new';
  // 刷新「当前会话」的三个镜像变量（切会话、流开始/结束时调用）
  function syncCur() {
    const k = curKey();
    streaming = !!streams[k];
    controller = streams[k] ? streams[k].controller : null;
    messages = sessionMsgs[k] || [];
  }
  function setMsgs(key, arr) {
    sessionMsgs = { ...sessionMsgs, [key]: arr };
    if (key === curKey()) messages = arr;
  }
  function updMsgs(key, fn) {
    setMsgs(key, fn(sessionMsgs[key] || []));
  }
  function pushMsg(key, msg) {
    updMsgs(key, (a) => [...a, msg]);
  }
  function patchAsst(key, asstId, fn) {
    updMsgs(key, (a) => a.map((m) => (m.id === asstId ? fn({ ...m }) : m)));
  }
  // 会话 id 从 '@new' 变成服务端分配的 id：缓冲 / 流 / 队列一起挪过去
  function rekey(from, to) {
    if (!from || from === to) return;
    const arr = sessionMsgs[from] || [];
    const nextMsgs = { ...sessionMsgs };
    delete nextMsgs[from];
    nextMsgs[to] = arr;
    sessionMsgs = nextMsgs;
    if (streams[from]) {
      const st = streams[from];
      const nextStreams = { ...streams };
      delete nextStreams[from];
      nextStreams[to] = st;
      streams = nextStreams;
    }
    if (outbox[from]) {
      const q = outbox[from];
      const nextOut = { ...outbox };
      delete nextOut[from];
      nextOut[to] = q;
      outbox = nextOut;
    }
    // 027：用量账本跟着 key 走（'@new' → 真 id 时不丢记账）
    if (usageBySession[from]) {
      const u = usageBySession[from];
      const nextU = { ...usageBySession };
      delete nextU[from];
      nextU[to] = u;
      usageBySession = nextU;
    }
    // M1：会话档位跟着 key 走（新会话切过档后拿到 id，档位不能丢）
    if (approvalBySession[from]) {
      const m = approvalBySession[from];
      const nextM = { ...approvalBySession };
      delete nextM[from];
      nextM[to] = m;
      approvalBySession = nextM;
    }
    if (curKey() === from) {
      currentSessionId = to;
      messages = arr;
    }
    syncCur();
  }

  // —— P8.5 多模态媒体附件与大图预览 ——
  let pendingAttachments = []; // 待发送附件列表 [{ name, mime, data, size }]
  let fileInput; // 隐藏文件选择器引用
  let previewImageModal = null; // 放大预览的图片 dataUrl

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

  // P9 B1 Plan/Act 手动档已移除：项目类任务由模型按系统提示自动规划步骤清单（todo list），普通聊天不规划

  onMount(async () => {
    await loadProviders();
    await loadSettings();
    await loadSessions(true);
    refreshGit(); // 顶条显示当前分支/未提交数（非仓库时自动隐藏）
    loadVersion(); // 021：侧栏「关于」行（版本 + 运行根）
    loadExt(); // 021：组件总线扩展清单（有扩展时侧栏才出入口）
    // 021：扩展 iframe 的数据桥（只认自己那个 iframe 的消息）
    window.addEventListener('message', onExtMessage);
    // 点工具栏菜单外部 → 全部收起（问题1）
    document.addEventListener('click', closeMenusOnOutside);
    // 单元素微调标记首屏补盖（会话恢复后元素才就位，故在加载完再盖一次）
    stampMicroUids();
  });

  function closeMenusOnOutside(e) {
    if (e.target && e.target.closest && (e.target.closest('.bar-menu') || e.target.closest('.side-menu') || e.target.closest('.row-btn') || e.target.closest('.conv-more') || e.target.closest('.git-dirty') || e.target.closest('.git-pop'))) return;
    modelMenuOpen = false;
    thinkMenuOpen = false;
    approvalMenuOpen = false;
    viewMenuOpen = false;
    sessMenuId = null;
    projMenuPath = null;
    gitPopOpen = false;
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
      wsModalOpen = false;
      // 工作区变了：会话/消息/圆环全部重置，重新拉列表（各会话缓冲与排队一并清掉）
      currentSessionId = null;
      sessionMsgs = {};
      outbox = {};
      usageBySession = {}; // 旧项目的会话账本一并清
      syncCur();
      restoreUsage('@new');
      await loadSettings();
      await loadSessions(true);
    } catch (e) {
      wsMsg = `切换失败: ${e.message}`;
    }
  }

  afterUpdate(() => {
    // 只有用户本来就钉在底部时才自动滚屏；上滑阅读时不打断
    if (chatPinned && chatBody) chatBody.scrollTop = chatBody.scrollHeight;
    // 单元素微调标记：Svelte 重渲染会重建节点，这里按存下的路径补盖 data-mt
    stampMicroUids();
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
  $: ctxColor = ctxPct >= 85 ? 'var(--tint-red-fg)' : ctxPct >= 60 ? 'var(--tint-yellow-fg)' : 'var(--g6)';
  $: ctxRows = (() => {
    const b = ctxUsage.breakdown || { messages: 0, tools: 0, system: 0, other: 0 };
    const sum = Math.max(1, b.messages + b.tools + b.system + b.other);
    const row = (label, v, color) => ({ label, v, color, pct: ((v / sum) * 100).toFixed(1) + '%' });
    return [
      row('消息', b.messages, 'var(--tint-blue-fg)'),
      row('工具结果', b.tools, 'var(--tint-cyan-fg)'),
      row('系统提示词', b.system, 'var(--tint-blue-fg)'),
      row('其他', b.other, 'var(--g5)'),
    ];
  })();
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
  // 四档语义（P19）：
  // 只读 = 只读操作自动放行，写/执行逐次询问（shell 恒沙箱）
  // 沙箱 = 只读 + 沙箱内执行；工作区和电脑都不改（写文件直接驳回）
  // 自动 = 工作区与整台电脑都可动（越界写按高危先问），shell 恒沙箱
  // 完全访问 = 整台电脑可动且免审批，shell 本机直跑（需 config 总开关）
  const APPROVAL_LABELS = { 'read-only': '只读', sandbox: '沙箱', auto: '自动', 'full-access': '完全访问' };
  function pickApproval(m) {
    const key = currentSessionId || '@new';
    approvalMode = m;
    approvalBySession = { ...approvalBySession, [key]: m };
    approvalMenuOpen = false;
    localStorage.setItem('approvalMode', m);
    // M1：带当前会话 id——多会话并发时只切本会话的档（不带 session 才改全局默认）
    fetch('/api/approval-mode', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ mode: m, session: currentSessionId || '' }),
    })
      .then((r) => r.json())
      .then((r) => {
        // 服务端可能把「完全访问」降级为「自动」（config 未开脱沙箱）：以服务端裁决为准
        if (r && r.mode && r.mode !== m) {
          approvalMode = r.mode;
          approvalBySession = { ...approvalBySession, [key]: r.mode };
          localStorage.setItem('approvalMode', r.mode);
        }
      })
      .catch(() => {});
  }

  function handleKey(e) {
    // 中文输入法组词中的 Enter（isComposing/229）只负责确认候选词，不触发发送
    if (e.isComposing || e.keyCode === 229) return;
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
    capabilities = s.capabilities || { tools: true, vision: false, video: false, audio: false };
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
    // 能力标记完全由用户手动勾选，切换模型不改写
    await sendSettings({ model: m });
  }

  // 快选菜单里跨配置选模型：先切激活配置，再把该配置的模型设为目标
  async function pickCrossModel(cfg, m) {
    modelMenuOpen = false;
    if (cfg.id === activeConfigId) {
      await pickModel(m);
      return;
    }
    await manageConfig('apply', cfg.id);
    if (model !== m) await sendSettings({ model: m });
  }

  // —— 加号菜单：光标处插入文本（@ 引用 / 能力模板 / $ 技能）——
  async function insertAtCursor(text) {
    const el = composerInputEl;
    if (!el) {
      input += text;
      return;
    }
    const s = el.selectionStart ?? input.length;
    const epos = el.selectionEnd ?? input.length;
    input = input.slice(0, s) + text + input.slice(epos);
    await tick(); // 等 Svelte 把新 value 刷进 DOM 后再恢复光标
    el.focus();
    const caret = s + text.length;
    el.setSelectionRange(caret, caret);
    el.style.height = 'auto';
    el.style.height = Math.min(el.scrollHeight, 160) + 'px';
  }

  async function openCtxPicker() {
    plusMenuOpen = false;
    ctxPickerOpen = true;
    ctxQuery = '';
    ctxLoading = true;
    try {
      const r = await fetch('/api/workspace/files').then((r) => r.json());
      ctxFiles = r.files || [];
    } catch (e) {
      ctxFiles = [];
    }
    ctxLoading = false;
  }

  function pickCtxFile(rel) {
    ctxPickerOpen = false;
    insertAtCursor(`@${rel} `);
  }

  function openAbilityMenu() {
    plusMenuOpen = false;
    abilityMenuOpen = true;
  }

  async function openSkillMenu() {
    plusMenuOpen = false;
    if (!skillsData.length) loadSkills();
    if (!mcpServers.length && !wasmPlugins.length) await loadMcp();
    skillMenuOpen = true;
  }

  // —— 顶栏右侧：终端面板 + 产物/预览右侧栏 ——
  let terminalOpen = false;
  let termLines = []; // { cmd, out, ok }
  let termInput = '';
  let termRunning = false;
  let termOutEl;
  // 右侧面板开合与页签持久化（跟 rpWidth/侧栏宽一个 pattern）——否则每次重开 App 面板
  // 都是关着的，内置浏览器得点两级才找得到（用户截图反馈过"浏览器不在侧边栏"）。
  // 首启默认停在「网页」：内置浏览器是该面板最需要被看见的能力。
  let rightPanelOpen = localStorage.getItem('rpOpen') === null ? true : localStorage.getItem('rpOpen') === '1';
  let rpTab = localStorage.getItem('rpTab') || 'web';
  let rpFiles = [];
  let rpLoading = false;
  let rpPreviewPath = '';
  let rpPreviewContent = '';

  // —— P20.1 网页（右侧面板「网页」页签）——
  // P21：截图轮询改 screencast 帧端点（内存直出）；面板可见时 600ms 自动刷帧
  // P22：桌面壳在跑时，页面是壳窗口里的 child webview（真页面真交互）——面板只留
  //      占位区并把矩形报给后端（壳里的 webview 摆到这块）；浏览器直开 5173 时
  //      仍是 screencast 帧流，两条路径由后端 state.mode + 是否在 Tauri 里共同决定。
  let webTabs = []; // { id, agent, title, url, loading, suspended, visible }
  let webActiveId = '';
  let webAddress = '';
  let webBusy = false;
  let webError = ''; // 建页/导航失败的面板内提示（不再静默）
  let webShotT = 0; // 帧缓存穿透参数（自动刷新与动作后递增触发 <img> 重载）
  let webBodyW = 0;
  let webBodyH = 0; // 面板内容区实际尺寸（驱动浏览器视口跟随，截图铺满无黑边）
  let webBodyEl = null; // 面板内容区元素（P22：量矩形给壳里的 child webview 定位）
  let webMode = 'headless'; // 后端宿主模式：shell（壳内真 webview）/ headless（帧流）
  let webMenuOpen = false; // 「⋯」菜单（默认浏览器打开 / 开发者工具）
  // P20.4：菜单改为向上弹（.web-menu-up）——往下弹会落进 .web-body，那块被壳里的
  // child webview（HWND，永远在网页内容之上）盖住，菜单等于看不见；向上弹到页签/
  // 工具栏一带（纯网页内容，实测 71px 菜单 < 88px 可用空间），网页全程不用藏
  function webToggleMenu() {
    webMenuOpen = !webMenuOpen;
  }
  function webCloseMenu() {
    webMenuOpen = false;
  }
  let webDialog = null; // 页面 JS 弹窗（alert/confirm/prompt）：面板内确定/取消，不卡死页面
  // 是否跑在 Tauri 壳里（决定壳内 webview 能不能落到本客户端窗口）
  function inTauriShell() {
    return typeof window !== 'undefined' && !!(window.__TAURI_INTERNALS__ || window.__TAURI__);
  }
  // 原生托管：壳模式 + 本客户端就是壳 → 面板内容区让给 child webview
  $: webNative = webMode === 'shell' && inTauriShell();
  // 右侧面板宽度（左缘拖拽可调，320–820；对齐 ZCode 的自由尺寸）
  let rpWidth = Number(localStorage.getItem('rpWidth')) || 420;
  let rpResizing = false;
  function startRpResize(e) {
    e.preventDefault();
    rpResizing = true;
    document.body.style.cursor = 'col-resize';
    document.body.style.userSelect = 'none';
    const move = (ev) => {
      rpWidth = Math.min(820, Math.max(320, Math.round(window.innerWidth - ev.clientX)));
    };
    const up = () => {
      rpResizing = false;
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
      localStorage.setItem('rpWidth', String(rpWidth));
      window.removeEventListener('mousemove', move);
      window.removeEventListener('mouseup', up);
    };
    window.addEventListener('mousemove', move);
    window.addEventListener('mouseup', up);
  }

  $: webActive = webTabs.find((t) => t.id === webActiveId) || null;
  // 右侧面板开合/页签持久化（换任何路径改这两个状态都会落盘）
  $: {
    try {
      localStorage.setItem('rpOpen', rightPanelOpen ? '1' : '0');
      localStorage.setItem('rpTab', rpTab);
    } catch {}
  }
  // 空白页（about:blank）显示空态引导；有地址就渲染截图
  $: webShowShot = !!(webActive && webActive.url && webActive.url !== 'about:blank');
  $: if (rpTab === 'web' && webBodyW && webActive) webQueueViewport();

  async function loadWebState(keepActive = true) {
    try {
      const s = await webApi('/api/browser/state', { cache: 'no-store' });
      webTabs = s.tabs || [];
      webMode = s.mode || 'headless';
      webDialog = s.dialog || null;
      if (!keepActive || !webTabs.some((t) => t.id === webActiveId)) {
        webActiveId = webTabs.length ? webTabs[webTabs.length - 1].id : '';
      }
      // 地址栏跟随当前标签页。注意不能读 $: webActive（函数内拿到的是本帧旧值），
      // 就地重查；用户正在地址栏打字时（聚焦中）不覆盖。
      const act = webTabs.find((t) => t.id === webActiveId) || null;
      const addrEl = document.querySelector('.web-address');
      if (act && document.activeElement !== addrEl) {
        webAddress = act.url === 'about:blank' ? '' : act.url || '';
      }
      // 上报可见页：后端据此只给这一页跑 screencast 帧流、挂起其余后台页
      if (webActiveId) webActivate(webActiveId);
    } catch (e) {
      webTabs = [];
      webError = String((e && e.message) || e);
    }
  }

  // 面板可见页上报（对齐 ZCode activateTab；驱动 screencast 与后台挂起）
  function webActivate(id) {
    webApi('/api/browser/activate', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id }),
    }).catch(() => {});
  }

  // 面板不看了（切走页签/关面板）：通知后端停帧流、放后台页去挂起；
  // P22 原生托管下空白页（about:blank）也通知隐藏——壳里那个 webview 要让位给空态引导
  function webNotifyHidden() {
    webApi('/api/browser/activate', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id: '' }),
    }).catch(() => {});
  }
  $: if (rpTab !== 'web' || !rightPanelOpen || (webNative && !webShowShot)) webNotifyHidden();

  // P20.4：agent 操作内置浏览器时自动唤起右侧面板（用户要求「在内置浏览器操作的时候
  // agent 需要自动唤起右侧边栏」）。面板没在看网页时每 2s 探一次 state，agent 最近
  // 10s 内碰过浏览器就开面板、切到「网页」、并选中 agent 正在操作的那个页。
  let webAgentWatch = null;
  let webAgentSeen = 0; // 已响应过的 agent 触碰时间戳（防自动开的面板被用户关上又立刻弹开）
  function webStopAgentWatch() {
    if (webAgentWatch) {
      clearInterval(webAgentWatch);
      webAgentWatch = null;
    }
  }
  function webStartAgentWatch() {
    if (webAgentWatch) return;
    webAgentWatch = setInterval(async () => {
      try {
        const s = await webApi('/api/browser/state', { cache: 'no-store' });
        if (!s.agentTouched || Date.now() - s.agentTouched > 10000) return;
        if (s.agentTouched <= webAgentSeen) return; // 这次触碰已响应过，别再弹
        rightPanelOpen = true;
        if (rpTab !== 'web') rpTab = 'web';
        if (s.tabs && s.tabs.length) {
          // 优先 agent 正在操作的那个页（用户要的就是看 agent 干活），其次面板可见页
          const vis = s.tabs.find((t) => t.agent) || s.tabs.find((t) => t.visible) || s.tabs[s.tabs.length - 1];
          webActiveId = vis.id;
          const act = s.tabs.find((t) => t.id === webActiveId);
          if (act && document.activeElement !== document.querySelector('.web-address')) {
            webAddress = act.url === 'about:blank' ? '' : act.url || '';
          }
        }
        webAgentSeen = s.agentTouched;
        webStopAgentWatch();
      } catch {}
    }, 2000);
  }
  $: if (rpTab === 'web' && rightPanelOpen) webStopAgentWatch();
  $: if (rpTab !== 'web' || !rightPanelOpen) webStartAgentWatch();

  // 帧自动刷新：面板开着且有页面时 600ms 一帧（screencast 内存直出，零 CDP 往返）。
  // P22 原生托管不看帧——壳里就是真页面，帧流停掉省编码开销
  let webFrameTimer = null;
  $: if (rpTab === 'web' && rightPanelOpen && webShowShot && !webNative && !webFrameTimer) {
    webFrameTimer = setInterval(() => { webShotT += 1; }, 600);
  } else if ((rpTab !== 'web' || !rightPanelOpen || !webShowShot || webNative) && webFrameTimer) {
    clearInterval(webFrameTimer);
    webFrameTimer = null;
  }

  // 面板 → 后端的响应统一解析：旧版服务（无 /api/browser/*）会返回 404 空体或 HTML，
  // 直接 r.json() 只会报天书——这里翻成「请重启服务」的明确提示。
  async function webApi(path, opts) {
    let resp;
    try {
      resp = await fetch(path, opts);
    } catch (e) {
      throw new Error('服务连不上：' + String(e.message || e));
    }
    const text = await resp.text();
    let data = null;
    try {
      data = text ? JSON.parse(text) : null;
    } catch {
      if (resp.status === 404) throw new Error('服务端没有浏览器接口，跑的还是旧版程序——请重启服务（先 cargo build --release 再 start.bat）');
      throw new Error('服务返回异常（HTTP ' + resp.status + '），请重启服务后重试');
    }
    if (!resp.ok) throw new Error((data && data.error) || '请求失败（HTTP ' + resp.status + '）');
    return data || {};
  }
  function webNormalizeUrl(u) {
    const s = (u || '').trim();
    if (!s) return '';
    if (s === 'about:blank' || /^(https?|file):\/\//.test(s)) return s;
    return 'https://' + s;
  }

  // 浏览器视口跟随面板内容区（防抖 350ms；截图铺满无黑边，网页按真实窗口宽度排版）。
  // P22：x/y 一并上报——shell 模式下后端据此把壳里的 child webview 摆到这块区域。
  // **触发不能只靠 webBodyW/H 的响应式**：窗口横向拉伸时面板宽高都不变、只有 x 变，
  // 响应式不重算 → 壳里 webview 停在旧位置（真机错位 bug）。改用 ResizeObserver 看
  // 尺寸 + window resize 看位置，两条都汇进同一个防抖队列。
  let webViewportTimer = null;
  let webViewportKey = ''; // 已应用的「标签|x,y|宽x高」，防止重复请求打爆 CDP actor
  let webRectObs = null;
  function webQueueViewport() {
    clearTimeout(webViewportTimer);
    webViewportTimer = setTimeout(webSyncViewport, 350);
  }
  function webWatchRect(on) {
    if (on) {
      if (!webRectObs && typeof ResizeObserver !== 'undefined') {
        webRectObs = new ResizeObserver(() => webQueueViewport());
      }
      if (webRectObs && webBodyEl) webRectObs.observe(webBodyEl);
      window.addEventListener('resize', webOnWindowResize);
    } else if (webRectObs && webBodyEl) {
      webRectObs.unobserve(webBodyEl);
      window.removeEventListener('resize', webOnWindowResize);
    }
  }
  function webOnWindowResize() {
    webQueueViewport();
  }
  // 面板在原生托管且停在「网页」时才需要跟（壳里 webview 的显隐与它联动）
  $: if (webNative && rpTab === 'web' && rightPanelOpen && webBodyEl) webWatchRect(true);
  $: if (!(webNative && rpTab === 'web' && rightPanelOpen)) webWatchRect(false);
  function webSyncViewport() {
    if (!webActive || rpTab !== 'web') return;
    const el = webBodyEl;
    if (!el) return;
    const r = el.getBoundingClientRect();
    const w = Math.max(320, Math.min(1600, Math.round(r.width)));
    const h = Math.max(400, Math.min(2400, Math.round(r.height)));
    if (!w || !h) return;
    const x = Math.round(r.left);
    const y = Math.round(r.top);
    const key = webActive.id + '|' + x + ',' + y + '|' + w + 'x' + h;
    if (key === webViewportKey) return;
    webViewportKey = key;
    webApi('/api/browser/viewport', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id: webActive.id, x, y, w, h }),
    })
      .then(() => {
        webShotT += 1;
      })
      .catch(() => {
        webViewportKey = ''; // 失败重试
      });
  }
  function webMenuAction(kind) {
    webCloseMenu();
    if (!webActive) return;
    webApi(`/api/browser/${kind}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ id: webActive.id }),
    }).catch((e) => {
      webError = String(e.message || e);
    });
  }

  // 处理页面 JS 弹窗（确定/取消；prompt 类只能确定或取消，输入内容走后端默认值）
  async function webDialogAction(accept) {
    if (!webActive) return;
    try {
      await webApi('/api/browser/dialog', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id: webActive.id, accept }),
      });
      webDialog = null;
      webShotT += 1;
      await loadWebState();
    } catch (e) {
      webError = String(e.message || e);
    }
  }
  async function webNewTab() {
    if (webBusy) return;
    webBusy = true;
    webError = '';
    try {
      const r = await webApi('/api/browser/new', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: '{}',
      });
      if (r.id) {
        webActiveId = r.id;
      } else {
        webError = r.error || '新建标签页失败';
      }
      await loadWebState(true);
      webShotT += 1;
    } catch (e) {
      webError = String(e.message || e);
    }
    webBusy = false;
  }

  async function webCloseTab(id) {
    webBusy = true;
    webError = '';
    try {
      await webApi(`/api/browser/tab?id=${encodeURIComponent(id)}`, { method: 'DELETE' });
      await loadWebState();
      webShotT += 1;
    } catch {}
    webBusy = false;
  }

  async function webGo() {
    const url = webNormalizeUrl(webAddress);
    if (!url || webBusy) return;
    webBusy = true;
    webError = '';
    try {
      // 没有标签页、或当前是空白页（含壳重启后连接已断的失效页）时，直接开新页导航：
      // 空白页没什么可丢的，也顺带绕开「往死连接上导航 → CDP actor 已退出」
      if (!webActive || !webActive.url || webActive.url === 'about:blank') {
        const r = await webApi('/api/browser/new', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ url }),
        });
        if (r.id) webActiveId = r.id;
        else webError = r.error || '打开网页失败';
      } else {
        const r = await webApi('/api/browser/navigate', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ id: webActive.id, url }),
        });
        if (r.error) webError = r.error;
      }
      webShotT += 1;
      await loadWebState();
    } catch (e) {
      webError = String(e.message || e);
    }
    webBusy = false;
  }

  async function webAction(action) {
    if (!webActive || webBusy) return;
    webBusy = true;
    try {
      await webApi(`/api/browser/${action}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id: webActive.id }),
      });
      webShotT += 1;
      await loadWebState();
    } catch {}
    webBusy = false;
  }

  async function webShotClick(ev) {
    if (!webActive || webBusy) return;
    const rect = ev.currentTarget.getBoundingClientRect();
    const nw = ev.currentTarget.naturalWidth || 1280;
    const nh = ev.currentTarget.naturalHeight || 900;
    const x = Math.round(((ev.clientX - rect.left) * nw) / rect.width);
    const y = Math.round(((ev.clientY - rect.top) * nh) / rect.height);
    webBusy = true;
    try {
      await webApi('/api/browser/click', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id: webActive.id, x, y }),
      });
      webShotT += 1;
      await loadWebState();
    } catch {}
    webBusy = false;
  }

  // 滚轮合并：连续滚动只按 120ms 批量发一次（deltaY 累加），滚动不独占 busy，
  // 截图在批次结束后刷新——一次一来回会把快速滚动卡成幻灯片。
  let webScrollPending = 0;
  let webScrollTimer = null;
  function webShotWheel(ev) {
    ev.preventDefault();
    const dy = Math.max(-800, Math.min(800, ev.deltaY || 0));
    if (!dy || !webActive) return;
    webScrollPending += dy;
    if (webScrollTimer) return;
    webScrollTimer = setTimeout(() => {
      webScrollTimer = null;
      const batch = webScrollPending;
      webScrollPending = 0;
      if (!batch || !webActive || webBusy) return;
      webApi('/api/browser/scroll', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id: webActive.id, deltaY: batch }),
      })
        .then(() => {
          webShotT += 1;
        })
        .catch (() => {});
    }, 120);
  }

  async function runTerm(cmd) {
    termRunning = true;
    termLines = [...termLines, { cmd, out: '', ok: true }];
    termInput = '';
    await tick();
    if (termOutEl) termOutEl.scrollTop = termOutEl.scrollHeight;
    try {
      const r = await fetch('/api/shell', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ command: cmd }),
      });
      termLines = termLines.map((l, i) =>
        i === termLines.length - 1 ? { ...l, out: (r.output || '').trim(), ok: r.ok !== false } : l
      );
    } catch (e) {
      termLines = termLines.map((l, i) => (i === termLines.length - 1 ? { ...l, out: `请求失败: ${e.message || e}`, ok: false } : l));
    }
    termRunning = false;
    await tick();
    if (termOutEl) termOutEl.scrollTop = termOutEl.scrollHeight;
  }

  // 当前任务基线 = 消息流里最近一次 checkpoint
  function currentBaseline() {
    for (let i = messages.length - 1; i >= 0; i--) {
      if (messages[i].cp && messages[i].cp.id) return messages[i].cp.id;
    }
    return '';
  }

  async function loadArtifacts() {
    const baseline = currentBaseline();
    if (!baseline) {
      rpFiles = [];
      rpLoading = false;
      return;
    }
    rpLoading = true;
    try {
      const r = await fetch(`/api/changes?baseline=${encodeURIComponent(baseline)}`).then((r) => r.json());
      rpFiles = r.files || [];
    } catch (e) {
      rpFiles = [];
    }
    rpLoading = false;
  }

  async function previewFile(f) {
    rpPreviewPath = f;
    rpTab = 'preview';
    rpPreviewContent = '';
    try {
      const r = await fetch(`/api/workspace/file?path=${encodeURIComponent(f)}`).then((r) => r.json());
      rpPreviewContent = r.ok ? r.content : `读取失败: ${r.error}`;
    } catch (e) {
      rpPreviewContent = `读取失败: ${e.message || e}`;
    }
  }

  function openRightPanel() {
    // 不再强制跳「产物」——记住用户上次停在哪个页签（首启默认「网页」）
    if (rpTab === 'artifacts') loadArtifacts();
    if (rpTab === 'web') loadWebState();
  }

  // —— 改动条：钉在输入框上方。实时会话用消息流里的检查点；
  // 旧会话（消息流无检查点）回退到影子仓库基线 + diff 文件数。
  // 基线优先取**本会话自己的**检查点（检查点带 Agent-Session 归属），没有才用全局最新一个
  // —— 同一项目里多条会话来回切时，各自的改动条不会互相串。
  let dockCp = null; // { id, files, additions, deletions } additions/deletions 可为 null（旧会话未知）
  let gitInfo = null; // { is_repo, branch, head, dirty, upstream, ahead, behind, files: [{status, path}] }
  let gitPopOpen = false; // 「N 处未提交」点开后的待提交明细
  async function refreshGit() {
    try {
      gitInfo = await fetch('/api/git/status').then((r) => r.json());
    } catch (e) {
      gitInfo = null;
    }
  }
  // porcelain 状态码 → 人看的短标签
  // 点文件 → 右侧面板直接预览内容（改动条里的路径同样可读）
  function openGitFile(path) {
    gitPopOpen = false;
    rightPanelOpen = true;
    terminalOpen = false;
    previewFile(path);
  }

  // —— 手动提交（界面上直接 commit，不必让 AI 代劳）——
  // 默认勾选已跟踪的改动；未跟踪文件（日志、构建产物等）默认不勾，由用户自己决定
  let commitModal = null; // { msg, files: [{ path, status, sel }], busy, err }
  function openCommit() {
    if (!gitInfo || !gitInfo.is_repo) return;
    commitModal = {
      msg: '',
      files: (gitInfo.files || []).map((f) => ({ path: f.path, status: f.status, sel: !f.status.includes('?') })),
      busy: false,
      err: '',
    };
  }
  function toggleCommitFile(path) {
    if (!commitModal) return;
    commitModal = {
      ...commitModal,
      files: commitModal.files.map((f) => (f.path === path ? { ...f, sel: !f.sel } : f)),
    };
  }
  function commitSelCount() {
    return (commitModal?.files || []).filter((f) => f.sel).length;
  }
  async function doCommit() {
    const m = commitModal;
    if (!m || m.busy) return;
    const msg = m.msg.trim();
    if (!msg) {
      commitModal = { ...m, err: '写一句提交信息' };
      return;
    }
    const files = m.files.filter((f) => f.sel).map((f) => f.path);
    if (!files.length) {
      commitModal = { ...m, err: '至少勾一个文件' };
      return;
    }
    commitModal = { ...m, busy: true, err: '' };
    try {
      const r = await fetch('/api/git/commit', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ message: msg, files }),
      }).then((x) => x.json());
      if (!r.ok) {
        commitModal = { ...commitModal, busy: false, err: r.error || '提交失败' };
        return;
      }
      commitModal = null;
      pushMsg(curKey(), { id: nextId(), role: 'system', content: `已提交 ${r.id}（${files.length} 个文件）: ${msg}` });
      await refreshGit();
      refreshDock();
    } catch (e) {
      commitModal = { ...commitModal, busy: false, err: `提交失败: ${e.message || e}` };
    }
  }
  async function refreshDock() {
    refreshGit(); // 分支/脏文件数一并刷新（不阻塞改动条）
    for (let i = messages.length - 1; i >= 0; i--) {
      if (messages[i].cp && messages[i].cp.id) {
        dockCp = messages[i].cp;
        return;
      }
    }
    try {
      const q = currentSessionId ? `?session=${encodeURIComponent(currentSessionId)}` : '';
      const r = await fetch(`/api/checkpoints${q}`).then((r) => r.json());
      const latest = (r.mine || [])[0] || (r.checkpoints || [])[0];
      if (!latest) {
        dockCp = null;
        return;
      }
      const c = await fetch(`/api/changes?baseline=${encodeURIComponent(latest.id)}`).then((x) => x.json());
      dockCp = { id: latest.id, files: (c.files || []).length, additions: null, deletions: null };
    } catch (e) {
      dockCp = null;
    }
  }

  // —— CSS 注入器：用户自定义 CSS，实时生效并记住 ——
  let customCss = localStorage.getItem('customCss') || '';
  // 给每条顶层规则的选择器加 #app 前缀：压过 Svelte scoped 样式（.composer.s-xxx 两级类名）；
  // @media/@keyframes 等整块原样保留
  function escalateCss(css) {
    let out = '';
    let i = 0;
    const n = css.length;
    while (i < n) {
      const brace = css.indexOf('{', i);
      if (brace === -1) {
        out += css.slice(i);
        break;
      }
      const selector = css.slice(i, brace);
      if (selector.trim().startsWith('@')) {
        let depth = 1;
        let j = brace + 1;
        while (j < n && depth > 0) {
          if (css[j] === '{') depth++;
          else if (css[j] === '}') depth--;
          j++;
        }
        out += css.slice(i, j);
        i = j;
      } else {
        const prefixed = selector
          .split(',')
          .map((s) => {
            const t = s.trim();
            return t ? `#app ${t}` : t;
          })
          .join(', ');
        out += `${prefixed}{`;
        // 跳过这条规则的声明块，直到它的关闭 '}'（声明不嵌套）
        const close = css.indexOf('}', brace + 1);
        const end = close === -1 ? n : close + 1;
        out += css.slice(brace + 1, end);
        i = end;
      }
    }
    return out;
  }
  function applyCustomCss() {
    let el = document.getElementById('user-custom-css');
    if (!el) {
      el = document.createElement('style');
      el.id = 'user-custom-css';
      document.head.appendChild(el);
    }
    // 微调规则在前，手动 CSS 在后——注入框永远有最终话语权，手写 background: transparent 能盖掉微调
    const micro = microCss(microTweaks);
    el.textContent = (micro ? `${micro}\n` : '') + escalateCss(customCss);
    localStorage.setItem('customCss', customCss);
  }

  // —— 界面微调模式：右击元素弹出属性面板，实时预览，生成微调规则 ——
  // 元素别名表：语义名 → 稳定选择器（Svelte scoped 类名跨构建不稳定，禁止用原始类名当选择器）。
  // 每个别名可带 pick：命中大容器时进一步收敛到具体控件，避免"一点按钮选中整条底板"。
  //  - 'self'：只在点中容器自身（padding/空隙）时才算容器，点中子元素时继续往下落
  //  - 选择器：命中容器后，在容器内按该选择器找离点击最近的祖先，找到就用它
  const ELEMENT_ALIASES = [
    { sel: '.composer', name: '输入框', pick: 'self' },
    { sel: '.composer-input', name: '输入文本区' },
    { sel: '.composer-bar', name: '输入框工具栏', pick: 'self' },
    { sel: '.icon-btn', name: '图标按钮' },
    { sel: '.pill', name: '工具栏胶囊按钮' },
    { sel: '.send-btn', name: '发送按钮' },
    { sel: '.ctx-ring', name: '上下文圆环' },
    { sel: '.user-bubble', name: '用户消息气泡' },
    { sel: '.asst-bubble', name: 'AI 消息气泡', pick: 'self' },
    { sel: '.bubble', name: '消息气泡', pick: 'self' },
    { sel: '.error-bubble', name: '错误消息气泡' },
    { sel: '.tool-card', name: '工具卡' },
    { sel: '.tool-preview', name: '工具输出（代码块）' },
    { sel: '.tool-head', name: '工具卡头部' },
    { sel: '.tool-badge', name: '工具状态徽章' },
    { sel: '.tool-name', name: '工具名' },
    { sel: '.tool-args', name: '工具参数' },
    { sel: '.thinking', name: '思考折叠块', pick: 'self' },
    { sel: '.thinking-body', name: '思考内容' },
    { sel: '.approval-box', name: '审批框' },
    { sel: '.term-panel', name: '终端面板', pick: 'self' },
    { sel: '.term-head', name: '终端标题栏' },
    { sel: '.term-title', name: '终端标题' },
    { sel: '.term-hint', name: '终端提示文字' },
    { sel: '.term-clear', name: '终端清空按钮' },
    { sel: '.term-close', name: '终端关闭按钮' },
    { sel: '.term-in', name: '终端输入栏' },
    { sel: '.term-out', name: '终端输出区' },
    { sel: '.term-line', name: '终端输出行' },
    { sel: '.term-prompt', name: '终端命令提示符' },
    { sel: '.term-text', name: '终端输出文本' },
    { sel: '.cp-bar', name: '改动条', pick: 'self' },
    { sel: '.cp-btn', name: '改动条按钮' },
    { sel: '.new-chat', name: '新会话按钮' },
    { sel: '.chat-head', name: '顶栏', pick: 'self' },
    { sel: '.right-panel', name: '产物面板', pick: 'self' },
    { sel: '.rp-tabs', name: '产物面板标签栏', pick: 'self' },
    { sel: '.rp-tabs button', name: '产物面板标签' },
    { sel: '.rp-body', name: '产物面板内容区' },
    { sel: '.rp-empty', name: '产物面板空提示' },
    { sel: '.rp-refresh', name: '产物刷新按钮' },
    { sel: '.rp-file', name: '产物文件条目' },
    { sel: '.rp-preview', name: '预览代码块' },
    { sel: '.rp-preview-path', name: '预览文件路径' },
    { sel: '.modal.settings-modal', name: '设置弹窗', pick: 'self' },
    { sel: '.settings-nav button', name: '设置导航按钮' },
    { sel: '.theme-opt', name: '主题选项按钮' },
    { sel: '.agents-editor', name: 'CSS 注入框' },
    { sel: '.field input', name: '设置输入框' },
    { sel: '.field select', name: '设置下拉框' },
    { sel: '.btn', name: '按钮' },
    { sel: '.csel', name: '下拉框' },
    { sel: '.csel-caret', name: '下拉箭头' },
    { sel: '.csel-menu', name: '下拉菜单', pick: 'self' },
    { sel: '.csel-item', name: '下拉选项' },
    { sel: '.chip', name: '配置标签' },
    { sel: '.sidebar', name: '侧栏', pick: 'self' },
    { sel: '.side-head', name: '侧栏头部', pick: 'self' },
    { sel: '.row-btn', name: '侧栏工具按钮' },
    { sel: '.conv', name: '会话条目' },
    { sel: '.conv-title', name: '会话标题' },
    { sel: '.conv-time', name: '会话时间' },
    { sel: '.conv-more', name: '会话更多按钮' },
    { sel: '.proj', name: '项目行' },
    { sel: '.proj-name', name: '项目名' },
    { sel: '.foot-btn', name: '侧栏底部按钮' },
    { sel: '.handoff-btn', name: '交接文档按钮' },
    { sel: '.cp-dock', name: '改动条容器', pick: 'self' },
    { sel: '.cp-undo', name: '撤销按钮' },
    { sel: '.ctx-wrap', name: '上下文圆环容器', pick: 'self' },
    { sel: '.ctx-card', name: '上下文明细卡' },
    { sel: '.head-btn', name: '顶栏按钮' },
    { sel: '.user-text', name: '用户消息文字' },
    { sel: '.bar-dropdown', name: '下拉菜单', pick: 'self' },
    { sel: '.bar-item', name: '下拉菜单项' },
  ];
  let editMode = false;
  let microPanel = null; // { x, y, key, name, sel, el, color, bg, bgOpacity, radius, blur, fontSize, bgTransparent, *Dirty }
  let microTweaks = (() => {
    try {
      const raw = JSON.parse(localStorage.getItem('microTweaks') || '{}');
      // 一次性迁移（老代码残留）：旧版"应用"会把当时算出的背景色一并落盘，
      // 深色主题下就是 #17171a 这类近黑实色，带 #app 前缀后压过壁纸半透明规则，
      // 表现为"消息里几块死黑、写 CSS 也去不掉"。清掉全部存量背景键（只清一次，
      // 迁移后用户手动改的背景会正常保留），其他属性（圆角/字号/文字色）保留。
      if (localStorage.getItem('microTweaksBgMigrated') !== '2') {
        for (const k of Object.keys(raw || {})) {
          const t = raw[k];
          if (t && typeof t === 'object') {
            delete t.bg;
            delete t.bgOpacity;
          }
        }
        try { localStorage.setItem('microTweaks', JSON.stringify(raw)); } catch (e) {}
        try { localStorage.setItem('microTweaksBgMigrated', '2'); } catch (e) {}
      }
      return raw;
    } catch (e) {
      return {};
    }
  })();

  // 取色/透明度解析：同时支持 rgb()/rgba() 与 Chromium 解析 color-mix 后给出的
  // color(srgb r g b / a)（分量是 0-1 小数）。取不出返回 null，由调用方决定不存该属性，
  // 禁止再兜底成 #000000（那是黑底叠层的直接来源）
  function parseCssColor(c) {
    const s = String(c || '').trim();
    let m = s.match(/^rgba?\(\s*([\d.]+)[,\s]+([\d.]+)[,\s]+([\d.]+)(?:\s*[,/]\s*([\d.]+%?))?\s*\)$/);
    if (m) {
      const a = m[4] == null ? 1 : m[4].endsWith('%') ? parseFloat(m[4]) / 100 : parseFloat(m[4]);
      return { r: +m[1], g: +m[2], b: +m[3], a };
    }
    m = s.match(/^color\(srgb\s+([\d.]+)\s+([\d.]+)\s+([\d.]+)(?:\s*\/\s*([\d.]+))?\s*\)$/);
    if (m) {
      return { r: Math.round(+m[1] * 255), g: Math.round(+m[2] * 255), b: Math.round(+m[3] * 255), a: m[4] == null ? 1 : +m[4] };
    }
    return null;
  }
  function rgbToHex(rgb) {
    const c = parseCssColor(rgb);
    if (!c) return null;
    return '#' + [c.r, c.g, c.b].map((x) => Math.max(0, Math.min(255, Math.round(x))).toString(16).padStart(2, '0')).join('');
  }
  // 解析不出的按不透明处理
  function rgbAlpha(rgb) {
    const c = parseCssColor(rgb);
    return c ? Math.round(c.a * 100) : 100;
  }

  // 未登记元素：生成结构选择器。优先用稳定类名锚点（.icon-btn/.pill/.btn 等），
  // 比纯 tag:nth-of-type 链更抗排序变化；实在没有类名才回落序号链（止于 #app）
  function structuralSelector(el) {
    const clsOf = (node) => {
      const cls = typeof node.className === 'string' ? node.className : (node.getAttribute && node.getAttribute('class')) || '';
      return cls.split(/\s+/).filter((c) => c && !/^s-[A-Za-z0-9_-]+$/.test(c));
    };
    // 自身或近祖先有稳定类名：直接用 ".类名" 锚定（多类取首个非 svelte 作用域类）
    let cur = el;
    for (let up = 0; up < 3 && cur && cur.id !== 'app'; up++) {
      const stable = clsOf(cur).find((c) => /^(icon-btn|pill|btn|chip|bar-item|conv|proj|row-btn|foot-btn|theme-opt|agents-editor|ctx-ring|ctx-card|send-btn|new-chat|composer-input|tool-card|tool-preview|tool-head|thinking-body|approval-box|cp-bar|cp-undo|head-btn|user-text|proj-name|conv-title)$/.test(c));
      if (stable) return `.${stable}`;
      cur = cur.parentElement;
    }
    const parts = [];
    cur = el;
    while (cur && cur.id !== 'app') {
      if (cur.id && cur !== el) {
        parts.unshift(`#${CSS.escape(cur.id)}`);
        return parts.join(' > ');
      }
      const parent = cur.parentElement;
      if (!parent) break;
      const sibs = [...parent.children].filter((c) => c.tagName === cur.tagName);
      let seg = cur.tagName.toLowerCase();
      if (sibs.length > 1) seg += `:nth-of-type(${sibs.indexOf(cur) + 1})`;
      parts.unshift(seg);
      cur = parent;
    }
    return parts.join(' > ');
  }

  // —— 单元素定位（修"改一处、同类全改"）：给具体元素盖一个 data-mt 唯一标记，
  // 规则写成 #app [data-mt="mtXXXX"]，只作用这一个元素；Svelte 重渲染会重建节点丢标记，
  // 所以存下"从 #app 起的孩子序号路径"，每次更新后按路径找回并补盖 ——
  function pathOf(el) {
    const parts = [];
    let cur = el;
    while (cur && cur.id !== 'app') {
      const parent = cur.parentElement;
      if (!parent) break;
      parts.unshift(`${cur.tagName.toLowerCase()}:${[...parent.children].indexOf(cur)}`);
      cur = parent;
    }
    return parts.join('/');
  }
  function findByPath(path) {
    let cur = document.getElementById('app');
    for (const seg of String(path || '').split('/').filter(Boolean)) {
      const i = seg.lastIndexOf(':');
      const tag = seg.slice(0, i);
      const idx = Number(seg.slice(i + 1));
      if (!cur || !cur.children[idx]) return null;
      cur = cur.children[idx];
      if (tag && cur.tagName.toLowerCase() !== tag) return null;
    }
    return cur;
  }
  function makeUid() {
    return 'mt' + Math.random().toString(36).slice(2, 8);
  }
  // 补盖标记：只在元素还匹配原选择器时才盖，避免路径漂移后错标到别的元素
  function stampMicroUids() {
    for (const t of Object.values(microTweaks || {})) {
      if (!t || !t.uid || !t.path) continue;
      if (document.querySelector(`[data-mt="${t.uid}"]`)) continue;
      const el = findByPath(t.path);
      if (!el) continue;
      try {
        if (!t.sel || el.matches(t.sel)) el.setAttribute('data-mt', t.uid);
      } catch (e) {}
    }
  }
  function unstampAll() {
    for (const el of document.querySelectorAll('#app [data-mt]')) el.removeAttribute('data-mt');
  }

  function onMicroContext(e) {
    if (!editMode) return;
    if (e.target.closest && e.target.closest('.micro-panel')) return;
    // 目标归一化：SVG/文本节点先收敛到可调元素——点中图标里的 path 先升到 button，
    // 点中按钮里的文字/图标先升到该按钮，避免结构选择器落到 svg/path/span 碎片上
    let target = e.target;
    if (target && target.nodeType !== 1) target = target.parentElement;
    if (target && target.closest) {
      const svgHit = target.closest('svg');
      if (svgHit && svgHit.closest('button')) target = svgHit.closest('button');
      else if (target instanceof SVGElement) target = target.closest('button') || target.parentElement || target;
    }
    if (!target || !target.closest) return;
    // 候选收集：所有命中的别名都算上，按"离点击最近"排序；
    // pick:'self' 的大容器只有点中自身（空隙/padding）时才参选，点中子元素时让位给具体控件；
    // pick:选择器的容器先收敛到内部最近控件，找不到才回落容器自身
    const cands = [];
    for (const a of ELEMENT_ALIASES) {
      const el = target.closest(a.sel);
      if (!el) continue;
      let depth = 0;
      let cur = target;
      while (cur && cur !== el) {
        cur = cur.parentElement;
        depth++;
      }
      if (a.pick === 'self' && depth > 0) continue;
      if (typeof a.pick === 'string' && a.pick !== 'self') {
        const inner = target.closest(a.pick);
        if (inner && el.contains(inner)) {
          let d2 = 0;
          let c2 = target;
          while (c2 && c2 !== inner) {
            c2 = c2.parentElement;
            d2++;
          }
          cands.push({ alias: { sel: a.pick, name: `${a.name} · 子项` }, el: inner, depth: d2 });
          continue;
        }
      }
      cands.push({ alias: a, el, depth });
    }
    cands.sort((x, y) => x.depth - y.depth);
    e.preventDefault();
    if (cands.length && cands[0].depth <= 3) {
      const win = cands[0];
      openMicroPanel(e.clientX, e.clientY, win.alias.name, win.alias.sel, win.el);
      return;
    }
    // 兜底一：按元素自身语义类名精确命中（终端标题栏、清空按钮这类没登记的小条）
    const auto = autoClassTarget(target);
    if (auto) {
      openMicroPanel(e.clientX, e.clientY, auto.name, auto.sel, auto.el);
      return;
    }
    // 兜底二：附近没有别名 → 放宽 pick:'self'，回退到最近的"容器别名"（任意深度）。
    // 比结构选择器更稳：点右侧面板内部就选中「产物面板」，不会再出"未登记元素"的
    // div:nth-of-type 长链（那种选择器一改布局就失效）
    const relaxed = [];
    for (const a of ELEMENT_ALIASES) {
      const el = target.closest(a.sel);
      if (!el) continue;
      let depth = 0;
      let cur = target;
      while (cur && cur !== el) {
        cur = cur.parentElement;
        depth++;
      }
      relaxed.push({ alias: a, el, depth });
    }
    relaxed.sort((x, y) => x.depth - y.depth);
    if (relaxed.length) {
      const win = relaxed[0];
      openMicroPanel(e.clientX, e.clientY, win.alias.name, win.alias.sel, win.el);
      return;
    }
    // 兜底三：整条链都没有登记过 → 结构选择器（止于 #app 的序号链）
    openMicroPanel(e.clientX, e.clientY, '', structuralSelector(target), target);
  }

  // 元素自身是否画了背景（透明度 >5% 才算；transparent/全透明都不算）
  function paintsBg(el) {
    const c = parseCssColor(getComputedStyle(el).backgroundColor);
    return !!c && c.a > 0.05;
  }
  // 往上层找第一个真正画背景的祖先：用户常右键在透明的内容层上（如 .rp-body），
  // 改它当然没反应——真正那块"黑"是父层画的，这里把父层找出来供一键跳转
  function findBgHost(el) {
    let cur = el.parentElement;
    while (cur && cur.id !== 'app') {
      if (paintsBg(cur)) {
        for (const a of ELEMENT_ALIASES) {
          try {
            if (cur.matches(a.sel)) return { name: a.name, sel: a.sel, el: cur };
          } catch (e) {}
        }
        return { name: '', sel: structuralSelector(cur), el: cur };
      }
      cur = cur.parentElement;
    }
    return null;
  }

  // 通用兜底：按元素自身的语义类名自动生成选择器。跳过 svelte 作用域类与状态/通用类，
  // 优先带连字符的语义类（term-head、rp-body 这类），这样任何"小条/小按钮"不用逐个登记
  // 也能精确命中，而不是一路回退到整个大面板
  const GENERIC_CLASSES = new Set([
    'active', 'on', 'sel', 'expanded', 'open', 'haskey', 'danger', 'primary', 'ghost', 'stop',
    'running', 'fail', 'ok', 'dim', 'err', 'warn', 'crit', 'archived', 'pinned', 'empty', 'more',
    'group-head', 'wp-pick', 'css-injector', 'micro-readonly', 'content', 'user', 'assistant',
    'boxed', 'left', 'right', 'center', 'small', 'large', 'selected', 'checked', 'risk-0', 'risk-1', 'risk-2',
  ]);
  function autoClassTarget(el) {
    let cur = el;
    for (let up = 0; up < 3 && cur && cur.id !== 'app'; up++) {
      const raw = typeof cur.className === 'string' ? cur.className : '';
      const cls = raw
        .split(/\s+/)
        .filter((c) => c && !/^s-[A-Za-z0-9_-]+$/.test(c) && !GENERIC_CLASSES.has(c));
      if (cls.length) {
        const semantic = cls.find((c) => c.includes('-')) || cls[0];
        return { name: semantic, sel: `.${CSS.escape(semantic)}`, el: cur };
      }
      cur = cur.parentElement;
    }
    return null;
  }

  function openMicroPanel(x, y, name, sel, el) {
    const cs = getComputedStyle(el);
    const key = name || sel;
    // 优先用该元素已存的微调值（面板显示与实际生效一致）；没有才读 computed
    const saved = microTweaks[key] || Object.values(microTweaks).find((t) => t && t.sel === sel) || null;
    const numOf = (v, fallback) => {
      const n = parseFloat(v);
      return Number.isFinite(n) ? n : fallback;
    };
    // computed backdrop-filter: 'none' 或 'blur(Npx)'，提取 N
    const blurNow = (() => {
      const m = String(cs.backdropFilter || '').match(/blur\(([\d.]+)px\)/);
      return m ? parseFloat(m[1]) : 0;
    })();
    // 背景解析：color(srgb ...) 这类计算值也认；解析不出或全透明都算"透明（不动）"，
    // 面板照实显示色值+透明度，不再出现"背景色 #000000、透明 0%"这种吓人的假象
    const bgParsed = parseCssColor(cs.backgroundColor);
    const bgNow = rgbToHex(cs.backgroundColor);
    microPanel = {
      // 位置：右侧留出面板宽度，下方至少留 340px 可用高度（面板自身 max-height 按 y 计算，
      // 配合内部滚动，保证底部「应用/重置/取消」始终可见可点）
      x: Math.max(8, Math.min(x, window.innerWidth - 300)),
      y: Math.max(8, Math.min(y, window.innerHeight - 340)),
      key,
      name: name || '未登记元素',
      sel,
      el,
      color: saved?.color || rgbToHex(cs.color) || '#000000',
      bg: saved?.bg || bgNow || '#000000',
      // 背景取不出（渐变/color-mix 未解析）或全透明时不填黑：bgTransparent=true，
      // 预览与保存都跳过背景；只有用户真的点了颜色框（dirty）才落盘
      bgTransparent: !saved?.bg && (!bgParsed || bgParsed.a === 0),
      bgDirty: false,
      colorDirty: false,
      radiusDirty: false,
      blurDirty: false,
      fontSizeDirty: false,
      bgOpacity: saved != null && saved.bgOpacity != null ? saved.bgOpacity : bgParsed ? Math.round(bgParsed.a * 100) : 0,
      radius: numOf(saved?.radius, Math.round(parseFloat(cs.borderTopLeftRadius) || 0)),
      // 描边：宽度 + 颜色（用户显式改过才落盘；只改颜色时尊重元素原有边框样式）
      border: numOf(saved?.border, Math.round(parseFloat(cs.borderTopWidth) || 0)),
      borderColor: saved?.borderColor || rgbToHex(cs.borderTopColor) || '#000000',
      borderDirty: false,
      borderColorDirty: false,
      blur: numOf(saved?.blur, blurNow),
      fontSize: numOf(saved?.fontSize, Math.round(parseFloat(cs.fontSize) || 14)),
      // 自身透明但上层有实底时，面板给出"背景其实由上级绘制"的跳转入口
      bgHost: !paintsBg(el) ? findBgHost(el) : null,
      // 作用范围：默认只改这一个元素（修"改一处、同类全改"）；
      // 该元素已存的是"同类全部"规则（无 uid）时，切到"同类"以保持一致
      scope: saved && !saved.uid ? 'all' : 'only',
    };
  }

  // 跳到真正绘制背景的上级（用户右键在透明内容层时的救急入口）
  function useBgHost() {
    const p = microPanel;
    if (!p || !p.bgHost) return;
    const h = p.bgHost;
    clearMicroInline(p.el);
    openMicroPanel(p.x, p.y, h.name || '上级元素', h.sel, h.el);
  }

  function microPreview(prop, value) {
    if (!microPanel) return;
    // 脏标记：只记录用户亲手改过的属性，"应用"时只存这些
    const dirtyKey = { bg: 'bgDirty', bgOpacity: 'bgDirty', color: 'colorDirty', radius: 'radiusDirty', blur: 'blurDirty', fontSize: 'fontSizeDirty', border: 'borderDirty', borderColor: 'borderColorDirty' }[prop];
    microPanel = { ...microPanel, [prop]: value, ...(dirtyKey ? { [dirtyKey]: true, ...(prop === 'bg' || prop === 'bgOpacity' ? { bgTransparent: false } : {}) } : {}) };
    // 广播：自绘下拉等浮层收到后重新贴合定位（预览会改动样式/布局，菜单要跟着挪）
    try {
      window.dispatchEvent(new CustomEvent('micro-preview', { detail: { sel: microPanel.sel } }));
    } catch (e) {}
    const el = microPanel.el;
    if (!el || !el.isConnected) return;
    if (prop === 'border') {
      // 拖宽度必须同时给 border-style，否则 border:none 的元素看不出变化
      el.style.borderStyle = 'solid';
      el.style.borderWidth = `${value}px`;
      return;
    }
    if (prop === 'borderColor') {
      el.style.borderStyle = 'solid';
      el.style.borderColor = value;
      return;
    }
    if (prop === 'bg' || prop === 'bgOpacity') {
      // 透明底元素：预览也不烤实色，避免"还没点应用就先黑一块"吓到人
      if (microPanel.bgTransparent) return;
      el.style.backgroundColor = hexWithAlpha(microPanel.bg, microPanel.bgOpacity);
      return;
    }
    if (prop === 'blur') {
      el.style.backdropFilter = value > 0 ? `blur(${value}px)` : 'none';
      return;
    }
    if (prop === 'color') {
      el.style.color = value;
      return;
    }
    el.style[prop === 'radius' ? 'borderRadius' : 'fontSize'] = `${value}px`;
  }

  function hexWithAlpha(hex, opacityPct) {
    const m = /^#([0-9a-f]{6})$/i.exec(hex || '');
    if (!m) return hex;
    const a = Math.min(1, Math.max(0, (opacityPct ?? 100) / 100));
    const r = parseInt(m[1].slice(0, 2), 16);
    const g = parseInt(m[1].slice(2, 4), 16);
    const b = parseInt(m[1].slice(4, 6), 16);
    return `rgba(${r}, ${g}, ${b}, ${a})`;
  }

  function clearMicroInline(el) {
    for (const k of ['color', 'backgroundColor', 'borderRadius', 'backdropFilter', 'fontSize', 'borderWidth', 'borderColor', 'borderStyle']) el.style[k] = '';
  }

  function applyMicro() {
    const p = microPanel;
    if (!p) return;
    // 从面板控件读当前值（按 data-m 属性显式取值，不依赖顺序；不依赖组件状态的事件同步）
    const panelEl = document.querySelector('.micro-panel');
    const val = (name) => (panelEl ? panelEl.querySelector(`[data-m="${name}"]`)?.value : undefined);
    const num = (name) => {
      const v = val(name);
      return v == null || v === '' ? undefined : +v;
    };
    const color = val('color') ?? p.color;
    const bg = val('bg') ?? p.bg;
    const bgOpacity = num('bgOpacity') ?? p.bgOpacity;
    const blurN = num('blur') ?? p.blur;
    const radiusN = num('radius') ?? p.radius;
    const fontSizeN = num('fontSize') ?? p.fontSize;
    // 只落盘用户亲手改过的属性（dirty 标记），但保留该元素上一次已存的其他属性——
    // 老存量/别次改的背景不会因为"这次只改圆角"就被弄丢；没动背景就不碰 bg，透明底元素不会再被误写黑底
    const prev = microTweaks[p.key] && typeof microTweaks[p.key] === 'object' ? microTweaks[p.key] : {};
    const entry = { sel: p.sel, name: p.name };
    if (p.colorDirty) entry.color = color;
    else if (prev.color) entry.color = prev.color;
    if (p.bgTransparent) {
      if (prev.bg) {
        entry.bg = prev.bg;
        if (prev.bgOpacity != null) entry.bgOpacity = prev.bgOpacity;
      }
    } else if (p.bgDirty) {
      entry.bg = bg;
      entry.bgOpacity = bgOpacity;
    } else if (prev.bg) {
      entry.bg = prev.bg;
      if (prev.bgOpacity != null) entry.bgOpacity = prev.bgOpacity;
    }
    if (p.radiusDirty) entry.radius = `${radiusN}px`;
    else if (prev.radius) entry.radius = prev.radius;
    // 描边：宽度/颜色各自独立落盘；只改颜色时尊重原有边框样式
    const borderN = num('border') ?? p.border;
    const borderColorV = val('borderColor') ?? p.borderColor;
    if (p.borderDirty) entry.border = `${borderN}px`;
    else if (prev.border) entry.border = prev.border;
    if (p.borderColorDirty) entry.borderColor = borderColorV;
    else if (prev.borderColor) entry.borderColor = prev.borderColor;
    // 毛玻璃护栏：背景不透明（opacity>=85%）时存 blur 没有视觉意义，反而会在弹窗/遮罩下发灰叠影；
    // 这种组合直接丢弃 blur，只保留其他属性。想要毛玻璃先把背景透明度降下来。
    const bgEffOpaque = p.bgTransparent
      ? false
      : p.bgDirty
        ? (bgOpacity ?? 100) >= 85
        : prev.bg
          ? (prev.bgOpacity ?? 100) >= 85
          : (p.bgOpacity ?? 100) >= 85;
    if (p.blurDirty && !((parseInt(blurN, 10) || 0) > 0 && bgEffOpaque)) entry.blur = blurN;
    else if (prev.blur != null && !(bgEffOpaque && (parseInt(prev.blur, 10) || 0) > 0)) entry.blur = prev.blur;
    if (p.fontSizeDirty) entry.fontSize = `${fontSizeN}px`;
    else if (prev.fontSize) entry.fontSize = prev.fontSize;
    // 作用范围：仅此元素 → 盖唯一标记并记结构路径（重渲染后按路径补盖）；
    // 同类全部 → 清掉标记字段，规则改回元素选择器
    if (p.scope === 'all') {
      if (prev.uid) {
        const old = document.querySelector(`[data-mt="${prev.uid}"]`);
        if (old) old.removeAttribute('data-mt');
      }
    } else {
      entry.uid = prev.uid || makeUid();
      entry.path = pathOf(p.el);
      try {
        p.el.setAttribute('data-mt', entry.uid);
      } catch (e) {}
    }
    clearMicroInline(p.el);
    const cur = { ...microTweaks };
    cur[p.key] = entry;
    microTweaks = cur;
    localStorage.setItem('microTweaks', JSON.stringify(cur));
    applyCustomCss();
    microPanel = null;
  }

  function resetMicro() {
    const p = microPanel;
    if (!p) return;
    clearMicroInline(p.el);
    const prevUid = microTweaks[p.key] && microTweaks[p.key].uid;
    if (prevUid) {
      const stamped = document.querySelector(`[data-mt="${prevUid}"]`);
      if (stamped) stamped.removeAttribute('data-mt');
    }
    const cur = { ...microTweaks };
    delete cur[p.key];
    microTweaks = cur;
    localStorage.setItem('microTweaks', JSON.stringify(cur));
    applyCustomCss();
    microPanel = null;
  }

  // 一键清空所有微调规则（手写 CSS 不动）；用于清掉历史遗留的实色深底等顽固样式
  function clearAllMicro() {
    microTweaks = {};
    localStorage.setItem('microTweaks', '{}');
    unstampAll();
    applyCustomCss();
    settingsMsg = '已清空全部微调规则';
  }

  function cancelMicro() {
    if (!microPanel) return;
    clearMicroInline(microPanel.el);
    microPanel = null;
  }

  // —— CSS 规则导出 / 导入：把「微调规则」与「手写 CSS」打包成一个 JSON 文件，可备份、换机迁移 ——
  let cssImportInput;
  const CSS_RULES_KIND = 'zcode-ui-css-rules';

  function exportCssRules() {
    const payload = {
      type: CSS_RULES_KIND,
      version: 1,
      exportedAt: new Date().toISOString(),
      customCss,
      microTweaks,
    };
    const blob = new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = 'ui-css-rules.json';
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    const n = Object.keys(microTweaks || {}).length;
    settingsMsg = `已导出 CSS 规则（微调 ${n} 条${customCss.trim() ? ' + 手写 CSS' : ''}）`;
  }

  function onImportCssRules(e) {
    const file = e.target.files && e.target.files[0];
    if (e.target) e.target.value = ''; // 复位，允许再次选择同一个文件
    if (!file) return;
    const reader = new FileReader();
    reader.onerror = () => (settingsMsg = '导入失败：文件读取错误');
    reader.onload = (ev) => {
      const text = String(ev.target.result || '');
      let css = '';
      let tweaks = null; // null 表示"这次不动微调规则"（导入纯 CSS 文本时）
      try {
        const data = JSON.parse(text);
        if (!data || typeof data !== 'object' || Array.isArray(data)) throw new Error('不是有效的规则文件');
        if ('customCss' in data || 'microTweaks' in data || data.type === CSS_RULES_KIND) {
          css = typeof data.customCss === 'string' ? data.customCss : '';
          tweaks = data.microTweaks && typeof data.microTweaks === 'object' && !Array.isArray(data.microTweaks) ? data.microTweaks : {};
        } else {
          tweaks = data; // 兼容只含微调规则对象的旧文件
        }
      } catch (err) {
        // 看起来想写 JSON 却解析失败 → 报错；否则当作纯手写 CSS 文本导入
        if (/^[{[]/.test(text.trim())) {
          settingsMsg = `导入失败：${err.message || err}`;
          return;
        }
        css = text;
      }
      let n = 0;
      if (tweaks != null) {
        // 只收下结构合法的微调条目，避免脏数据让微调模式崩溃
        const clean = {};
        for (const [k, v] of Object.entries(tweaks)) {
          if (v && typeof v === 'object' && typeof v.sel === 'string' && v.sel) clean[k] = v;
        }
        tweaks = clean;
        n = Object.keys(tweaks).length;
      }
      const existing = Object.keys(microTweaks || {}).length;
      const overwriteMicro = tweaks != null;
      const scope = overwriteMicro ? `当前的 ${existing} 条微调规则和手写 CSS` : '当前的手写 CSS';
      if ((existing || customCss.trim()) && !confirm(`导入将覆盖${scope}，继续？`)) return;
      customCss = css;
      if (overwriteMicro) {
        microTweaks = tweaks;
        try { localStorage.setItem('microTweaks', JSON.stringify(tweaks)); } catch (err) {}
        unstampAll();
      }
      applyCustomCss();
      settingsMsg = overwriteMicro
        ? `已导入 CSS 规则（微调 ${n} 条${css.trim() ? ' + 手写 CSS' : ''}）`
        : '已导入手写 CSS';
    };
    reader.readAsText(file);
  }

  // 微调规则 → CSS（拼在手动 CSS 之前，手写 CSS 永远有最终话语权）；
  // 只输出当初亲手改过的属性（有键才输出），没动过的背景/颜色不会凭空出现。
  // background 用 background-color 而非简写，避免盖掉原有的渐变/图片/transparent 语义
  function microCss(tweaks) {
    let out = '';
    for (const t of Object.values(tweaks || {})) {
      if (!t || !t.sel) continue;
      const decls = [];
      if (t.color) decls.push(`color: ${t.color}`);
      if (t.bg) decls.push(`background-color: ${hexWithAlpha(t.bg, t.bgOpacity ?? 100)}`);
      if (t.border) decls.push(`border-style: solid`, `border-width: ${t.border}`);
      if (t.borderColor) decls.push(`border-color: ${t.borderColor}`);
      if (t.blur != null && String(t.blur) !== '' && (parseInt(t.blur, 10) || 0) !== 0) decls.push(`backdrop-filter: blur(${parseInt(t.blur, 10) || 0}px)`);
      if (t.radius) decls.push(`border-radius: ${t.radius}`);
      if (t.fontSize) decls.push(`font-size: ${t.fontSize}`);
      if (!decls.length) continue;
      // 单个元素：用 data-mt 唯一标记；同类全部：用元素选择器
      if (t.uid) out += `/* 微调:${t.name || t.sel} · 仅此元素 */\n#app [data-mt="${t.uid}"] { ${decls.join('; ')} }\n`;
      else out += `/* 微调:${t.name || t.sel} · 同类全部 */\n#app ${t.sel} { ${decls.join('; ')} }\n`;
    }
    return out;
  }
  // 启动时应用：微调规则 + 手动 CSS
  applyCustomCss();
  // (hmr-probe)

  // —— 对话 ——
  // 发送（027）：当前显示会话没在跑 → 直接发；正在跑 → **排队**（不发出去），
  // 气泡上出现「立即发送」按钮，点它 = 引导当前这一轮（并进去、不打断）。
  // 别的会话在跑不影响这里（每条会话各有自己的流）。
  async function send() {
    const msg = input.trim();
    const atts = [...pendingAttachments];
    if (!msg && atts.length === 0) return;
    const key = curKey();
    input = '';
    pendingAttachments = [];
    if (streams[key]) {
      queueOut(key, msg, atts);
      return;
    }
    await deliver(key, msg, atts);
  }

  // 排队：先显示成「排队中」气泡，当前这一轮结束后自动按序发出
  function queueOut(key, text, atts) {
    const item = { id: nextId(), text, atts };
    outbox = { ...outbox, [key]: [...(outbox[key] || []), item] };
    pushMsg(key, { id: item.id, role: 'user', content: text, attachments: atts, queued: true });
  }

  // 把「已经上屏的那条用户气泡」转成排队（服务端说这个会话在跑时用；不新增气泡）
  function markQueued(key, text, atts) {
    const arr = sessionMsgs[key] || [];
    const hit = [...arr].reverse().find((m) => m.role === 'user' && m.content === text && !m.queued);
    const id = hit ? hit.id : nextId();
    updMsgs(key, (a) =>
      hit ? a.map((m) => (m.id === id ? { ...m, queued: true } : m)) : [...a, { id, role: 'user', content: text, attachments: atts, queued: true }]
    );
    outbox = { ...outbox, [key]: [...(outbox[key] || []), { id, text, atts }] };
    return id;
  }

  // 真正发一轮（key 会话；调用时该会话没在跑）。返回 false = 这条没送进服务端。
  async function deliver(key, text, atts, opts = {}) {
    const effectiveMsg = text || (atts.length > 0 ? '请查看分析附带的媒体文件' : '');
    if (!effectiveMsg) return true;
    // 发送时收起所有输入区菜单
    plusMenuOpen = false;
    abilityMenuOpen = false;
    skillMenuOpen = false;
    modelMenuOpen = false;
    // 新会话：顶栏标题即时取首条消息（与后端初始标题一致），不等标题生成
    if (!currentSessionId && effectiveMsg) currentTitle = effectiveMsg.slice(0, 50);
    const history = (sessionMsgs[key] || [])
      .filter((m) => (m.role === 'user' && m.content && !m.queued) || (m.role === 'assistant' && m.content && !m.streaming))
      .map((m) => ({ role: m.role, content: m.content }));
    history.push({ role: 'user', content: effectiveMsg });
    const asstId = nextId();
    // 出队 / 立即发送的那条：原地把「排队中」气泡转正（保持顺序），不新增用户气泡
    const hasBubble = opts.bubbleId && (sessionMsgs[key] || []).some((m) => m.id === opts.bubbleId);
    updMsgs(key, (a) => [
      ...a.map((m) => (opts.bubbleId && m.id === opts.bubbleId ? { ...m, queued: false, steered: !!opts.steer } : m)),
      ...(hasBubble ? [] : [{ id: nextId(), role: 'user', content: effectiveMsg, attachments: atts }]),
      { id: asstId, role: 'assistant', content: '', thinking: '', tools: [], streaming: true },
    ]);
    const ac = new AbortController();
    streams = { ...streams, [key]: { asstId, controller: ac } };
    syncCur();
    const stream = streams[key];
    let skey = key; // 会话 id 到手后 key 会从 '@new' 变成真 id
    const patch = (fn) => patchAsst(skey, stream.asstId, fn);
    let ok = false;
    try {
      const res = await fetch('/api/chat', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          messages: history,
          message: effectiveMsg,
          attachments: atts.length > 0 ? atts.map((a) => ({ name: a.name, mime: a.mime, data: a.data })) : undefined,
          model,
          base_url: baseUrl,
          approval_mode: approvalBySession[key] || approvalMode,
          session_id: skey === '@new' ? '' : skey,
          steer: opts.steer === true,
        }),
        signal: ac.signal,
      });
      if (!res.ok) {
        // 服务端拒绝（如 409 同会话并发）：带出服务端文案，不伪装成连接错误
        let detail = '';
        try {
          detail = (await res.json()).error || '';
        } catch {}
        const err = new Error(detail || `HTTP ${res.status}`);
        err.busy = res.status === 409;
        throw err;
      }
      ok = true;
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
                // 新会话：服务端分配了 id → 缓冲 / 流 / 队列挪到真 id 下（显示中的话直接切过去）
                rekey(skey, d.session);
                skey = d.session;
              } else if (d.queued) {
                // 服务端判成「这个会话在跑」（别的窗口刚起了一轮）：本条转排队、撤掉空回答气泡
                updMsgs(skey, (a) => a.filter((m) => m.id !== stream.asstId));
                markQueued(skey, effectiveMsg, atts);
              } else if (d.steered) {
                // 这条已被并进正在跑的一轮（引导）：本页不再给它单开回答气泡
                updMsgs(skey, (a) => a.filter((m) => m.id !== stream.asstId));
              } else if (d.turn) {
                // 排队的消息开始跑了：气泡从「排队中」转正式 + 新开一个回答气泡
                const adds = [];
                for (const m of d.turn.messages || []) {
                  const q = (sessionMsgs[skey] || []).find((x) => x.queued && x.role === 'user' && x.content === m.content);
                  if (q) updMsgs(skey, (a) => a.map((x) => (x.id === q.id ? { ...x, queued: false } : x)));
                  else adds.push({ id: nextId(), role: 'user', content: m.content });
                }
                const nid = nextId();
                stream.asstId = nid;
                updMsgs(skey, (a) => [...a, ...adds, { id: nid, role: 'assistant', content: '', thinking: '', tools: [], streaming: true }]);
              } else if (d.content) patch((m) => ({ ...m, content: m.content + d.content }));
              else if (d.reasoning) patch((m) => ({ ...m, thinking: m.thinking + d.reasoning }));
              // 降级链换渠道重试：丢掉本步已推的半截内容（长度由服务端按 UTF-16 码元数给出）
              else if (d.reset)
                patch((m) => ({
                  ...m,
                  content: m.content.slice(0, Math.max(0, m.content.length - (d.reset.content || 0))),
                  thinking: (m.thinking || '').slice(0, Math.max(0, (m.thinking || '').length - (d.reset.reasoning || 0))),
                }));
              else if (d.steer) {
                // 「立即发送（引导）」的消息并入了本轮：别的窗口发的在这里补气泡；本页自己发过的已补过
                const add = [];
                for (const m of d.steer.messages || []) {
                  if (localSteers.length > 0 && localSteers[0] === m.content) localSteers.shift();
                  else add.push({ id: nextId(), role: 'user', content: m.content });
                }
                if (add.length) updMsgs(skey, (a) => [...a, ...add]);
              } else if (d.tool_call)
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
              else if (d.checkpoint) {
                pushMsg(skey, { id: nextId(), role: 'cp', cp: d.checkpoint });
                refreshDock(); // 改动条数据更新
              }
              else if (d.notice)
                pushMsg(skey, { id: nextId(), role: 'system', content: '[通知] ' + d.notice });
              else if (d.title) {
                loadSessions(true); // P8 TITLE：标题已生成，刷新会话列表
                if (d.title && d.title !== currentTitle) currentTitle = d.title; // 顶栏同步模型生成的标题
              }
              else if (d.context_usage) {
                // 用量按会话记账（后台会话写各自账本）；正在看的才刷到显示
                usageBySession = {
                  ...usageBySession,
                  [skey]: {
                    ctx: { tokens: d.context_usage.tokens, budget: d.context_usage.budget, breakdown: d.context_usage.breakdown || null },
                    api: (usageBySession[skey] || {}).api || { prompt: 0, cached: 0 },
                  },
                };
                if (skey === curKey())
                  ctxUsage = { tokens: d.context_usage.tokens, budget: d.context_usage.budget, breakdown: d.context_usage.breakdown || null };
              }
              else if (d.api_usage) {
                const prev = (usageBySession[skey] || {}).api || { prompt: 0, cached: 0 };
                const api = { prompt: prev.prompt + (d.api_usage.prompt_tokens || 0), cached: prev.cached + (d.api_usage.cached_tokens || 0) };
                usageBySession = {
                  ...usageBySession,
                  [skey]: { ctx: (usageBySession[skey] || {}).ctx || ctxUsage, api },
                };
                if (skey === curKey()) apiTotals = api;
              } else if (d.step_limit) {
                patch((m) => ({ ...m, streaming: false }));
                pushMsg(skey, { id: nextId(), role: 'error', content: `已达步数上限 ${d.step_limit}，循环终止` });
              } else if (d.error) {
                patch((m) => ({ ...m, streaming: false }));
                pushMsg(skey, { id: nextId(), role: 'error', content: d.error });
              } else if (d.cancelled) {
                patch((m) => ({ ...m, streaming: false }));
                pushMsg(skey, { id: nextId(), role: 'system', content: '已停止' });
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
        pushMsg(skey, { id: nextId(), role: 'error', content: e.busy ? e.message : `[连接错误] ${e}` });
      }
      // 停止后把还挂着的审批框标为失效，避免界面停在「等待决定」
      updMsgs(skey, (a) =>
        a.map((m) => (m.approval && m.approval.decided === null ? { ...m, approval: { ...m.approval, decided: false } } : m))
      );
      patch((m) => ({ ...m, streaming: false }));
    } finally {
      // 这条流结束（本会话可再次发送）；只清本会话的流登记，别的会话在跑不受影响
      const nextStreams = { ...streams };
      delete nextStreams[skey];
      streams = nextStreams;
      localSteers = [];
      syncCur();
      loadSessions(); // 刷新会话列表（新会话出现/标题更新）
      // 出队（027）：这一轮跑完了，把排在最前的排队消息自动发出去（发完它再发下一条）
      const q = outbox[skey] || [];
      if (q.length > 0 && !streams[skey]) {
        const [item, ...rest] = q;
        outbox = { ...outbox, [skey]: rest };
        deliver(skey, item.text, item.atts, { bubbleId: item.id });
      }
    }
    return ok;
  }

  // 「立即发送」（027）：把排队的这条马上并进正在跑的那一轮——不打断对话，
  // 模型下一个决策点就看到（引导当前回答）。跑完了/没送进去会自动退回队列。
  async function sendNow(itemId) {
    const key = curKey();
    const item = (outbox[key] || []).find((x) => x.id === itemId);
    if (!item) return;
    if (item.atts && item.atts.length > 0) {
      pushMsg(key, { id: nextId(), role: 'error', content: '「立即发送」暂不支持带附件的消息：这一轮结束后会自动发出去。' });
      return;
    }
    // 先出队（免得本轮结束时又自动发一遍）
    outbox = { ...outbox, [key]: (outbox[key] || []).filter((x) => x.id !== itemId) };
    if (!streams[key]) {
      // 这一轮已经跑完了：按正常一轮发出去
      await deliver(key, item.text, item.atts, { bubbleId: item.id });
      return;
    }
    localSteers.push(item.text);
    const ok = await deliver(key, item.text, [], { bubbleId: item.id, steer: true });
    if (!ok) {
      // 没送进服务端：退回队列，等这一轮结束自动发
      localSteers = localSteers.filter((t) => t !== item.text);
      outbox = { ...outbox, [key]: [item, ...(outbox[key] || [])] };
      updMsgs(key, (a) => a.map((m) => (m.id === item.id ? { ...m, queued: true, steered: false } : m)));
    }
  }

  function stop() {
    // 会话级停止（D2）：只停当前会话在跑的这一轮，别的会话不受影响；
    // 会话 id 未知（极早期）时退化为停全部
    const q = currentSessionId ? `?session=${encodeURIComponent(currentSessionId)}` : '';
    fetch('/cancel' + q, { method: 'POST' });
    if (controller) controller.abort();
  }

  // —— P6 恢复与续命 ——
  function contentText(c) {
    if (typeof c === 'string') return c;
    if (Array.isArray(c)) return c.map((p) => p.text || '').join('');
    return '';
  }

  // —— 顶栏标题（仅对话标题；审批/备份状态在输入区与改动条上，不再重复展示）——
  // currentTitle：直接跟随当前会话（新会话首条消息即时上屏，不等标题生成）
  let currentTitle = '';
  // 注意：必须用 $: 响应式声明——模板里直接调 currentSessionTitle() 不会因 currentTitle 变化而重渲染（Svelte 4 不跨函数追踪依赖）
  $: displayTitle =
    currentTitle ||
    (sessions.find((x) => x.id === currentSessionId)?.title ?? '') ||
    '新的对话';

  // —— P6.1 多会话：列表 / 切换 / 新建（新建不清空历史）——
  async function loadSessions(autoPick = false) {
    try {
      const r = await fetch('/api/sessions').then((r) => r.json());
      projectName = r.project || '';
      sessions = r.sessions || [];
      // 启动/切项目时恢复最近一条会话：只认当前工作区里的会话。
      // 不筛会打开别的项目的会话，而 /api/sessions/open 会把工作区切回那个项目（侧栏项目来回弹）
      if (autoPick && currentSessionId === null) {
        const mine = sessions.filter(
          (s) => !s.workspace || normPath(s.workspace).toLowerCase() === normPath(workspace).toLowerCase(),
        );
        if (mine.length) await loadSession(mine[0].id);
      }
    } catch (e) {
      sessions = [];
    }
  }

  // 切到某会话：从按会话账本恢复用量；没账本的新会话清零（预算保留——圆环常驻）
  function restoreUsage(key) {
    const u = usageBySession[key];
    ctxUsage = u ? u.ctx : { tokens: 0, budget: ctxUsage.budget, breakdown: null };
    apiTotals = u ? u.api : { prompt: 0, cached: 0 };
  }

  // 打开会话：后端会在其所属项目与当前项目不同时先切过去（切会话 = 回到那个项目），
  // 所以走 /api/sessions/open 而不是只读的 /api/session。
  async function loadSession(id) {
    // 027：切会话不再被别的会话的运行挡住（不同对话互不影响）；
    // 该会话自己有在跑的流 → 直接用本地缓冲（服务端文件此刻还没有最新内容）
    if (streams[id]) {
      currentSessionId = id;
      currentTitle = sessions.find((x) => x.id === id)?.title ?? currentTitle;
      syncCur();
      restoreUsage(id); // 从按会话账本恢复（跑完的会话不重发事件，不恢复就显示 0）
      dockCp = null;
      refreshDock();
      return;
    }
    try {
      const r = await fetch('/api/sessions/open', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id }),
      });
      if (r.error) {
        settingsMsg = r.error;
        return;
      }
      currentSessionId = id;
      currentTitle = r.title || (sessions.find((x) => x.id === id)?.title ?? '');
      // 本地已有缓冲就用本地（含流式思考等尚未落盘的新内容），没有才从服务端重建
      if (!sessionMsgs[id]) {
        sessionMsgs = { ...sessionMsgs, [id]: rebuildFromSession(r.messages || []) };
      }
      syncCur();
      restoreUsage(id); // 从按会话账本恢复（跑完的会话不重发事件，不恢复就显示 0）
      dockCp = null;
      if (r.warning) settingsMsg = r.warning;
      if (r.switched) {
        // 项目换了：配置（工作区路径/AGENTS.md）与会话分组都要跟着刷新
        await loadSettings();
        await loadSessions();
      }
      refreshDock();
    } catch (e) {
      // 加载失败不阻塞
    }
  }

  // 新建会话：立即在后端落一条空会话（侧栏当前项目下马上可见），首条消息写进同一条。
  // 同一项目已有一条空会话时后端会复用，连点不会堆出一排。
  async function startNewSession() {
    if (streams['@new']) return; // 还没落 id 的新会话正在跑：等它拿到 id 再说
    currentSessionId = null;
    currentTitle = '';
    dockCp = null;
    sessionMsgs = { ...sessionMsgs, '@new': [] };
    syncCur();
    restoreUsage('@new'); // 新会话：账本里没有它 → 清零
    try {
      const r = await fetch('/api/sessions/new', { method: 'POST' });
      if (r.ok && r.id) {
        rekey('@new', r.id);
        currentTitle = r.title || '';
        if (workspace) {
          // 新会话就在当前项目下：展开该项目，马上能看到这条空会话
          expandedFlags = { ...expandedFlags, [workspace]: true };
          localStorage.setItem('projExpanded', JSON.stringify(expandedFlags));
        }
        await loadSessions();
      } else if (r.error) {
        settingsMsg = `新建会话失败: ${r.error}`;
      }
    } catch (e) {
      settingsMsg = `新建会话失败: ${e.message || e}`;
    }
  }

  async function deleteSession(id) {
    if (!confirm('删除这条会话？其余会话不受影响')) return;
    await fetch(`/api/session?id=${encodeURIComponent(id)}`, { method: 'DELETE' });
    if (id === currentSessionId) {
      await startNewSession();
      return; // startNewSession 里已刷新列表
    }
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
  // 终端面板高度（上缘拖拽可调，120–560，持久化）——对齐 ZCode 的可调尺寸
  let termH = Number(localStorage.getItem('termH')) || 260;
  let termResizing = false;
  function startTermResize(e) {
    e.preventDefault();
    termResizing = true;
    document.body.style.cursor = 'row-resize';
    document.body.style.userSelect = 'none';
    const move = (ev) => {
      termH = Math.min(560, Math.max(120, Math.round(window.innerHeight - ev.clientY)));
    };
    const up = () => {
      termResizing = false;
      document.body.style.cursor = '';
      document.body.style.userSelect = '';
      localStorage.setItem('termH', String(termH));
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
    if (Object.keys(streams).length > 0) {
      // 切项目会重置所有会话缓冲，等所有会话跑完再切
      settingsMsg = '有会话正在运行，稍后再切换项目';
      return;
    }
    await applyWorkspace(path);
  }

  async function newSessionInProject(path, active) {
    if (!active) {
      await switchProject(path);
      // 没真正切过去（流式输出中/目录不存在）就别新建，否则会话会落到别的项目里
      if (normPath(workspace).toLowerCase() !== normPath(path).toLowerCase()) return;
    }
    await startNewSession();
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
    if (checkedIds.includes(currentSessionId)) await startNewSession();
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
      const r = await fetch('/api/workspace/picker', { method: 'POST' });
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
      });
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
      pushMsg(curKey(), {
        id: nextId(),
        role: 'system',
        content: d.ok ? `已撤销：${cp.files} 个文件恢复到任务开始前` : `撤销失败：${d.error}`,
      });
      refreshDock(); // 撤销后改动数变化，刷新钉条
    } catch (e) {
      pushMsg(curKey(), { id: nextId(), role: 'error', content: `[撤销请求失败] ${e}` });
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
          // 027：思考内容已随消息落盘（reasoning_content），切回会话时恢复
          thinking: m.reasoning_content || '',
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
      pushMsg(curKey(), {
        id: nextId(),
        role: 'system',
        content: d.ok ? `交接文档已生成：${d.path}` : `HANDOFF 生成失败：${d.error}`,
      });
    } catch (e) {
      pushMsg(curKey(), { id: nextId(), role: 'error', content: `[HANDOFF 请求失败] ${e}` });
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
    updMsgs(curKey(), (a) =>
      a.map((m) => (m.approval && m.approval.id === id ? { ...m, approval: { ...m.approval, decided: approved } } : m))
    );
  }

  // —— 主题系统（深/浅/跟随系统）——
  const sysDark = window.matchMedia('(prefers-color-scheme: dark)');
  let themeMode = ['dark', 'light', 'system'].includes(localStorage.getItem('theme'))
    ? localStorage.getItem('theme')
    : 'dark';
  let theme = resolveTheme(themeMode);
  function resolveTheme(mode) {
    return mode === 'system' ? (sysDark.matches ? 'dark' : 'light') : mode;
  }
  function applyThemeMode(m) {
    themeMode = m;
    localStorage.setItem('theme', m);
    theme = resolveTheme(m);
    document.documentElement.dataset.theme = theme;
  }
  function toggleTheme() {
    applyThemeMode(theme === 'dark' ? 'light' : 'dark');
  }
  sysDark.addEventListener('change', () => {
    if (themeMode === 'system') {
      theme = resolveTheme('system');
      document.documentElement.dataset.theme = theme;
    }
  });
  applyThemeMode(themeMode); // 首屏即应用（组件初始化阶段，早于首帧渲染）

  // —— 自定义背景壁纸（dataURL 存 localStorage；选图时降采样防超配额）——
  let wallpaper = localStorage.getItem('wallpaper') || '';
  let wallpaperOpacity = Number(localStorage.getItem('wallpaperOpacity') ?? 30);
  let wallpaperMsg = '';
  $: document.body.classList.toggle('has-wallpaper', !!wallpaper);
  // 微调模式：body 标记（hover 虚线高亮由 CSS 接管）；关闭时收起面板
  $: {
    document.body.classList.toggle('micro-edit-mode', editMode);
    if (!editMode) microPanel = null;
  }
  $: localStorage.setItem('wallpaperOpacity', String(wallpaperOpacity));
  function onWallpaperPick(e) {
    const file = e.currentTarget.files && e.currentTarget.files[0];
    e.currentTarget.value = '';
    if (!file) return;
    const reader = new FileReader();
    reader.onload = () => {
      const img = new Image();
      img.onload = () => {
        const max = 1920;
        const scale = Math.min(1, max / Math.max(img.naturalWidth, img.naturalHeight));
        const w = Math.max(1, Math.round(img.naturalWidth * scale));
        const h = Math.max(1, Math.round(img.naturalHeight * scale));
        const cv = document.createElement('canvas');
        cv.width = w;
        cv.height = h;
        const ctx = cv.getContext('2d');
        ctx.fillStyle = '#ffffff';
        ctx.fillRect(0, 0, w, h);
        ctx.drawImage(img, 0, 0, w, h);
        try {
          wallpaper = cv.toDataURL('image/jpeg', 0.85);
          localStorage.setItem('wallpaper', wallpaper);
          wallpaperMsg = '';
        } catch (err) {
          wallpaperMsg = '壁纸保存失败（图片过大），换张小图试试';
        }
      };
      img.src = String(reader.result);
    };
    reader.readAsDataURL(file);
  }
  function clearWallpaper() {
    wallpaper = '';
    localStorage.removeItem('wallpaper');
    wallpaperMsg = '';
  }

  // —— P10.5 设置分区（左侧导航）——
  // 'params' 页签已并入「模型配置」；旧 localStorage 值回退到 model
  let settingsTab = ['appearance', 'model', 'roles', 'memory', 'skills', 'mcp', 'ext'].includes(localStorage.getItem('settingsTab'))
    ? localStorage.getItem('settingsTab')
    : 'model';
  function pickSettingsTab(t) {
    settingsTab = t;
    localStorage.setItem('settingsTab', t);
    if (t === 'mcp' || t === 'plugins') loadMcp();
    if (t === 'skills') loadSkills();
    if (t === 'ext') { loadGitRuntime(); loadExt(); loadSandboxInfo(); }
  }

  // —— P10.5 记忆分区（三类约定文件编辑 + 状态）——
  // 系统约定 prompts/system.md、全局约定 ~/.agent/AGENTS.md、项目约定 工作区根 AGENTS.md。
  // 文本框始终显示文件当前内容：进入分区/切 chips/保存后都回读一次盘。
  let memData = null;
  let memTab = 'system';
  let memText = '';
  let memSaving = false;
  let memMsg = '';
  // 有未保存的编辑：后台回读不得覆盖输入框（否则刚写的约定会被静默清空/回退成旧内容）
  let memDirty = false;
  const MEM_SCOPES = {
    system: { text: 'system_prompt', path: 'system_path', label: '系统约定' },
    global: { text: 'agents_global', path: 'global_agents_path', label: '全局约定' },
    project: { text: 'agents_project', path: 'project_agents_path', label: '项目约定' },
  };
  function memTextOf(r, tab) {
    const k = (MEM_SCOPES[tab] || MEM_SCOPES.project).text;
    return (r && r[k]) || '';
  }
  // 当前约定文件路径。必须是响应式变量：模板里直接调用函数不会随 memTab 重算（Svelte 看不到函数体依赖）
  $: memPath = (memData && MEM_SCOPES[memTab] ? memData[MEM_SCOPES[memTab].path] : '') || '';
  async function loadMemory() {
    try {
      // 带时间戳绕开浏览器缓存：显示的一定是盘上当下的内容
      const r = await fetch(`/api/memory?t=${Date.now()}`).then((r) => r.json());
      memData = r;
      if (!memDirty) memText = memTextOf(r, memTab);
    } catch (e) {
      if (!memData) memMsg = `读取失败: ${e.message || e}`;
    }
  }
  function pickMemTab(t) {
    if (!memData || memTab === t) return;
    memTab = t;
    memText = memTextOf(memData, t); // 先用上一份快照，随即回读盘上真容
    memDirty = false; // 换 tab = 看那个文件的内容，放弃当前未保存编辑
    memMsg = '';
    loadMemory();
  }
  async function saveAgents() {
    memSaving = true;
    memMsg = '';
    try {
      const r = await fetch('/api/memory/agents', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ scope: memTab, content: memText }),
      });
      if (r.ok) {
        memDirty = false; // 先清脏标记再回读，框里保留的就是文件真实内容
        await loadMemory();
        memMsg = `已保存到 ${r.path}`;
      } else {
        memMsg = `保存失败: ${r.error}`;
      }
    } catch (e) {
      memMsg = `保存失败: ${e.message || e}`;
    } finally {
      memSaving = false;
    }
  }
  // 打开设置面板 / 切到记忆分区时都回读文件（含直接打开面板、持久化分区的情形）
  $: if (settingsOpen && settingsTab === 'memory') loadMemory();

  // —— 技能（Skill / SKILL.md）分区 ——
  let skillsData = [];
  let skillsLoading = false;
  let skillMsg = '';
  let skillEditor = null; // { isNew, name, description, content, source }
  async function loadSkills() {
    skillsLoading = true;
    try {
      const r = await fetch('/api/skills').then((r) => r.json());
      skillsData = r.skills || [];
    } catch (e) {
      skillsData = [];
    }
    skillsLoading = false;
  }
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
      });
      if (r.error) {
        skillMsg = `保存失败: ${r.error}`;
        return;
      }
      skillMsg = `已保存到 ${r.dir}`;
      skillEditor = null;
      await loadSkills();
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
      });
      skillMsg = r.error ? `删除失败: ${r.error}` : `已删除 ${name}`;
      await loadSkills();
    } catch (e) {
      skillMsg = `删除失败: ${e.message || e}`;
    }
  }

  // —— P11（施工单 016）：MCP 插件分区 ——
  let mcpServers = [];
  // 021：transport 二选一。stdio=本地子进程；http=远端 MCP（Streamable HTTP）
  let mcpForm = { name: '', transport: 'stdio', command: '', args: '', url: '', headers: '' };
  let mcpRiskChoice = 'normal';
  let wasmPlugins = [];
  // git 运行时状态（插件分区顶部卡片）：系统装的 / 内置包解压的 / 不可用
  let gitRuntime = null;
  // 021：沙箱自检（真拉起受限进程跑探针套件）
  let sandboxProbe = null;
  let sbRunning = false;
  // 021：应用版本（侧栏「关于」行）
  let appVersion = null;
  async function loadVersion() {
    try {
      appVersion = await fetch('/api/version', { cache: 'no-store' });
    } catch (e) {
      appVersion = null;
    }
  }
  async function runSandboxProbe() {
    sbRunning = true;
    try {
      sandboxProbe = await fetch('/api/sandbox/selftest', { cache: 'no-store' });
    } catch (e) {
      sandboxProbe = { error: String(e.message || e) };
    }
    sbRunning = false;
  }
  async function loadGitRuntime() {
    try {
      gitRuntime = await fetch('/api/git/info', { cache: 'no-store' });
    } catch (e) {
      gitRuntime = null;
    }
  }
  // P20 沙箱档位（运行状态卡首卡）：配置档 / 生效档（含 AC→受限回退与原因）/ 探测链
  let sandboxInfo = null;
  let sbInfoLoading = false;
  async function loadSandboxInfo(refresh) {
    sbInfoLoading = true;
    try {
      sandboxInfo = await fetch(`/api/sandbox/info${refresh ? '?refresh=1' : ''}`, { cache: 'no-store' });
    } catch (e) {
      sandboxInfo = null;
    }
    sbInfoLoading = false;
  }
  async function loadMcp() {
    try {
      const r = await fetch('/api/mcp').then((r) => r.json());
      mcpServers = r.servers || [];
      wasmPlugins = r.wasm || [];
    } catch (e) {
      mcpServers = [];
      wasmPlugins = [];
    }
  }
  async function mcpAction(action, name, risk, server) {
    try {
      const r = await fetch('/api/mcp', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ action, name, risk, server }),
      });
      if (r.error) settingsMsg = `MCP ${action} 失败: ${r.error}`;
      if (action === 'refresh' && r.ok) settingsMsg = `已连接，发现 ${r.tools_found} 个工具`;
    } catch (e) {
      settingsMsg = `MCP ${action} 失败: ${e.message || e}`;
    }
    await loadMcp();
  }
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
    const isHttp = mcpForm.transport === 'http';
    const args = mcpForm.args.trim() ? mcpForm.args.trim().split(/\s+/) : [];
    const srv = isHttp
      ? {
          name: mcpForm.name.trim(),
          transport: 'http',
          url: mcpForm.url.trim(),
          headers: parseHeaders(mcpForm.headers),
        }
      : {
          name: mcpForm.name.trim(),
          transport: 'stdio',
          command: mcpForm.command.trim(),
          args,
        };
    mcpAction('save', null, null, srv);
    mcpForm = { name: '', transport: mcpForm.transport, command: '', args: '', url: '', headers: '' };
  }
  // 档位名兜底（与 core::rules::RiskLevel::label 对齐；正常情况 SSE 已带 risk_label）
  function riskLabelFor(risk) {
    return risk >= 2 ? '高危操作' : risk === 1 ? '常规改动' : '安全';
  }

  // —— 021 组件总线 v0：UI 扩展（iframe 插槽 + 白名单数据桥）——
  // 隔离：iframe sandbox="allow-scripts"（无 allow-same-origin）→ 不透明源，拿不到宿主
  // DOM / localStorage / cookie；唯一通道是 postMessage，父侧只放行下面这几个只读方法。
  let extList = [];
  let extInfo = { app_dir: '', workspace_dir: '' };
  let extOpen = false;
  let extCurrent = null; // 当前打开的扩展（{ name, title, ... }）
  let extFrame = null; // 面板里那个 iframe 元素（当前打开的扩展）
  let extMsg = '';
  let extSeq = 0;
  const extPending = new Map(); // 请求 id → 发起方（这次是父侧主动调用扩展，留口）

  async function loadExt() {
    try {
      const r = await fetch('/api/ext', { cache: 'no-store' });
      extList = r.extensions || [];
      extInfo = { app_dir: r.app_dir || '', workspace_dir: r.workspace_dir || '' };
    } catch (e) {
      extList = [];
      extMsg = `读取插件失败: ${e.message || e}`;
    }
  }

  // 已启用的扩展（停用的不加载、不给按钮）
  $: enabledExts = extList.filter((e) => e.enabled !== false && e.ok);
  // 声明了 toolbar 插槽的扩展 → 顶栏按钮
  $: slotExts = enabledExts.flatMap((e) =>
    (e.ui || []).filter((u) => u.slot === 'toolbar').map((u) => ({ ext: e, slot: u }))
  );
  // 面板打开时待投递的插槽事件（iframe 还没 load 完就先排队）
  let extPendingMsgs = [];

  function openExt(e, slotEvent) {
    // 面板此刻是不是已经开着同一个扩展（决定插槽事件直接投还是排队等面板 iframe load）
    const wasOpenSame = extOpen && extCurrent && extCurrent.name === e.name;
    extCurrent = e;
    extMsg = '';
    extOpen = true;
    // 与右侧产物面板互斥，避免三个面板挤在一起
    rightPanelOpen = false;
    terminalOpen = false;
    if (slotEvent) {
      const msg = { type: 'agent:slot', slot: slotEvent.slot, id: slotEvent.id || '', title: slotEvent.title || '' };
      // 注意：打开的瞬间后台那个常驻 iframe 会被卸载，别往里投——排队等**面板** iframe load 时投
      if (wasOpenSame && extFrameEls[e.name] && extFrameEls[e.name].contentWindow) postToName(e.name, msg);
      else extPendingMsgs.push({ name: e.name, msg });
    }
  }

  /// 面板 iframe 加载完成：登记元素 + 发 init + 补投排队中的插槽事件
  function onExtFrameLoad(name) {
    // 面板那个 iframe 绑的是 extFrame，这里补登记：否则 postToName 找不到它，
    // init / 插槽事件都投不进去（后台 iframe 此时已经卸载）
    if (extFrame && extCurrent && extCurrent.name === name) extFrameEls[name] = extFrame;
    const ext = extList.find((e) => e.name === name);
    postToName(name, { type: 'agent:init', payload: extInitPayload(ext) });
    extPendingMsgs = extPendingMsgs.filter((p) => {
      if (p.name !== name) return true;
      postToName(name, p.msg);
      return false;
    });
  }

  /// 扩展插槽按钮点击：打开该扩展面板并把事件交给它（面板里自己决定做什么）
  function clickSlot(entry) {
    openExt(entry.ext, entry.slot);
  }

  /// 扩展启用/停用（P17）：写设置 + 刷新清单
  async function toggleExt(e) {
    try {
      const r = await fetch('/api/ext/toggle', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: e.name, enabled: e.enabled === false }),
      });
      if (r.extensions) extList = r.extensions;
      if (e.enabled !== false && extCurrent && extCurrent.name === e.name) extOpen = false; // 停用后关面板
    } catch (err) {
      extMsg = `切换失败: ${err.message || err}`;
    }
  }

  // P18 契约 v1（027）：draft 转正（落盘持久化、来源改 user）/ 丢弃（内存即清）
  async function promoteExt(e) {
    try {
      const r = await fetch('/api/ext/promote', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: e.name }),
      });
      if (r.error) extMsg = `转正失败: ${r.error}`;
      else extMsg = `已转正「${e.title || e.name}」（来源已改为 user，重启后仍在）`;
      if (r.extensions) extList = r.extensions;
    } catch (err) {
      extMsg = `转正失败: ${err.message || err}`;
    }
  }

  async function discardExt(e) {
    if (!confirm(`丢弃草稿插件「${e.title || e.name}」？内存内容将立即消失。`)) return;
    try {
      const r = await fetch(`/api/ext/draft?name=${encodeURIComponent(e.name)}`, { method: 'DELETE' });
      if (r.error) extMsg = `丢弃失败: ${r.error}`;
      else extMsg = `已丢弃「${e.title || e.name}」`;
      if (r.extensions) extList = r.extensions;
    } catch (err) {
      extMsg = `丢弃失败: ${err.message || err}`;
    }
  }

  function extInitPayload(ext) {
    return {
      version: appVersion?.version || '',
      root: appVersion?.root || '',
      theme: themeMode,
      sessionId: currentSessionId,
      title: displayTitle,
      // P17：告诉扩展「你是谁」（权限 / 插槽声明它自己在 manifest 里写）
      ext: ext ? { name: ext.name, title: ext.title, permissions: ext.permissions || [] } : null,
    };
  }

  /// 扩展 iframe 元素表：面板里那个 + 后台常驻的那些（扩展间通信要它们都活着）。
  let extFrameEls = {};
  function extWindowName(win) {
    for (const [name, el] of Object.entries(extFrameEls)) {
      if (el && el.contentWindow === win) return name;
    }
    return null;
  }
  function postToName(name, msg) {
    const el = extFrameEls[name];
    if (el && el.contentWindow) el.contentWindow.postMessage(msg, '*');
  }

  /// 扩展 → 宿主：只读白名单 + P17 写操作。写操作按 manifest 声明的 permissions 放行，
  /// 没声明一律拒；写工作区文件额外弹一次确认（宿主替扩展问）。
  async function onExtMessage(ev) {
    const from = extWindowName(ev.source);
    if (!from) return; // 只认我们挂出去的扩展 iframe
    const ext = extList.find((e) => e.name === from) || {};
    if (ext.enabled === false) return; // 停用的扩展不服务
    const perms = ext.permissions || [];
    const d = ev.data || {};
    if (d.type === 'agent:hello') {
      postToName(from, { type: 'agent:init', payload: extInitPayload(ext) });
      return;
    }
    if (d.type !== 'agent:request') return;
    const id = d.id;
    const params = d.params || {};
    const reply = (ok, result) => postToName(from, { type: 'agent:response', id, ok, result });
    const get = (url) => fetch(url, { cache: 'no-store' });
    try {
      switch (d.method) {
        case 'app.info':
          reply(true, extInitPayload());
          break;
        case 'workspace.info':
          reply(true, await get('/api/workspace'));
          break;
        case 'workspace.files':
          reply(true, await get('/api/workspace/files'));
          break;
        case 'git.status':
          reply(true, await get('/api/git/status'));
          break;
        case 'memory.search':
          reply(
            true,
            await fetch('/api/memory/search', {
              method: 'POST',
              headers: { 'Content-Type': 'application/json' },
              // 兼容扩展作者两种写法：top_k（后端字段）或 limit
              body: JSON.stringify({ query: params.query || '', top_k: params.top_k || params.limit || 10 }),
            }).then((r) => r.json()),
          );
          break;
        case 'session.info':
          reply(true, { id: currentSessionId, title: displayTitle });
          break;
        // —— P17 写操作（都要 manifest 里先声明权限）——
        case 'composer.insert': {
          if (!perms.includes('composer.insert')) return reply(false, '未声明权限 composer.insert');
          const text = String(params.text || '');
          input = input ? `${input}\n${text}` : text;
          reply(true, { ok: true });
          break;
        }
        case 'notify': {
          if (!perms.includes('notify')) return reply(false, '未声明权限 notify');
          pushMsg(curKey(), {
            id: nextId(),
            role: 'system',
            content: `[${ext.title || from}] ${String(params.text || '')}`,
          });
          reply(true, { ok: true });
          break;
        }
        case 'bus.publish': {
          // 扩展间通信：宿主转发给其它**已启用**扩展（发布者自己不发回）
          if (!perms.includes('bus.publish')) return reply(false, '未声明权限 bus.publish');
          const topic = String(params.topic || '').trim();
          if (!topic) return reply(false, '缺少 topic');
          let delivered = 0;
          for (const e of enabledExts) {
            if (e.name === from) continue;
            const el = extFrameEls[e.name];
            if (el && el.contentWindow) {
              el.contentWindow.postMessage(
                { type: 'bus.message', from, topic, payload: params.payload ?? null },
                '*'
              );
              delivered += 1;
            }
          }
          reply(true, { delivered });
          break;
        }
        case 'workspace.write': {
          if (!perms.includes('workspace.write')) return reply(false, '未声明权限 workspace.write');
          const path = String(params.path || '').trim();
          const content = String(params.content ?? '');
          if (!path) return reply(false, '缺少 path');
          // 写工作区是敏感动作：宿主替扩展问一次用户
          if (!confirm(`插件「${ext.title || from}」要写工作区文件：\n\n${path}（${content.length} 字节）\n\n允许吗？`)) {
            return reply(false, '用户拒绝');
          }
          const r = await fetch('/api/workspace/file', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ path, content }),
          });
          reply(!!r.ok, r.ok ? { ok: true, path: r.path, bytes: r.bytes } : r.error || '写入失败');
          break;
        }
        // —— P18 存储一门（027）：插件命名空间内的结构化读写（须声明 storage 权限）——
        // 后端按插件名隔离目录（plugin_data/<name>/），内核保留区（会话/设置/记忆）不在任何
        // 插件写路径上；键名白名单在前后端双重校验。
        case 'storage.read': {
          if (!perms.includes('storage')) return reply(false, '未声明权限 storage');
          const key = String(params.key || '');
          const r = await fetch(`/api/plugin/data?name=${encodeURIComponent(from)}&key=${encodeURIComponent(key)}`, { cache: 'no-store' });
          if (r.status === 404) return reply(true, null); // 键不存在 = 空值，不算错
          const j = await r.json();
          reply(r.ok && j.ok, j.ok ? j.value : j.error || '读取失败');
          break;
        }
        case 'storage.write': {
          if (!perms.includes('storage')) return reply(false, '未声明权限 storage');
          const key = String(params.key || '');
          if (!/^[A-Za-z0-9_-]{1,64}$/.test(key)) return reply(false, '键名只能用字母数字与 - _（≤64 字符）');
          const r = await fetch('/api/plugin/data', {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ name: from, key, value: params.value ?? null }),
          });
          reply(!!r.ok, r.ok ? { ok: true, bytes: r.bytes } : r.error || '写入失败');
          break;
        }
        case 'storage.delete': {
          if (!perms.includes('storage')) return reply(false, '未声明权限 storage');
          const key = String(params.key || '');
          if (!/^[A-Za-z0-9_-]{1,64}$/.test(key)) return reply(false, '键名只能用字母数字与 - _（≤64 字符）');
          const r = await fetch(`/api/plugin/data?name=${encodeURIComponent(from)}&key=${encodeURIComponent(key)}`, { method: 'DELETE' });
          reply(!!r.ok, r.ok ? { ok: true, found: r.found } : r.error || '删除失败');
          break;
        }
        // —— P19 只读桥：trace 时间线面板（ui-ext/trace 试点）拉最近任务事件流 ——
        // 只读信息类方法与 workspace.info 同级，不占权限白名单。
        case 'trace.recent': {
          const limit = Math.min(Math.max(Number(params.limit) || 10, 1), 50);
          reply(true, await get(`/api/trace?limit=${limit}`));
          break;
        }
        default:
          reply(false, `未知方法: ${d.method}`);
      }
    } catch (e) {
      reply(false, String(e.message || e));
    }
  }

  // —— 记忆库星云：每个光点 = 一次会话；搜索走向量检索 ——
  let galaxyOpen = false;
  function openSessionFromGalaxy(e) {
    galaxyOpen = false;
    loadSession(e.detail);
  }

  // P10-A trace 时间线已插件化（P19）：ui-ext/trace 面板 + 宿主只读桥 trace.recent，
  // 原内置弹窗（状态/函数/标记/样式）整体撤下——顶栏的插件按钮即入口。

  function nextId() {
    return ++seq;
  }
</script>

<svelte:window on:contextmenu={onMicroContext} />

{#if wallpaper}
  <div class="wallpaper" style="background-image:url('{wallpaper}'); opacity:{wallpaperOpacity / 100}"></div>
{/if}

<div class="app">
  <aside class="sidebar" class:collapsed={sidebarCollapsed} class:resizing={resizing} style="width:{sidebarW}px">
    <div class="side-head">
      <div class="brand-row">
        <span class="brand-mark" aria-hidden="true"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3.5 20 7.5v5c0 4.5-3.2 7.6-8 9-4.8-1.4-8-4.5-8-9v-5Z"/><path d="m9 11.5 2.2 2.2L15.5 9.5"/></svg></span>
        <span class="brand-name">Agent</span>
        <button class="icon-btn" title={theme === 'dark' ? '切到浅色主题' : '切到深色主题'} on:click={toggleTheme}>
          {#if theme === 'dark'}
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><circle cx="12" cy="12" r="4.2"/><path d="M12 3v2.2M12 18.8V21M3 12h2.2M18.8 12H21M5.6 5.6l1.6 1.6M16.8 16.8l1.6 1.6M18.4 5.6l-1.6 1.6M7.2 16.8l-1.6 1.6"/></svg>
          {:else}
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M20 14.5A8.5 8.5 0 0 1 9.5 4a7 7 0 1 0 10.5 10.5Z"/></svg>
          {/if}
        </button>
        <button class="icon-btn" title="收起侧栏" on:click={() => (sidebarCollapsed = true)}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="3" y="4.5" width="18" height="15" rx="3.2"/><line x1="9.8" y1="4.5" x2="9.8" y2="19.5"/></svg>
        </button>
      </div>

      <button class="new-chat" on:click={startNewSession}>
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
          <button class="icon-btn" title="添加项目" on:click={addProject}>
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
            class:active={row.active}
            role="button"
            tabindex="0"
            title={row.path === '__ungrouped__' ? '无工作区记录的历史会话' : `${row.path}（点行切换项目，点左侧图标展开/收起列表）`}
            on:click={() => ((row.path === '__ungrouped__' || row.active) ? toggleProjectRow(row.path) : switchProject(row.path))}
            on:keydown={(e) => e.key === 'Enter' && ((row.path === '__ungrouped__' || row.active) ? toggleProjectRow(row.path) : switchProject(row.path))}
          >
            <!-- 展开/收起：独立按钮，任何项目都能点开看会话（点行本身仍是切换项目） -->
            <button
              class="proj-toggle"
              title={!!searchQ || !!expandedFlags[row.path] ? '收起会话列表' : '展开会话列表'}
              on:click|stopPropagation={() => toggleProjectRow(row.path)}
            >
              {#if !!searchQ || !!expandedFlags[row.path]}
                <svg viewBox="0 0 24 24"><path d="M6.5 9.2 12 14.7l5.5-5.5z" fill="currentColor"/></svg>
              {:else}
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M20 19.5a1.8 1.8 0 0 0 1.8-1.8V8.3a1.8 1.8 0 0 0-1.8-1.8h-7.4a1.8 1.8 0 0 1-1.5-.8l-.8-1.2a1.8 1.8 0 0 0-1.5-.8H4a1.8 1.8 0 0 0-1.8 1.8v12.2A1.8 1.8 0 0 0 4 19.5Z"/></svg>
              {/if}
            </button>
            <span class="proj-name">{row.name}{row.count ? '' : ''}</span>
            {#if row.active}<span class="proj-cur" title="当前项目"></span>{/if}
            {#if row.path !== '__ungrouped__'}
              <span class="proj-side">
                <button class="row-btn" class:on={projMenuPath === row.path} title="项目操作" on:click|stopPropagation={() => (projMenuPath = projMenuPath === row.path ? null : row.path)}>
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
              <span class="conv-check" class:on={checkedIds.includes(s.id)}><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round"><path d="M20 6 9 17l-5-5"/></svg></span>
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
      <button class="foot-btn galaxy-btn" on:click={() => (galaxyOpen = true)} title="记忆库：以星云看每一次会话，支持语义搜索">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="1.6" fill="currentColor" stroke="none"/><ellipse cx="12" cy="12" rx="9.2" ry="3.6" transform="rotate(-24 12 12)"/><circle cx="19.4" cy="8.6" r="1.1" fill="currentColor" stroke="none"/><circle cx="5.2" cy="15.2" r="0.9" fill="currentColor" stroke="none"/></svg>
        记忆库
      </button>
      <button class="foot-btn" on:click={() => (settingsOpen = true)}>设置</button>
      {#if appVersion}
        <div class="side-version" title={`运行根：${appVersion.root}`}>v{appVersion.version}</div>
      {/if}
    </div>
    <div class="resize-handle" on:mousedown={startResize}></div>
  </aside>

  <main class="chat" class:side-collapsed={sidebarCollapsed}>
    {#if sidebarCollapsed}
      <button class="sidebar-restore" title="展开侧栏" on:click={() => (sidebarCollapsed = false)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="3" y="4.5" width="18" height="15" rx="3.2"/><line x1="9.8" y1="4.5" x2="9.8" y2="19.5"/></svg>
      </button>
    {/if}
    <header class="chat-head">
      <div class="head-main">
        <div class="chat-title">{displayTitle}</div>
      </div>
      <div class="head-side">
        <!-- P17：扩展声明的顶栏插槽（点击打开该扩展并把事件交给它） -->
        {#each slotExts as s (s.ext.name + ':' + (s.slot.id || s.slot.slot))}
          <button
            class="head-btn"
            class:active={extOpen && extCurrent && extCurrent.name === s.ext.name}
            title={`插件：${s.slot.title || s.ext.title}`}
            on:click={() => clickSlot(s)}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="7" height="7" rx="2"/><rect x="14" y="3" width="7" height="7" rx="2"/><rect x="3" y="14" width="7" height="7" rx="2"/><path d="M14 17.5h7M17.5 14v7"/></svg>
          </button>
        {/each}
        <button
          class="head-btn"
          class:active={terminalOpen}
          title="终端（沙箱内执行命令）"
          on:click={() => {
            terminalOpen = !terminalOpen;
            if (terminalOpen) rightPanelOpen = false;
          }}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4.5" width="18" height="15" rx="3.2"/><path d="m7.5 9.5 3.2 2.8-3.2 2.8"/><path d="M12.8 15.4h4"/></svg>
        </button>
        <button
          class="head-btn"
          class:active={rightPanelOpen}
          title="产物与预览"
          on:click={() => {
            rightPanelOpen = !rightPanelOpen;
            if (rightPanelOpen) {
              terminalOpen = false;
              openRightPanel();
            }
          }}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"><rect x="3" y="4.5" width="18" height="15" rx="3.2"/><path d="M14.5 4.5v15"/><path d="M17.5 9.5h1M17.5 12.5h1" stroke-linecap="round"/></svg>
        </button>

      </div>
    </header>

    <div class="chat-main">
      <div class="chat-body" bind:this={chatBody} on:scroll={onChatScroll}>
      {#if messages.length === 0}
        <div class="empty-hint"><div class="empty-title">开始新的对话</div><div class="empty-sub">输入消息后按 Enter 发送，支持粘贴截图与拖拽文件</div></div>
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
                      <div class="msg-att-file"><svg class="att-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m8 12.5 6.5-6.5a2.8 2.8 0 0 1 4 4L9 19.5a4.5 4.5 0 0 1-6.4-6.4l8.5-8.5"/></svg> {a.name}</div>
                    {/if}
                  {/each}
                </div>
              {/if}
              {#if m.content}
                <div class="user-text">{m.content}</div>
              {/if}
              {#if m.queued}
                <div class="queued-row">
                  <span class="queued-badge">排队中 · 这一轮结束后自动发送</span>
                  <button class="queued-send" on:click={() => sendNow(m.id)} title="并进正在跑的这一轮（引导当前回答，不打断）">立即发送</button>
                </div>
              {:else if m.steered}
                <div class="queued-row"><span class="queued-badge steered">已引导当前回答</span></div>
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
                      ><span class="approval-kicker">审批请求</span><b>{m.approval.tool}</b>
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
                    <div class="approval-state">{m.approval.decided ? '已允许' : '已拒绝'}</div>
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
                <div class="typing"><span class="typing-dot"></span><span class="typing-dot"></span><span class="typing-dot"></span></div>
              {/if}
              {#if m.streaming && m.content}<span class="cursor"></span>{/if}
            </div>
          </div>
        {:else if m.role === 'error'}
          <div class="msg system"><div class="bubble error-bubble">{m.content}</div></div>
        {:else if m.role === 'cp'}
          <!-- 检查点消息只给改动条提供基线（dockCp），不渲染 -->
        {:else}
          <div class="msg system"><div class="sys-text">{m.content ?? ''}</div></div>
        {/if}
      {/each}
    </div>

    {#if !chatPinned}
      <button class="jump-bottom" on:click={scrollToBottom} title="回到底部">回到底部</button>
    {/if}

    <div class="handoff-bar">
      <button class="handoff-btn" on:click={makeHandoff} title="把当前会话压成交接文档 HANDOFF.md（写入工作区）">交接文档</button>
    </div>

    <footer class="chat-input">
      {#if dockCp || (gitInfo && gitInfo.is_repo)}
        <div class="cp-dock">
          <div class="cp-bar">
            {#if gitInfo && gitInfo.is_repo}
              <span class="git-ref" title={gitInfo.upstream ? `上游：${gitInfo.upstream}` : '未设置上游分支'}>
                <svg class="git-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="6.5" cy="6" r="2.5"/><circle cx="6.5" cy="18" r="2.5"/><circle cx="17.5" cy="12" r="2.5"/><path d="M6.5 8.5v7M9 6h3.5a3 3 0 0 1 3 3v1"/></svg>
                <b>{gitInfo.branch}</b>{#if gitInfo.head}<span class="git-head">@{gitInfo.head}</span>{/if}
              </span>
              {#if gitInfo.ahead || gitInfo.behind}
                <span class="git-ab" title="相对上游的领先/落后提交数">{#if gitInfo.ahead}↑{gitInfo.ahead}{/if}{#if gitInfo.behind}↓{gitInfo.behind}{/if}</span>
              {/if}
              {#if gitInfo.dirty}
                <button
                  class="git-dirty"
                  class:on={gitPopOpen}
                  title="点开看待提交文件清单（{gitInfo.dirty} 处）"
                  on:click={() => (gitPopOpen = !gitPopOpen)}
                >{gitInfo.dirty} 处未提交</button>
              {/if}
            {/if}
            {#if dockCp && dockCp.files > 0}
              <span class="cp-sum">› <b>{dockCp.files}</b> 个文件已更改</span>
              {#if dockCp.additions != null}
                <span class="cp-add">+{dockCp.additions}</span>
                <span class="cp-del">-{dockCp.deletions}</span>
              {/if}
            {/if}
            <span class="cp-actions">
              {#if dockCp && dockCp.files > 0}
                <button class="cp-undo" on:click={() => undoCheckpoint(dockCp)}>撤销</button>
              {/if}
              {#if gitInfo && gitInfo.is_repo && gitInfo.dirty > 0}
                <button class="cp-commit" on:click={openCommit}>提交</button>
              {/if}
            </span>
          </div>
          {#if gitPopOpen && gitInfo && gitInfo.is_repo}
            <div class="git-pop">
              <div class="git-pop-head">
                <b>{gitInfo.dirty}</b> 处未提交改动
                <span class="git-pop-hint">点文件名看内容</span>
              </div>
              <div class="git-pop-list">
                {#each gitInfo.files as f (f.path + f.status)}
                  <button class="git-pop-row" title={f.path} on:click={() => openGitFile(f.path)}>
                    <span class="git-st">{gitStatusLabel(f.status)}</span>
                    <span class="git-path">{f.path}</span>
                  </button>
                {/each}
                {#if gitInfo.dirty > gitInfo.files.length}
                  <div class="git-pop-more">还有 {gitInfo.dirty - gitInfo.files.length} 处未列出</div>
                {/if}
              </div>
            </div>
          {/if}
        </div>
      {/if}
      <div class="composer">
        {#if pendingAttachments.length > 0}
          <div class="composer-attachments">
            {#each pendingAttachments as att, idx}
              <div class="att-chip" title={`${att.name} (${fmtBytes(att.size)})`}>
                {#if att.mime.startsWith('image/')}
                  <img class="att-thumb" src={att.data} alt={att.name} />
                {:else if att.mime.startsWith('video/')}
                  <svg class="att-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><rect x="3" y="5.5" width="18" height="13" rx="2.5"/><path d="M3 9.5h18M7.5 5.5v4M16.5 5.5v4"/></svg>
                {:else if att.mime.startsWith('audio/')}
                  <svg class="att-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><circle cx="7.5" cy="17.5" r="2.5"/><circle cx="17.5" cy="15.5" r="2.5"/><path d="M10 17.5V6l10-2.5V15.5"/></svg>
                {:else}
                  <svg class="att-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m8 12.5 6.5-6.5a2.8 2.8 0 0 1 4 4L9 19.5a4.5 4.5 0 0 1-6.4-6.4l8.5-8.5"/></svg>
                {/if}
                <span class="att-name">{att.name}</span>
                <span class="att-size">{fmtBytes(att.size)}</span>
                <button class="att-del" on:click={() => removeAttachment(idx)} title="移除" aria-label="移除"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18"/></svg></button>
              </div>
            {/each}
          </div>
        {/if}
        <textarea
          class="composer-input"
          rows="1"
          bind:this={composerInputEl}
          bind:value={input}
          placeholder={streaming
            ? '当前会话运行中：Enter 发送会先排队（本轮结束后自动发出）；想马上引导当前回答，点气泡上的「立即发送」'
            : '输入消息，Enter 发送，Shift+Enter 换行（支持粘贴截图、拖拽文件）'}
          on:keydown={handleKey}
          on:input={autoRows}
          on:paste={handlePaste}
          on:dragover|preventDefault
          on:drop={handleDrop}
        ></textarea>
        <div class="composer-bar">
          <div class="composer-left">
            <div class="bar-menu">
              <button
                class="icon-btn"
                title="添加上下文 / 能力 / 技能"
                on:click={() => {
                  plusMenuOpen = !plusMenuOpen;
                  abilityMenuOpen = false;
                  skillMenuOpen = false;
                }}
              ><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M12 5v14M5 12h14"/></svg></button>
              {#if plusMenuOpen}
                <div class="bar-dropdown">
                  <button class="bar-item" on:click={openCtxPicker}>
                    <span class="menu-sym">@</span> 使用 @ 添加上下文
                  </button>
                  <button class="bar-item" on:click={openAbilityMenu}>
                    <span class="menu-sym">/</span> 使用 / 选择能力
                  </button>
                  <button class="bar-item" on:click={openSkillMenu}>
                    <span class="menu-sym">$</span> 使用 $ 选择技能
                  </button>
                </div>
              {/if}
              {#if abilityMenuOpen}
                <div class="bar-dropdown">
                  {#each ABILITY_TEMPLATES as t}
                    <button class="bar-item" on:click={() => { abilityMenuOpen = false; insertAtCursor(t.text); }}>
                      <span class="menu-sym">/</span> {t.cmd}
                    </button>
                  {/each}
                </div>
              {/if}
              {#if skillMenuOpen}
                <div class="bar-dropdown">
                  {#if !skillsData.length && !mcpServers.length && !wasmPlugins.length}
                    <div class="menu-empty">暂无技能：可在「设置 → 技能」创建，或添加 MCP/WASM 插件</div>
                  {/if}
                  {#each skillsData as sk (sk.name)}
                    <button class="bar-item" on:click={() => { skillMenuOpen = false; insertAtCursor(`$${sk.name} `); }}>
                      <span class="menu-sym">$</span> {sk.name}
                    </button>
                  {/each}
                  {#each mcpServers as srv}
                    {#if srv.online !== false && srv.tools && srv.tools.length}
                      {#each srv.tools as t}
                        <button class="bar-item" on:click={() => { skillMenuOpen = false; insertAtCursor(`$mcp__${srv.name}__${t.name} `); }}>
                          <span class="menu-sym">$</span> {t.name}
                        </button>
                      {/each}
                    {/if}
                  {/each}
                  {#each wasmPlugins as w}
                    {#each w.tools || [] as t}
                      <button class="bar-item" on:click={() => { skillMenuOpen = false; insertAtCursor(`$wasm__${w.stem}__${t.name} `); }}>
                        <span class="menu-sym">$</span> wasm__{w.stem}__{t.name}
                      </button>
                    {/each}
                  {/each}
                </div>
              {/if}
            </div>
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
            >
              <svg class="att-ico-lg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m8 12.5 6.5-6.5a2.8 2.8 0 0 1 4 4L9 19.5a4.5 4.5 0 0 1-6.4-6.4l8.5-8.5"/></svg>
            </button>
            <div class="bar-menu">
              <button
                class="pill"
                class:danger={approvalEffective === 'full-access'}
                on:click={() => (approvalMenuOpen = !approvalMenuOpen)}
                title="审批档位（按会话记忆，运行中切换下一步即生效）：只读=写/执行逐次问 · 沙箱=只在沙箱执行不动工作区和电脑 · 自动=可动工作区与电脑高危先问 · 完全访问=全免审本机直跑"
              >
                <span class="pill-label">审批:</span> {APPROVAL_LABELS[approvalEffective] || approvalEffective}
                <svg class="caret-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>
              </button>
              {#if approvalMenuOpen}
                <div class="bar-dropdown">
                  {#each Object.entries(APPROVAL_LABELS) as [val, label]}
                    <button class="bar-item" class:active={val === approvalEffective} on:click={() => pickApproval(val)}>
                      {label}
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
              <button
                class="pill model-pill"
                on:click={() => {
                  modelMenuOpen = !modelMenuOpen;
                  if (modelMenuOpen) menuExpandedCfg = activeConfigId;
                }}
                title="切换模型（按配置分组）"
              >
                {model || '未选模型'}
                <svg class="caret-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>
              </button>
              {#if modelMenuOpen}
                <div class="bar-dropdown right">
                  {#each configs as cfg}
                    <button
                      class="bar-item group-head"
                      class:active={cfg.id === activeConfigId}
                      on:click={() => (menuExpandedCfg = menuExpandedCfg === cfg.id ? '' : cfg.id)}
                    >
                      <span>{configLabel(cfg)}{cfg.id === activeConfigId ? ' · 当前' : ''}</span>
                      <svg class="caret-ico" class:open={menuExpandedCfg === cfg.id} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 6 6 6-6 6"/></svg>
                    </button>
                    {#if menuExpandedCfg === cfg.id}
                      {#if !(cfg.models || []).length}
                        <div class="menu-empty">（该配置还没有模型，去设置里获取）</div>
                      {/if}
                      {#each cfg.models || [] as m}
                        <button
                          class="bar-item sub"
                          class:active={cfg.id === activeConfigId && m === model}
                          on:click={() => pickCrossModel(cfg, m)}
                        >
                          {m}
                        </button>
                      {/each}
                    {/if}
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
                <svg class="caret-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>
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
              <button class="send-btn stop" on:click={stop} title="停止" aria-label="停止"><svg viewBox="0 0 24 24" fill="currentColor"><rect x="7" y="7" width="10" height="10" rx="2"/></svg></button>
            {/if}
            <button class="send-btn" on:click={send} disabled={!input.trim()} title={streaming ? '发送（进入队列，这一轮结束后自动发出）' : '发送（Enter）'} aria-label="发送"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 19V5M5 12l7-7 7 7"/></svg></button>
          </div>
        </div>
      </div>
    </footer>
    </div>
    {#if terminalOpen}
      <div class="term-panel" class:resizing={termResizing} style="height:{termH}px">
        <div class="term-resize" title="拖拽调整高度" on:mousedown={startTermResize}></div>
        <div class="term-head">
          <span class="term-title">终端</span>
          <span class="term-hint">工作区内 · 沙箱执行</span>
          <span class="flex-sp"></span>
          <button class="term-clear" on:click={() => (termLines = [])}>清空</button>
          <button class="term-close" title="关闭终端" on:click={() => (terminalOpen = false)}>×</button>
        </div>
        <div class="term-out" bind:this={termOutEl}>
          {#if !termLines.length}
            <div class="term-line dim">输入命令回车执行（如 dir、git status）。命令在你的工作区内、受限沙箱中运行。</div>
          {/if}
          {#each termLines as l}
            <div class="term-block" class:err={!l.ok}>
              <div class="term-line"><span class="term-prompt">❯</span>{l.cmd}</div>
              {#if l.out}<pre class="term-text">{l.out}</pre>{/if}
            </div>
          {/each}
          {#if termRunning}
            <div class="term-line dim">执行中…</div>
          {/if}
        </div>
        <div class="term-in">
          <span class="term-prompt">❯</span>
          <input
            bind:value={termInput}
            placeholder="输入命令，Enter 运行"
            disabled={termRunning}
            spellcheck="false"
            on:keydown={(e) => {
              if (e.isComposing || e.keyCode === 229) return;
              if (e.key === 'Enter' && !e.shiftKey) {
                e.preventDefault();
                const cmd = termInput.trim();
                if (cmd && !termRunning) runTerm(cmd);
              }
            }}
          />
        </div>
      </div>
    {/if}
  </main>

  {#if rightPanelOpen}
    <aside class="right-panel" class:resizing={rpResizing} style="width:{rpWidth}px">
      <div class="rp-resize" title="拖拽调整宽度" on:mousedown={startRpResize}></div>
      <div class="rp-tabs">
        <button class:active={rpTab === 'artifacts'} on:click={() => { rpTab = 'artifacts'; loadArtifacts(); }}>产物</button>
        <button class:active={rpTab === 'preview'} on:click={() => { rpTab = 'preview'; if (!rpPreviewContent && rpFiles.length) previewFile(rpFiles[0]); }}>预览</button>
        <button class:active={rpTab === 'web'} on:click={() => { rpTab = 'web'; loadWebState(); }}>网页</button>
        <span class="flex-sp"></span>
        {#if rpTab === 'artifacts'}
          <button class="rp-refresh" title="刷新产物清单" on:click={loadArtifacts}>↻</button>
        {/if}
      </div>
      <div class="rp-body">
        {#if rpTab === 'artifacts'}
          {#if rpLoading}
            <div class="trace-empty">加载中…</div>
          {:else if !rpFiles.length}
            <div class="rp-empty">本任务还没有产物。<br />agent 修改/创建文件后，这里会列出改动清单（相对本次任务起点）。</div>
          {:else}
            {#each rpFiles as f (f)}
              <button class="rp-file" class:sel={f === rpPreviewPath} title={f} on:click={() => previewFile(f)}>
                {f}
              </button>
            {/each}
          {/if}
        {:else if rpTab === 'preview'}
          {#if rpPreviewPath}
            <div class="rp-preview-path">{rpPreviewPath}</div>
            <pre class="rp-preview">{rpPreviewContent || '（空文件）'}</pre>
          {:else}
            <div class="rp-empty">从「产物」里点一个文件查看内容</div>
          {/if}
        {:else}
          <div class="web-wrap">
            {#if webTabs.length}
              <div class="web-tabbar">
                {#each webTabs as t (t.id)}
                  <button
                    class="web-tab"
                    class:active={t.id === webActiveId}
                    title={t.url || 'about:blank'}
                    on:click={() => { webActiveId = t.id; if (!webBusy) webAddress = t.url === 'about:blank' ? '' : t.url || ''; webShotT += 1; webActivate(t.id); }}
                  >
                    <span class="web-tab-title">{t.title || (t.url === 'about:blank' ? '空白页' : t.url) || '空白页'}</span>
                    {#if t.agent}<span class="web-tab-agent" title="agent 浏览器工具的工作页">agent</span>{/if}
                    <span
                      class="web-tab-x"
                      title="关闭标签页"
                      role="button"
                      tabindex="-1"
                      on:click|stopPropagation={() => webCloseTab(t.id)}
                    >×</span>
                  </button>
                {/each}
              </div>
            {/if}
            <div class="web-toolbar">
              <button class="web-nav" title="后退" disabled={!webActive || webBusy} on:click={() => webAction('back')}>
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m14.5 6-6 6 6 6"/></svg>
              </button>
              <button class="web-nav" title="前进" disabled={!webActive || webBusy} on:click={() => webAction('forward')}>
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m9.5 6 6 6-6 6"/></svg>
              </button>
              <button class="web-nav" title="刷新" disabled={!webActive || webBusy} on:click={() => webAction('reload')}>
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M19 12a7 7 0 1 1-2.05-4.95"/><path d="M19 3.5V7h-3.5"/></svg>
              </button>
              <input
                class="web-address"
                placeholder="输入网址后回车"
                bind:value={webAddress}
                disabled={webBusy}
                spellcheck="false"
                on:keydown={(e) => {
                  if (e.isComposing || e.keyCode === 229) return;
                  if (e.key === 'Enter') { e.preventDefault(); webGo(); }
                }}
              />
              {#if webNative && webActive && webActive.loading}
                <span class="web-native-loading" title="页面加载中">●</span>
              {/if}
              <button class="web-add" title="新建标签页" on:click={webNewTab}>+</button>
              <button class="web-nav" title="更多" on:click={webToggleMenu}>
                <svg viewBox="0 0 24 24" fill="currentColor"><circle cx="5" cy="12" r="1.8"/><circle cx="12" cy="12" r="1.8"/><circle cx="19" cy="12" r="1.8"/></svg>
              </button>
              {#if webMenuOpen}
                <!-- svelte-ignore a11y-click-events-have-key-events a11y-no-static-element-interactions -->
                <div class="web-menu-backdrop" on:click={webCloseMenu}></div>
                <div class="web-menu" class:web-menu-up={webNative}>
                  <button on:click={() => webMenuAction('open-external')}>
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M14 4h6v6"/><path d="M20 4 11 13"/><path d="M19 14v5a2 2 0 0 1-2 2H6a2 2 0 0 1 2-2h5"/></svg>
                    在默认浏览器中打开
                  </button>
                  {#if webNative}
                    <!-- P22：壳里的 child webview 能开真 DevTools（无头时代做不到） -->
                    <button on:click={() => { webCloseMenu(); webMenuAction('devtools'); }}>
                      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M14.7 6.3a4.5 4.5 0 0 0-6 6L3 18l3 3 5.7-5.7a4.5 4.5 0 0 0 6-6L14 14l-4-4 4.7-3.7z"/></svg>
                      开发者工具
                    </button>
                  {/if}
                </div>
              {/if}
            </div>
            {#if webError}
              <div class="web-error">{webError}</div>
            {/if}
            {#if webDialog}
              <!-- 页面 JS 弹窗（alert/confirm/prompt）：无头页面的原生弹窗在这处理，不卡死页面 -->
              <!-- svelte-ignore a11y-click-events-have-key-events a11y-no-static-element-interactions -->
              <div class="web-dialog" on:click|stopPropagation>
                <span class="web-dialog-msg">页面弹窗〔{webDialog.kind}〕{webDialog.message || '（无内容）'}</span>
                <span class="flex-sp"></span>
                <button class="btn ghost" on:click={() => webDialogAction(true)}>确定</button>
                <button class="btn ghost" on:click={() => webDialogAction(false)}>取消</button>
              </div>
            {/if}
            <div class="web-body" bind:this={webBodyEl} bind:clientWidth={webBodyW} bind:clientHeight={webBodyH}>
              {#if !webShowShot}
                <div class="web-empty">
                  <svg class="web-globe" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"><circle cx="12" cy="12" r="8.5"/><path d="M3.5 12h17M12 3.5c2.6 2.3 3.9 5.1 3.9 8.5s-1.3 6.2-3.9 8.5c-2.6-2.3-3.9-5.1-3.9-8.5s1.3-6.2 3.9-8.5Z"/></svg>
                  <div class="web-empty-title">网页</div>
                  <div class="web-empty-sub">粘贴或输入 URL 以打开网页。</div>
                </div>
              {:else if webNative}
                <!-- P22：壳窗口里的 child webview 就浮在这块区域上（原生 HWND 盖在 Svelte 之上），
                     这里只留占位——点击/滚动/输入全部走真实网页，不再坐标回传 -->
                <div class="web-native" aria-hidden="true"></div>
              {:else}
                {#if webBusy || (webActive && webActive.loading)}<div class="web-loading">加载中…</div>{/if}
                <!-- svelte-ignore a11y-click-events-have-key-events a11y-no-static-element-interactions -->
                <img
                  class="web-shot"
                  class:busy={webBusy}
                  src={`/api/browser/frame?id=${encodeURIComponent(webActive.id)}&t=${webShotT}`}
                  alt="页面画面"
                  draggable="false"
                  on:click={webShotClick}
                  on:wheel={webShotWheel}
                />
              {/if}
            </div>
          </div>
        {/if}
      </div>
    </aside>
  {/if}

  {#if extOpen && extCurrent}
    <aside class="ext-panel">
      <div class="rp-tabs">
        <b class="ext-title">{extCurrent.title}</b>
        <span class="mcp-status">{extCurrent.source === 'workspace' ? '工作区' : '内置'}</span>
        <span class="flex-sp"></span>
        <button
          class="rp-refresh"
          title="重新加载插件"
          on:click={() => {
            if (extFrame) extFrame.src = extFrame.src;
          }}>↻</button
        >
        <button class="btn ghost" on:click={() => (extOpen = false)}>关闭</button>
      </div>
      <iframe
        class="ext-frame"
        bind:this={extFrame}
        title={extCurrent.title}
        sandbox="allow-scripts"
        src={`/ui-ext/${encodeURIComponent(extCurrent.name)}/${extCurrent.entry || 'index.html'}`}
        on:load={() => onExtFrameLoad(extCurrent.name)}
      ></iframe>
    </aside>
  {/if}

  <!-- P17 扩展间通信：其它已启用扩展常驻后台（不可见），消息由宿主转发。 -->
  {#each enabledExts.filter((e) => e.entry) as e (e.name)}
    {#if !(extOpen && extCurrent && extCurrent.name === e.name)}
      <iframe
        class="ext-hidden"
        title={`插件后台：${e.title}`}
        sandbox="allow-scripts"
        bind:this={extFrameEls[e.name]}
        src={`/ui-ext/${encodeURIComponent(e.name)}/${e.entry || 'index.html'}`}
        on:load={() => onExtFrameLoad(e.name)}
      ></iframe>
    {/if}
  {/each}

</div>

{#if microPanel}
  <div
    class="micro-panel"
    style="left:{microPanel.x}px; top:{microPanel.y}px; max-height: calc(100vh - {microPanel.y}px - 10px);"
  >
    <div class="micro-head">
      <div class="micro-name">{microPanel.name}</div>
      <div class="micro-sel">{microPanel.sel}</div>
    </div>
    <div class="micro-body">
      <div class="micro-row">
        <label>文字色</label>
        <input type="color" data-m="color" value={microPanel.color} on:input={(e) => microPreview('color', e.target.value)} />
        <span class="micro-val">{microPanel.color}</span>
      </div>
      <div class="micro-row">
        <label>背景色</label>
        <input type="color" data-m="bg" value={microPanel.bg} on:input={(e) => microPreview('bg', e.target.value)} />
        <span class="micro-val">{microPanel.bgTransparent && !microPanel.bgDirty ? '透明（不动）' : microPanel.bg}</span>
      </div>
      <div class="micro-row">
        <label>背景透明</label>
        <input type="range" min="0" max="100" data-m="bgOpacity" value={microPanel.bgOpacity} on:input={(e) => microPreview('bgOpacity', +e.target.value)} />
        <span class="micro-val">{microPanel.bgOpacity}%</span>
      </div>
      {#if microPanel.bgHost}
        <div class="micro-warn">这个元素本身是透明的，你看到的底色其实由上级「{microPanel.bgHost.name || microPanel.bgHost.sel}」绘制。改本层背景不会有变化。</div>
        <div class="micro-actions" style="margin-top:0">
          <button class="btn primary" on:click={useBgHost}>改上级「{microPanel.bgHost.name || microPanel.bgHost.sel}」</button>
        </div>
      {/if}
      <div class="micro-row">
        <label>模糊</label>
        <input type="range" min="0" max="30" data-m="blur" value={microPanel.blur} on:input={(e) => microPreview('blur', +e.target.value)} />
        <span class="micro-val">{microPanel.blur}px</span>
      </div>
      {#if microPanel.blur > 0 && microPanel.bgOpacity >= 85 && !microPanel.bgTransparent}
        <div class="micro-warn">毛玻璃需要半透明底：模糊要透出下层，背景不透明度建议先降到 85% 以下，否则模糊不可见还会在弹窗下发灰。现在这条模糊不会被保存（半透明后可再加）。</div>
      {/if}
      <div class="micro-row">
        <label>圆角</label>
        <input type="range" min="0" max="40" data-m="radius" value={microPanel.radius} on:input={(e) => microPreview('radius', +e.target.value)} />
        <span class="micro-val">{microPanel.radius}px</span>
      </div>
      <div class="micro-row">
        <label>描边</label>
        <input type="range" min="0" max="8" data-m="border" value={microPanel.border} on:input={(e) => microPreview('border', +e.target.value)} />
        <span class="micro-val">{microPanel.border}px</span>
      </div>
      <div class="micro-row">
        <label>描边色</label>
        <input type="color" data-m="borderColor" value={microPanel.borderColor} on:input={(e) => microPreview('borderColor', e.target.value)} />
        <span class="micro-val">{microPanel.borderColor}</span>
      </div>
      <div class="micro-row">
        <label>字号</label>
        <input type="number" min="9" max="24" data-m="fontSize" value={microPanel.fontSize} on:input={(e) => microPreview('fontSize', +e.target.value)} />
        <span class="micro-val">{microPanel.fontSize}px</span>
      </div>
      <div class="micro-row">
        <label>作用范围</label>
        <div class="scope-opts">
          <button class="scope-opt" class:active={microPanel.scope !== 'all'} on:click={() => (microPanel = { ...microPanel, scope: 'only' })}>仅此元素</button>
          <button class="scope-opt" class:active={microPanel.scope === 'all'} on:click={() => (microPanel = { ...microPanel, scope: 'all' })}>同类全部</button>
        </div>
      </div>
      <div class="micro-hint">改动实时预览；「应用」只保存你动过的属性。作用范围默认「仅此元素」，要统一改一类元素就切「同类全部」。生成的规则见下方 CSS 注入器里的只读预览，手写 CSS 永远优先。</div>
    </div>
    <div class="micro-foot">
      <div class="micro-actions">
        <button class="btn primary" on:click={applyMicro}>应用</button>
        <button class="btn" on:click={resetMicro}>重置此元素</button>
        <button class="btn ghost" on:click={cancelMicro}>取消</button>
      </div>
    </div>
  </div>
{/if}

{#if settingsOpen}
  <div class="modal-backdrop" on:click={() => (settingsOpen = false)}>
    <div class="modal settings-modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>设置</b>
        <span class="hint">{settingsMsg}</span>
        <button class="btn ghost" on:click={() => (settingsOpen = false)}>关闭</button>
      </div>
      <div class="settings-body">
        <div class="settings-nav">
          <button class:active={settingsTab === 'appearance'} on:click={() => pickSettingsTab('appearance')}><svg class="nav-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><circle cx="12" cy="12" r="8.5"/><circle cx="9" cy="10" r="1.2" fill="currentColor"/><circle cx="12.5" cy="8.5" r="1.2" fill="currentColor"/><circle cx="15.5" cy="11" r="1.2" fill="currentColor"/></svg> 外观</button>
          <button class:active={settingsTab === 'model'} on:click={() => pickSettingsTab('model')}><svg class="nav-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M9 7V3.5M15 7V3.5M7 7h10v4a5 5 0 0 1-10 0Z"/><path d="M12 16v5"/></svg> 模型配置</button>
          <button class:active={settingsTab === 'roles'} on:click={() => pickSettingsTab('roles')}><svg class="nav-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><circle cx="9" cy="8.5" r="3"/><path d="M3.5 19.5c.6-3 2.8-4.5 5.5-4.5s4.9 1.5 5.5 4.5"/><circle cx="17" cy="9.5" r="2.4"/><path d="M16 15.2c2.4.2 3.9 1.6 4.4 4"/></svg> 角色分工</button>
          <button class:active={settingsTab === 'memory'} on:click={() => pickSettingsTab('memory')}><svg class="nav-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M12 4.5A7.5 7.5 0 0 0 4.5 12c0 2.4 1.1 4.2 2.8 5.5.4.3.7.9.7 1.5v1.5h9V19c0-.6.3-1.2.7-1.5 1.7-1.3 2.8-3.1 2.8-5.5A7.5 7.5 0 0 0 12 4.5Z"/></svg> 记忆</button>
          <button class:active={settingsTab === 'skills'} on:click={() => pickSettingsTab('skills')}><svg class="nav-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M13 2.5 5.5 13.5H11l-1 8L18.5 10H13l1-7.5Z"/></svg> 技能</button>
          <button class:active={settingsTab === 'mcp'} on:click={() => pickSettingsTab('mcp')}><svg class="nav-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M9 7V3.5M15 7V3.5M7 7h10v4a5 5 0 0 1-10 0Z"/><path d="M12 16v5"/></svg> MCP</button>
          <button class:active={settingsTab === 'ext'} on:click={() => pickSettingsTab('ext')}><svg class="nav-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><rect x="3.5" y="4.5" width="17" height="15" rx="2.6"/><path d="M3.5 9.5h17"/><path d="M8 14h5" stroke-linecap="round"/></svg> 插件</button>
        </div>
        <div class="settings-content">
        {#if settingsTab === 'appearance'}
          <div class="field">
            <label>主题</label>
            <div class="theme-opts">
              <button class="theme-opt" class:active={themeMode === 'dark'} on:click={() => applyThemeMode('dark')}>深色</button>
              <button class="theme-opt" class:active={themeMode === 'light'} on:click={() => applyThemeMode('light')}>浅色</button>
              <button class="theme-opt" class:active={themeMode === 'system'} on:click={() => applyThemeMode('system')}>跟随系统</button>
            </div>
            <div class="param-hint">即时生效并记住；侧栏顶部可快速切换</div>
          </div>
          <div class="field">
            <label>背景壁纸</label>
            <div class="wp-row">
              <label class="theme-opt wp-pick">选择图片
                <input type="file" accept="image/*" on:change={onWallpaperPick} />
              </label>
              {#if wallpaper}
                <button class="theme-opt" on:click={clearWallpaper}>清除壁纸</button>
                <span class="wp-opacity">不透明度 <b>{wallpaperOpacity}</b>
                  <input type="range" min="0" max="100" bind:value={wallpaperOpacity} />
                </span>
              {:else}
                <span class="param-hint">聊天区背景图（0=隐藏，100=不透明）</span>
              {/if}
            </div>
            {#if wallpaperMsg}<div class="param-hint">{wallpaperMsg}</div>{/if}
          </div>
          <div class="field">
            <label>CSS 注入器</label>
            <textarea
              class="agents-editor css-injector"
              rows="8"
              spellcheck="false"
              bind:value={customCss}
              on:input={() => applyCustomCss()}
              placeholder={'直接写 CSS 规则，实时生效并记住。例如：\n.composer { border-radius: 24px; }\n.chat-head { display: none; }'}
            ></textarea>
            <div class="param-hint">手写 CSS 优先级最高；改崩了就清空重来</div>
            {#if microCss(microTweaks)}
              <label>微调生成的规则（只读预览，实际已生效）</label>
              <textarea
                class="agents-editor css-injector micro-readonly"
                rows="6"
                spellcheck="false"
                readonly
                value={microCss(microTweaks)}
              ></textarea>
              <div class="sub-row">
                <button class="btn" on:click={clearAllMicro}>清空全部微调规则</button>
                <span class="param-hint" style="align-self:center">一键清空微调规则（不影响手写 CSS）</span>
              </div>
            {/if}
            <div class="sub-row">
              <button class="btn" on:click={exportCssRules}>导出规则</button>
              <button class="btn" on:click={() => cssImportInput && cssImportInput.click()}>导入规则</button>
              <input
                class="css-rules-file"
                type="file"
                accept=".json,.css,.txt,application/json,text/css,text/plain"
                bind:this={cssImportInput}
                on:change={onImportCssRules}
              />
              <span class="param-hint" style="align-self:center">导出 JSON 含微调+手写；导入 JSON 覆盖两者，.css 只覆盖手写</span>
            </div>
          </div>
          <div class="field">
            <label>界面微调模式</label>
            <div class="sub-row">
              <button class="btn" class:primary={editMode} on:click={() => (editMode = !editMode)}>
                {editMode ? '● 微调中：右击界面元素即可修改（再点关闭）' : '开启微调：右击界面元素修改样式'}
              </button>
            </div>
            <div class="param-hint">开启后右击元素弹出属性面板，改动实时预览并保存；刷新后需重新开启</div>
          </div>
        {/if}
        {#if settingsTab === 'model'}

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
          <button class="chip add" on:click={newConfig}>新建配置</button>
          <button class="chip del" on:click={deleteConfig}>删除当前</button>
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
        <Select
          value={providerId}
          title="供应商预设"
          options={providers.map((p) => ({ value: p.id, label: p.name }))}
          on:change={(e) => {
            providerId = e.detail;
            onProviderChange();
          }}
        />
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
        <div class="param-hint">左：上下文长度　右：最大输出长度；以下参数均随当前配置独立保存，切换配置即切换参数</div>
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
        <div class="param-hint">勾选即时保存；未勾的模态自动转给「角色分工」的解析专家</div>
      </div>

      {/if}
      {#if settingsTab === 'roles'}
      <div class="field roles-section">
        <label>角色分工与多模态转接（专职专家席位）</label>
        <div class="roles-grid">
          <div class="role-item">
            <span class="role-name">会话摘要 (Summary)</span>
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
            <span class="role-name">标题生成 (Title)</span>
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
            <span class="role-name">向量嵌入 (Embedding)</span>
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
            <span class="role-name">图片理解专家 (Vision)</span>
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
            <span class="role-name">视频理解专家 (Video)</span>
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
            <span class="role-name">音频理解专家 (Audio)</span>
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
        <div class="param-hint">留空 = 自动兜底（摘要/标题用主力，嵌入用本地向量）；专家下拉只列勾了对应解析能力的配置</div>
      </div>
      {/if}
      {#if settingsTab === 'memory'}
        {#if memData}
          <div class="field">
            <label>记忆存储（自动记录，非约定）</label>
            <div class="mem-stats">
              <div>· 全局目录 <code>{memData.global_dir}</code> · {memData.projects_registered} 个项目</div>
              <div>· 项目目录 <code>{memData.project_dir}</code> · events.log {memData.events_lines} 行</div>
            </div>
            <div class="param-hint">events.log 是自动流水，不是约定。</div>
          </div>
          <div class="field">
            <label>记忆库</label>
            <div class="sub-row">
              <button class="btn" on:click={() => { settingsOpen = false; galaxyOpen = true; }}>打开记忆库</button>
              <span class="hint">一次会话 = 一个光点，可搜索</span>
            </div>
          </div>
          <div class="field">
            <label>约定文件（注入 system，留空 = 不注入）</label>
            <div class="chips">
              <button class="chip" class:active={memTab === 'system'} on:click={() => pickMemTab('system')}>系统约定</button>
              <button class="chip" class:active={memTab === 'global'} on:click={() => pickMemTab('global')}>全局约定</button>
              <button class="chip" class:active={memTab === 'project'} on:click={() => pickMemTab('project')}>项目约定</button>
            </div>
            <textarea class="agents-editor" rows="12" spellcheck="false" bind:value={memText} on:input={() => (memDirty = true)}></textarea>
            <div class="sub-row">
              <button class="btn" on:click={saveAgents} disabled={memSaving}>{memSaving ? '保存中…' : '保存'}</button>
              <span class="hint">{memMsg}</span>
            </div>
            <div class="param-hint">
              {#if memTab === 'system'}基础提示词。{/if}
              {#if memTab === 'global'}所有项目共用。{/if}
              {#if memTab === 'project'}仅当前工作区。{/if}
              文件：<code>{memPath}</code>
            </div>
          </div>
        {:else}
          <div class="trace-empty">{memMsg || '加载中…'}</div>
        {/if}
      {/if}
      {#if settingsTab === 'skills'}
        <div class="param-hint">一个技能 = 一个文件夹 + SKILL.md；目录：工作区 skills/ 与 ~/.agent/skills/（同名项目优先）。system 只注入名字+描述，消息写 $技能名 指定</div>
        <div class="sub-row">
          <button class="btn primary" on:click={newSkill}>新建技能</button>
          <button class="btn" on:click={loadSkills}>刷新</button>
          <span class="hint">{skillMsg}</span>
        </div>
        {#if skillEditor}
          <div class="field">
            <label>{skillEditor.isNew ? '新建技能' : `编辑技能：${skillEditor.name}`}{skillEditor.source === 'global' ? '（全局技能 · 保存将在工作区生成同名项目级覆盖）' : ''}</label>
            <div class="sub-row">
              <input placeholder="技能名（可中文，禁 / \ ..）" bind:value={skillEditor.name} style="max-width:200px" disabled={skillEditor.isNew ? false : skillEditor.source === 'global'} />
              <input placeholder="一句话描述（注入索引给模型看）" bind:value={skillEditor.description} />
            </div>
            <textarea class="agents-editor" rows="14" spellcheck="false" bind:value={skillEditor.content}></textarea>
            <div class="sub-row">
              <button class="btn primary" on:click={saveSkill}>保存</button>
              <button class="btn" on:click={() => (skillEditor = null)}>取消</button>
            </div>
          </div>
        {/if}
        {#if skillsLoading}
          <div class="trace-empty">加载中…</div>
        {:else}
          {#each skillsData as s (s.name)}
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
      {#if settingsTab === 'mcp'}
        <div class="param-hint">接入外部工具（mcp__服务__工具）。首次接入要先确认工具清单；每服务可设审批档</div>
        {#each mcpServers as sv}
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
              {sv.transport === 'http' ? `HTTP · ${sv.url}` : `${sv.command} ${(sv.args || []).join(' ')}`}
            </div>
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
          <label>添加 MCP server（stdio：本地命令；HTTP：远端 Streamable HTTP 端点）</label>
          <div class="sub-row">
            <input placeholder="名称（字母数字-_）" bind:value={mcpForm.name} style="max-width:160px" />
            <Select
              value={mcpForm.transport}
              title="transport"
              options={[
                { value: 'stdio', label: 'stdio（本地子进程）' },
                { value: 'http', label: 'http（远端 MCP）' },
              ]}
              on:change={(e) => (mcpForm.transport = e.detail)}
            />
            {#if mcpForm.transport === 'http'}
              <input placeholder="https://host/mcp" bind:value={mcpForm.url} />
            {:else}
              <input placeholder="命令，如 node / npx" bind:value={mcpForm.command} />
              <input placeholder="参数（空格分隔）" bind:value={mcpForm.args} />
            {/if}
            <button
              class="btn primary"
              on:click={addMcpServer}
              disabled={!mcpForm.name.trim() ||
                (mcpForm.transport === 'http' ? !mcpForm.url.trim() : !mcpForm.command.trim())}>添加</button
            >
          </div>
          {#if mcpForm.transport === 'http'}
            <div class="sub-row">
              <textarea
                placeholder={'自定义请求头，每行一条：\nAuthorization: Bearer xxx'}
                bind:value={mcpForm.headers}
                rows="2"
              ></textarea>
            </div>
            <div class="param-hint">
              远端 MCP 走规范里的 Streamable HTTP：POST JSON-RPC，响应支持 application/json 与
              text/event-stream（SSE 帧），initialize 拿到的 Mcp-Session-Id 会自动回带。
              请求头只存在本地 settings.json 与内存，绝不进日志。
            </div>
          {:else}
            <div class="param-hint">示例（官方 filesystem）：名称 fs · 命令 npx · 参数 -y @modelcontextprotocol/server-filesystem D://some//dir</div>
          {/if}
        </div>
      {/if}
      {#if settingsTab === 'ext'}
        <div class="param-hint">
          插件（契约 v1）：一个目录 + 一份 manifest.json = 一个插件，面板与工具都在清单里声明，
          不分类型。<b>应用根 ui-ext/</b> 放随包插件，<b>当前工作区 ui-ext/</b> 放项目自带插件
          （同名时工作区优先）；工具文件放 <b>plugins/</b>（.wasm/.wat 或 目录 + manifest）。
          面板在 <code>sandbox="allow-scripts"</code> 的 iframe 里跑——不透明源、读不到宿主 DOM
          与 localStorage，只能通过白名单接口拿数据。
        </div>
        {#if extMsg}<div class="mcp-cmd">{extMsg}</div>{/if}
        {#if extList.length}
          {#each extList as e}
            <div class="field mcp-card">
              <div class="mcp-head">
                <span class="dot" class:haskey={e.ok}></span>
                <b>{e.title}</b>
                {#if e.version}<span class="mcp-risk">v{e.version}</span>{/if}
                <span
                  class="mcp-status"
                  title={e.shadowed ? '同名内置插件存在，被当前工作区的这一份覆盖' : ''}
                >{e.draft ? '草稿 · 未转正' : e.source === 'workspace' ? (e.shadowed ? '工作区 · 覆盖内置' : '工作区') : (e.tools && e.tools.length ? '工具插件' : '内置')}</span>
                {#if e.origin === 'model_generated'}<span class="mcp-risk">模型自产</span>{/if}
                {#if e.enabled === false}<span class="mcp-risk">已停用</span>{/if}
                <span class="flex-sp"></span>
                {#if e.draft}
                  <button class="btn primary" on:click={() => promoteExt(e)}>转正</button>
                  <button class="btn ghost" on:click={() => discardExt(e)}>丢弃</button>
                {:else}
                  <button class="btn ghost" on:click={() => toggleExt(e)}>{e.enabled === false ? '启用' : '停用'}</button>
                  {#if e.entry}<button class="btn primary" on:click={() => openExt(e)} disabled={!e.ok || e.enabled === false}>打开</button>{/if}
                {/if}
              </div>
              {#if e.description}<div class="mcp-cmd">{e.description}</div>{/if}
              {#if e.tools && e.tools.length}
                <div class="mcp-cmd">工具：{e.tools.map((t) => `wasm__${e.name}__${t[0] ?? t.name}`).join('、')}</div>
              {/if}
              {#if e.permissions && e.permissions.length}
                <div class="mcp-cmd">权限：{e.permissions.join('、')}（未声明的写操作一律拒绝）</div>
              {/if}
              {#if e.ui && e.ui.length}
                <div class="mcp-cmd">界面插槽：{e.ui.map((u) => u.slot).join('、')}（顶栏按钮 → 插件收到 agent:slot 事件）</div>
              {/if}
              {#if e.error}
                <div class="mcp-cmd">{e.error}</div>
              {:else}
                <div class="mcp-cmd">{e.dir}</div>
              {/if}
            </div>
          {/each}
        {:else}
          <div class="menu-empty">
            还没有插件。在 <code>{extInfo.workspace_dir}</code> 下建一个目录（内含 manifest.json
            与 index.html）即可。
          </div>
        {/if}
        <div class="field">
          <label>插件目录约定</label>
          <div class="mcp-cmd">内置：{extInfo.app_dir || '（读取中）'}</div>
          <div class="mcp-cmd">工作区：{extInfo.workspace_dir || '（读取中）'}</div>
          <div class="param-hint">
            manifest.json 形如 {`{"apiVersion":1,"source":"user","title":"记忆看板","description":"…","version":"0.1.0","entry":"index.html","ui":[],"permissions":[]}`}；
            apiVersion 不匹配的插件会被拒载并提示升级；permissions 可声明
            composer.insert / workspace.write / bus.publish / notify / storage（storage =
            插件自己命名空间内的读写，碰不到会话与设置）。
            入口页里用 <code>postMessage({'{'}type:'agent:hello'{'}'})</code> 握手，之后可调
            app.info / workspace.info / workspace.files / git.status / memory.search / session.info /
            storage.read / storage.write / storage.delete。
          </div>
        </div>
        <div class="field"><label>运行状态（本机环境）</label></div>
        <div class="field mcp-card">
          <div class="mcp-head">
            <span class="dot" class:haskey={sandboxInfo?.ok === true}></span>
            <b>沙箱档位</b>
            <span class="mcp-status">
              {sandboxInfo ? sandboxInfo.label + (sandboxInfo.effective !== sandboxInfo.requested ? `（请求的 ${sandboxInfo.requested} 已回退）` : '') : '读取中…'}
            </span>
            <span class="flex-sp"></span>
            <button class="btn ghost" on:click={() => loadSandboxInfo(true)} disabled={sbInfoLoading}>
              {sbInfoLoading ? '探测中…' : '重新检测'}
            </button>
          </div>
          {#if sandboxInfo}
            <div class="mcp-cmd">生效档 {sandboxInfo.effective} · 配置档 {sandboxInfo.mode_configured} · 直跑开关{sandboxInfo.allow_unsandboxed ? '已开启' : '未开启'}</div>
            {#if sandboxInfo.fallback_reason}
              <div class="mcp-cmd" style="color: var(--tint-orange-fg)">⚠ {sandboxInfo.fallback_reason}</div>
            {/if}
            <div class="param-hint">{sandboxInfo.describe}</div>
          {/if}
        </div>
        <div class="field mcp-card">
          <div class="mcp-head">
            <span class="dot" class:haskey={gitRuntime?.ok}></span>
            <b>Git 运行时</b>
            <span class="mcp-status">
              {gitRuntime?.ok
                ? gitRuntime.source === 'system'
                  ? '系统 Git'
                  : gitRuntime.source === 'portable'
                    ? '便携版（exe 同级）'
                    : '内置 Git'
                : gitRuntime
                  ? '不可用'
                  : '读取中…'}
            </span>
            <span class="flex-sp"></span>
            <button class="btn ghost" on:click={loadGitRuntime}>重新检测</button>
          </div>
          {#if gitRuntime?.ok}
            <div class="mcp-cmd">版本 {gitRuntime.version} · {gitRuntime.path}</div>
          {:else if gitRuntime}
            <div class="mcp-cmd">{gitRuntime.error}</div>
          {/if}
          <div class="param-hint">
            优先使用你机器上已装的 Git（PATH / 注册表 / 常见安装位置）；没装时才解压随包内置的
            MinGit 到 {gitRuntime?.runtime_dir || '%LOCALAPPDATA%\\agent\\runtime\\git'}（无需联网、不要管理员）。
            内置包{gitRuntime?.zip_available ? '已随发行包携带' : '未携带'}；新装 Git 后需重启服务才会重新检测。
          </div>
        </div>
        <div class="field mcp-card">
          <div class="mcp-head">
            <span class="dot" class:haskey={sandboxProbe?.ok === true}></span>
            <b>沙箱自检</b>
            <span class="mcp-status">
              {sbRunning ? '运行中…' : sandboxProbe ? (sandboxProbe.ok ? '全部通过' : '有失败项') : '未运行'}
            </span>
            <span class="flex-sp"></span>
            {#if sandboxInfo}
              <span class="tool-badge ok">{sandboxInfo.label}</span>
            {/if}
            <button class="btn ghost" on:click={runSandboxProbe} disabled={sbRunning}>运行自检</button>
          </div>
          {#if sandboxProbe?.results?.length}
            <div class="probe-list">
              {#each sandboxProbe.results as p}
                <div class="probe-row">
                  <span class="dot" class:haskey={p.ok}></span>
                  <code>{p.name}</code>
                  <span class="probe-detail">{p.detail}</span>
                </div>
              {/each}
            </div>
          {:else if sandboxProbe?.error}
            <div class="mcp-cmd">{sandboxProbe.error}</div>
          {/if}
          <div class="param-hint">
            真实拉起受限进程跑一遍隔离面（输出捕获 / 环境白名单 / 特权剥离 / 工作目录可达 / 超时团灭）。
            只读操作，不会改动任何文件；kill-tree 与内存上限两项较慢，只在 cargo test 里跑。
          </div>
        </div>
      {/if}
        </div>
      </div>
    </div>
  </div>
{/if}

{#if ctxPickerOpen}
  <div class="modal-backdrop" on:click={() => (ctxPickerOpen = false)}>
    <div class="modal ctx-modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>使用 @ 添加上下文</b>
        <span class="hint">选择工作区文件，以 @相对路径 插入消息，模型会自行读取</span>
        <button class="btn ghost" on:click={() => (ctxPickerOpen = false)}>关闭</button>
      </div>
      <input class="ctx-search" placeholder="搜索文件…" bind:value={ctxQuery} />
      <div class="ctx-file-list">
        {#if ctxLoading}
          <div class="trace-empty">加载中…</div>
        {:else}
          {#each ctxFiles.filter((f) => !ctxQuery || f.toLowerCase().includes(ctxQuery.toLowerCase())) as f (f)}
            <button class="ctx-file" on:click={() => pickCtxFile(f)}>{f}</button>
          {:else}
            <div class="menu-empty">没有匹配的文件</div>
          {/each}
        {/if}
      </div>
    </div>
  </div>
{/if}

<WorkspaceModal
    open={wsModalOpen}
    msg={wsMsg}
    workspace={workspace}
    recent={recentWorkspaces}
    on:apply={(e) => applyWorkspace(e.detail)}
    on:close={() => (wsModalOpen = false)}
  />
<RenameModal modal={renameModal} on:close={() => (renameModal = null)} on:submit={submitRename} />

<ModelPicker
    open={modelModalOpen}
    models={fetchedModels}
    saved={savedModels}
    on:close={() => (modelModalOpen = false)}
    on:toggle={(e) => toggleModel(e.detail)}
    on:selectall={selectAllModels}
    on:selectnone={selectNoModels}
  />

<ImagePreview src={previewImageModal} on:close={() => (previewImageModal = null)} />

{#if galaxyOpen}
  <!-- 记忆库星云：光点 = 会话；搜索 = 向量检索过滤 -->
  <Galaxy currentSessionId={currentSessionId || ''} on:close={() => (galaxyOpen = false)} on:open={openSessionFromGalaxy} />
{/if}

{#if commitModal}
  <div class="modal-backdrop" on:click={() => (commitModal = null)}>
    <div class="modal commit-modal" on:click|stopPropagation>
      <div class="modal-head">
        <b>提交改动</b>
        <span class="hint">{gitInfo?.branch || ''}{gitInfo?.head ? '@' + gitInfo.head : ''}</span>
        <button class="btn ghost" on:click={() => (commitModal = null)}>关闭</button>
      </div>
      <div class="commit-body">
        <div class="commit-files">
          <div class="commit-files-head">
            已选 {commitSelCount()} / {commitModal.files.length} 个文件
            <span class="param-hint">未跟踪文件默认不勾（日志/构建产物等），需要就自己勾上</span>
          </div>
          <div class="commit-files-list">
            {#each commitModal.files as f (f.path)}
              <label class="commit-file" title={f.path}>
                <input type="checkbox" checked={f.sel} on:change={() => toggleCommitFile(f.path)} />
                <span class="git-st">{gitStatusLabel(f.status)}</span>
                <span class="git-path">{f.path}</span>
              </label>
            {/each}
          </div>
        </div>
        <textarea
          class="agents-editor commit-msg"
          rows="3"
          spellcheck="false"
          placeholder="提交信息（一句话说清这次改了什么）"
          bind:value={commitModal.msg}
        ></textarea>
        {#if commitModal.err}<div class="commit-err">{commitModal.err}</div>{/if}
        <div class="sub-row">
          <button class="btn primary" disabled={commitModal.busy} on:click={doCommit}>
            {commitModal.busy ? '提交中…' : `提交 ${commitSelCount()} 个文件`}
          </button>
          <span class="param-hint" style="align-self:center">只做本地 commit，不推送。</span>
        </div>
      </div>
    </div>
  </div>
{/if}

<style>

  .app {
    display: flex;
    height: 100vh;
    position: relative;
    z-index: 1;
  }
  /* —— 背景壁纸层：fixed 铺满，压在内容之下；开启后主要表面改半透明透出壁纸 —— */
  .wallpaper {
    position: fixed;
    inset: 0;
    z-index: 0;
    background-size: cover;
    background-position: center;
    background-repeat: no-repeat;
    pointer-events: none;
  }
  :global(body.has-wallpaper) {
    background: transparent;
  }
  :global(body.has-wallpaper) .sidebar {
    background: color-mix(in srgb, var(--g0) 60%, transparent);
    border-right-color: color-mix(in srgb, var(--g4) 30%, transparent);
  }
  :global(body.has-wallpaper) .chat-head {
    background: color-mix(in srgb, var(--g1) 62%, transparent);
    border-bottom-color: color-mix(in srgb, var(--g4) 30%, transparent);
  }
  /* 消息流内的深色面全部转半透明：88% 实色在壁纸上就是一块黑，用户 CSS 想盖也因为
     规则分散（.tool-card/.cp-bar/...）而显得"去不掉"，这里统一收口 */
  :global(body.has-wallpaper) .asst-bubble {
    background: color-mix(in srgb, var(--g1) 72%, transparent);
    border-color: color-mix(in srgb, var(--g2) 50%, transparent);
  }
  :global(body.has-wallpaper) .composer {
    background: color-mix(in srgb, var(--g2) 72%, transparent);
  }
  :global(body.has-wallpaper) .tool-card,
  :global(body.has-wallpaper) .tool-preview,
  :global(body.has-wallpaper) .thinking-body,
  :global(body.has-wallpaper) .cp-bar,
  :global(body.has-wallpaper) .approval-box,
  :global(body.has-wallpaper) .ctx-card {
    background: color-mix(in srgb, var(--g1) 66%, transparent);
    border-color: color-mix(in srgb, var(--g3) 50%, transparent);
  }
  :global(body.has-wallpaper) .term-panel {
    background: color-mix(in srgb, #101216 82%, transparent);
  }
  :global(body.has-wallpaper) .new-chat {
    background: color-mix(in srgb, var(--g1) 66%, transparent);
  }
  :global(body.has-wallpaper) .conv.active {
    background: color-mix(in srgb, var(--g2) 66%, transparent);
  }
  :global(body.has-wallpaper) .conv:hover {
    background: color-mix(in srgb, var(--g1) 55%, transparent);
  }
  :global(body.has-wallpaper) .proj,
  :global(body.has-wallpaper) .side-search {
    background: color-mix(in srgb, var(--g2) 60%, transparent);
  }
  /* 自绘下拉的弹出菜单：壁纸模式下同样半透明，保持与其他浮层一致 */
  :global(body.has-wallpaper) .csel-menu {
    background: color-mix(in srgb, var(--g1) 80%, transparent);
    border-color: color-mix(in srgb, var(--g3) 60%, transparent);
  }
  /* 右侧产物/预览面板：整块 g0 实色，之前漏了壁纸半透明，表现为"改了内部元素透明度还是黑" */
  :global(body.has-wallpaper) .right-panel {
    background: color-mix(in srgb, var(--g0) 60%, transparent);
    border-left-color: color-mix(in srgb, var(--g4) 30%, transparent);
  }
  :global(body.has-wallpaper) .rp-preview {
    background: color-mix(in srgb, var(--g1) 66%, transparent);
    border-color: color-mix(in srgb, var(--g3) 50%, transparent);
  }
  .sidebar {
    width: 300px;
    box-sizing: border-box; /* P9 修复：宽=sidebarW 语义对齐（渲染宽不再多出 padding+border 25px，拖拽手柄贴手指） */
    flex: none;
    background: var(--g0);
    border-right: 1px solid var(--g1);
    display: flex;
    flex-direction: column;
    padding: 14px 12px 10px;
    position: relative;
  }
  .sidebar.resizing {
    border-right-color: var(--g3);
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
    background: var(--veil);
  }
  .sidebar.collapsed {
    display: none;
  }
  /* 侧栏收起时展开按钮浮在左上角，标题要让位，否则两者叠在一起 */
  .chat.side-collapsed .chat-head {
    padding-left: 60px;
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
  .brand-mark {
    width: 30px;
    height: 30px;
    border-radius: 8px;
    background: var(--g7);
    color: var(--g0);
    display: flex;
    align-items: center;
    justify-content: center;
    flex: none;
  }
  .brand-mark svg {
    width: 18px;
    height: 18px;
  }
  .brand-name {
    flex: 1;
    font-size: 16px;
    font-weight: 650;
    color: var(--g7);
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
    color: var(--g6);
    cursor: pointer;
    flex: none;
  }
  .icon-btn:hover {
    background: var(--g1);
    color: var(--g7);
  }
  .icon-btn.on {
    background: var(--g2);
    color: var(--g7);
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
    height: 40px;
    border-radius: 10px;
    border: 1px solid var(--g3);
    background: var(--g1);
    color: var(--g7);
    font-size: 13.5px;
    font-weight: 600;
    cursor: pointer;
  }
  .new-chat:hover {
    background: var(--g2);
    border-color: var(--g4);
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
    color: var(--g5);
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
    background: var(--g1);
    border: 1px solid var(--g2);
    border-radius: 10px;
    color: var(--g7);
    font-size: 14px;
    padding: 9px 12px;
    outline: none;
  }
  .side-search:focus {
    border-color: var(--g3);
  }
  .manage-bar {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 4px 2px 10px;
    flex-wrap: wrap;
  }
  .m-count {
    color: var(--g5);
    font-size: 13px;
    margin-right: auto;
    padding-left: 4px;
  }
  .m-btn {
    background: var(--g1);
    border: 1px solid var(--g2);
    color: var(--g6);
    font-size: 13px;
    border-radius: 8px;
    padding: 5px 10px;
    cursor: pointer;
  }
  .m-btn:hover:not(:disabled) {
    background: var(--g2);
  }
  .m-btn:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .m-btn.primary {
    background: var(--g3);
    color: var(--g7);
    border-color: var(--g5);
    font-weight: 650;
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
    color: var(--g7);
    font-size: 15.5px;
  }
  .proj:hover {
    background: var(--g1);
  }
  .proj.expanded {
    background: var(--g2);
  }
  .proj > svg {
    width: 19px;
    height: 19px;
    color: var(--g6);
    flex: none;
  }
  /* 展开/收起的独立热区（在项目行内部，点它不切项目） */
  .proj-toggle {
    width: 22px;
    height: 22px;
    margin: 0 -3px 0 -2px;
    padding: 0;
    border: none;
    background: transparent;
    color: var(--g6);
    display: inline-flex;
    align-items: center;
    justify-content: center;
    cursor: pointer;
    flex: none;
    border-radius: 6px;
  }
  .proj-toggle:hover {
    color: var(--g7);
    background: var(--g3);
  }
  .proj-toggle svg {
    width: 19px;
    height: 19px;
  }
  .proj-name {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .proj.active .proj-name {
    font-weight: 600;
  }
  /* 当前项目的小圆点标记 */
  .proj-cur {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--g5);
    flex: none;
    margin-left: 2px;
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
    color: var(--g6);
    cursor: pointer;
    flex: none;
  }
  .row-btn:hover {
    background: var(--g2);
    color: var(--g7);
  }
  .row-btn.on {
    border-color: var(--g7);
    color: var(--g7);
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
    color: var(--g6);
    font-size: 14.5px;
  }
  .conv:hover {
    background: var(--g1);
  }
  .conv.active {
    background: var(--g2);
    color: var(--g7);
  }
  .conv-pin {
    width: 15px;
    height: 15px;
    color: var(--g5);
    flex: none;
  }
  .conv-check {
    position: relative;
    width: 16px;
    height: 16px;
    border: 1.5px solid var(--g4);
    border-radius: 4.5px;
    flex: none;
  }
  .conv-check svg {
    position: absolute;
    inset: 1px;
    width: calc(100% - 2px);
    height: calc(100% - 2px);
    color: var(--g1);
    opacity: 0;
  }
  .conv-check.on {
    background: var(--g7);
    border-color: var(--g7);
  }
  .conv-check.on svg {
    opacity: 1;
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
    color: var(--g5);
    border: 1px solid var(--g2);
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
    color: var(--g6);
    cursor: pointer;
    flex: none;
  }
  .conv-more:hover {
    background: var(--g2);
    color: var(--g7);
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
    color: var(--g5);
    flex: none;
  }
  .expand-more {
    display: block;
    width: 100%;
    text-align: left;
    background: none;
    border: none;
    color: var(--g5);
    font-size: 14px;
    padding: 10px 10px 10px 38px;
    cursor: pointer;
    border-radius: 10px;
  }
  .expand-more:hover {
    color: var(--g6);
    background: var(--g1);
  }
  .side-empty {
    color: var(--g4);
    font-size: 14px;
    padding: 18px 12px;
  }
  .side-menu {
    position: absolute;
    z-index: 40;
    min-width: 172px;
    background: var(--g1);
    border: 1px solid var(--g2);
    border-radius: 14px;
    padding: 6px;
    box-shadow: 0 12px 32px var(--overlay);
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
    color: var(--g7);
    font-size: 14px;
    padding: 10px 12px;
    border-radius: 9px;
    cursor: pointer;
    text-align: left;
    white-space: nowrap;
  }
  .side-menu button:hover {
    background: var(--g2);
  }
  .side-menu button svg {
    width: 17px;
    height: 17px;
    color: var(--g6);
    flex: none;
  }
  .side-menu .sel-check {
    margin-left: auto;
    color: var(--g7);
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
    background: var(--g1);
    border: 1px solid var(--g2);
    color: var(--g6);
    cursor: pointer;
  }
  .sidebar-restore:hover {
    color: var(--g7);
    background: var(--g2);
  }
  .sidebar-restore svg {
    width: 20px;
    height: 20px;
  }
  .side-foot {
    flex: none;
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    padding-top: 8px;
  }
  .foot-btn {
    flex: 1;
    background: transparent;
    border: 1px solid var(--g2);
    color: var(--g5);
    font-size: 13px;
    border-radius: 10px;
    padding: 8px 0;
    cursor: pointer;
  }
  .foot-btn:hover {
    color: var(--g6);
    background: var(--g1);
  }
  .galaxy-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 6px;
  }
  .galaxy-btn svg {
    width: 15px;
    height: 15px;
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
    color: var(--g4);
    font-size: 12px;
    padding: 3px 8px;
    border-radius: 7px;
    cursor: pointer;
  }
  .handoff-btn:hover {
    color: var(--g6);
    background: var(--g1);
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
    bottom: 130px; /* 悬浮输入框上方 */
    left: 50%;
    transform: translateX(-50%);
    z-index: 6;
    padding: 6px 14px;
    border-radius: 16px;
    border: 1px solid var(--g3);
    background: var(--g1);
    color: var(--tint-blue-fg);
    font-size: 12px;
    cursor: pointer;
    box-shadow: 0 4px 12px var(--overlay);
  }
  .jump-bottom:hover {
    border-color: var(--g4);
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
    background: var(--g1);
    border: 1px solid var(--g3);
    border-radius: 10px;
    padding: 12px 14px;
    box-shadow: 0 8px 24px var(--overlay);
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
    color: var(--tint-blue-fg);
  }
  .ctx-card-head span {
    color: var(--g6);
    font-size: 12px;
  }
  .ctx-bar {
    height: 8px;
    border-radius: 4px;
    background: var(--g2);
    overflow: hidden;
    margin-bottom: 12px;
  }
  .ctx-bar-fill {
    height: 100%;
    border-radius: 4px;
    background: var(--tint-blue-fg);
    transition: width 0.3s;
  }
  .ctx-row {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 0;
    color: var(--tint-blue-fg);
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
    color: var(--g6);
    font-variant-numeric: tabular-nums;
  }
  .ctx-card-foot {
    margin-top: 10px;
    padding-top: 8px;
    border-top: 1px solid var(--g2);
    color: var(--g5);
    font-size: 11px;
  }
  .cache-row {
    margin-top: 8px;
    padding-top: 8px;
    border-top: 1px solid var(--g2);
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
  .chat-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 12px;
    padding: 12px 20px;
    border-bottom: 1px solid var(--g2);
    background: var(--g1);
  }
  .head-main {
    min-width: 0;
  }
  .chat-title {
    font-size: 15px;
    font-weight: 650;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .head-side {
    display: flex;
    align-items: center;
    gap: 8px;
    flex: none;
  }
  .chat-main {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    position: relative;
  }
  .head-btn {
    width: 32px;
    height: 30px;
    display: flex;
    align-items: center;
    justify-content: center;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 8px;
    color: var(--g5);
    cursor: pointer;
    flex: none;
  }
  .head-btn svg {
    width: 19px;
    height: 19px;
  }
  .head-btn:hover {
    color: var(--g7);
    background: var(--g2);
  }
  .head-btn.active {
    color: var(--brand);
    border-color: var(--g3);
    background: var(--g2);
  }
  /* —— 终端面板：占据对话区底部，把聊天内容顶起 —— */
  .term-panel {
    flex: none;
    position: relative;
    display: flex;
    flex-direction: column;
    background: #101216;
    color: #d4d4d4;
    border-top: 1px solid var(--g3);
    font-family: Consolas, 'Cascadia Mono', monospace;
    font-size: 12.5px;
  }
  .term-resize {
    position: absolute;
    top: -3px;
    left: 0;
    right: 0;
    height: 7px;
    cursor: row-resize;
    z-index: 6;
  }
  .term-panel.resizing {
    border-top-color: var(--g5);
  }
  .term-head {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 12px;
    background: rgba(255, 255, 255, 0.04);
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
    color: #9aa0ae;
  }
  .term-title {
    font-weight: 650;
    color: #d4d4d4;
  }
  .term-hint {
    font-size: 11px;
    color: #6f7686;
  }
  .term-clear,
  .term-close {
    background: transparent;
    border: none;
    color: #9aa0ae;
    cursor: pointer;
    font-size: 12px;
    padding: 2px 6px;
    border-radius: 5px;
  }
  .term-clear:hover,
  .term-close:hover {
    background: rgba(255, 255, 255, 0.08);
    color: #fff;
  }
  .term-close {
    font-size: 16px;
    line-height: 1;
  }
  .term-out {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 8px 12px;
  }
  .term-line {
    white-space: pre-wrap;
    word-break: break-all;
    color: #d4d4d4;
  }
  .term-line.dim {
    color: #6f7686;
  }
  .term-block.err .term-text {
    color: #ff9b9b;
  }
  .term-prompt {
    color: #8fe3ae;
    margin-right: 8px;
  }
  .term-text {
    margin: 2px 0 6px;
    padding-left: 18px;
    white-space: pre-wrap;
    word-break: break-all;
    color: #c9cdd6;
    font-family: inherit;
  }
  .term-in {
    display: flex;
    align-items: center;
    padding: 6px 12px;
    border-top: 1px solid rgba(255, 255, 255, 0.08);
  }
  .term-in input {
    flex: 1;
    background: transparent;
    border: none;
    outline: none;
    color: #d4d4d4;
    font-family: inherit;
    font-size: 12.5px;
  }
  /* —— 右侧栏：产物 / 预览 —— */
  .right-panel {
    flex: none;
    position: relative;
    display: flex;
    flex-direction: column;
    background: var(--g0);
    border-left: 1px solid var(--g3);
    min-height: 0;
  }
  /* 左缘拖拽调宽手柄（对齐 ZCode 的自由尺寸） */
  .rp-resize {
    position: absolute;
    left: -3px;
    top: 0;
    bottom: 0;
    width: 7px;
    cursor: col-resize;
    z-index: 6;
  }
  .right-panel.resizing {
    border-left-color: var(--g5);
  }
  .rp-tabs {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 8px 10px;
    border-bottom: 1px solid var(--g3);
  }
  /* P17：常驻后台的扩展 iframe（扩展间通信要用；不显示但保持运行） */
  .ext-hidden {
    position: absolute;
    width: 1px;
    height: 1px;
    border: 0;
    opacity: 0;
    pointer-events: none;
    left: -9999px;
  }
  /* 021 组件总线：UI 扩展面板（iframe 插槽；与产物面板同构，但不共用滚动容器） */
  .ext-panel {
    width: 460px;
    flex: none;
    display: flex;
    flex-direction: column;
    background: var(--g0);
    border-left: 1px solid var(--g3);
    min-height: 0;
  }
  .ext-title {
    color: var(--g7);
    font-size: 13px;
    margin-right: 2px;
  }
  .ext-frame {
    flex: 1;
    min-height: 0;
    width: 100%;
    border: none;
    background: var(--g1);
  }
  /* P20.1 网页面板（内置浏览器：截图渲染 + 坐标回传，与 agent 工具共享实例） */
  .web-wrap {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
  }
  .web-error {
    color: var(--tint-red-fg, #ff6b6b);
    font-size: 12px;
    padding: 4px 2px 0;
  }
  .web-dialog {
    display: flex;
    align-items: center;
    gap: 8px;
    margin: 4px 0;
    padding: 6px 8px;
    border: 1px solid var(--g3);
    border-radius: 6px;
    background: var(--g1);
    font-size: 12px;
  }
  .web-dialog-msg {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 60%;
  }
  .web-tabbar {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 6px 8px;
    border-bottom: 1px solid var(--g3);
  }
  .web-tabs {
    display: flex;
    align-items: center;
    gap: 4px;
    overflow-x: auto;
    flex: 1;
    min-width: 0;
  }
  .web-tab {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    max-width: 150px;
    padding: 3px 6px;
    border: 1px solid var(--g3);
    border-radius: 7px;
    background: var(--g1);
    color: var(--g6);
    font-size: 12px;
    cursor: pointer;
    white-space: nowrap;
    flex: none;
  }
  .web-tab.active {
    background: var(--g3);
    color: var(--g7);
  }
  .web-tab-title {
    overflow: hidden;
    text-overflow: ellipsis;
    max-width: 96px;
  }
  .web-tab-agent {
    font-size: 10px;
    color: var(--tint-blue-fg, #6aa9ff);
    border: 1px solid currentColor;
    border-radius: 4px;
    padding: 0 3px;
    flex: none;
  }
  .web-tab-x {
    opacity: 0.55;
    padding: 0 2px;
  }
  .web-tab-x:hover {
    opacity: 1;
  }
  .web-add {
    flex: none;
    width: 22px;
    height: 22px;
    border: 1px solid var(--g3);
    border-radius: 7px;
    background: var(--g1);
    color: var(--g6);
    font-size: 15px;
    line-height: 1;
    cursor: pointer;
  }
  .web-add:hover {
    color: var(--g7);
  }
  .web-menu-backdrop {
    position: fixed;
    inset: 0;
    z-index: 30;
  }
  .web-menu {
    position: absolute;
    top: calc(100% + 2px);
    right: 8px;
    z-index: 31;
    background: var(--g1);
    border: 1px solid var(--g3);
    border-radius: 9px;
    padding: 4px;
    min-width: 180px;
    box-shadow: 0 8px 28px rgba(0, 0, 0, 0.35);
  }
  /* P20.4 原生托管：菜单向上弹。往下弹会落进 .web-body —— 那块被壳里的 child
     webview（HWND，永远在网页内容之上）盖住，菜单等于看不见。向上弹到页签/工具栏
     一带（纯网页内容，实测 71px 菜单 < 88px 可用空间），网页全程不用藏。 */
  .web-menu-up {
    top: auto;
    bottom: calc(100% + 2px);
  }
  .web-menu button {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 7px 10px;
    border: none;
    background: transparent;
    color: var(--g7);
    font-size: 12.5px;
    border-radius: 6px;
    cursor: pointer;
    text-align: left;
  }
  .web-menu button:hover {
    background: var(--g3);
  }
  .web-menu svg {
    width: 14px;
    height: 14px;
    flex: none;
    color: var(--g6);
  }
  .web-toolbar {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 6px 8px;
    border-bottom: 1px solid var(--g3);
    position: relative;
  }
  .web-nav {
    flex: none;
    width: 26px;
    height: 26px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border: none;
    background: transparent;
    color: var(--g6);
    border-radius: 7px;
    cursor: pointer;
  }
  .web-nav:hover:not(:disabled) {
    background: var(--g2);
    color: var(--g7);
  }
  .web-nav:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .web-nav svg {
    width: 15px;
    height: 15px;
  }
  .web-address {
    flex: 1;
    min-width: 0;
    padding: 5px 10px;
    border: 1px solid var(--g3);
    border-radius: 8px;
    background: var(--g1);
    color: var(--g7);
    font-size: 12.5px;
    outline: none;
  }
  .web-address:focus {
    border-color: var(--g5);
  }
  .web-body {
    flex: 1;
    min-height: 0;
    position: relative;
    overflow: auto;
    background: var(--g1);
    display: flex;
    align-items: flex-start;
    justify-content: center;
  }
  .web-empty {
    margin: auto;
    text-align: center;
    color: var(--g5);
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    padding: 40px 10px;
  }
  .web-globe {
    width: 52px;
    height: 52px;
    opacity: 0.5;
  }
  .web-empty-title {
    font-size: 14px;
    color: var(--g6);
  }
  .web-empty-sub {
    font-size: 12px;
  }
  .web-shot {
    width: 100%;
    height: auto;
    display: block;
    cursor: pointer;
    user-select: none;
  }
  /* P22：壳里 child webview 的占位区（原生 HWND 盖在这块上面，这里只是撑出尺寸） */
  .web-native {
    position: absolute;
    inset: 0;
    cursor: default;
  }
  .web-native-loading {
    color: var(--g-accent, #6cf);
    font-size: 9px;
    line-height: 1;
    animation: web-pulse 1s ease-in-out infinite;
  }
  @keyframes web-pulse {
    0%, 100% { opacity: 0.25; }
    50% { opacity: 1; }
  }
  .web-shot.busy {
    opacity: 0.6;
  }
  .web-loading {
    position: absolute;
    top: 8px;
    right: 10px;
    font-size: 11px;
    color: var(--g6);
    background: var(--g2);
    border: 1px solid var(--g3);
    border-radius: 6px;
    padding: 2px 8px;
    z-index: 2;
  }
  .ext-panel .rp-tabs {
    flex-wrap: nowrap;
  }
  /* 沙箱自检结果行（插件分区） */
  .probe-list {
    margin: 6px 0 2px;
  }
  .probe-row {
    display: flex;
    align-items: baseline;
    gap: 8px;
    padding: 3px 0;
    font-size: 12px;
    color: var(--g6);
  }
  .probe-row code {
    color: var(--g7);
    flex: none;
    min-width: 110px;
  }
  .probe-detail {
    color: var(--g5);
    word-break: break-all;
  }
  .side-version {
    flex: 0 0 100%;
    text-align: center;
    font-size: 11px;
    color: var(--g4);
    letter-spacing: 0.3px;
    padding-top: 2px;
  }
  .rp-tabs button {
    padding: 5px 14px;
    border-radius: 7px;
    border: none;
    background: transparent;
    color: var(--g5);
    font-size: 13px;
    cursor: pointer;
  }
  .rp-tabs button:hover {
    color: var(--g7);
  }
  .rp-tabs button.active {
    background: var(--g2);
    color: var(--g7);
    font-weight: 600;
  }
  .rp-refresh {
    background: transparent;
    border: none;
    color: var(--g5);
    font-size: 15px;
    cursor: pointer;
    padding: 2px 8px;
    border-radius: 5px;
  }
  .rp-refresh:hover {
    color: var(--g7);
    background: var(--g2);
  }
  .rp-body {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 8px;
    display: flex;
    flex-direction: column;
  }
  .rp-empty {
    margin: auto;
    text-align: center;
    color: var(--g5);
    font-size: 12.5px;
    line-height: 1.7;
    padding: 0 16px;
  }
  .rp-file {
    text-align: left;
    padding: 7px 10px;
    border: none;
    border-radius: 6px;
    background: transparent;
    color: var(--g6);
    font-family: Consolas, monospace;
    font-size: 12.5px;
    cursor: pointer;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .rp-file:hover {
    background: var(--g2);
    color: var(--g7);
  }
  .rp-file.sel {
    background: var(--g2);
    color: var(--brand);
  }
  /* 路径标签吸顶：代码滚动时它不跟着滚，也不会被裁掉半个字 */
  .rp-preview-path {
    position: sticky;
    top: -8px; /* 抵消 .rp-body 的 8px padding，贴住面板上沿 */
    z-index: 2;
    flex: none;
    margin: -8px -8px 6px;
    padding: 8px 10px;
    background: var(--g1);
    border-bottom: 1px solid var(--g2);
    color: var(--g5);
    font-family: Consolas, monospace;
    font-size: 11.5px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .rp-preview {
    margin: 0;
    padding: 8px;
    background: var(--g1);
    border: 1px solid var(--g3);
    border-radius: 8px;
    color: var(--g6);
    font-family: Consolas, monospace;
    font-size: 12px;
    /* 代码按词换行，不从单词中间劈开（原来 break-all 会把 SECURITY 拆成两行） */
    white-space: pre-wrap;
    word-break: normal;
    overflow-wrap: break-word;
    tab-size: 4;
  }
  /* —— 界面微调模式 —— */
  :global(body.micro-edit-mode *:hover) {
    outline: 1px dashed color-mix(in srgb, var(--brand) 55%, transparent);
    outline-offset: -1px;
  }
  .micro-panel {
    position: fixed;
    z-index: 200; /* 盖过设置弹窗（z-20）等一切浮层 */
    width: 270px;
    box-sizing: border-box; /* max-height 必须把 padding/border 一起算进去，否则底部会溢出视口 */
    background: var(--g2);
    border: 1px solid var(--brand);
    border-radius: 10px;
    padding: 10px;
    box-shadow: 0 8px 24px var(--overlay);
    font-size: 12px;
    /* 纵向弹性布局：头部与底部按钮固定，中间属性区滚动——
       面板比视口高时下面的「应用/重置/取消」也始终可见可点 */
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .micro-head {
    flex: none;
  }
  .micro-body {
    flex: 1 1 auto;
    min-height: 0;
    overflow-y: auto;
    overscroll-behavior: contain;
    scrollbar-width: thin;
    scrollbar-color: var(--g4) transparent;
    padding-right: 2px;
  }
  .micro-foot {
    flex: none;
    padding-top: 8px;
    margin-top: 4px;
    border-top: 1px solid var(--g3);
  }
  .micro-foot .micro-actions {
    margin-top: 0;
  }
  .micro-name {
    font-weight: 650;
    color: var(--g7);
    margin-bottom: 4px;
  }
  .micro-sel {
    font-family: Consolas, monospace;
    font-size: 10.5px;
    color: var(--g5);
    margin-bottom: 8px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .micro-row {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 6px;
  }
  .micro-row label {
    width: 52px;
    flex: none;
    color: var(--g5);
  }
  .micro-row input[type='color'] {
    width: 34px;
    height: 24px;
    padding: 0;
    border: 1px solid var(--g3);
    border-radius: 5px;
    background: transparent;
    cursor: pointer;
  }
  .micro-row input[type='range'] {
    flex: 1;
    accent-color: var(--brand);
  }
  .micro-row input[type='number'] {
    width: 64px;
    background: var(--g1);
    color: var(--g7);
    border: 1px solid var(--g3);
    border-radius: 5px;
    padding: 3px 6px;
  }
  .micro-val {
    color: var(--g6);
    font-family: Consolas, monospace;
    font-size: 11px;
    min-width: 52px;
  }
  .micro-actions {
    display: flex;
    gap: 8px;
    margin-top: 8px;
  }
  .micro-hint {
    margin-top: 6px;
    color: var(--g5);
    font-size: 11px;
    line-height: 1.5;
  }
  .micro-warn {
    margin: 6px 0;
    padding: 6px 8px;
    border-radius: 6px;
    background: var(--tint-yellow-bg);
    color: var(--tint-yellow-fg);
    font-size: 11px;
    line-height: 1.5;
  }
  .scope-opts {
    display: flex;
    gap: 4px;
    flex: 1;
  }
  .scope-opt {
    flex: 1;
    padding: 4px 6px;
    border-radius: 7px;
    border: 1px solid var(--g3);
    background: var(--g1);
    color: var(--g6);
    font-size: 11.5px;
    cursor: pointer;
  }
  .scope-opt:hover {
    border-color: var(--g4);
    color: var(--g7);
  }
  .scope-opt.active {
    background: var(--brand);
    border-color: var(--brand);
    color: var(--brand-ink);
  }
  .chat-body {
    flex: 1;
    overflow-y: auto;
    padding: 20px 0 170px; /* 底部留出悬浮输入框 + 改动条的高度，最后一条消息不被遮挡 */
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  .empty-hint {
    margin: auto;
    color: var(--g4);
    font-size: 13px;
    text-align: center;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .empty-title {
    font-size: 17px;
    font-weight: 600;
    color: var(--g6);
  }
  .empty-sub {
    font-size: 12.5px;
    color: var(--g5);
  }
  /* —— 成熟化图标体系：单色线性，尺寸统一 —— */
  .nav-ico {
    width: 16px;
    height: 16px;
    flex: none;
  }
  .settings-nav button {
    display: flex;
    align-items: center;
    gap: 9px;
  }
  .att-ico {
    width: 15px;
    height: 15px;
    flex: none;
    color: var(--g6);
  }
  .msg-att-file .att-ico {
    vertical-align: -2px;
    margin-right: 2px;
  }
  .att-ico-lg {
    width: 17px;
    height: 17px;
    display: block;
  }

  .att-del svg {
    width: 13px;
    height: 13px;
    display: block;
  }
  .caret-ico {
    width: 13px;
    height: 13px;
    flex: none;
    color: var(--g5);
  }
  .send-btn svg {
    width: 16px;
    height: 16px;
    display: block;
    margin: auto;
  }
  .approval-kicker {
    display: inline-block;
    font-size: 11px;
    font-weight: 600;
    color: var(--g6);
    border: 1px solid var(--g3);
    border-radius: 4px;
    padding: 1px 6px;
    margin-right: 6px;
    vertical-align: 1px;
  }
  .typing-dot {
    display: inline-block;
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--g5);
    margin-right: 4px;
    animation: typing-bounce 1.2s ease-in-out infinite;
  }
  .typing-dot:nth-child(2) {
    animation-delay: 0.15s;
  }
  .typing-dot:nth-child(3) {
    animation-delay: 0.3s;
    margin-right: 0;
  }
  @keyframes typing-bounce {
    0%,
    60%,
    100% {
      transform: translateY(0);
      opacity: 0.5;
    }
    30% {
      transform: translateY(-4px);
      opacity: 1;
    }
  }
  .cursor {
    display: inline-block;
    width: 2px;
    height: 1em;
    background: var(--g6);
    vertical-align: -2px;
    animation: blink 1s step-start infinite;
  }
  .msg {
    display: flex;
    width: 100%;
    max-width: 880px;
    margin: 0 auto;
    padding: 0 24px;
    box-sizing: border-box;
  }
  .msg.user {
    justify-content: flex-end;
  }
  /* 排队中的消息（027）：气泡里带「立即发送」按钮——并进正在跑的那一轮，引导它的回答 */
  .queued-row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    margin-top: 6px;
    padding-top: 6px;
    border-top: 1px dashed var(--line, rgba(128, 128, 128, 0.35));
  }
  .queued-badge {
    font-size: 12px;
    opacity: 0.75;
  }
  .queued-badge.steered {
    color: var(--accent, #4a9);
  }
  .queued-send {
    font-size: 12px;
    padding: 2px 10px;
    border-radius: 999px;
    border: 1px solid var(--accent, #4a9);
    color: var(--accent, #4a9);
    background: transparent;
    cursor: pointer;
  }
  .queued-send:hover {
    background: color-mix(in srgb, var(--accent, #4a9) 14%, transparent);
  }
  .msg.assistant {
    justify-content: flex-start;
  }
  .msg.system {
    justify-content: center;
  }
  /* pre-wrap 只放在真正承载文本的叶子节点上；放 .bubble 上会把模板标签间的
     换行缩进也渲染成一个空行（用户气泡开头凭空多一行） */
  .bubble {
    max-width: 78%;
    padding: 10px 14px;
    border-radius: 12px;
    font-size: 14px;
    line-height: 1.65;
    word-break: break-word;
  }
  .user-text,
  .content {
    white-space: pre-wrap;
  }
  .user-bubble {
    background: var(--brand);
    color: var(--brand-ink);
    border-bottom-right-radius: 4px;
  }
  .asst-bubble {
    background: var(--g1);
    border: 1px solid var(--g2);
    border-bottom-left-radius: 4px;
    max-width: 100%;
    width: 100%;
  }
  .msg.assistant .bubble {
    max-width: 100%;
  }
  .error-bubble {
    background: var(--tint-red-bg);
    border: 1px solid var(--tint-red-bg);
    color: var(--tint-red-fg);
    white-space: pre-wrap;
  }
  .sys-text {
    font-size: 12px;
    color: var(--g5);
  }
  .thinking {
    margin-bottom: 8px;
  }
  .thinking summary {
    cursor: pointer;
    font-size: 12px;
    color: var(--g6);
    user-select: none;
  }
  .thinking-body {
    margin-top: 6px;
    padding: 8px;
    background: var(--g1);
    border-left: 2px solid var(--g3);
    font-size: 13px;
    color: var(--g6);
    white-space: pre-wrap;
  }
  .tool-card {
    margin-bottom: 8px;
    background: var(--g1);
    border: 1px solid var(--g2);
    border-left: 2px solid var(--g4);
    border-radius: 10px;
    padding: 8px 10px;
  }
  .approval-box {
    margin-bottom: 8px;
    background: var(--g1);
    border: 1px solid var(--g3);
    border-left: 2px solid var(--tint-yellow-fg);
    border-radius: 10px;
    padding: 10px 12px;
    font-size: 13px;
  }
  .approval-box.danger {
    border-left-color: var(--tint-red-fg);
  }
  .approval-head {
    display: flex;
    gap: 8px;
    align-items: center;
    flex-wrap: wrap;
    color: var(--g7);
  }
  .approval-reason {
    display: inline-block;
    padding: 1px 6px;
    border-radius: 4px;
    background: var(--g2);
    color: var(--g6);
    font-size: 11px;
  }
  .approval-reason.danger {
    background: var(--tint-red-bg);
    color: var(--tint-red-fg);
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
    background: var(--tint-green-bg);
    color: var(--tint-green-fg);
  }
  .approval-risk.risk-1 {
    background: var(--tint-blue-bg);
    color: var(--tint-blue-fg);
  }
  .approval-risk.risk-2 {
    background: var(--tint-red-bg);
    color: var(--tint-red-fg);
  }
  .approval-actions {
    display: flex;
    gap: 8px;
    margin-top: 8px;
  }
  .approval-state {
    margin-top: 6px;
    color: var(--g6);
    font-size: 12px;
  }
  .approval-hint {
    color: var(--g5);
    font-size: 11px;
    align-self: center;
  }

  /* 待发送附件条 */
  .composer-attachments {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
    padding: 8px 12px 4px;
    border-bottom: 1px solid var(--g2);
  }
  .att-chip {
    display: flex;
    align-items: center;
    gap: 6px;
    background: var(--g2);
    border: 1px solid var(--g3);
    border-radius: 6px;
    padding: 3px 8px;
    font-size: 12px;
    color: var(--tint-blue-fg);
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
    color: var(--g5);
  }
  .att-del {
    background: transparent;
    border: none;
    color: var(--g6);
    cursor: pointer;
    font-size: 11px;
    padding: 0 2px;
    line-height: 1;
  }
  .att-del:hover {
    color: var(--tint-red-fg);
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
    border: 1px solid var(--veil);
    cursor: zoom-in;
    object-fit: contain;
    background: var(--g0);
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
    color: var(--tint-blue-fg);
    padding: 4px 8px;
    background: var(--veil);
    border-radius: 4px;
  }

  /* 图片预览模态框 */
  .cp-dock {
    display: flex;
    justify-content: center;
    margin-bottom: 8px;
    position: relative; /* 「待提交」明细气泡的定位基准 */
  }
  .cp-dock .cp-bar {
    width: 100%;
    max-width: 880px;
  }
  .cp-bar {
    display: flex;
    align-items: center;
    gap: 10px;
    background: var(--g1);
    border: 1px solid var(--g2);
    border-radius: 8px;
    padding: 8px 12px;
    font-size: 13px;
    color: var(--g6);
    max-width: 720px;
  }
  .cp-sum b {
    color: var(--tint-blue-fg);
  }
  .cp-add {
    color: var(--tint-green-fg);
    font-family: monospace;
    font-size: 12px;
  }
  .cp-del {
    color: var(--tint-red-fg);
    font-family: monospace;
    font-size: 12px;
  }
  /* 右侧动作组：撤销 / 提交（margin-left:auto 只加在容器上，两个按钮才不会各分一半空白） */
  .cp-actions {
    margin-left: auto;
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .cp-undo {
    background: transparent;
    border: none;
    color: var(--g6);
    font-size: 12px;
    cursor: pointer;
    padding: 2px 4px;
    font-family: inherit;
  }
  .cp-undo:hover {
    color: var(--g7);
  }
  .cp-commit {
    background: var(--g3);
    border: 1px solid var(--g5);
    color: var(--g7);
    font-size: 12px;
    font-weight: 650;
    cursor: pointer;
    padding: 3px 10px;
    border-radius: 6px;
    font-family: inherit;
  }
  .cp-commit:hover {
    background: var(--g4);
    border-color: var(--g6);
  }
  /* 提交弹窗 */
  .commit-modal {
    width: 560px;
  }
  .commit-body {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin-top: 10px;
  }
  .commit-files {
    border: 1px solid var(--g2);
    border-radius: 8px;
    overflow: hidden;
  }
  .commit-files-head {
    display: flex;
    align-items: baseline;
    gap: 8px;
    flex-wrap: wrap;
    padding: 8px 10px;
    font-size: 12px;
    color: var(--g6);
    border-bottom: 1px solid var(--g2);
  }
  .commit-files-list {
    max-height: 240px;
    overflow-y: auto;
    padding: 4px;
  }
  .commit-file {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 8px;
    border-radius: 6px;
    font-size: 12px;
    color: var(--g6);
    font-family: Consolas, monospace;
    cursor: pointer;
  }
  .commit-file:hover {
    background: var(--g2);
    color: var(--g7);
  }
  .commit-file input {
    flex: none;
    cursor: pointer;
    accent-color: var(--g5);
  }
  .commit-msg {
    width: 100%;
    box-sizing: border-box;
    resize: vertical;
  }
  .commit-err {
    font-size: 12px;
    color: var(--tint-red-fg);
    white-space: pre-wrap;
    word-break: break-word;
  }
  /* —— git 概览（分支 / 领先落后 / 未提交数），钉在改动条左侧 —— */
  .git-ref {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    color: var(--g7);
    font-size: 12px;
    max-width: 260px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .git-ref b {
    font-weight: 600;
  }
  .git-ico {
    width: 13px;
    height: 13px;
    flex: 0 0 auto;
    color: var(--g5);
  }
  .git-head {
    color: var(--g5);
    font-family: monospace;
    font-size: 11px;
  }
  .git-ab {
    color: var(--g6);
    font-family: monospace;
    font-size: 11px;
  }
  /* 「N 处未提交」可点开：待提交文件明细 */  .git-dirty {
    color: var(--tint-yellow-fg);
    font-size: 11.5px;
    background: transparent;
    border: none;
    padding: 2px 4px;
    border-radius: 5px;
    cursor: pointer;
    font-family: inherit;
  }
  .git-dirty:hover,
  .git-dirty.on {
    background: var(--g2);
  }
  .git-pop {
    position: absolute;
    left: 0;
    right: 0;
    bottom: calc(100% + 6px);
    margin: 0 auto;
    max-width: 880px;
    background: var(--g1);
    border: 1px solid var(--g3);
    border-radius: 10px;
    box-shadow: 0 10px 30px var(--overlay);
    z-index: 30;
    overflow: hidden;
  }
  .git-pop-head {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 8px 12px;
    font-size: 12px;
    color: var(--g6);
    border-bottom: 1px solid var(--g2);
  }
  .git-pop-head b {
    color: var(--g7);
  }
  .git-pop-hint {
    margin-left: auto;
    color: var(--g5);
    font-size: 11px;
  }
  .git-pop-list {
    max-height: 240px;
    overflow-y: auto;
    padding: 4px;
  }
  .git-pop-row {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    text-align: left;
    background: transparent;
    border: none;
    color: var(--g6);
    font-size: 12px;
    padding: 5px 8px;
    border-radius: 6px;
    cursor: pointer;
    font-family: Consolas, monospace;
  }
  .git-pop-row:hover {
    background: var(--g2);
    color: var(--g7);
  }
  .git-st {
    flex: 0 0 auto;
    min-width: 42px;
    text-align: center;
    font-size: 10.5px;
    font-family: sans-serif;
    color: var(--g5);
    border: 1px solid var(--g3);
    border-radius: 4px;
    padding: 0 4px;
    line-height: 16px;
  }
  .git-path {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .git-pop-more {
    padding: 6px 10px;
    font-size: 11px;
    color: var(--g5);
  }
  .cp-bar .cp-sum {
    margin-left: 4px;
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
    background: var(--g3);
    color: var(--tint-blue-fg);
  }
  .tool-badge.ok {
    background: var(--tint-green-bg);
    color: var(--tint-green-fg);
  }
  .tool-badge.fail {
    background: var(--tint-red-bg);
    color: var(--tint-red-fg);
  }
  .tool-name {
    font-family: Consolas, monospace;
    color: var(--tint-blue-fg);
  }
  .tool-args {
    color: var(--g5);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 320px;
  }
  .tool-preview {
    margin: 6px 0 0;
    padding: 6px 8px;
    background: var(--g1);
    border-radius: 6px;
    font-size: 12px;
    color: var(--g6);
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
    color: var(--g6);
  }
  /* —— 输入区：悬浮在对话区底部（无分隔线，消息从其下方穿过）—— */
  .chat-input {
    position: absolute;
    bottom: 0;
    left: 0;
    right: 0;
    z-index: 5;
    padding: 12px 0 16px;
    background: transparent;
    pointer-events: none; /* 空白区域点击穿透到底下的消息 */
  }
  .chat-input > * {
    pointer-events: auto;
    width: 100%;
    max-width: 880px;
    margin-left: auto;
    margin-right: auto;
    padding-left: 24px;
    padding-right: 24px;
    box-sizing: border-box;
  }
  .composer {
    background: var(--g2);
    border: 1px solid var(--g3);
    border-radius: 16px;
    padding: 6px 12px 10px;
    transition: border-color 0.15s, box-shadow 0.15s;
    box-shadow: 0 4px 16px var(--overlay);
  }
  .composer:focus-within {
    border-color: var(--g4);
    box-shadow: 0 6px 20px var(--overlay);
  }
  .composer-input {
    display: block;
    width: 100%;
    box-sizing: border-box;
    resize: none;
    border: none;
    outline: none;
    background: transparent;
    color: var(--g7);
    font-size: 14px;
    line-height: 1.5;
    padding: 8px 6px 6px;
    max-height: 160px;
    font-family: inherit;
  }
  .composer-input::placeholder {
    color: var(--g5);
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
    color: var(--g6);
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
  }
  .icon-btn:hover {
    background: var(--g2);
    color: var(--g7);
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
    color: var(--g6);
    font-size: 12.5px;
    cursor: pointer;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .pill:hover,
  .pill.active {
    background: var(--g2);
    color: var(--g7);
  }
  .pill.danger {
    color: var(--tint-red-fg);
    border: 1px solid var(--tint-red-bg);
  }
  /* 完全访问档悬停不再垫红底：沿用 .pill:hover 的灰色高亮，红字红边只作静态警示 */
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
    background: var(--g2);
    border: 1px solid var(--g3);
    border-radius: 10px;
    padding: 4px;
    z-index: 40;
    box-shadow: 0 8px 24px var(--overlay);
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
    color: var(--g7);
    font-size: 13px;
    cursor: pointer;
    white-space: nowrap;
  }
  .bar-item:hover {
    background: var(--g2);
  }
  /* 模型快选：配置分组头 + 子项 */
  .bar-item.group-head {
    font-weight: 650;
    color: var(--g6);
    align-items: center;
  }
  .bar-item.group-head.active {
    color: var(--g7);
  }
  .bar-item.group-head .caret-ico {
    width: 13px;
    height: 13px;
    flex: none;
    transition: transform 0.15s;
  }
  .bar-item.group-head .caret-ico.open {
    transform: rotate(90deg);
  }
  .bar-item.sub {
    padding-left: 24px;
  }
  .menu-sym {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 20px;
    height: 20px;
    flex: none;
    border-radius: 5px;
    background: var(--g3);
    font-size: 12px;
    font-weight: 650;
    margin-right: 8px;
  }
  .ctx-modal {
    width: 620px;
    height: 70vh;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
  .ctx-search {
    background: var(--g1);
    color: var(--g7);
    border: 1px solid var(--g3);
    border-radius: 8px;
    padding: 8px 10px;
    font-size: 13px;
    outline: none;
  }
  .ctx-search:focus {
    border-color: var(--brand);
  }
  .ctx-file-list {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .ctx-file {
    text-align: left;
    padding: 6px 10px;
    border: none;
    border-radius: 6px;
    background: transparent;
    color: var(--g6);
    font-family: Consolas, monospace;
    font-size: 12.5px;
    cursor: pointer;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ctx-file:hover {
    background: var(--g2);
    color: var(--g7);
  }
  .bar-item.active {
    background: var(--g3);
    color: var(--g7);
  }
  /* 上下文圆环：空色轨道 + 实色占用弧 */
  .ctx-ring .ctx-track {
    fill: none;
    stroke: var(--g3);
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
    color: var(--g6);
    font-variant-numeric: tabular-nums;
  }
  .ctx-pct.warn {
    color: var(--tint-yellow-fg);
  }
  .ctx-pct.crit {
    color: var(--tint-red-fg);
  }
  .send-btn {
    width: 34px;
    height: 34px;
    border-radius: 10px;
    border: none;
    background: var(--brand);
    color: var(--brand-ink);
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    flex-shrink: 0;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .send-btn:hover:not(:disabled) {
    background: var(--brand-hover);
  }
  .send-btn:disabled {
    background: var(--g3);
    color: var(--g5);
    cursor: default;
  }
  .send-btn.stop {
    background: var(--tint-red-bg);
    color: var(--tint-red-fg);
  }
  .send-btn.stop:hover {
    background: var(--tint-red-bg);
    filter: brightness(1.12);
  }
  /* —— P8.2 工作区切换 —— */
  .menu-sep {
    height: 1px;
    background: var(--g2);
    margin: 4px 2px;
  }
  .menu-empty {
    color: var(--g5);
    font-size: 12px;
    padding: 8px 10px;
  }
  /* P10.5 设置分区（左导航 + 内容区） */
  .settings-modal {
    width: 880px;
    max-width: 94vw;
    /* 固定高度：所有设置页统一窗口大小（以最高的「模型配置」页为准），短页留白，内容区自行滚动 */
    height: 85vh;
  }
  .settings-body {
    display: flex;
    gap: 14px;
    flex: 1;
    min-height: 0;
  }
  .settings-nav {
    display: flex;
    flex-direction: column;
    gap: 6px;
    width: 150px;
    flex-shrink: 0;
  }
  .settings-nav button {
    text-align: left;
    padding: 9px 12px;
    border-radius: 8px;
    background: transparent;
    border: none;
    color: var(--g6);
    cursor: pointer;
    font-size: 13px;
  }
  .settings-nav button:hover {
    background: var(--g2);
    color: var(--g7);
  }
  .settings-nav button.active {
    background: var(--g3);
    color: var(--g7);
    font-weight: 600;
  }
  .settings-content {
    flex: 1;
    overflow-y: auto;
    padding-right: 4px;
    min-width: 0;
    /* 各分区之间留白，避免标题/说明/控件挤成一团 */
    display: flex;
    flex-direction: column;
    gap: 20px;
  }
  .theme-opts {
    display: flex;
    gap: 8px;
    flex-wrap: wrap;
    align-items: center;
  }
  .theme-opt {
    padding: 7px 16px;
    border-radius: 8px;
    border: 1px solid var(--g3);
    background: var(--g2);
    color: var(--g7);
    font-size: 13px;
    cursor: pointer;
  }
  .theme-opt:hover {
    border-color: var(--g4);
  }
  .theme-opt.active {
    background: var(--g3);
    border-color: var(--g5);
    color: var(--g7);
    font-weight: 650;
  }
  .wp-row {
    display: flex;
    align-items: center;
    gap: 10px;
    flex-wrap: wrap;
  }
  .wp-pick input[type='file'] {
    display: none;
  }
  .css-rules-file {
    display: none;
  }
  .wp-opacity {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    font-size: 12.5px;
    color: var(--g6);
  }
  .wp-opacity b {
    color: var(--g7);
    font-variant-numeric: tabular-nums;
    min-width: 26px;
    text-align: right;
  }
  .wp-opacity input[type='range'] {
    width: 180px;
    accent-color: var(--g5);
  }
  .mem-stats {
    font-size: 12px;
    color: var(--g6);
    display: flex;
    flex-direction: column;
    gap: 7px;
  }
  .mem-stats code {
    font-size: 11px;
    color: var(--g5);
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
  .agents-editor.micro-readonly {
    background: var(--g0);
    color: var(--g5);
    resize: vertical;
  }
  /* P11 MCP 插件分区 */
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
  .modal-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 10px;
  }
  /* 标题 + 刷新等操作靠左成组（刷新按钮原来被 space-between 挤到中间） */
  .mh-left {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
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
  .field > :global(.csel-wrap) {
    width: 100%;
  }
  .sub-row > :global(.csel-wrap) {
    flex: 1 1 auto;
    width: auto;
  }
  .role-item :global(.csel) {
    font-size: 12px;
    padding: 6px 8px;
  }
</style>
