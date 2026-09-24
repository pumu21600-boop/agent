// 冒烟测试用 mock 渠道（不是 provider 实现，仅供无 key 时打通流式 + 工具循环）。
// 用法：node scripts/mock_provider.mjs
// 剧本（按最后一条 user 消息判断，且只在请求带 tools 时生效）：
//   「读 <路径>」  → read_file 工具调用（不写路径则读 prompts/system.md）
//   「编辑 <路径>」→ edit_file 工具调用（old=「状态：TODO」→ new=「状态：已完成」）
//   「派子代理」    → subagent_spawn 工具调用（P22-C：子代理独立会话跑 read_file 闭环）
//   「派两个」      → 一轮两个 subagent_spawn（验证并行派发与并发度排队）
//   「死循环」     → 每轮都返回工具调用（验证步数上限）
//   「慢循环」     → 同死循环，但每步先等 1.5s（验证运行中插话 / steering）
//   「慢流」       → 30 片 × 300ms 的长流（验证中途取消）
//   其余          → 普通流式回复；已回填 tool 消息 → 返回最终回答
// 注意：工具剧本必须**显式触发**（不能"带 tools 参数就吐工具调用"）——否则任何
// 普通消息都会被卷进工具循环，慢流 / 纯流式冒烟全测不到（P16 真机踩过）。
// 另：子代理剧本必须带 !hasToolMsg 守卫——子代理那轮的 user 消息里也含「子代理」字样，
// 不加守卫会让子代理再派子代理（P22-C v1 禁嵌套，真机上就演变成递归刷额度）。
import http from 'node:http';

const PORT = 9399;

const CHUNKS = ['你好，', '这是', '模拟渠道', '的流式回复。', '（中文测试 OK）'];

function sendSSE(res, obj) {
  res.write(`data: ${JSON.stringify(obj)}\n\n`);
}

/** 分片模拟流式 delta.tool_calls（和真实渠道同形）。 */
function emitToolCall(res, name, args) {
  sendSSE(res, {
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
        const msgs = parsed.messages || [];
        const hasToolMsg = msgs.some((m) => m.role === 'tool');
        const hasToolsParam = Array.isArray(parsed.tools) && parsed.tools.length > 0;
        // 死循环剧本：最后一条 user 消息含「死循环」时，每轮都返回工具调用（验证步数上限）
        const lastUser = [...msgs].reverse().find((m) => m.role === 'user');
        const lastText = lastUser?.content || '';
        const loopForever = /死循环/.test(lastText);
        // 慢循环：同死循环，但每步先等 1.5s——留出「运行中插话」的窗口（验证 steering）
        const slowLoop = /慢循环/.test(lastText);
        // 多步剧本（P20）：「清洗」= read_file → write_file → 最终回答；
        // 「浏览 <url>」= browser navigate → click → snapshot → 最终回答（按 example.com 语义点链接）
        const browseUrl = (lastText.match(/浏览\s*(\S+)/) || [])[1];
        const cleaning = /清洗/.test(lastText);
        // P22-C 子代理剧本（必须 !hasToolMsg：子代理那轮的 user 消息也含「子代理」字样）
        const spawnTwo = hasToolsParam && !hasToolMsg && /派两个/.test(lastText);
        const spawnOne = hasToolsParam && !hasToolMsg && /派子代理/.test(lastText);
        const toolMsgCount = msgs.filter((m) => m.role === 'tool').length;
        // 工具结果回填后（hasToolMsg）不再吐工具调用，直接走最终回答；死循环/慢循环/多步剧本除外
        const wantsTool =
          hasToolsParam &&
          (loopForever ||
            slowLoop ||
            browseUrl ||
            cleaning ||
            (!hasToolMsg && /读|编辑|插件搜索/.test(lastText)));
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
          sendSSE(res, { choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] });
          res.write('data: [DONE]\n\n');
          res.end();
        };
        if (spawnTwo) {
          // P22-C：一轮两个派发——验证并行组 + per_parent 上限排队（第二个等第一个完）。
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
              // 收尾：显式关掉浏览器进程（close_browser，P21 承 P20 的 browser_close）
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
            // 插件工具剧本（P19）：调用目录形态工具插件 wasm__web_search__search，
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
            // 互相踩会错拍——P22-C 真机踩过，子代理被误判重复调用终止）。
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
          sendSSE(res, { choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] });
          res.write('data: [DONE]\n\n');
          res.end();
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
              res.write('data: [DONE]\n\n');
              res.end();
              clearInterval(timer);
            }
          }, 120);
        }
      });
    } else {
      res.writeHead(404);
      res.end();
    }
  })
  .listen(PORT, () => console.log(`[mock] provider on http://127.0.0.1:${PORT}`));
