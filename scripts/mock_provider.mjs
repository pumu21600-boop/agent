// 冒烟测试用 mock 渠道（不是 provider 实现，仅供无 key 时打通流式 + 工具循环）。
// 用法：node scripts/mock_provider.mjs
// 剧本（按最后一条 user 消息判断，且只在请求带 tools 时生效）：
//   「读 <路径>」  → read_file 工具调用（不写路径则读 prompts/system.md）
//   「编辑 <路径>」→ edit_file 工具调用（old=「状态：TODO」→ new=「状态：已完成」）
//  「派子代理」→ subagent_spawn 工具调用（：子代理独立会话跑 read_file 闭环）
//   「派两个」      → 一轮两个 subagent_spawn（验证并行派发与并发度排队）
//   「重试失败」    → 连续两轮相同的 read_file（必失败：文件不存在），验证「失败后重试
//                   不判死循环」——旧护栏会当场终止并弹天书横幅（用户反馈）
//   「死循环」     → 每轮都返回工具调用（验证步数上限）
//   「慢循环」     → 同死循环，但每步先等 1.5s（验证运行中插话 / steering）
//   「慢流」       → 30 片 × 300ms 的长流（验证中途取消）
//   其余          → 普通流式回复；已回填 tool 消息 → 返回最终回答
// 注意：工具剧本必须**显式触发**（不能"带 tools 参数就吐工具调用"）——否则任何
// 普通消息都会被卷进工具循环，慢流 / 纯流式冒烟全测不到（真机踩过）。
// 另：子代理剧本必须带 !hasToolMsg 守卫——子代理那轮的 user 消息里也含「子代理」字样，
// 不加守卫会让子代理再派子代理（v1 禁嵌套，真机上就演变成递归刷额度）。
import http from 'node:http';

const PORT = 9399;

const CHUNKS = ['你好，', '这是', '模拟渠道', '的流式回复。', '（中文测试 OK）'];

function sendSSE(res, obj) {
  res.write(`data: ${JSON.stringify(obj)}\n\n`);
}

/** SPA 测试页（web_fetch 渲染回退 e2e 用）：静态只有空骨架，正文靠客户端 JS 水合——
  静态抓取只能拿到 < 200 字的壳，必须回落浏览器渲染才看得到正文。 */
const SPA_HTML = `<!doctype html><html><head><title>SPA 演示页</title></head>
<body><div id="app">加载中…</div>
<script>
setTimeout(function () {
  var app = document.getElementById('app');
  app.textContent = '这是 JS 渲染后才出现的正文：据传 2026 年的缓存命中率已经卷到 99.6%，' +
    '而静态抓取只能看到一个空壳。页面水合完成，innerText 可以读了。';
}, 300);
</script></body></html>`;

/** 收尾块带 usage（真实渠道都有）：网关取 prompt_tokens 做圆环真实占用/台账。
 *  大小按请求体 JSON 长度 /3 拟真（含 tools 定义，量级与真渠道一致）。 */
let curParsed = null; // 当前请求体（emitToolCall 拟真 usage 用；并发子代理场景下允许串扰，假数字无碍）
function usageOf(parsed) {
  return { prompt_tokens: Math.max(1, Math.round(JSON.stringify(parsed || curParsed || {}).length / 3)), completion_tokens: 41 };
}

