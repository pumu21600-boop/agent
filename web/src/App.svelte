<script>
  import { onMount, afterUpdate, onDestroy, tick } from 'svelte';
  import Select from './Select.svelte';
  import Galaxy from './Galaxy.svelte';
  import ImagePreview from './lib/ImagePreview.svelte';
  import RenameModal from './lib/RenameModal.svelte';
  import ConfirmModal from './lib/ConfirmModal.svelte';
  import WorkspaceModal from './lib/WorkspaceModal.svelte';
  import ModelPicker from './lib/ModelPicker.svelte';
  import MemoryTab from './lib/MemoryTab.svelte';
  import SkillsTab from './lib/SkillsTab.svelte';
  import McpTab from './lib/McpTab.svelte';
  import ScheduleTab from './lib/ScheduleTab.svelte';
  import { fmtBytes, relTime, fmtTok, norm, configLabel, gitStatusLabel, normPath, relShort, contentText, webNormalizeUrl, riskLabelFor } from './lib/format.js';
  import { parseCssColor, rgbToHex, rgbAlpha, hexWithAlpha, structuralSelector, pathOf, findByPath, makeUid, ELEMENT_ALIASES, escalateCss, microCss } from './lib/dom.js';

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
  // 上下文占用（P5 圆环+明细卡）。口径对齐 DSH token-meter：三个数语义分开——
  // projected=下一次请求的预估占用（圆环分子，实报锚点+带符号增量）、
  // pressure=最近一次渠道实报的 prompt（锚点，没拿到时为 null）、window=模型窗口（圆环分母）
  let ctxUsage = { projected: 0, pressure: null, window: 0, breakdown: null };
  let ctxLen = 0; // 当前模型上下文长度（0=默认）
  let maxOut = 0; // 当前模型最大输出长度（0=默认）
  let temperature = null; // 采样温度（null=渠道默认）
  let topP = null; // 核采样（null=渠道默认）
  let thinking = ''; // 思考开关：''=渠道默认 / on / off
  let thinkingStyle = 'auto'; // 生效的思考参数形态（P9，OpenClaw thinkingFormat 词表）
  let capabilities = { tools: true, vision: false, video: false, audio: false }; // P8.5 模型能力标记（纯手动勾选）
  let roles = {}; // P8.5 角色路由映射
  // 内置功能开关（插件页「内置」组功能行；后端 settings.features，缺省全开）
  let features = { roles: true, css_tool: true, galaxy: true };
  let sandboxCpu = 80; // 沙箱整树 CPU 硬上限（单核 %；settings.sandbox_cpu_percent，缺省 80）
  // —— P8.2 工作区 ——
  let workspace = '';
  let recentWorkspaces = [];
  let wsModalOpen = false;
  let wsMsg = '';
  let apiTotals = { prompt: 0, cached: 0 }; // 渠道真实用量累计（平均缓存命中率）
  // 027：用量按会话记账 —— key → { ctx, api }。后台会话的流写各自账本，
  // 切回会话时从账本恢复（跑完的会话不再发事件，不记账就会显示 0）。
  let usageBySession = {};
  let serverUsage = {}; // 服务端会话文件里的用量快照（重启/重开后恢复圆环；打开会话时刷新）

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
  // 服务端接管排队消息后的对账定时器（key → interval id）：本页流已死拿不到回答，
  // 轮询到服务端空闲后从会话文件重载（2026-09-27 教训，见 d.queued 分支注释）
  const reconcileTimers = {};
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

  async function addAttachmentFile(file) {
    if (file.size > 25 * 1024 * 1024) {
      await confirmAsk(`文件「${file.name}」超过 25MB 上限`, true);
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

  // 无边框壳窗口控制 API（壳注入；浏览器客户端没有 → 隐藏窗口按钮/热区）
  let winApi = null;
  let winMax = false; // 窗口是否最大化（按钮 ▢ ↔ 还原图标）
  function adoptWinApi() {
    if (!winApi && window.agentWinApi) winApi = window.agentWinApi;
    refreshWinMax();
  }
  function refreshWinMax() {
    if (winApi && winApi.isMaximized) {
      winApi.isMaximized().then((v) => { winMax = !!v; }).catch(() => {});
    }
  }
  onMount(() => {
    adoptWinApi();
    window.addEventListener('agentwinready', adoptWinApi);
    // 兜底轮询：注入事件若在监听注册前已发过（首屏加载竞态），靠它接住
    const t = setInterval(() => {
      adoptWinApi();
      if (winApi) clearInterval(t);
    }, 500);
    // 最大化态跟随：系统级 Win+↑ / 双击标题栏 / 壳外还原都会 resize，节流 300ms 查一次
    let rzT = null;
    const onRz = () => { clearTimeout(rzT); rzT = setTimeout(refreshWinMax, 300); };
    window.addEventListener('resize', onRz);
    return () => { clearInterval(t); clearTimeout(rzT); window.removeEventListener('resize', onRz); };
  });

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
    // 面板上次就是开着且停在「网页」时，补一次浏览器状态加载：openRightPanel 只在用户
    // 点工具栏开关时跑，刷新页面这条路不走它——不补的话 webActiveId 一直是空的，面板
    // 显示空态引导、壳里 webview 也不显示（用户刷新后看到的「网页没了」）
    if (rightPanelOpen && rpTab === 'web') loadWebState();
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
    try {
      const r = await fetch('/api/workspace', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ path }),
      });
      const d = await r.json();
      if (!r.ok || d.error) throw new Error(d.error || `HTTP ${r.status}`);
      wsModalOpen = false;
      // 工作区变了：会话/消息/圆环全部重置，重新拉列表（各会话缓冲与排队一并清掉）
      currentSessionId = null;
      // 在跑的流一并断开登记（服务端那轮继续跑——SSE 断 ≠ 取消，坑 26）。不清的话
      // 切回来 loadSession 走「本地缓冲」分支，拿到的是刚被清空的缓冲，历史全不见
      // （2026-09-27 用户实锤「切界面丢对话内容」）；断开后对账轮询会把跑完的内容补回来。
      const deadStreams = Object.values(streams);
      streams = {};
      streaming = false;
      controller = null;
      for (const st of deadStreams) {
        try { st.controller.abort(); } catch {}
      }
      sessionMsgs = {};
      outbox = {};
      usageBySession = {}; // 旧项目的会话账本一并清
      syncCur();
      restoreUsage('@new');
      await loadSettings();
      await loadSessions(true);
    } catch (e) {
      // wsMsg 只渲染在 WorkspaceModal 里，而那个弹窗没有任何置 true 的入口（死代码）——
      // 切换失败一直是静默的。改应用内提示弹窗，壳内外都可见。
      await confirmAsk(`切换项目失败：${e.message}`, true);
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
  // 分子 = projected（下一次请求的预估占用），分母 = window（模型窗口）
  $: ctxPct = ctxUsage.window ? Math.min(100, Math.round((ctxUsage.projected / ctxUsage.window) * 100)) : 0;
  // 占用极少时百分比仍为 0，但弧保底 2%（实色弧可见，圆环"活"着）
  $: ctxArcPct = ctxUsage.projected > 0 && ctxPct === 0 ? 2 : ctxPct;
  $: ctxPctText = ctxUsage.projected > 0 && ctxPct === 0 ? '<1%' : ctxPct + '%';
  $: ctxCirc = 2 * Math.PI * 13;
  // 常态浅灰（同参考样式）；占用高时转警示色
  $: ctxColor = ctxPct >= 85 ? 'var(--tint-red-fg)' : ctxPct >= 60 ? 'var(--tint-yellow-fg)' : 'var(--g6)';
  // 构成行：估算构成**按圆环的百分比分摊**（DSH 同一根环的画法）——各行加起来正好是
  // 圆环那个数，不再各算各的分母；构成本身是近似值，只当构成看，不当总量
  $: ctxRows = (() => {
    const b = ctxUsage.breakdown || { messages: 0, tools: 0, system: 0, other: 0, tools_schema: 0 };
    const sum = Math.max(1, b.messages + b.tools + b.system + b.other + (b.tools_schema || 0));
    // 构成比（各行 / 上下文总量）：回答「上下文里装的是什么」。
    // 不要按圆环百分比分摊——窗口大时 ctxPct 舍入成 0，五行会一起变 0.0%（实锤）
    const row = (label, v, color) => ({ label, v, color, pct: ((v / sum) * 100).toFixed(1) + '%' });
    return [
      row('消息', b.messages, 'var(--tint-blue-fg)'),
      row('工具结果', b.tools, 'var(--tint-cyan-fg)'),
      row('系统提示词', b.system, 'var(--tint-blue-fg)'),
      // 随请求发送的 tools JSON Schema（后端计入总量；旧后端无此字段按 0 兜底）
      row('工具定义', b.tools_schema || 0, 'var(--tint-green-fg)'),
      row('其他', b.other, 'var(--g5)'),
    ];
  })();
  // —— ZCode 式输入框：思考模式选择器（常驻）——
  // 复用设置面板的 thinking 字段（唯一数据源）：off/on=明确设置；''=渠道默认
  let thinkMenuOpen = false;
  const THINK_LABELS = { '': '默认', on: '开启', off: '关闭', low: '低档', medium: '中档', high: '高档', max: '最大' };
  // P23.2 思考三态机制：默认('')/关闭('off')/开启('on'+档位)。
  // 选「开启」后菜单不收起，才展开该厂商预设映射的档位（点档位才收尾）；
  // 档位到线上参数的映射在网关：anthropic=预算比例（低20%/中50%/高80%，GLM 端点归 effort 档）
  const THINKING_ON_VALUES = ['on', 'low', 'medium', 'high', 'max', 'xhigh', 'minimal'];
  $: thinkingOn = THINKING_ON_VALUES.includes(thinking);
  // 提示按「生效形态」给（覆盖 > 端点探测），auto/无参数渠道给可操作指引
  $: thinkHint =
    thinkingStyle === 'zai'
      ? 'GLM/豆包 thinking 对象参数'
      : thinkingStyle === 'deepseek'
        ? 'DeepSeek thinking 对象参数'
        : thinkingStyle === 'qwen'
          ? 'Qwen 系 enable_thinking 布尔'
          : thinkingStyle === 'anthropic'
            ? 'Anthropic Messages thinking：低/中/高=预算 20%/50%/80%'
            : thinkingStyle === 'qwen-chat-template'
            ? '本地推理服务 chat_template_kwargs（Qwen3 系模板）'
            : thinkingStyle === 'openai'
              ? 'OpenAI 风格 reasoning_effort 档位'
              : thinkingStyle === 'none'
                ? '形态=不发参数，思考由渠道决定'
                : '该渠道无标准开关参数——关不掉到设置改「思考参数形态」';
  function pickThinking(v, keepOpen = false) {
    thinking = v;
    // 「开启」不收起菜单——档位要等开启后才出现（P23.2 三态机制）
    if (!keepOpen) thinkMenuOpen = false;
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
  // 审批档切换的请求序号：并发乱序时以「最后一次点击」为准，过期响应丢弃（F14）
  let approvalPickSeq = 0;
  function pickApproval(m) {
    const key = currentSessionId || '@new';
    approvalMode = m;
    approvalBySession = { ...approvalBySession, [key]: m };
    approvalMenuOpen = false;
    localStorage.setItem('approvalMode', m);
    // M1：带当前会话 id——多会话并发时只切本会话的档（不带 session 才改全局默认）
    const seq = ++approvalPickSeq;
    fetch('/api/approval-mode', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ mode: m, session: currentSessionId || '' }),
    })
      .then((r) => r.json())
      .then((r) => {
        if (seq !== approvalPickSeq) return; // 过期响应：之后又有新点击，不许覆盖本地档位
        // 服务端可能把「完全访问」降级为「自动」（config 未开脱沙箱）：以服务端裁决为准
        if (r && r.mode && r.mode !== m) {
          approvalMode = r.mode;
          approvalBySession = { ...approvalBySession, [key]: r.mode };
          localStorage.setItem('approvalMode', r.mode);
        }
      })
      .catch((e) => console.warn('审批档位切换失败', e)); // 原来空吞错，失败至少留痕（F13）
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

  // 程序化改动 input 后手动回收高度（不触发 input 事件，autoRows 不会跑）
  function resetComposerHeight() {
    const el = composerInputEl;
    if (!el) return;
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
    features = s.features || { roles: true, css_tool: true, galaxy: true };
    sandboxCpu = s.sandbox_cpu_percent ?? 80;
    workspaceAliases = s.workspace_aliases || {};
    workspace = s.workspace || '';
    recentWorkspaces = s.recent_workspaces || [];
    // 圆环常驻：会话未开始时也用配置预算初始化（context_length 或全局默认 96000）
    if (!ctxUsage.window) {
      ctxUsage = { ...ctxUsage, window: s.context_length || 96000 };
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
        return false;
      }
      apiKey = '';
      applyState(d);
      flashSaved();
      return true;
    } catch (e) {
      settingsMsg = `保存失败: ${e}`;
      return false;
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
    const prevModels = savedModels;
    const prevModel = model;
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
    if (!(await sendSettings({ models: list, model: newModel, api_key: apiKey.trim() || undefined }))) {
      savedModels = prevModels; // F3：保存失败回滚，勾选态不许与后端脱节
      model = prevModel;
    }
  }

  async function selectAllModels() {
    const prevModels = savedModels;
    const prevModel = model;
    const list = [...fetchedModels].sort();
    savedModels = list;
    let newModel = model;
    if (!list.includes(model) && list.length) {
      newModel = list[0];
      model = newModel;
    }
    // A2：同上，不随选择改写能力标记
    if (!(await sendSettings({ models: list, model: newModel }))) {
      savedModels = prevModels;
      model = prevModel;
    }
  }

  async function selectNoModels() {
    const prevModels = savedModels;
    const prevModel = model;
    savedModels = [];
    model = '';
    if (!(await sendSettings({ models: [], model: '' }))) {
      savedModels = prevModels;
      model = prevModel;
    }
  }

  // —— 模型快选 ——
  async function pickModel(m) {
    const prevModel = model;
    model = m;
    modelMenuOpen = false;
    // 能力标记完全由用户手动勾选，切换模型不改写
    if (!(await sendSettings({ model: m }))) {
      model = prevModel;
    }
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
    resetComposerHeight();
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
  // 思考过程展开态（按消息 id 记；流式 patch 换对象不丢开合——原生 details 在重渲染下会闪回，用户反馈展开不了）
  let thinkingOpenMap = {};
  let webActiveId = '';
  let webAddress = '';
  let webBusy = false;
  let webError = ''; // 建页/导航失败的面板内提示（不再静默）
  let webShotT = 0; // 帧缓存穿透参数（自动刷新与动作后递增触发 <img> 重载）
  let webBodyW = 0;
  let webBodyH = 0; // 面板内容区实际尺寸（驱动浏览器视口跟随，截图铺满无黑边）
  let webBodyEl = null; // 面板内容区元素（P22：量矩形给壳里的 child webview 定位）
  let webStateReady = false; // 浏览器状态已加载过一次（就绪前不量矩形：web-tabbar 行随标签增删整行出现/消失，未定型前量的都是瞬态矩形）
  let webMode = 'headless'; // 后端宿主模式：shell（壳内真 webview）/ headless（帧流）
  let webMenuOpen = false; // 「⋯」菜单（仅无头模式的 HTML 下拉；壳模式是壳里浮层，前端不记状态）
  // 壳模式：「⋯」菜单是壳里一个**透明置顶的 webview**，浮在网页之上——网页纹丝不动、
  // 不被遮也不被藏（对齐 ZCode）。前端把面板矩形和按钮矩形交给服务端，壳负责建浮层；
  // 浮层自己关闭（点项/backdrop/Esc）时前端不知道，所以开合状态以服务端为准——这里
  // 的点击一律发 toggle。无头模式没有壳：菜单就是下面的 HTML 下拉（盖在截图上没问题）。
  function webMenuOverlay(body) {
    if (!webNative) return;
    webApi('/api/browser/overlay', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    }).catch(() => {});
  }
  function webToggleMenu(e) {
    if (!webNative) {
      webMenuOpen = !webMenuOpen; // 无头：HTML 下拉本地切换
      return;
    }
    // 壳模式：面板矩形 + ⋯ 按钮矩形（都是窗口坐标）交给后端
    const p = document.querySelector('.right-panel');
    const btn = e && e.currentTarget;
    if (!p || !btn) return;
    const pr = p.getBoundingClientRect();
    const br = btn.getBoundingClientRect();
    webMenuOverlay({
      toggle: true,
      panel: { x: pr.x, y: pr.y, w: pr.width, h: pr.height },
      anchor: { x: br.x, y: br.y, w: br.width, h: br.height },
    });
  }
  function webCloseMenu() {
    if (webNative) webMenuOverlay({ open: false });
    webMenuOpen = false;
  }
  // 离开网页页签/关面板：壳模式让壳关浮层（幂等），无头模式收起 HTML 下拉
  $: if (rpTab !== 'web' || !rightPanelOpen) {
    if (webNative) webMenuOverlay({ open: false });
    else if (webMenuOpen) webMenuOpen = false;
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
  // P26：面板「正在展示网页」的布尔态（只用原始值，避免对象标识每 2s 轮询都变导致重复触发）。
  // 后端在面板隐藏时会作废上次矩形（防收起再弹时旧矩形闪现），所以这个态每次
  // false→true（侧边栏重开/空白页加载出来/切回网页页签）都必须重发一次矩形，
  // 去重键先清——不然壳永远等不到矩形，网页不显示（用户实测实锤）。
  $: webShowing = rpTab === 'web' && rightPanelOpen && !!webActiveId && webShowShot;
  $: if (webShowing && webBodyW) {
    webViewportKey = '';
    webQueueViewport();
  }
  // 常规同步路径（尺寸变化/切页；去重键挡重复）：无头模式没有 ResizeObserver，
  // 靠 webActive 随轮询变标识兜底触发——保留它，别让上面那个跃迁响应式取代
  $: if (rpTab === 'web' && rightPanelOpen && webBodyW && webActive) webQueueViewport();

  async function loadWebState(keepActive = true) {
    try {
      const s = await webApi('/api/browser/state', { cache: 'no-store' });
      webTabs = s.tabs || [];
      webMode = s.mode || 'headless';
      webDialog = s.dialog || null;
      // 就绪门：状态到手后 web-tabbar 行才定型（有标签 = 多一行，无标签 = 整行消失），
      // 之前量的矩形都是瞬态（webview 会跳一下再跳回）。就绪后补一发同步——若标签条
      // 增删没引起 web-body 尺寸变化，ResizeObserver 不会开火，这一发是唯一机会。
      if (!webStateReady) {
        webStateReady = true;
        webQueueViewport();
      }
      if (!keepActive || !webTabs.some((t) => t.id === webActiveId)) {
        webActiveId = webTabs.length ? webTabs[webTabs.length - 1].id : '';
      }
      // 地址栏跟随当前标签页。注意不能读 $: webActive（函数内拿到的是本帧旧值），
      // 就地重查；用户正在地址栏打字时（聚焦中）不覆盖。
      // 无活动标签页（最后一个被关）时清空地址栏——否则还挂着已关标签的地址（用户反馈 bug）。
      const act = webTabs.find((t) => t.id === webActiveId) || null;
      const addrEl = document.querySelector('.web-address');
      if (document.activeElement !== addrEl) {
        webAddress = act ? (act.url === 'about:blank' ? '' : act.url || '') : '';
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
      body: JSON.stringify({ id, native: !!webNative }),
    }).catch(() => {});
  }

  // 面板不看了（切走页签/关面板）：通知后端停帧流、放后台页去挂起；
  // P22 原生托管下空白页（about:blank）也通知隐藏——壳里那个 webview 要让位给空态引导
  function webNotifyHidden() {
    // 清去重键：面板重新打开时即使矩形没变也要重发一遍——后端在面板收起时会作废
    // 上次矩形（防旧矩形闪现），等新矩形才显示 webview；键不清就永远等不到
    webViewportKey = '';
    webApi('/api/browser/activate', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      // native：只有壳里那个客户端的「不看了」能藏壳里的 webview（壳外浏览器客户端
      // 切走页签不该把壳里的网页一起藏了）
      body: JSON.stringify({ id: '', native: !!webNative }),
    }).catch(() => {});
  }
  $: if (rpTab !== 'web' || !rightPanelOpen || (webNative && !webShowShot)) webNotifyHidden();

  // P20.4：agent 操作内置浏览器时，面板自动唤起并**跟随 agent 正在操作的页**
  // （用户要求「agent 操作时自动唤起右侧边栏」+ 预期「面板跟着 agent 走」）。
  // agent 自己的切页不抢用户视图（服务端 agent_activate 不写 visible_tab），但界面要
  // 能看见 agent 在哪儿干活——常驻每 2s 探一次，agent 最近 10s 有动作就跟随过去。
  let webAgentWatch = null;
  let webAgentSeen = 0; // 已响应过的 agent 触碰时间戳（防同一动作反复弹跳）
  function webStartAgentWatch() {
    if (webAgentWatch) return;
    webAgentWatch = setInterval(async () => {
      try {
        const s = await webApi('/api/browser/state', { cache: 'no-store' });
        // 这条轮询也刷新浏览器状态——就绪门在这里同样要放行：agent 自动唤起面板
        // 走的就是这条路（不经过 loadWebState），不置标志的话视口同步被永远挡死，
        // 拖面板宽度时壳里的 webview 纹丝不动（2026-09-27 用户实锤「宽度不同步」）。
        // 注意放在标签清单更新**之后**：补发的那一发必须量到标签条挂出后的定型几何。
        // 宿主模式也随时同步：webMode 以前只在 loadWebState 时更新，而 agent 自动开面板
        // 走的是这条路——不同步的话面板按**无头模式**渲染（截图 img 分支），壳里的真
        // webview 随后才_pop_进来，用户看到的就是「先闪几下」（坏截图/暗区→真页面）。
        // 壳模式面板直接渲染 .web-native 占位，等 webview 就位即可。
        if (s.mode && s.mode !== webMode) webMode = s.mode;
        if (s.dialog !== undefined) webDialog = s.dialog || null;
        // 标签清单随时同步：agent 开新页/导航之后，页签条上的标题和地址必须跟着变。
        // 以前只有 loadWebState 才刷这份清单——agent 操作完面板里留着旧标题，用户看到的
        // 就是「一个 DeepSeek 标签显示着 Google」这种灵异现象
        const tabs = s.tabs || [];
        const stale =
          tabs.length !== webTabs.length ||
          tabs.some((t, i) => {
            const o = webTabs[i];
            return !o || o.id !== t.id || o.title !== t.title || o.url !== t.url || o.agent !== t.agent;
          });
        if (stale) {
          webTabs = tabs;
          // 活动页兜底：onMount 那条加载路径漏了时（面板开着但状态没加载过）、或服务重启后
          // 老 tab id 已失效时，这里补上——不补的话 webActive 一直是 null，面板显示空态、
          // 视口同步也永远不发（壳里 webview 跟着一直藏着）
          if (!tabs.some((t) => t.id === webActiveId)) {
            webActiveId = tabs.length ? tabs[tabs.length - 1].id : '';
          }
          const act = tabs.find((t) => t.id === webActiveId);
          if (act && document.activeElement !== document.querySelector('.web-address')) {
            webAddress = act.url === 'about:blank' ? '' : act.url || '';
          }
        }
        if (!webStateReady) {
          webStateReady = true;
          webQueueViewport();
        }
        if (!s.agentTouched || Date.now() - s.agentTouched > 10000) return;
        if (s.agentTouched <= webAgentSeen) return; // 这次触碰已响应过
        webAgentSeen = s.agentTouched;
        rightPanelOpen = true;
        if (rpTab !== 'web') rpTab = 'web';
        if (s.agentTab && s.agentTab !== webActiveId && (s.tabs || []).some((t) => t.id === s.agentTab)) {
          webActiveId = s.agentTab;
          const act = (s.tabs || []).find((t) => t.id === webActiveId);
          if (act && document.activeElement !== document.querySelector('.web-address')) {
            webAddress = act.url === 'about:blank' ? '' : act.url || '';
          }
          webActivate(webActiveId);
        }
      } catch {}
    }, 2000);
  }
  webStartAgentWatch();
  onDestroy(() => {
    if (webAgentWatch) clearInterval(webAgentWatch); // N2：常开轮询随组件销毁收掉
  });


  // 帧自动刷新：面板开着且有页面时 600ms 一帧（screencast 内存直出，零 CDP 往返）。
  // P22 原生托管不看帧——壳里就是真页面，帧流停掉省编码开销
  let webFrameTimer = null;
  $: if (rpTab === 'web' && rightPanelOpen && webShowShot && !webNative && !webFrameTimer) {
    webFrameTimer = setInterval(() => { webShotT += 1; }, 600);
  } else if ((rpTab !== 'web' || !rightPanelOpen || !webShowShot || webNative) && webFrameTimer) {
    clearInterval(webFrameTimer);
    webFrameTimer = null;
  }

  // 面板 → 后端的响应统一解析：旧版服务（没有新接口）会返回 404 或 SPA 兜底的 HTML，
  // 直接 r.json() 只会报天书——这里翻成「请重启服务」的明确提示。
  // missingMsg 说明缺的是哪类接口（浏览器 / 用量台账…）。
  async function apiJson(path, opts, missingMsg) {
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
      // 接口路径落到 SPA 兜底 = 服务端没这个路由（旧版程序），HTML 开头就是铁证
      if (text.trimStart().startsWith('<')) throw new Error(missingMsg);
      if (resp.status === 404) throw new Error(missingMsg);
      throw new Error('服务返回异常（HTTP ' + resp.status + '），请重启服务后重试');
    }
    if (!resp.ok) throw new Error((data && data.error) || '请求失败（HTTP ' + resp.status + '）');
    return data || {};
  }
  async function webApi(path, opts) {
    return apiJson(path, opts, '服务端没有浏览器接口，跑的还是旧版程序——请重启服务（先 cargo build --release 再 start.bat）');
  }

  // 浏览器视口跟随面板内容区（16ms 节流≈每帧；截图铺满无黑边，网页按真实窗口宽度排版）。
  // P22：x/y 一并上报——shell 模式下后端据此把壳里的 child webview 摆到这块区域。
  // **触发不能只靠 webBodyW/H 的响应式**：窗口横向拉伸时面板宽高都不变、只有 x 变，
  // 响应式不重算 → 壳里 webview 停在旧位置（真机错位 bug）。改用 ResizeObserver 看
  // 尺寸 + window resize 看位置，两条都汇进同一个防抖队列。
  let webViewportTimer = null;
  let webViewportLast = 0; // 最近一次实际发出的时间戳（节流窗口用）
  let webViewportKey = ''; // 已应用的「标签|x,y|宽x高」，防止重复请求打爆 CDP actor
  let webRectObs = null;
  // 拖面板宽度/拉伸窗口时**马上跟**：旧实现是 350ms 尾防抖——拖的过程中定时器被不断
  // 重置、一次都不发，停手才补一发，用户看到的就是"网页宽度不跟着变、很迟钝"。
  // 100ms 节流仍能看出台阶，压到 16ms（≈每帧一次）：首变化立即发，拖动期间至多每
  // 16ms 一发，停手补最后一发。
  function webQueueViewport() {
    const now = Date.now();
    const wait = 16 - (now - webViewportLast);
    if (wait <= 0) {
      if (webViewportTimer) {
        clearTimeout(webViewportTimer);
        webViewportTimer = null;
      }
      webSyncViewport();
    } else if (!webViewportTimer) {
      webViewportTimer = setTimeout(() => {
        webViewportTimer = null;
        webSyncViewport();
      }, wait);
    }
  }
  function webWatchRect(on) {
    // 根容器一并观察：内容区自身尺寸不变、只有 x 平移时（主滚动条出现/消失把弹性行
    // 挤窄 16px、窗口拉伸后面板左缘横移）ResizeObserver 不会为内容区开火——不观察
    // 根容器这种"只挪不变"的位移就漏一次同步，壳里 webview 卡在旧 x 上（用户实锤）
    const rootEl = document.querySelector('.app');
    if (on) {
      if (!webRectObs && typeof ResizeObserver !== 'undefined') {
        webRectObs = new ResizeObserver(() => webQueueViewport());
      }
      if (webRectObs) {
        if (webBodyEl) webRectObs.observe(webBodyEl);
        if (rootEl && rootEl !== webBodyEl) webRectObs.observe(rootEl);
      }
      window.addEventListener('resize', webOnWindowResize);
    } else if (webRectObs) {
      if (webBodyEl) webRectObs.unobserve(webBodyEl);
      if (rootEl) webRectObs.unobserve(rootEl);
      window.removeEventListener('resize', webOnWindowResize);
    }
  }
  function webOnWindowResize() {
    webQueueViewport();
  }
  // 面板在原生托管且停在「网页」时才需要跟（壳里 webview 的显隐与它联动）
  $: if (webNative && rpTab === 'web' && rightPanelOpen && webBodyEl) webWatchRect(true);
  $: if (!(webNative && rpTab === 'web' && rightPanelOpen)) webWatchRect(false);
  // 自愈兜底（P27）：上面的同步全是**事件驱动**的，漏一个事件壳里 webview 就永久卡在
  // 旧矩形上——用户看到的就是「网页浮在错误位置、盖住聊天列右侧」。漏事件的路径不止
  // 一种：只挪不变的位移（观察根容器已补）、渲染竞态量到的瞬态矩形、面板切走期间发生
  // 的布局变化（那时 webSyncViewport 直接 return）。所以再加一条 1s 心跳：重量一次
  // 矩形，**变了才发**（去重键挡着，不变就是零请求），任何漏掉的变化最多 1s 内归位。
  let webSyncHeartbeat = null;
  $: if (rpTab === 'web' && rightPanelOpen && webActive && webStateReady && !webSyncHeartbeat) {
    webSyncHeartbeat = setInterval(() => webQueueViewport(), 1000);
  } else if (!(rpTab === 'web' && rightPanelOpen && webActive && webStateReady) && webSyncHeartbeat) {
    clearInterval(webSyncHeartbeat);
    webSyncHeartbeat = null;
  }
  function webSyncViewport() {
    if (!webActive || rpTab !== 'web') return;
    // 就绪门：浏览器状态没到手前，web-tabbar 行未定型（有标签=多一行 / 无标签=整行消失），
    // 量的矩形是瞬态——发出去 webview 会先跳错位再跳回。等 loadWebState 就绪后补发。
    if (!webStateReady) return;
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
    webViewportLast = Date.now();
    webApi('/api/browser/viewport', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      // native：本客户端就是壳里那个（壳模式 + 跑在 Tauri 里）时，量的 x/y 才是壳窗口
      // 坐标，服务端据此挪壳里的 child webview。浏览器直开 5173 的第二个客户端也走这个
      // 接口（截图要跟它自己的面板尺寸），但它的矩形是属于那个浏览器窗口的——不标明白
      // 就会被当成壳的矩形，壳里网页整体漂到错误位置（用户实锤「界面卡位」）。
      body: JSON.stringify({ id: webActive.id, x, y, w, h, native: !!webNative }),
    })
      .then((res) => {
        // 服务端拒收（越界等）：清 key，等下一次观察器/节流开火重试，别把坏矩形当成已应用
        if (res && res.skipped) webViewportKey = '';
        else webShotT += 1;
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
        body: JSON.stringify({ native: !!webNative }),
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
          body: JSON.stringify({ url, native: !!webNative }),
        });
        if (r.id) webActiveId = r.id;
        else webError = r.error || '打开网页失败';
      } else {
        const r = await webApi('/api/browser/navigate', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ id: webActive.id, url, native: !!webNative }),
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
      }).then((r) => r.json());
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
    reader.onload = async (ev) => {
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
      if ((existing || customCss.trim()) && !(await confirmAsk(`导入将覆盖${scope}，继续？`))) return;
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
    resetComposerHeight(); // F4：发送后清空是程序化赋值，不触发 input 事件，高度得手动回收
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
    // 引导（立即发送）不是一轮对话：**不登记 streams**——登记会顶掉运行中那轮的
    // 流条目，引导响应一结束 finally 又把它删掉，原轮还在跑本页却以为空闲了
    // （再发消息走错分支、出队逻辑乱序）。引导的 SSE 只回执不产出，用不上这些。
    const ownsTurn = opts.steer !== true;
    if (ownsTurn) streams = { ...streams, [key]: { asstId, controller: ac } };
    syncCur();
    const stream = { asstId, controller: ac };
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
                // 服务端判成「这个会话在跑」（流断了/别的窗口刚起了一轮）：本条转排队、撤掉空回答气泡。
                // **服务端接管了这条的投递**（本轮收尾它自己排水跑掉）——必须把它从出队队列摘掉：
                // 否则「排队→流结束→出队→再发→又排队」就是无限请求风暴
                // （2026-09-27 真机踩到：SSE 断流后发一条消息，10 秒被重发 2000 次塞满队列）。
                // 跑完的答案也拿不到（d.turn 走的是已死的原流）——由 reconcileServerQueued 对账。
                updMsgs(skey, (a) => a.filter((m) => m.id !== stream.asstId));
                const qid = markQueued(skey, effectiveMsg, atts);
                outbox = { ...outbox, [skey]: (outbox[skey] || []).filter((x) => x.id !== qid) };
                reconcileServerQueued(skey);
              } else if (d.steered) {
                // 这条已被并进正在跑的一轮（引导）：本页不再给它单开回答气泡
                updMsgs(skey, (a) => a.filter((m) => m.id !== stream.asstId));
              } else if (d.turn) {
                // 排队的消息开始跑了：气泡从「排队中」转正式 + 新开一个回答气泡
                // （已引导过的也算命中——正常到不了这里，但收尾竞态下别让它再补一条重复气泡）
                const adds = [];
                for (const m of d.turn.messages || []) {
                  const q = (sessionMsgs[skey] || []).find((x) => (x.queued || x.steered) && x.role === 'user' && x.content === m.content);
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
                // 用量按会话记账（后台会话写各自账本）；正在看的才刷到显示。
                // 圆环的**唯一**写入口：后端按「实报锚点 + 带符号增量」算好 projected 推来，
                // 前端只做展示，不再自己加减（旧写法与 api_usage 两处都写，口径会打架）
                const ctx = {
                  projected: d.context_usage.projected || 0,
                  pressure: d.context_usage.pressure ?? null,
                  window: d.context_usage.window || 0,
                  breakdown: d.context_usage.breakdown || null,
                };
                usageBySession = {
                  ...usageBySession,
                  [skey]: { ctx, api: (usageBySession[skey] || {}).api || { prompt: 0, cached: 0 } },
                };
                if (skey === curKey()) ctxUsage = ctx;
              }
              else if (d.api_usage) {
                // 渠道实报只进「累计流量」账（平均缓存命中率行）；圆环不在这里改
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
      if (ownsTurn) {
        // 这条流结束（本会话可再次发送）；只清本会话的流登记，别的会话在跑不受影响。
        // 引导请求（ownsTurn=false）绝不能走这段：它的流条目根本没登记，误删会把
        // 还在跑的那一轮登记清掉（本页从此以为会话空闲，出队/重发全部乱套）。
        const nextStreams = { ...streams };
        delete nextStreams[skey];
        streams = nextStreams;
        localSteers = [];
        syncCur();
        // 流死了没产出任何内容（网络断/服务重启等）：撤掉空回答气泡，别留一个空壳。
        // 缓冲已被清掉的会话（切工作区 abort）不复活条目——否则空数组是 truthy，
        // 切回来会被当成「本地缓冲」用，历史照样全不见。
        if (sessionMsgs[skey]) {
          updMsgs(skey, (a) =>
            a.filter((m) => m.id !== stream.asstId || m.content || (m.tools && m.tools.length) || m.approval)
          );
        }
        loadSessions(); // 刷新会话列表（新会话出现/标题更新）
        // 出队（027）：这一轮跑完了，把排在最前的排队消息自动发出去（发完它再发下一条）
        const q = outbox[skey] || [];
        if (q.length > 0 && !streams[skey]) {
          const [item, ...rest] = q;
          outbox = { ...outbox, [skey]: rest };
          deliver(skey, item.text, item.atts, { bubbleId: item.id });
        }
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

  // 服务端接管排队消息后的对账（2026-09-27）：本页的 SSE 流已死，服务端跑这条消息时
  // 发的 d.turn / 回答流进的是那条死连接——轮询到服务端空闲后从会话文件重载，
  // 把「排队中」气泡对成真消息 + 补上错过的一切。不重发任何东西（消息已在服务端队列里）。
  function reconcileServerQueued(skey) {
    if (reconcileTimers[skey]) return;
    reconcileTimers[skey] = setInterval(async () => {
      try {
        if (streams[skey]) return; // 本页又起了流：对账交给正常流程
        const c = await fetch(`/api/cancel?session=${encodeURIComponent(skey)}`).then((r) => r.json());
        if (c.running) return; // 服务端还在跑：继续等
        clearInterval(reconcileTimers[skey]);
        delete reconcileTimers[skey];
        if (currentSessionId !== skey) {
          // 不在这个会话的页面：丢掉本地陈旧缓冲，下次打开时从服务端重建
          const next = { ...sessionMsgs };
          delete next[skey];
          sessionMsgs = next;
          return;
        }
        if (streams[skey]) return;
        const r = await fetch(`/api/session?id=${encodeURIComponent(skey)}`).then((r) => r.json());
        if ((r.messages || []).length > 0) {
          sessionMsgs = { ...sessionMsgs, [skey]: rebuildFromSession(r.messages || []) };
          syncCur();
          loadSessions();
        }
      } catch (e) {
        // 网络抖动：下一跳再试
      }
    }, 2000);
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
      // 不筛会打开别的项目的会话，而 /api/sessions/open 会把工作区切回那个项目（侧栏项目来回弹）。
      // 无 workspace 标记的遗留会话不许自动弹：新建工作区会翻出最老的一批测试会话
      // （2026-09-29 用户实锤「只回复两个字：收到」轮）；侧栏「未分组」里仍可手动打开。
      if (autoPick && currentSessionId === null) {
        const mine = sessions.filter(
          (s) => s.workspace && normPath(s.workspace).toLowerCase() === normPath(workspace).toLowerCase(),
        );
        if (mine.length) await loadSession(mine[0].id);
      }
    } catch (e) {
      sessions = [];
    }
  }

  // 服务端用量快照（/api/sessions/open 的 r.usage）→ 圆环/缓存行同形。
  // 旧会话文件里是 tokens/budget（改造前口径）——后端已用 serde alias 收下并改写成
  // projected/window，这里对老服务端载荷也做一次兜底，免得切回旧会话圆环空掉
  function usageFromServer(su) {
    return {
      ctx: {
        projected: su.projected ?? su.tokens ?? 0,
        pressure: su.pressure ?? null,
        window: su.window ?? su.budget ?? 0,
        breakdown: {
          messages: su.messages || 0,
          tools: su.tools || 0,
          system: su.system || 0,
          other: su.other || 0,
          tools_schema: su.tools_schema || 0,
        },
      },
      api: { prompt: su.api_prompt || 0, cached: su.api_cached || 0 },
    };
  }

  // 切到某会话：从按会话账本恢复用量；没账本的新会话清零（窗口保留——圆环常驻）
  function restoreUsage(key) {
    // 活数据（本页收到的 SSE 事件）最新；没有才落回服务端快照（重启/重开后的恢复路径），
    // 并扶正为会话账本，后续切换不再反复读盘
    const u = usageBySession[key] || serverUsage[key];
    if (u && !usageBySession[key]) usageBySession = { ...usageBySession, [key]: u };
    ctxUsage = u ? u.ctx : { projected: 0, pressure: null, window: ctxUsage.window, breakdown: null };
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
      }).then((r) => r.json());
      if (r.error) {
        // 侧栏点会话打不开：settingsMsg 只在设置弹窗里渲染，这里必须可见反馈
        await confirmAsk(`打开会话失败：${r.error}`, true);
        return;
      }
      currentSessionId = id;
      currentTitle = r.title || (sessions.find((x) => x.id === id)?.title ?? '');
      // 服务端用量快照（会话文件里随事件落盘的那份）：重启/重开程序后圆环和缓存行
      // 从这里恢复，不用等下一轮请求再攒。活数据优先，这里只兜底
      if (r.usage) serverUsage = { ...serverUsage, [id]: usageFromServer(r.usage) };
      // 本地已有缓冲就用本地（含流式思考等尚未落盘的新内容），没有才从服务端重建
      if (!sessionMsgs[id]) {
        sessionMsgs = { ...sessionMsgs, [id]: rebuildFromSession(r.messages || []) };
      }
      // 对账（2026-09-27 用户实锤「切界面/开关程序丢对话内容」）：本页没有这条会话的流，
      // 但服务端可能还有一轮在跑——切工作区回来/重开程序都会走到这里，拿到的是**旧文件**
      // 快照，运行中那轮的产出从此没人补。挂上对账轮询：服务端空闲后从会话文件重载，
      // 缺的尾巴自动回来。（本页有自己的流在跑时不进这个分支，用不到对账。）
      fetch(`/api/cancel?session=${encodeURIComponent(id)}`)
        .then((r) => r.json())
        .then((c) => {
          if (c.running && !streams[id]) reconcileServerQueued(id);
        })
        .catch(() => {});
      syncCur();
      restoreUsage(id); // 从按会话账本恢复（跑完的会话不重发事件，不恢复就显示 0）
      dockCp = null;
      if (r.warning) await confirmAsk(r.warning, true);
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
      const r = await fetch('/api/sessions/new', { method: 'POST' }).then((r) => r.json());
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
        await confirmAsk(`新建会话失败：${r.error}`, true);
      }
    } catch (e) {
      await confirmAsk(`新建会话失败：${e.message || e}`, true);
    }
  }

  async function deleteSession(id) {
    if (!(await confirmAsk('删除这条会话？其余会话不受影响'))) return;
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
  // P27 通用确认弹窗：桌面壳（Tauri WebView2）不实现 window.confirm/alert（静默无操作），
  // 全部确认/提示走应用内弹窗。{ text, okOnly, resolve }。
  let confirmModal = null;
  function confirmAsk(text, okOnly = false) {
    return new Promise((resolve) => {
      confirmModal = { text, okOnly, resolve };
    });
  }
  const CONV_PREVIEW = 5; // 项目收起态先显 5 条，其余走「展开其余 N 个会话」

  $: if (workspace && !(workspace in expandedFlags)) {
    expandedFlags = { ...expandedFlags, [workspace]: true };
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
      // 切项目会重置所有会话缓冲。同上：拒绝可以，但要让人看见原因
      await confirmAsk('有会话正在运行，等它跑完再切换项目。', true);
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
    if (!checkedIds.length || !(await confirmAsk(`删除选中的 ${checkedIds.length} 条会话？不可恢复`))) return;
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
      const r = await fetch('/api/workspace/picker', { method: 'POST' }).then((r) => r.json());
      if (r.canceled || !r.path) return;
      await applyWorkspace(r.path);
    } catch (e) {
      // settingsMsg 只在设置弹窗里渲染——侧栏操作时不可见，改应用内提示弹窗
      await confirmAsk(`选择文件夹失败：${e.message || e}`, true);
    }
  }

  async function removeProject(path, name) {
    if (normPath(path).toLowerCase() === normPath(workspace).toLowerCase()) {
      // 列表最上面那条就是当前项目（projectList 把它固定放首位）。拦下来可以，但理由
      // 必须可见：settingsMsg 只在设置弹窗里渲染，侧栏操作时写了等于没写（用户实锤
      // 「最上面的删除项目无效」）——应用内提示弹窗说清楚。
      await confirmAsk(`「${name}」是当前正在使用的项目，不能移除。\n先切换到其他项目，再来移除它。`, true);
      return;
    }
    if (!(await confirmAsk(`从侧边栏移除项目「${name}」？\n只移出列表，不删除磁盘上的文件夹与会话。`))) return;
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
      await confirmAsk(`分叉失败：${e.message || e}`, true);
    }
  }

  function submitRename() {
    if (!renameModal) return;
    const v = renameModal.value.trim();
    const modal = renameModal;
    renameModal = null;
    if (modal.kind === 'session') {
      // orig = 打开弹窗时的原标题。输入框 bind 直接改 modal.value，拿它比较
      // 恒为假——请求永远发不出（F1），必须拿原值比
      if (v && v !== modal.orig) {
        fetch('/api/sessions/meta', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ id: modal.target, title: v }),
        }).then(() => {
          // F5：改的就是当前会话时同步顶栏（否则侧栏变了顶栏仍旧名）
          if (modal.target === currentSessionId) currentTitle = v.slice(0, 60); // 与后端 60 字截断对齐
          loadSessions();
        });
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
    if (!(await confirmAsk(`撤销本次任务的改动？\n${cp.files} 个文件将恢复到任务开始前（含删除本次新建的已跟踪文件）。\nshell 副作用不可回滚。`)))
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
      } else if (m.role === 'system') {
        // P27：通知/错误随会话落盘（display_only），重启/切回会话后气泡还在
        out.push({ id: nextId(), role: 'system', content: contentText(m.content) });
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

  // 审批决定：本地乐观标记 + 通知后端（后端也会回 approval_decided，幂等）。
  // remember=true → 本会话内「同类操作」不再询问（P9 A3）；后端拒绝记忆 Dangerous 档。
  async function decideApproval(id, approved, remember = false) {
    patchMessageApproval(id, approved);
    try {
      const r = await fetch('/api/approvals', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id, approved, remember }),
      });
      if (!r.ok) throw new Error(`HTTP ${r.status}`);
    } catch (e) {
      // F13：失败回滚本地标记（后端没收到决定，UI 不能显示已决定）+ 人话提示
      updMsgs(curKey(), (a) =>
        a.map((m) => (m.approval && m.approval.id === id ? { ...m, approval: { ...m.approval, decided: null } } : m))
      );
      pushMsg(curKey(), { id: nextId(), role: 'error', content: `[审批决定提交失败，已恢复待决定] ${e}` });
    }
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
  let settingsTab = ['appearance', 'model', 'roles', 'memory', 'skills', 'mcp', 'ext', 'schedule'].includes(localStorage.getItem('settingsTab'))
    ? localStorage.getItem('settingsTab')
    : 'model';
  function pickSettingsTab(t) {
    settingsTab = t;
    localStorage.setItem('settingsTab', t);
    if (t === 'mcp' || t === 'plugins') loadMcp();
    if (t === 'skills') loadSkills();
    if (t === 'ext') { loadGitRuntime(); loadExt(); loadSandboxInfo(); }
  }
  // 功能桥接被关掉时若正停在该页签，自动切走（features 随 applyState 回来，可能晚于首屏）
  $: if (settingsTab === 'roles' && features && features.roles === false) settingsTab = 'model';

  // 内置功能开关（插件页功能行）：写 settings.features，联动收起受影响的入口
  function toggleFeature(k) {
    features = { ...features, [k]: !features[k] };
    autoSave({ features }, 0);
    if (k === 'css_tool' && !features.css_tool && editMode) editMode = false;
    if (k === 'galaxy' && !features.galaxy && galaxyOpen) galaxyOpen = false;
  }

  // 沙箱 CPU 上限：空/非数字 = 回默认 80，越界钳回 0-100（0 = 不限）
  function commitCpu(v) {
    const raw = Number(v);
    const n = Number.isFinite(raw) ? Math.max(0, Math.min(100, Math.round(raw))) : 80;
    sandboxCpu = n;
    autoSave({ sandbox_cpu_percent: n }, 0);
  }

  // 同上：技能/MCP/插件分区的数据原先只在 pickSettingsTab 的点击里拉，页签从 localStorage
  // 恢复时（上次停在「技能」再开机）打开设置看到的是空列表，点一下页签才有——照记忆分区补齐。
  $: if (settingsOpen && settingsTab === 'skills') loadSkills();
  $: if (settingsOpen && settingsTab === 'mcp') loadMcp();
  $: if (settingsOpen && settingsTab === 'ext') {
    loadGitRuntime();
    loadSandboxInfo();
  }


  // —— 技能（Skill / SKILL.md）分区 ——
  let skillsData = [];
  let skillsLoading = false;
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

  // —— P11（施工单 016）：MCP 插件分区 ——
  let mcpServers = [];
  let wasmPlugins = []; // WASM 工具插件清单（loadMcp 灌；此前漏声明，能力菜单一开就 ReferenceError）
  // git 运行时状态（插件分区顶部卡片）：系统装的 / 内置包解压的 / 不可用
  let gitRuntime = null;
  // 021：沙箱自检（真拉起受限进程跑探针套件）
  let sandboxProbe = null;
  let sbRunning = false;
  // 021：应用版本（侧栏「关于」行）
  let appVersion = null;
  async function loadVersion() {
    try {
      appVersion = await fetch('/api/version', { cache: 'no-store' }).then((r) => r.json());
    } catch (e) {
      appVersion = null;
    }
  }
  async function runSandboxProbe() {
    sbRunning = true;
    try {
      sandboxProbe = await fetch('/api/sandbox/selftest', { cache: 'no-store' }).then((r) => r.json());
    } catch (e) {
      sandboxProbe = { error: String(e.message || e) };
    }
    sbRunning = false;
  }
  async function loadGitRuntime() {
    try {
      gitRuntime = await fetch('/api/git/info', { cache: 'no-store' }).then((r) => r.json());
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
      sandboxInfo = await fetch(`/api/sandbox/info${refresh ? '?refresh=1' : ''}`, { cache: 'no-store' }).then((r) => r.json());
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
      }).then((r) => r.json());
      if (r.error) settingsMsg = `MCP ${action} 失败: ${r.error}`;
      if (action === 'refresh' && r.ok) settingsMsg = `已连接，发现 ${r.tools_found} 个工具`;
      // P24：授权发起后带回事务快照（pending 时卡片里会给可点的授权链接）
      if (action === 'oauth_authorize' && r.tx) settingsMsg = r.tx.message || '授权已发起';
      if (action === 'oauth_revoke') settingsMsg = '已撤销授权';
    } catch (e) {
      settingsMsg = `MCP ${action} 失败: ${e.message || e}`;
    }
    await loadMcp();
  }

  // —— 021 组件总线 v0：UI 扩展（iframe 插槽 + 白名单数据桥）——
  // 隔离：iframe sandbox="allow-scripts"（无 allow-same-origin）→ 不透明源，拿不到宿主
  // DOM / localStorage / cookie；唯一通道是 postMessage，父侧只放行下面这几个只读方法。
  let extList = [];
  let extInfo = { app_dir: '', workspace_dir: '' };
  let extOpen = false;
  let extCurrent = null; // 当前打开的扩展（{ name, title, ... }）
  let extFrame = null; // 面板里那个 iframe 元素（当前打开的扩展）
  let dockExtFrame = null; // 右侧面板里承载 dock 插槽插件的 iframe
  let extMsg = '';
  let extSeq = 0;
  const extPending = new Map(); // 请求 id → 发起方（这次是父侧主动调用扩展，留口）

  // 内置插件的图标（24×24 stroke 内联标记，画在 .plug-ico 里；未知插件回退首字）
  const EXT_ICONS = {
    board: '<path d="M4.6 16.5a7.6 7.6 0 1 1 14.8 0"/><path d="M12 14.5 15.5 9.5"/><circle cx="12" cy="15" r="0.9" fill="currentColor" stroke="none"/>',
    trace: '<path d="M5 6.5h11.5M5 12h6.5M5 17.5h13.5"/><circle cx="19.6" cy="6.5" r="1.4"/><circle cx="14.4" cy="12" r="1.4"/><circle cx="21.4" cy="17.5" r="1.4"/>',
    usage: '<circle cx="12" cy="12" r="8.4"/><path d="M8.8 7.5 12 12l3.2-4.5"/><path d="M12 12v5.2"/><path d="M9.3 13.8h5.4"/>',
    browser: '<circle cx="12" cy="12" r="8.4"/><path d="M3.6 12h16.8"/><ellipse cx="12" cy="12" rx="3.8" ry="8.4"/>',
    documents: '<path d="M7 3.5h6.5L18 8v12.5H7z"/><path d="M13.5 3.5V8H18"/><path d="M9.5 12h5M9.5 15h5M9.5 18h3"/>',
    github: '<circle cx="6" cy="6" r="2.3"/><path d="M6 8.3V21"/><circle cx="18" cy="18" r="2.3"/><path d="M13 6h2.5A2.5 2.5 0 0 1 18 8.5v7.2"/>',
    pdf: '<path d="M7 3.5h6.5L18 8v12.5H7z"/><path d="M13.5 3.5V8H18"/><path d="M9.5 15.5c1.7 0 1.7-2.8 3.4-2.8s1.7 2.8 3.4 2.8"/>',
    presentations: '<path d="M3 3.5h18"/><path d="M20 3.5V13a1.8 1.8 0 0 1-1.8 1.8H5.8A1.8 1.8 0 0 1 4 13V3.5"/><path d="M12 14.8V18"/><path d="M8.5 21 12 18l3.5 3"/>',
    spreadsheets: '<rect x="4" y="5" width="16" height="14" rx="1.5"/><path d="M4 10.2h16"/><path d="M10.2 5v14"/><path d="M4 15h16"/>',
  };
  function extIcon(name) {
    return EXT_ICONS[name] || '';
  }

  async function loadExt() {
    try {
      const r = await fetch('/api/plugins', { cache: 'no-store' }).then((r) => r.json());
      extList = r.plugins || [];
      extInfo = { app_dir: r.app_dir || '', workspace_dir: r.workspace_dir || '' };
    } catch (e) {
      extList = [];
      extMsg = `读取插件失败: ${e.message || e}`;
      // 壳可能先于服务就绪（start.bat 是壳拉起服务）：拉不到就 2s 后重试几次，
      // 否则 dock 插件页签 / 后台 iframe 要等用户手动进一次设置→插件才出现
      if (extLoadRetry < 5) {
        extLoadRetry += 1;
        setTimeout(loadExt, 2000);
      }
    }
  }
  let extLoadRetry = 0;

  // 已启用的扩展（停用的不加载、不给按钮）
  $: enabledExts = extList.filter((e) => e.enabled !== false && e.ok);
  // 分组（用户 2026-09-30：Git/沙箱/内置面板插件统一归「内置」一组，别分两层）；
  // 第三方（工作区来源 / user 来源 / 草稿）单独一组
  $: builtInExts = extList.filter((e) => e.source !== 'workspace' && e.origin !== 'user' && !e.draft);
  $: thirdPartyExts = extList.filter((e) => !builtInExts.includes(e));
  // P21：声明了 dock 插槽的启用插件 → 右侧面板多一个页签（与「产物/预览/网页」并列）。
  // 停用插件即从页签消失——插件自己决定要不要这个槽，宿主只认 manifest 声明。
  $: rpPluginTabs = enabledExts.flatMap((e) =>
    (e.ui || []).filter((u) => u.slot === 'dock').map((u) => ({ key: 'ext:' + e.name, name: e.name, title: u.title || e.title || e.name }))
  );
  function openDockPlugin(t) {
    rpTab = t.key;
    const ext = extList.find((e) => e.name === t.name);
    if (ext) extCurrent = ext;
  }
  // 开机恢复 dock 页签：rpTab 存在 localStorage 里、extCurrent 不在。上次停在「用量」这类
  // dock 页签时，插件清单（/api/plugins）还没拉回来，右侧面板已经照着 ext:xxx 渲染 → extCurrent
  // 还是 null → 显示「插件未加载」，必须切到别的页签再点回来才好（openDockPlugin 才补 extCurrent）。
  // 这里等清单一到就补上；插件被停用/删了则退回「产物」，别把用户留在死页签上。
  // 只在插件面板没占屏时同步——extOpen 时 extCurrent 归那个面板的 iframe 用，别抢。
  $: if (!extOpen && rpTab.startsWith('ext:')) {
    const want = rpTab.slice(4);
    const ext = enabledExts.find((e) => e.name === want);
    if (ext && (!extCurrent || extCurrent.name !== want)) extCurrent = ext;
    else if (!ext && extList.length && (!extCurrent || extCurrent.name !== want)) rpTab = 'artifacts';
  }
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
    // dock 页签里的 iframe 也要登记（postToName 靠 extFrameEls 找它）
    if (dockExtFrame && extCurrent && extCurrent.name === name) extFrameEls[name] = dockExtFrame;
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

  /// 打开插件的指定页（manifest settings_entry，如「用量」插件的价格设置页）
  function openExtPage(e, page) {
    const wasOpenSame = extOpen && extCurrent && extCurrent.name === e.name;
    extCurrent = { ...e, entry: page };
    extMsg = '';
    extOpen = true;
    rightPanelOpen = false;
    terminalOpen = false;
    if (!wasOpenSame) extPendingMsgs.push({ name: e.name, msg: { type: 'agent:init', payload: extInitPayload(e) } });
  }

  /// 扩展插槽按钮点击：打开该扩展面板并把事件交给它（面板里自己决定做什么）
  function clickSlot(entry) {
    openExt(entry.ext, entry.slot);
  }

  /// 扩展启用/停用（P17）：写设置 + 刷新清单
  // 行内 tooltip：把版本/目录/工具/权限/界面插槽收进悬停提示，行体只留一句描述（用户 2026-09-30：单行紧凑，别占地方）
  function extTip(e) {
    const parts = [];
    if (e.version) parts.push(`v${e.version}`);
    if (e.dir) parts.push(e.dir);
    if (e.tools?.length) parts.push(`工具: ${e.tools.map((t) => `wasm__${e.name}__${t[0] ?? t.name}`).join('、')}`);
    if (e.permissions?.length) parts.push(`权限: ${e.permissions.join('、')}`);
    if (e.ui?.length) parts.push(`界面: ${e.ui.map((u) => u.slot).join('、')}`);
    return parts.join('\n');
  }

  async function toggleExt(e) {
    try {
      const r = await fetch('/api/plugins/toggle', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: e.name, enabled: e.enabled === false }),
      }).then((r) => r.json());
      if (r.plugins) extList = r.plugins;
      if (e.enabled !== false && extCurrent && extCurrent.name === e.name) extOpen = false; // 停用后关面板
    } catch (err) {
      extMsg = `切换失败: ${err.message || err}`;
    }
  }

  // P18 契约 v1（027）：draft 转正（落盘持久化、来源改 user）/ 丢弃（内存即清）
  async function promoteExt(e) {
    try {
      const r = await fetch('/api/plugins/promote', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ name: e.name }),
      }).then((r) => r.json());
      if (r.error) extMsg = `转正失败: ${r.error}`;
      else extMsg = `已转正「${e.title || e.name}」（来源已改为 user，重启后仍在）`;
      if (r.plugins) extList = r.plugins;
    } catch (err) {
      extMsg = `转正失败: ${err.message || err}`;
    }
  }

  async function discardExt(e) {
    if (!(await confirmAsk(`丢弃草稿插件「${e.title || e.name}」？内存内容将立即消失。`))) return;
    try {
      const r = await fetch(`/api/plugins/draft?name=${encodeURIComponent(e.name)}`, { method: 'DELETE' }).then((r) => r.json());
      if (r.error) extMsg = `丢弃失败: ${r.error}`;
      else extMsg = `已丢弃「${e.title || e.name}」`;
      if (r.plugins) extList = r.plugins;
    } catch (err) {
      extMsg = `丢弃失败: ${err.message || err}`;
    }
  }

  // P21 红线3：第三方插件来源验签——pin/unpin 内容指纹（user 来源插件先受信任才加载）
  async function trustExt(e, action) {
    const key = e.trust_key;
    if (!key) return;
    const verb = action === 'pin' ? '信任' : '取消信任';
    if (action === 'pin' && !(await confirmAsk(`信任插件「${e.title || e.name}」？\n\n将记录它当前的内容指纹，之后每次加载都比对；文件被改动后会再次要求重新信任。`))) return;
    try {
      const r = await fetch('/api/plugins/trust', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ key, action }),
      }).then((r) => r.json());
      if (r.error) extMsg = `${verb}失败: ${r.error}`;
      else extMsg = action === 'pin' ? `已信任「${e.title || e.name}」（内容指纹已固定）` : `已取消信任「${e.title || e.name}」`;
      if (r.plugins) extList = r.plugins;
    } catch (err) {
      extMsg = `${verb}失败: ${err.message || err}`;
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
    // 桥上的 GET 回传的必须是**解析后的 JSON**：fetch 的 Response 过不了 postMessage 的
    // 结构化克隆——真机原话 "Response object could not be cloned"，插件侧只看到这句天书，
    // 面板整个瘫掉（用量/价格/trace/工作区/git 六个只读方法全走这里）。
    // 旧版服务没有这些接口时请求落进 SPA 兜底返回 HTML，统一翻成「请重启服务」的人话。
    const get = (url) =>
      apiJson(url, { cache: 'no-store' }, '服务端没有这个接口，跑的还是旧版程序——请重启服务（先 cargo build --release 再 start.bat）');
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
          if (!(await confirmAsk(`插件「${ext.title || from}」要写工作区文件：\n\n${path}（${content.length} 字节）\n\n允许吗？`))) {
            return reply(false, '用户拒绝');
          }
          const r = await fetch('/api/workspace/file', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ path, content }),
          });
          const j = await r.json().catch(() => ({}));
          reply(!!r.ok, r.ok ? { ok: true, path: j.path, bytes: j.bytes } : j.error || '写入失败');
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
          const j = await r.json().catch(() => ({}));
          reply(!!r.ok, r.ok ? { ok: true, bytes: j.bytes } : j.error || '写入失败');
          break;
        }
        case 'storage.delete': {
          if (!perms.includes('storage')) return reply(false, '未声明权限 storage');
          const key = String(params.key || '');
          if (!/^[A-Za-z0-9_-]{1,64}$/.test(key)) return reply(false, '键名只能用字母数字与 - _（≤64 字符）');
          const r = await fetch(`/api/plugin/data?name=${encodeURIComponent(from)}&key=${encodeURIComponent(key)}`, { method: 'DELETE' });
          const j = await r.json().catch(() => ({}));
          reply(!!r.ok, r.ok ? { ok: true, found: j.found } : j.error || '删除失败');
          break;
        }
        // —— P21 只读桥：「用量」插件（plugins/usage 试点）拉当前会话的用量统计 ——
        case 'usage.session': {
          const group = params.group === 'model' ? 'model' : 'provider';
          const range = ['today', '7d', '30d', 'all'].includes(params.range) ? params.range : 'all';
          // 面板口径 = 当前对话：session 由宿主注入（插件拿不到别的会话的也不该看）
          const sid = encodeURIComponent(currentSessionId || '');
          reply(true, await get(`/api/usage?group=${group}&range=${range}&session=${sid}`));
          break;
        }
        // 圆环同源的当前上下文占用（活数据）：面板的「输入 token」是各次请求累计，
        // 和圆环的窗口占用是两个口径——同一张卡里并排放，对照关系自明
        case 'session.usage': {
          reply(true, {
            projected: ctxUsage.projected,
            pressure: ctxUsage.pressure,
            window: ctxUsage.window,
            breakdown: ctxUsage.breakdown,
            api: apiTotals,
          });
          break;
        }
        // —— P21 价格设置（写操作，须声明 usage.prices 权限）——
        case 'usage.prices': {
          reply(true, await get('/api/usage/prices'));
          break;
        }
        case 'usage.prices.save': {
          if (!perms.includes('usage.prices')) return reply(false, '未声明权限 usage.prices');
          const body = {
            prices: Array.isArray(params.prices) ? params.prices : [],
            currency: params.currency === 'CNY' ? 'CNY' : 'USD',
            usd_cny_rate: Number(params.usd_cny_rate) || 7.2,
          };
          const r = await fetch('/api/usage/prices', {
            method: 'PUT',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body),
          });
          const j = await r.json().catch(() => ({}));
          reply(r.ok && j.ok !== false, r.ok ? j : j.error || '保存失败');
          break;
        }
        case 'usage.recompute': {
          if (!perms.includes('usage.prices')) return reply(false, '未声明权限 usage.prices');
          if (!(await confirmAsk(`用当前价格表重算全部历史记录的费用？

之前已记的账会按新价改写（累计花费同步刷新）。`))) {
            return reply(false, '用户取消');
          }
          const r = await fetch('/api/usage/recompute', { method: 'POST' });
          const j = await r.json().catch(() => ({}));
          reply(r.ok && j.ok !== false, r.ok ? j : j.error || '重算失败');
          break;
        }
        // —— P19 只读桥：trace 时间线面板（plugins/trace 试点）拉最近任务事件流 ——
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

  // P10-A trace 时间线已插件化（P19）：plugins/trace 面板 + 宿主只读桥 trace.recent，
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
                <button on:click|stopPropagation={() => { sessMenuId = null; renameModal = { kind: 'session', target: s.id, value: s.title, orig: s.title }; }}>
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
      {#if features.galaxy}
      <button class="foot-btn galaxy-btn" on:click={() => (galaxyOpen = true)} title="记忆库：以星云看每一次会话，支持语义搜索">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="1.6" fill="currentColor" stroke="none"/><ellipse cx="12" cy="12" rx="9.2" ry="3.6" transform="rotate(-24 12 12)"/><circle cx="19.4" cy="8.6" r="1.1" fill="currentColor" stroke="none"/><circle cx="5.2" cy="15.2" r="0.9" fill="currentColor" stroke="none"/></svg>
        记忆库
      </button>
      {/if}
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
    <header class="chat-head" data-tauri-drag-region>
      <div class="head-main">
        <div class="chat-title">{displayTitle}</div>
      </div>
      <div class="head-side">
        <!-- P17：扩展声明的顶栏插槽（点击打开该扩展并把事件交给它）；图标与插件页同一张表 -->
        {#each slotExts as s (s.ext.name + ':' + (s.slot.id || s.slot.slot))}
          <button
            class="head-btn"
            class:active={extOpen && extCurrent && extCurrent.name === s.ext.name}
            title={`插件：${s.slot.title || s.ext.title}`}
            on:click={() => clickSlot(s)}
          >
            {#if extIcon(s.ext.name)}
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">{@html extIcon(s.ext.name)}</svg>
            {:else}
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="7" height="7" rx="2"/><rect x="14" y="3" width="7" height="7" rx="2"/><rect x="3" y="14" width="7" height="7" rx="2"/><path d="M14 17.5h7M17.5 14v7"/></svg>
            {/if}
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

        <!-- 窗口控制（无边框壳，对齐 ZCode：原生黑条撤掉，按钮做进内容行）。
             只有桌面壳里才有 agentWinApi（壳每 2s 注入）；浏览器打开的第二个客户端不显示。
             侧栏展开时按钮在侧栏右端（见 brand 行），这里只兜收起态 -->
        {#if winApi && !rightPanelOpen}
          <div class="win-ctrls">
            <button class="head-btn win-btn" title="最小化" aria-label="最小化" on:click={() => winApi.minimize()}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M6 12h12"/></svg>
            </button>
            <button class="head-btn win-btn" title={winMax ? '还原窗口' : '最大化'} aria-label={winMax ? '还原窗口' : '最大化'} on:click={() => winApi.toggleMax().then(refreshWinMax, refreshWinMax)}>
              {#if winMax}
                <!-- 全屏态：两个叠放矩形 = 还原（对齐 ZCode） -->
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="4.5" y="7.5" width="12" height="12" rx="2"/><path d="M8 7.5V6a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2h-1.5"/></svg>
              {:else}
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><rect x="5.5" y="5.5" width="13" height="13" rx="2"/></svg>
              {/if}
            </button>
            <button class="head-btn win-btn win-close" title="关闭到托盘（服务保持运行）" aria-label="关闭到托盘" on:click={() => winApi.hide()}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M6.5 6.5l11 11M17.5 6.5l-11 11"/></svg>
            </button>
          </div>
        {/if}

      </div>
    </header>

    <!-- 无边框窗边缘拉伸热区（Windows 无原生 resize 边框）：6px 四边+四角，按下起拖 -->
    {#if winApi}
      <div class="win-rz win-rz-n" on:mousedown={() => winApi.startResize('North')} title=""></div>
      <div class="win-rz win-rz-s" on:mousedown={() => winApi.startResize('South')} title=""></div>
      <div class="win-rz win-rz-w" on:mousedown={() => winApi.startResize('West')} title=""></div>
      <div class="win-rz win-rz-e" on:mousedown={() => winApi.startResize('East')} title=""></div>
      <div class="win-rz win-rz-nw" on:mousedown={() => winApi.startResize('NorthWest')} title=""></div>
      <div class="win-rz win-rz-ne" on:mousedown={() => winApi.startResize('NorthEast')} title=""></div>
      <div class="win-rz win-rz-sw" on:mousedown={() => winApi.startResize('SouthWest')} title=""></div>
      <div class="win-rz win-rz-se" on:mousedown={() => winApi.startResize('SouthEast')} title=""></div>
    {/if}

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
                {@const thinkingOpen = thinkingOpenMap[m.id] || false}
                <div class="thinking">
                  <button
                    class="thinking-summary"
                    on:click={() => (thinkingOpenMap = { ...thinkingOpenMap, [m.id]: !thinkingOpen })}
                    title={thinkingOpen ? '收起思考过程' : '展开思考过程'}
                  >
                    <span class="thinking-caret">{thinkingOpen ? '▼' : '▶'}</span>
                    思考过程
                  </button>
                  {#if thinkingOpen}
                    <div class="thinking-body">{m.thinking}</div>
                  {/if}
                </div>
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
            {#if ctxUsage.window > 0}
              <div class="ctx-wrap">
                <svg class="ctx-ring" viewBox="0 0 30 30" width="22" height="22" title="上下文占用（下一次请求的预估）">
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
                    <span>{fmtTok(ctxUsage.projected)}/{fmtTok(ctxUsage.window)}（{ctxPctText}）</span>
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
                  <!-- 口径说清：分子是「下一次请求」的预估（渠道实报锚点 + 之后的上下文增减，
                       裁剪会让它当场回落）；构成行是估算值按圆环百分比分摊（加总 = 圆环） -->
                  <div class="ctx-card-foot">占用 = 最近一次请求的渠道实报（输入+输出）+ 其后的上下文增减；构成行为估算分摊。超过预算自动归档旧工具结果并折叠早期对话</div>
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
                class:active={thinkingOn}
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
                  <button class="bar-item" class:active={thinking === 'off'} on:click={() => pickThinking('off')}>
                    关闭
                  </button>

                  {#if thinkingStyle === 'deepseek' || thinkingStyle === 'openai' || thinkingStyle === 'anthropic'}
                    <!-- P23.2 三态机制：开启后菜单不收起，才展开该厂商预设映射的档位。
                         anthropic：低/中/高=预算 20%/50%/80%；
                         deepseek：低/中/高/最大=reasoning_effort；openai：低/中/高=reasoning_effort -->
                    <button class="bar-item" class:active={thinking === 'on'} on:click={() => pickThinking('on', true)}>
                      开启
                    </button>
                    {#if thinkingOn}
                      <button class="bar-item" class:active={thinking === 'low'} on:click={() => pickThinking('low')}>
                        低
                      </button>
                      <button class="bar-item" class:active={thinking === 'medium'} on:click={() => pickThinking('medium')}>
                        中
                      </button>
                      <button class="bar-item" class:active={thinking === 'high'} on:click={() => pickThinking('high')}>
                        高
                      </button>
                      {#if thinkingStyle === 'deepseek'}
                        <button class="bar-item" class:active={thinking === 'max'} on:click={() => pickThinking('max')}>
                          最大
                        </button>
                      {/if}
                    {/if}
                  {:else if thinkingStyle !== 'none'}
                    <!-- 通义千问 (Qwen)、智谱 GLM（标准端点）、字节豆包、本地服务及通用渠道：
                         官方原生开关，无档位（默认/关闭在上方，开启即收尾） -->
                    <button class="bar-item" class:active={thinkingOn} on:click={() => pickThinking('on')}>
                      开启
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
        {#each rpPluginTabs as t (t.key)}
          <button class:active={rpTab === t.key} on:click={() => openDockPlugin(t)}>{t.title}</button>
        {/each}
        <span class="flex-sp"></span>
        {#if rpTab === 'artifacts'}
          <button class="rp-refresh" title="刷新产物清单" on:click={loadArtifacts}>↻</button>
        {/if}
        <!-- 窗口控制（无边框壳）：右面板开着时贴右侧栏右缘（用户拍板的「侧边栏右边」指这栏）；
             右面板没开时回顶栏 head-side -->
        {#if winApi}
          <span class="rp-win-gap"></span>
          <div class="win-ctrls">
            <button class="head-btn win-btn" title="最小化" aria-label="最小化" on:click={() => winApi.minimize()}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M6 12h12"/></svg>
            </button>
            <button class="head-btn win-btn" title={winMax ? '还原窗口' : '最大化'} aria-label={winMax ? '还原窗口' : '最大化'} on:click={() => winApi.toggleMax().then(refreshWinMax, refreshWinMax)}>
              {#if winMax}
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="4.5" y="7.5" width="12" height="12" rx="2"/><path d="M8 7.5V6a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2h-1.5"/></svg>
              {:else}
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><rect x="5.5" y="5.5" width="13" height="13" rx="2"/></svg>
              {/if}
            </button>
            <button class="head-btn win-btn win-close" title="关闭到托盘（服务保持运行）" aria-label="关闭到托盘" on:click={() => winApi.hide()}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M6.5 6.5l11 11M17.5 6.5l-11 11"/></svg>
            </button>
          </div>
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
        {:else if rpTab.startsWith('ext:')}
          <div class="web-wrap">
            {#if extCurrent}
              <iframe
                class="ext-frame"
                title={extCurrent.title}
                sandbox="allow-scripts"
                bind:this={dockExtFrame}
                src={`/plugins/${encodeURIComponent(extCurrent.name)}/${extCurrent.entry || 'index.html'}`}
                on:load={() => onExtFrameLoad(extCurrent.name)}
              ></iframe>
            {:else}
              <div class="rp-empty">{extList.length || extMsg ? '插件未加载' : '加载中…'}</div>
            {/if}
          </div>
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
              <button class="web-nav" title="更多" on:click={(e) => webToggleMenu(e)}>
                <svg viewBox="0 0 24 24" fill="currentColor"><circle cx="5" cy="12" r="1.8"/><circle cx="12" cy="12" r="1.8"/><circle cx="19" cy="12" r="1.8"/></svg>
              </button>
              {#if webMenuOpen && !webNative}
                <!-- svelte-ignore a11y-click-events-have-key-events a11y-no-static-element-interactions -->
                <div class="web-menu-backdrop" on:click={webCloseMenu}></div>
                <div class="web-menu">
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
                  src={`/api/browser/frame?id=${encodeURIComponent(webActive.id)}&t=${webShotT}&native=0`}
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
        src={`/plugins/${encodeURIComponent(extCurrent.name)}/${extCurrent.entry || 'index.html'}`}
        on:load={() => onExtFrameLoad(extCurrent.name)}
      ></iframe>
    </aside>
  {/if}

  <!-- P17 扩展间通信：其它已启用扩展常驻后台（不可见），消息由宿主转发。 -->
  {#each enabledExts.filter((e) => e.entry && !(e.ui || []).some((u) => u.slot === 'dock')) as e (e.name)}
    {#if !(extOpen && extCurrent && extCurrent.name === e.name)}
      <iframe
        class="ext-hidden"
        title={`插件后台：${e.title}`}
        sandbox="allow-scripts"
        bind:this={extFrameEls[e.name]}
        src={`/plugins/${encodeURIComponent(e.name)}/${e.entry || 'index.html'}`}
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
          {#if features.roles}<button class:active={settingsTab === 'roles'} on:click={() => pickSettingsTab('roles')}><svg class="nav-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><circle cx="9" cy="8.5" r="3"/><path d="M3.5 19.5c.6-3 2.8-4.5 5.5-4.5s4.9 1.5 5.5 4.5"/><circle cx="17" cy="9.5" r="2.4"/><path d="M16 15.2c2.4.2 3.9 1.6 4.4 4"/></svg> 功能桥接</button>{/if}
          <button class:active={settingsTab === 'memory'} on:click={() => pickSettingsTab('memory')}><svg class="nav-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"><path d="M12 4.5A7.5 7.5 0 0 0 4.5 12c0 2.4 1.1 4.2 2.8 5.5.4.3.7.9.7 1.5v1.5h9V19c0-.6.3-1.2.7-1.5 1.7-1.3 2.8-3.1 2.8-5.5A7.5 7.5 0 0 0 12 4.5Z"/></svg> 记忆</button>
          <button class:active={settingsTab === 'skills'} on:click={() => pickSettingsTab('skills')}><svg class="nav-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M13 2.5 5.5 13.5H11l-1 8L18.5 10H13l1-7.5Z"/></svg> 技能</button>
          <button class:active={settingsTab === 'mcp'} on:click={() => pickSettingsTab('mcp')}><svg class="nav-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M9 7V3.5M15 7V3.5M7 7h10v4a5 5 0 0 1-10 0Z"/><path d="M12 16v5"/></svg> MCP</button>
          <button class:active={settingsTab === 'ext'} on:click={() => pickSettingsTab('ext')}><svg class="nav-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><rect x="3.5" y="4.5" width="17" height="15" rx="2.6"/><path d="M3.5 9.5h17"/><path d="M8 14h5" stroke-linecap="round"/></svg> 插件</button>
          <button class:active={settingsTab === 'schedule'} on:click={() => pickSettingsTab('schedule')}><svg class="nav-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/></svg> 定时</button>
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
              {/if}
            </div>
            {#if wallpaperMsg}<div class="param-hint">{wallpaperMsg}</div>{/if}
          </div>
          {#if features.css_tool}
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
            {#if microCss(microTweaks)}
              <label>微调生成的规则（只读预览）</label>
              <textarea
                class="agents-editor css-injector micro-readonly"
                rows="6"
                spellcheck="false"
                readonly
                value={microCss(microTweaks)}
              ></textarea>
              <div class="sub-row">
                <button class="btn" on:click={clearAllMicro}>清空全部微调规则</button>
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
              <span class="param-hint" style="align-self:center">导入会覆盖现有规则</span>
            </div>
          </div>
          <div class="field">
            <label>界面微调模式</label>
            <div class="sub-row">
              <button class="btn" class:primary={editMode} on:click={() => (editMode = !editMode)}>
                {editMode ? '● 微调中：右击界面元素即可修改（再点关闭）' : '开启微调：右击界面元素修改样式'}
              </button>
            </div>
          </div>
          {/if}
        {/if}
        {#if settingsTab === 'model'}

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
            bind:value={ctxLen}
            title="上下文长度：决定圆环预算与历史裁剪时机"
            on:change={() => autoSave({ context_length: Number(ctxLen) || 0 }, 0)}
          />
          <input
            class="num-input"
            type="number"
            min="0"
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
      {#if settingsTab === 'roles' && features.roles}
      <div class="field roles-section">
        <label>功能桥接</label>
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
        <div class="param-hint">留空 = 自动兜底</div>
      </div>
      {/if}
      <MemoryTab tab={settingsTab} showGalaxy={features.galaxy} on:openGalaxy={() => { settingsOpen = false; galaxyOpen = true; }} />
      <SkillsTab tab={settingsTab} skills={skillsData} loading={skillsLoading} on:reload={loadSkills} />
      <McpTab tab={settingsTab} servers={mcpServers} on:action={(e) => mcpAction(e.detail.action, e.detail.name, e.detail.risk, e.detail.server)} />
      {#if settingsTab === 'ext'}
        <div class="param-hint">来源为 user 的第三方插件要先点「信任」才会加载</div>
        {#if extMsg}<div class="mcp-cmd">{extMsg}</div>{/if}

        <div class="group-label">内置 {5 + builtInExts.length}</div>
        <div class="plug-row" title={gitRuntime?.ok ? `版本 ${gitRuntime.version} · ${gitRuntime.path}` : (gitRuntime?.error || '')}>
          <div class="plug-ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><circle cx="6" cy="6" r="2.4"/><circle cx="6" cy="18" r="2.4"/><circle cx="18" cy="8" r="2.4"/><path d="M6 8.4v7.2M18 10.4c0 3-2.5 3.6-6 3.6"/></svg></div>
          <div class="plug-meta">
            <div class="plug-title">Git</div>
            <div class="plug-desc">{gitRuntime ? (gitRuntime.ok ? `版本控制运行时 · ${gitRuntime.version}` : `不可用 · ${gitRuntime.error}`) : '读取中…'}</div>
          </div>
          <span class="plug-chip" class:warn={gitRuntime && !gitRuntime.ok}>{gitRuntime?.ok ? (gitRuntime.source === 'system' ? '系统 Git' : gitRuntime.source === 'portable' ? '便携版' : '内置 Git') : gitRuntime ? '不可用' : '…'}</span>
          <button class="btn ghost" on:click={loadGitRuntime}>检测</button>
        </div>
        <div class="plug-row" title={sandboxInfo ? `生效档 ${sandboxInfo.effective} · 配置档 ${sandboxInfo.mode_configured} · 直跑开关${sandboxInfo.allow_unsandboxed ? '已开启' : '未开启'}` : ''}>
          <div class="plug-ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><path d="M12 3l7 3v5c0 4.5-3 8.4-7 10-4-1.6-7-5.5-7-10V6z"/></svg></div>
          <div class="plug-meta">
            <div class="plug-title">沙箱</div>
            <div class="plug-desc">{sandboxInfo ? `命令在受限进程树里跑${sandboxInfo.fallback_reason ? ` · ${sandboxInfo.fallback_reason}` : ''}` : '读取中…'}</div>
          </div>
          {#if sandboxProbe}
            <span class="plug-chip" class:warn={sbRunning || !sandboxProbe.ok}>{sbRunning ? '检测中…' : sandboxProbe.ok ? '检测通过' : '检测有失败'}</span>
          {/if}
          <span class="plug-chip">{sandboxInfo ? sandboxInfo.label + (sandboxInfo.effective !== sandboxInfo.requested ? ' · 已回退' : '') : '…'}</span>
          <label class="plug-cpu" title="沙箱命令可用的 CPU 上限（单核的百分比）；默认 80%，0 = 不限">
            CPU <input type="number" min="0" max="100" value={sandboxCpu} on:change={(e) => commitCpu(e.currentTarget.value)} /> %
          </label>
          <!-- 检测 = 一次做完:刷新档位 + 真拉起受限进程跑探针（用户拍板:一行一个按钮） -->
          <button class="btn ghost" on:click={() => { loadSandboxInfo(true); runSandboxProbe(); }} disabled={sbInfoLoading || sbRunning}>检测</button>
        </div>
        {#if sandboxProbe?.error}<div class="plug-sub">{sandboxProbe.error}</div>{/if}
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
        {/if}

        <div class="plug-row" title="给摘要/标题/嵌入/多模态/子代理/预算降级等辅助调用指定专用模型渠道（原「角色分工」）">
          <div class="plug-ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="6" cy="12" r="2.6"/><circle cx="18" cy="12" r="2.6"/><path d="M8.6 12h6.8M8.6 12l2.2-2.2M8.6 12l2.2 2.2M15.4 12l-2.2-2.2M15.4 12l-2.2 2.2"/></svg></div>
          <div class="plug-meta">
            <div class="plug-title">功能桥接</div>
            <div class="plug-desc">辅助调用走专用渠道 · 关掉全部回主渠道（多模态附件退回占位文本）</div>
          </div>
          <button class="switch" class:on={features.roles} role="switch" aria-checked={features.roles} aria-label="功能桥接 启用/停用" on:click={() => toggleFeature('roles')}><span class="knob"></span></button>
        </div>
        <div class="plug-row" title="界面微调引擎（右击元素改样式）与手写 CSS 注入；已调好的样式不受开关影响，重开即恢复">
          <div class="plug-ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 20h4L19 9l-4-4L4 16z"/><path d="M14.5 6.5l3 3"/></svg></div>
          <div class="plug-meta">
            <div class="plug-title">css-ui注入</div>
            <div class="plug-desc">界面微调与手写 CSS · 关掉只收工具，已调样式继续生效</div>
          </div>
          <button class="switch" class:on={features.css_tool} role="switch" aria-checked={features.css_tool} aria-label="css-ui注入 启用/停用" on:click={() => toggleFeature('css_tool')}><span class="knob"></span></button>
        </div>
        <div class="plug-row" title="记忆库可视化与语义检索入口；记忆功能本体不受开关影响">
          <div class="plug-ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="1.4" fill="currentColor" stroke="none"/><ellipse cx="12" cy="12" rx="8.4" ry="3.2" transform="rotate(-24 12 12)"/><circle cx="18.8" cy="9" r="1" fill="currentColor" stroke="none"/><circle cx="5.6" cy="14.8" r="0.9" fill="currentColor" stroke="none"/></svg></div>
          <div class="plug-meta">
            <div class="plug-title">星云</div>
            <div class="plug-desc">记忆库可视化与检索入口 · 关掉不影响记忆功能</div>
          </div>
          <button class="switch" class:on={features.galaxy} role="switch" aria-checked={features.galaxy} aria-label="星云 启用/停用" on:click={() => toggleFeature('galaxy')}><span class="knob"></span></button>
        </div>
        <div class="plug-row" title="开 = 检测到网页断开（关页面/断网/刷新）就取消本轮任务，不再继续烧 token；关 = 任务在后台跑完，刷新页面回来能接着聊">
          <div class="plug-ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M10 13a4 4 0 0 0 5.7 0l2.3-2.3a4 4 0 0 0-5.7-5.7L11 6.3"/><path d="M14 11a4 4 0 0 0-5.7 0L6 13.3a4 4 0 0 0 5.7 5.7l1.3-1.3"/><path d="M4 4l16 16"/></svg></div>
          <div class="plug-meta">
            <div class="plug-title">断线取消</div>
            <div class="plug-desc">网页断开即停本轮 · 关则后台跑完（刷新可续聊）</div>
          </div>
          <button class="switch" class:on={features.cancel_on_disconnect} role="switch" aria-checked={features.cancel_on_disconnect} aria-label="断线取消 启用/停用" on:click={() => toggleFeature('cancel_on_disconnect')}><span class="knob"></span></button>
        </div>

        {#each builtInExts as e}
          <div class="plug-row" title={extTip(e)}>
            <div class="plug-ico">
              {#if extIcon(e.name)}<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">{@html extIcon(e.name)}</svg>{:else}{(e.title || e.name || '?').slice(0, 1).toUpperCase()}{/if}
            </div>
            <div class="plug-meta">
              <div class="plug-title">{e.title}</div>
              <div class="plug-desc">{e.description || e.dir}</div>
            </div>
            {#if e.draft}<span class="plug-chip warn">草稿</span>{/if}
            {#if e.origin === 'model_generated'}<span class="plug-chip">模型自产</span>{/if}
            {#if e.origin === 'user' && !e.trusted}<span class="plug-chip warn" title="第三方来源：未记录内容指纹，暂不加载">未验签</span>{/if}
            {#if e.source === 'workspace'}<span class="plug-chip" title={e.shadowed ? '同名内置插件被工作区这份覆盖' : ''}>{e.shadowed ? '覆盖内置' : '工作区'}</span>{/if}
            {#if !e.ok}<span class="plug-chip warn">加载失败</span>{/if}
            <span class="flex-sp"></span>
            {#if e.draft}
              <button class="btn ghost" on:click={() => promoteExt(e)}>转正</button>
              <button class="btn ghost" on:click={() => discardExt(e)}>丢弃</button>
            {:else}
              {#if e.origin === 'user'}
                {#if e.trusted}
                  <button class="btn ghost" on:click={() => trustExt(e, 'unpin')} title="取消后该插件立即停止加载">取消信任</button>
                {:else}
                  <button class="btn primary" on:click={() => trustExt(e, 'pin')}>信任</button>
                {/if}
              {/if}
              {#if e.settings_entry}<button class="btn ghost" on:click={() => openExtPage(e, e.settings_entry)} disabled={!e.ok || e.enabled === false}>设置</button>{/if}
              {#if e.entry}<button class="btn ghost" on:click={() => openExt(e)} disabled={!e.ok || e.enabled === false}>打开</button>{/if}
              <button class="switch" class:on={e.enabled !== false} role="switch" aria-checked={e.enabled !== false} aria-label="{e.title} 启用/停用" on:click={() => toggleExt(e)}><span class="knob"></span></button>
            {/if}
          </div>
          {#if e.error}<div class="plug-sub">{e.error}</div>{/if}
        {/each}

        {#if thirdPartyExts.length}
          <div class="group-label">第三方 {thirdPartyExts.length}</div>
          {#each thirdPartyExts as e}
            <div class="plug-row" title={extTip(e)}>
              <div class="plug-ico">
                {#if extIcon(e.name)}<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">{@html extIcon(e.name)}</svg>{:else}{(e.title || e.name || '?').slice(0, 1).toUpperCase()}{/if}
              </div>
              <div class="plug-meta">
                <div class="plug-title">{e.title}</div>
                <div class="plug-desc">{e.description || e.dir}</div>
              </div>
              {#if e.draft}<span class="plug-chip warn">草稿</span>{/if}
              {#if e.origin === 'model_generated'}<span class="plug-chip">模型自产</span>{/if}
              {#if e.origin === 'user' && !e.trusted}<span class="plug-chip warn" title="第三方来源：未记录内容指纹，暂不加载">未验签</span>{/if}
              {#if e.source === 'workspace'}<span class="plug-chip" title={e.shadowed ? '同名内置插件被工作区这份覆盖' : ''}>{e.shadowed ? '覆盖内置' : '工作区'}</span>{/if}
              {#if !e.ok}<span class="plug-chip warn">加载失败</span>{/if}
              <span class="flex-sp"></span>
              {#if e.draft}
                <button class="btn ghost" on:click={() => promoteExt(e)}>转正</button>
                <button class="btn ghost" on:click={() => discardExt(e)}>丢弃</button>
              {:else}
                {#if e.origin === 'user'}
                  {#if e.trusted}
                    <button class="btn ghost" on:click={() => trustExt(e, 'unpin')} title="取消后该插件立即停止加载">取消信任</button>
                  {:else}
                    <button class="btn primary" on:click={() => trustExt(e, 'pin')}>信任</button>
                  {/if}
                {/if}
                {#if e.settings_entry}<button class="btn ghost" on:click={() => openExtPage(e, e.settings_entry)} disabled={!e.ok || e.enabled === false}>设置</button>{/if}
                {#if e.entry}<button class="btn ghost" on:click={() => openExt(e)} disabled={!e.ok || e.enabled === false}>打开</button>{/if}
                <button class="switch" class:on={e.enabled !== false} role="switch" aria-checked={e.enabled !== false} aria-label="{e.title} 启用/停用" on:click={() => toggleExt(e)}><span class="knob"></span></button>
              {/if}
            </div>
            {#if e.error}<div class="plug-sub">{e.error}</div>{/if}
          {/each}
        {:else if !extList.length}
          <div class="menu-empty">
            还没有插件。在 <code>{extInfo.workspace_dir}</code> 下建一个目录（内含 manifest.json
            与 index.html）即可。
          </div>
        {/if}
        <div class="param-hint">目录：内置 {extInfo.app_dir || '（读取中）'} ｜ 工作区 {extInfo.workspace_dir || '（读取中）'}</div>
      {/if}

      <ScheduleTab tab={settingsTab} workspace={workspace} recentWorkspaces={recentWorkspaces} />

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
        <span class="hint">插入 @相对路径（模型自行读取）</span>
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
<ConfirmModal
  modal={confirmModal}
  on:answer={(e) => {
    const resolve = confirmModal && confirmModal.resolve;
    confirmModal = null;
    if (resolve) resolve(e.detail);
  }}
/>

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
    /* 无边框壳：头部空白处可拖拽移动窗口（data-tauri-drag-region 已加） */
    -webkit-app-region: drag;
  }
  .chat-head button, .chat-head input, .chat-head .plug-chip {
    -webkit-app-region: no-drag;
  }
  /* 窗口控制按钮组（─/▢/✕）：ZCode 同款，纯文字 tooltip，hover 才显底色 */
  .win-ctrls {
    display: flex;
    align-items: center;
    gap: 2px;
    margin-left: 4px;
  }
  .win-btn {
    width: 30px;
    height: 30px;
    padding: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: 6px;
    color: var(--g5);
  }
  .rp-win-gap { width: 8px; flex: none; }
  /* 右栏 tabs 行里的窗口按钮：.rp-tabs button 的特异性(0,1,1)会压过 .win-btn(0,1,0)，
     把 padding 撑坏、图标缩成小不点——这里用更高特异性钉死尺寸 */
  .rp-tabs .win-btn {
    width: 28px;
    height: 28px;
    padding: 0;
    flex: none;
    display: inline-flex;
    align-items: center;
    justify-content: center;
  }
  .rp-tabs .win-btn svg {
    width: 17px;
    height: 17px;
  }
  .win-btn:hover {
    background: var(--g3);
    color: var(--g7);
  }
  .win-close:hover {
    background: #c42b1c;
    color: #fff;
  }
  /* 无边框边缘拉伸热区：四边 6px + 四角 10px 见方，fixed 贴着视口边缘 */
  .win-rz { position: fixed; z-index: 900; }
  .win-rz-n { top: 0; left: 10px; right: 10px; height: 6px; cursor: ns-resize; }
  .win-rz-s { bottom: 0; left: 10px; right: 10px; height: 6px; cursor: ns-resize; }
  .win-rz-w { left: 0; top: 10px; bottom: 10px; width: 6px; cursor: ew-resize; }
  .win-rz-e { right: 0; top: 10px; bottom: 10px; width: 6px; cursor: ew-resize; }
  .win-rz-nw { top: 0; left: 0; width: 10px; height: 10px; cursor: nwse-resize; }
  .win-rz-ne { top: 0; right: 0; width: 10px; height: 10px; cursor: nesw-resize; }
  .win-rz-sw { bottom: 0; left: 0; width: 10px; height: 10px; cursor: nesw-resize; }
  .win-rz-se { bottom: 0; right: 0; width: 10px; height: 10px; cursor: nwse-resize; }
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
  /* 「⋯」菜单（仅无头模式：HTML 下拉盖在截图上，z-index 正常）。
     shell 模式不用这个浮层——菜单是壳里一个透明置顶的 webview（见 webToggleMenu：
     浮在网页之上、网页不动），因为 child webview 是 HWND，HTML 永远盖不过它。 */
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
  .thinking-summary {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 0;
    border: none;
    background: none;
    cursor: pointer;
    font-size: 12px;
    color: var(--g6);
    user-select: none;
    font-family: inherit;
  }
  .thinking-caret {
    font-size: 10px;
    line-height: 1;
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
  /* —— 插件页（用户 2026-09-30 指示：学 ZCode 紧凑行——单行 + 开关，Git/沙箱并入「内置能力」，撤独立「运行状态」卡） —— */
  .group-label {
    margin: 16px 2px 8px;
    font-size: 12px;
    color: var(--g5);
  }
  .plug-row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 9px 12px;
    border: 1px solid var(--g3);
    border-radius: 10px;
    margin-bottom: 6px;
    background: var(--g2);
  }
  .plug-ico {
    flex: none;
    width: 34px;
    height: 34px;
    border-radius: 9px;
    background: var(--veil);
    display: flex;
    align-items: center;
    justify-content: center;
    font-weight: 700;
    font-size: 15px;
    color: var(--g6);
  }
  .plug-ico svg {
    width: 18px;
    height: 18px;
  }
  .plug-meta {
    flex: 1 1 auto;
    min-width: 0;
  }
  .plug-title {
    font-weight: 650;
    font-size: 13.5px;
  }
  .plug-desc {
    font-size: 12px;
    color: var(--g5);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .plug-chip {
    flex: none;
    font-size: 11px;
    line-height: 1;
    padding: 4px 9px;
    border-radius: 999px;
    border: 1px dashed var(--g4);
    color: var(--g5);
    white-space: nowrap;
  }
  .plug-chip.warn {
    color: var(--tint-orange-fg);
    border-color: currentColor;
    border-style: solid;
  }
  .plug-sub {
    font-size: 12px;
    color: var(--tint-orange-fg);
    margin: -2px 4px 8px 46px;
    word-break: break-all;
  }
  .plug-row .btn {
    flex: none;
    padding: 4px 10px;
    font-size: 12px;
  }
  .plug-cpu {
    flex: none;
    display: flex;
    align-items: center;
    gap: 4px;
    font-size: 12px;
    color: var(--g5);
    white-space: nowrap;
  }
  .plug-cpu input {
    width: 52px;
    padding: 4px 6px;
    font-size: 12px;
    border: 1px solid var(--g3);
    border-radius: 6px;
    background: var(--g1);
    color: var(--g7);
    text-align: right;
  }
  .plug-cpu input::-webkit-outer-spin-button,
  .plug-cpu input::-webkit-inner-spin-button {
    -webkit-appearance: none;
    margin: 0;
  }
  .switch {
    flex: none;
    position: relative;
    width: 36px;
    height: 20px;
    border-radius: 999px;
    border: none;
    background: var(--g4);
    cursor: pointer;
    padding: 0;
    transition: background 0.15s;
  }
  .switch .knob {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: #fff;
    transition: left 0.15s;
  }
  .switch.on {
    background: var(--brand);
  }
  .switch.on .knob {
    left: 18px;
  }
</style>


