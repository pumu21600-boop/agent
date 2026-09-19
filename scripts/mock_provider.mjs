// 冒烟测试用 mock 渠道（不是 provider 实现，仅供无 key 时打通流式 + 工具循环）。
// 用法：node scripts/mock_provider.mjs
// 剧本（按最后一条 user 消息判断，且只在请求带 tools 时生效）：
//   「读 <路径>」  → read_file 工具调用（不写路径则读 prompts/system.md）
//   「编辑 <路径>」→ edit_file 工具调用（old=「状态：TODO」→ new=「状态：已完成」）
//   「死循环」     → 每轮都返回工具调用（验证步数上限 / 取消）
//   「慢循环」     → 同死循环，但每步先等 1.5s（验证运行中插话 / steering）
//   「慢流」       → 30 片 × 300ms 的长流（验证中途取消）
//   其余          → 普通流式回复；已回填 tool 消息 → 返回最终回答
// 注意：工具剧本必须**显式触发**（不能"带 tools 参数就吐工具调用"）——否则任何
// 普通消息都会被卷进工具循环，慢流 / 纯流式冒烟全测不到（P16 真机踩过）。
import http from 'node:http';

const PORT = 9399;
let readCalls = 0; // 慢循环交替路径用（避免撞核心循环的重复工具调用检测）
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
        // 工具结果回填后（hasToolMsg）不再吐工具调用，直接走最终回答；死循环/慢循环剧本除外
        const wantsTool =
          hasToolsParam && (loopForever || slowLoop || (!hasToolMsg && /读|编辑|插件搜索/.test(lastText)));
        res.writeHead(200, {
          'Content-Type': 'text/event-stream',
          'Cache-Control': 'no-cache',
        });
        if (wantsTool) {
          if (!loopForever && /插件搜索/.test(lastText)) {
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
            const pathMatch = lastText.match(/读\s*([^\s，。]+)/);
            const path = pathMatch ? pathMatch[1] : 'prompts/system.md';
            readCalls += 1;
            const alt = readCalls % 2 === 0 ? 'config.toml' : path;
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