// —— Anthropic 提示缓存拟真（2026-10-20 缓存命中率 e2e 冒烟用）——
// 真 Anthropic 语义 = 「最长已缓存前缀命中」：新请求与某条历史请求归一化后
// 逐字符相同的前缀算 cache_read，只对新增尾部计新鲜 input。归一化规则对齐
// 真渠道：string content ≡ [{type:text,text}]（网关只给末条消息挂断点，上一轮
// 末条到下轮会解包回字符串，两种形态是同一内容）；cache_control 是缓存元数据，
// 不计入内容比较。前缀被打断（工具顺序变/动态内容前移）→ 命中率当场掉。
const anthropicCacheHistory = [];
function normalizeForCache(parsed) {
  const c = JSON.parse(JSON.stringify(parsed));
  const strip = (blocks) => {
    if (Array.isArray(blocks)) for (const b of blocks) delete b.cache_control;
  };
  strip(c.system);
  strip(c.tools);
  for (const m of c.messages || []) {
    if (typeof m.content === 'string') m.content = [{ type: 'text', text: m.content }];
    strip(m.content);
  }
  // 按 Anthropic 的**前缀顺序**重组再比：system → tools → messages。
  // 请求 JSON 的键序把易变的 messages 排在最前（max_tokens/messages/model/stream/
  // system/tools），字符级 LCP 会在对话追加点提前断掉——必须按语义前缀序比。
  // model/max_tokens/stream 不参与前缀比较（不是被缓存的前缀内容）。
  return JSON.stringify({ system: c.system ?? null, tools: c.tools ?? null, messages: c.messages ?? [] });
}
function longestCachedPrefix(cur) {
  let best = 0;
  for (const prev of anthropicCacheHistory) {
    const max = Math.min(prev.length, cur.length);
    let i = 0;
    while (i < max && prev[i] === cur[i]) i++;
    if (i > best) best = i;
  }
  return best;
}
/** Anthropic 口径 usage：input_tokens 不含 cache_read；cached = 命中前缀。 */
function anthropicCacheUsage(parsed) {
  const norm = normalizeForCache(parsed);
  const lcp = longestCachedPrefix(norm);
  anthropicCacheHistory.push(norm);
  if (anthropicCacheHistory.length > 64) anthropicCacheHistory.shift();
  const total = Math.max(1, Math.round(norm.length / 3));
  const cached = Math.min(total, Math.round(lcp / 3));
  return { input_tokens: total - cached, cache_read_input_tokens: cached };
}
function finishWithUsage(res, parsed) {
  sendSSE(res, {
    choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
    usage: usageOf(parsed),
  });
  res.write('data: [DONE]\n\n');
  res.end();
}

/** 分片模拟流式 delta.tool_calls（和真实渠道同形）。 */
function emitToolCall(res, name, args) {  sendSSE(res, {
    choices: [
      {
        index: 0,
        delta: {
          tool_calls: [
            { index: 0, id: 'call-1', type: 'function', function: { name, arguments: '' } },
          ],
        },
      },
    ],
  });
  const text = JSON.stringify(args);
  for (let i = 0; i < text.length; i += 8) {
    sendSSE(res, {
      choices: [
        { index: 0, delta: { tool_calls: [{ index: 0, function: { arguments: text.slice(i, i + 8) } }] } },
      ],
    });
  }
  sendSSE(res, { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] });
  // 工具回合也报 usage（真实渠道如此）——圆环真实值才能在多步任务中途生效
  sendSSE(res, { choices: [], usage: usageOf() });
  res.write('data: [DONE]\n\n');
  res.end();
}

/** 长流剧本：慢速多片，供中途取消测试。 */
function emitSlowStream(res) {
  let i = 0;
  const timer = setInterval(() => {
    i += 1;
    if (i <= 30) {
      sendSSE(res, { choices: [{ index: 0, delta: { content: `片${i}。` } }] });
    } else {
      res.write('data: [DONE]\n\n');
      res.end();
      clearInterval(timer);
    }
  }, 300);
}

http
  .createServer((req, res) => {
    if (req.method === 'GET' && req.url.includes('/models')) {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(
        JSON.stringify({
          object: 'list',
          data: [{ id: 'mock-chat' }, { id: 'mock-reasoner' }, { id: 'DeepSeek-V3' }],
        })
      );
    } else if (req.method === 'POST' && req.url.includes('/chat/completions')) {
      let body = '';
      req.on('data', (c) => (body += c));
      req.on('end', () => {
        console.log('[mock] request body:', body);
        const parsed = JSON.parse(body);
        curParsed = parsed;
        const msgs = parsed.messages || [];
        const hasToolMsg = msgs.some((m) => m.role === 'tool');
        const hasToolsParam = Array.isArray(parsed.tools) && parsed.tools.length > 0;
        // 死循环剧本：最后一条 user 消息含「死循环」时，每轮都返回工具调用（验证步数上限）
        const lastUser = [...msgs].reverse().find((m) => m.role === 'user');
        const lastText = lastUser?.content || '';
        const loopForever = /死循环/.test(lastText);
        // 慢循环：同死循环，但每步先等 1.5s——留出「运行中插话」的窗口（验证 steering）
        const slowLoop = /慢循环/.test(lastText);
        // 多步剧本：「清洗」= read_file → write_file → 最终回答；
        // 「浏览 <url>」= browser navigate → click → snapshot → 最终回答（按 example.com 语义点链接）
        const browseUrl = (lastText.match(/浏览\s*(\S+)/) || [])[1];
        const cleaning = /清洗/.test(lastText);
        // 子代理剧本（必须 !hasToolMsg：子代理那轮的 user 消息也含「子代理」字样）
        const spawnTwo = hasToolsParam && !hasToolMsg && /派两个/.test(lastText);
        const spawnOne = hasToolsParam && !hasToolMsg && /派子代理/.test(lastText);
        // 重试失败剧本：前两轮发同一个必失败的 read_file（文件不存在），验证失败重试放行
        const retryFail = hasToolsParam && /重试失败/.test(lastText);
        const toolMsgCount = msgs.filter((m) => m.role === 'tool').length;
        // 工具结果回填后（hasToolMsg）不再吐工具调用，直接走最终回答；死循环/慢循环/多步剧本除外
        const wantsTool =
          hasToolsParam &&
          (loopForever ||
            slowLoop ||
            browseUrl ||
            cleaning ||
            (!hasToolMsg && /读|编辑|插件搜索/.test(lastText)));
        // 降级剧本：「降级」= 请求带思考字段（thinking/enable_thinking/reasoning_effort/
        // chat_template_kwargs）就 400 拒收；剥参重试（不带思考字段）才 200——验证网关
        // 「400 → 剥参重试 → 登记渠道 → 下一轮人话提示」全链路
        if (/降级/.test(lastText)) {
          const hasThinking = ['thinking', 'enable_thinking', 'reasoning_effort', 'chat_template_kwargs'].some(
            (k) => Object.prototype.hasOwnProperty.call(parsed, k)
          );
          if (hasThinking) {
            res.writeHead(400, { 'Content-Type': 'application/json' });
            res.end(JSON.stringify({ error: { message: 'Unknown parameter: thinking is not supported by this endpoint' } }));
            return;
          }
          res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache' });
          sendSSE(res, { choices: [{ index: 0, delta: { content: '剥参重试成功：这轮没带思考字段。' } }] });
          sendSSE(res, { choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] });
          res.write('data: [DONE]\n\n');
          res.end();
          return;
        }
        res.writeHead(200, {
          'Content-Type': 'text/event-stream',
          'Cache-Control': 'no-cache',
        });
        /** 多步剧本的最终回答（与单步剧本同形：reasoning 分片 + content 分片）。 */
        const emitFinal = (text) => {
          sendSSE(res, { choices: [{ index: 0, delta: { reasoning_content: '剧本步骤已走完，' } }] });
          sendSSE(res, { choices: [{ index: 0, delta: { reasoning_content: '总结回答即可。' } }] });
          for (let i = 0; i < text.length; i += 6) {
            sendSSE(res, { choices: [{ index: 0, delta: { content: text.slice(i, i + 6) } }] });
          }
          finishWithUsage(res, parsed);
        };
        if (retryFail) {
          // 前两轮同一个必失败调用（路径不存在）；第三轮起让模型收尾说话
          if (toolMsgCount < 2) {
            emitToolCall(res, 'read_file', { path: '绝不存在的文件.txt' });
          } else {
            emitFinal('两次读取都失败了：访问超时/文件不存在，建议稍后重试或换个别的方式。（mock 重试失败剧本闭环）');
          }
        } else if (spawnTwo) {
          // 一轮两个派发——验证并行组 + per_parent 上限排队（第二个等第一个完）。
          // 注意 emitToolCall 内部会 end 响应，两个调用必须都用「不收尾」的裸帧写。
          const calls = [
            { id: 'call-1', args: { task: '读 prompts/system.md', label: '子代理甲' } },
            // 乙用慢循环剧本：每步 1.5s，制造「父任务取消时子代理还在跑」的时间窗
            { id: 'call-2', args: { task: '慢循环读 config.toml', label: '子代理乙' } },
          ];
          calls.forEach((c, idx) => {
            sendSSE(res, {
              choices: [
                {
                  index: 0,
                  delta: {
                    tool_calls: [
                      { index: idx, id: c.id, type: 'function', function: { name: 'subagent_spawn', arguments: '' } },
                    ],
                  },
                },
              ],
            });
            const text = JSON.stringify(c.args);
            for (let i = 0; i < text.length; i += 8) {
              sendSSE(res, {
                choices: [
                  { index: 0, delta: { tool_calls: [{ index: idx, function: { arguments: text.slice(i, i + 8) } }] } },
                ],
              });
            }
          });
          sendSSE(res, { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] });
          res.write('data: [DONE]\n\n');
          res.end();
        } else if (spawnOne) {
          emitToolCall(res, 'subagent_spawn', { task: '读 prompts/system.md', label: 'mock 子代理' });
        } else if (wantsTool) {
          if (browseUrl) {
            if (toolMsgCount === 0) {
              emitToolCall(res, 'browser', { method: 'navigate', url: browseUrl });
            } else if (toolMsgCount === 1) {
              // example.com 语义：点击可见文本匹配的链接，验证真实站点语义定位（链接文本以快照为准）
              emitToolCall(res, 'browser', { method: 'click', text: 'Learn more' });
            } else if (toolMsgCount === 2) {
              // 动作后观察周期：再拿一次快照验证点击效果（对齐 browser-use 技能纪律）
              emitToolCall(res, 'browser', { method: 'snapshot' });
            } else if (toolMsgCount === 3) {
              // 收尾：显式关掉浏览器进程（close_browser，承既定约定）
              emitToolCall(res, 'browser', { method: 'close_browser' });
            } else {
              emitFinal('已按剧本完成浏览：打开页面、语义点击链接、复核快照并关闭浏览器。（mock 浏览循环闭环）');
            }
          } else if (cleaning) {
            // 清洗规则（与 eval 任务 csv-clean 一致）：去空行 + 去重复行
            const CLEANED = 'name,age,city\n张三,25,北京\n李四,,上海\n王五,30,';
            if (toolMsgCount === 0) {
              emitToolCall(res, 'read_file', { path: 'data.csv' });
            } else if (toolMsgCount === 1) {
              emitToolCall(res, 'write_file', { path: 'clean.csv', content: CLEANED });
            } else {
              emitFinal('已清洗 data.csv 并写出 clean.csv（去空行、去重复行）。（mock 清洗循环闭环）');
            }
          } else if (!loopForever && /插件搜索/.test(lastText)) {
            // 插件工具剧本：调用目录形态工具插件 wasm__web_search__search，
            // 验证 模型 → 审批门（wasm 工具风险研判）→ WASM 引擎 → 结果回填 全链路
            emitToolCall(res, 'wasm__web_search__search', { query: '插件契约 v1' });
          } else if (!loopForever && /编辑/.test(lastText)) {
            // edit_file 剧本：路径取「编辑」后的 token，替换固定的一行
            const m = lastText.match(/编辑\s*([^\s，。]+)/);
            emitToolCall(res, 'edit_file', {
              path: m ? m[1] : 'notes.txt',
              old_string: '状态：TODO',
              new_string: '状态：已完成',
            });
          } else {
            // read_file 剧本（含死循环/慢循环：每轮都读同一个文件）。
            // 慢循环额外做两件事：每步等 1.5s（留插话窗口）、交替两个路径——
            // 路径写死会让核心循环的「重复工具调用」检测提前终止，跑不满步数。
            // 交替基准用**本请求内**的 tool 消息数（原来是全局计数器，父子多个请求
            // 互相踩会错拍—— 真机踩过，子代理被误判重复调用终止）。
            const pathMatch = lastText.match(/读\s*([^\s，。]+)/);
            const path = pathMatch ? pathMatch[1] : 'prompts/system.md';
            // 交替目标必须与 path 不同：路径本身就是它时，连续两轮仍会撞重复调用检测
            const other = path === 'prompts/system.md' ? 'config.toml' : 'prompts/system.md';
            const alt = toolMsgCount % 2 === 0 ? path : other;
            const readCall = () => emitToolCall(res, 'read_file', { path: alt });
            if (slowLoop) setTimeout(readCall, 1500);
            else readCall();
          }
        } else if (hasToolMsg) {
          // 工具结果已回填 → 最终回答（附带 reasoning 分片，测 thought 事件）
          sendSSE(res, { choices: [{ index: 0, delta: { reasoning_content: "已经拿到文件内容，" } }] });
          sendSSE(res, { choices: [{ index: 0, delta: { reasoning_content: "总结回答即可。" } }] });
          const toolMsg = [...msgs].reverse().find((m) => m.role === 'tool');
          const preview = (toolMsg?.content || '').slice(0, 60);
          const final = `已读取文件。开头是：${preview}\n（mock 工具循环闭环）`;
          for (let i = 0; i < final.length; i += 6) {
            sendSSE(res, { choices: [{ index: 0, delta: { content: final.slice(i, i + 6) } }] });
          }
          finishWithUsage(res, parsed);
        } else if (/慢流/.test(lastText)) {
          emitSlowStream(res);
        } else {
          let i = 0;
          const timer = setInterval(() => {
            if (i < CHUNKS.length) {
              sendSSE(res, {
                id: 'mock',
                choices: [{ index: 0, delta: { content: CHUNKS[i++] } }],
              });
            } else {
              finishWithUsage(res, parsed);
              clearInterval(timer);
            }
          }, 120);
        }
      });
    } else if (req.method === 'GET' && req.url.startsWith('/spa')) {
      // SPA 演示页：web_fetch 渲染回退的靶子（静态=空壳，JS 水合后才出正文）
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end(SPA_HTML);
    } else if (req.method === 'GET' && req.url.startsWith('/static')) {
      // 静态演示页：正文直接在后端 HTML 里（静态抓取就该拿到，不许起浏览器）
      res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
      res.end(
        '<!doctype html><html><head><title>静态演示页</title></head><body><h1>静态正文标题</h1>' +
          '<p>这是服务端直接渲染的长正文，静态抓取一次拿到，不需要浏览器。'.repeat(20) + '</p></body></html>'
      );
    } else if (req.method === 'POST' && req.url.includes('/messages')) {
      // Anthropic Messages 协议 mock：请求按 Messages 形状解析（system 顶层 / content 块 /
      // tool_use·tool_result / input_schema），响应按官方 SSE 事件流吐出
      // （message_start → thinking/text/tool_use 块 → message_delta → message_stop）。
      // 最终回答带「翻译回声」：收到的 tool_result 数量与内容、system 长度——
      // 断言网关真的把 OpenAI 会话形状翻译成了 Messages 形状，而不是碰巧跑通。
      let body = '';
      req.on('data', (c) => (body += c));
      req.on('end', () => {
        console.log('[mock-anthropic] request body:', body);
        const parsed = JSON.parse(body);
        // 提示缓存拟真：按最长已缓存前缀报 cache_read（首轮无历史=全新鲜）
        const cacheUsage = anthropicCacheUsage(parsed);
        const bpCount = JSON.stringify(parsed).match(/"cache_control"/g)?.length || 0;
        console.log(
          `[mock-anthropic] cache: breakpoints=${bpCount} total=${cacheUsage.input_tokens + cacheUsage.cache_read_input_tokens} ` +
            `read=${cacheUsage.cache_read_input_tokens} fresh=${cacheUsage.input_tokens}`
        );
        res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache' });
        const ev = (name, obj) => res.write(`event: ${name}\ndata: ${JSON.stringify(obj)}\n\n`);
        let blockIdx = -1;
        const startBlock = (obj) => {
          blockIdx += 1;
          ev('content_block_start', { type: 'content_block_start', index: blockIdx, content_block: obj });
          return blockIdx;
        };
        const stopBlock = () => ev('content_block_stop', { type: 'content_block_stop', index: blockIdx });
        ev('message_start', {
          type: 'message_start',
          message: {
            usage: {
              input_tokens: cacheUsage.input_tokens,
              cache_creation_input_tokens: 0,
              cache_read_input_tokens: cacheUsage.cache_read_input_tokens,
            },
          },
        });
        const msgs = parsed.messages || [];
        const blockText = (m) =>
          Array.isArray(m.content)
            ? m.content.filter((b) => b.type === 'text').map((b) => b.text).join('')
            : m.content || '';
        const lastUser = [...msgs].reverse().find((m) => m.role === 'user');
        const lastText = blockText(lastUser || {});
        // 网关翻译正确性回声：tool_result 块（挂在 user 消息上）与 system 顶层参数
        const toolResults = [];
        for (const m of msgs) {
          if (m.role === 'user' && Array.isArray(m.content)) {
            for (const b of m.content) if (b.type === 'tool_result') toolResults.push(b);
          }
        }
        const hasToolsParam = Array.isArray(parsed.tools) && parsed.tools.length > 0;
        // 子代理那轮的消息带派发包裹（【子代理任务】），且 brief 会把父任务首句
        // （如「派两个」）带进上下文——剧本匹配必须排除子代理轮，否则子代理会
        // 尝试再派（嵌套，生产由深度闸拦，这里让 mock 别添乱）
        const isChildRound = /【子代理任务/.test(lastText);
        // 子代理剧本（与 OpenAI 分支对齐）
        const spawnTwo = hasToolsParam && !isChildRound && toolResults.length === 0 && /派两个/.test(lastText);
        const spawnThree = hasToolsParam && !isChildRound && toolResults.length === 0 && /派三个/.test(lastText);
        const spawnOne = hasToolsParam && !isChildRound && toolResults.length === 0 && /派子代理/.test(lastText);
        // 慢循环：**每轮**都睡 1.5s 再吐工具调用。注意 Anthropic 协议里工具结果
        // 会并进 user 消息（tool_result 块），lastText 之后恒为空——所以这里搜
        // 全历史而不是末条（OpenAI 协议的 tool 消息不顶掉 user 消息，没这问题）。
        // 只让**子代理轮**慢循环：父任务历史里也会出现乙的任务描述（工具结果回声），
        // 不设 isChildRound 守卫会把父任务也拖进循环（本轮实测踩过）。
        // 抓取剧本：「抓取 <url>」→ web_fetch 工具调用（验静态抓取 + JS 渲染回退）。
        // 带「只要静态」→ render=never（SPA 页也只给静态壳，不许起浏览器）
        const fetchUrl = (lastText.match(/抓取\s*(https?:\/\/\S+)/) || [])[1];
        const fetchNoRender = /只要静态/.test(lastText);
        const spawnFetch = hasToolsParam && !isChildRound && toolResults.length === 0 && !!fetchUrl;
        const slowLoop = isChildRound && /慢循环/.test(JSON.stringify(msgs));
        const wantsTool =
          hasToolsParam && !spawnTwo && !spawnThree && !spawnOne && !spawnFetch && (/读/.test(lastText) || slowLoop);
        // 思考块：无论如何都吐一段（验证 thinking_delta → reasoning_content 通道）
        const ti = startBlock({ type: 'thinking', thinking: '' });
        ev('content_block_delta', {
          type: 'content_block_delta',
          index: ti,
          delta: { type: 'thinking_delta', thinking: '模拟思考：请求形状是 Messages 协议。' },
        });
        stopBlock();
        // 流式吐一个 tool_use 块（官方形状：start → input_json_delta 分片 → stop）
        const emitToolUse = (id, name, input) => {
          const s = startBlock({ type: 'tool_use', id, name, input: {} });
          const args = JSON.stringify(input);
          for (let i = 0; i < args.length; i += 8) {
            ev('content_block_delta', {
              type: 'content_block_delta',
              index: s,
              delta: { type: 'input_json_delta', partial_json: args.slice(i, i + 8) },
            });
          }
          stopBlock();
          return s;
        };
        const endWithToolUse = () =>
          ev('message_delta', { type: 'message_delta', delta: { stop_reason: 'tool_use' }, usage: { output_tokens: 33 } });
        if (spawnThree) {
          // 一轮三个派发：per_parent=2 → 第三个必排队（验 queued 态上屏 + 出队即跑）
          emitToolUse('toolu_mock_a', 'subagent_spawn', { task: '读 prompts/system.md', label: '子代理甲' });
          emitToolUse('toolu_mock_b', 'subagent_spawn', { task: '慢循环读 config.toml', label: '子代理乙' });
          emitToolUse('toolu_mock_c', 'subagent_spawn', { task: '读 prompts/system.md', label: '子代理丙' });
          endWithToolUse();
        } else if (spawnFetch) {
          // 抓取剧本：web_fetch 打 SPA 页——静态只能拿到空壳，auto 档应回落浏览器渲染；
          // 带「只要静态」走 render=never（只给壳、不起浏览器）
          emitToolUse('toolu_fetch_1', 'web_fetch', fetchNoRender ? { url: fetchUrl, render: 'never' } : { url: fetchUrl });
          endWithToolUse();
        } else if (spawnTwo) {
          // 一轮两个派发：甲读文件秒完，乙慢循环（1.5s/步）——并行组 + 取消窗口
          emitToolUse('toolu_mock_a', 'subagent_spawn', { task: '读 prompts/system.md', label: '子代理甲' });
          emitToolUse('toolu_mock_b', 'subagent_spawn', { task: '慢循环读 config.toml', label: '子代理乙' });
          endWithToolUse();
        } else if (spawnOne) {
          emitToolUse('toolu_mock_1', 'subagent_spawn', { task: '读 prompts/system.md', label: 'mock 子代理' });
          endWithToolUse();
        } else if (wantsTool) {
          if (slowLoop) {
            // 睡 1.5s 再吐调用：子代理进程还活着，取消才落得下
            setTimeout(() => {
              emitToolUse('toolu_slow_1', 'read_file', { path: 'prompts/system.md' });
              endWithToolUse();
              ev('message_stop', { type: 'message_stop' });
              res.end();
            }, 1500);
            return;
          }
          emitToolUse('toolu_mock_1', 'read_file', { path: 'prompts/system.md' });
          endWithToolUse();
        } else {
          const echo = toolResults.length
            ? `收到 ${toolResults.length} 条工具结果，第一条开头：${String(toolResults[0]?.content || '').slice(0, 30)}；系统提示 ${String(parsed.system || '').length} 字。`
            : '';
          const text = echo || '你好，这是 Anthropic mock 渠道的流式回复。（中文 OK）';
          const s = startBlock({ type: 'text', text: '' });
          for (let i = 0; i < text.length; i += 6) {
            ev('content_block_delta', {
              type: 'content_block_delta',
              index: s,
              delta: { type: 'text_delta', text: text.slice(i, i + 6) },
            });
          }
          stopBlock();
          ev('message_delta', { type: 'message_delta', delta: { stop_reason: 'end_turn' }, usage: { output_tokens: 55 } });
        }
        ev('message_stop', { type: 'message_stop' });
        res.end();
      });
    } else {
      res.writeHead(404);
      res.end();
    }
  })
  .listen(PORT, () => console.log(`[mock] provider on http://127.0.0.1:${PORT}`));
