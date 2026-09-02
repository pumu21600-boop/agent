// 冒烟测试用 mock 渠道（不是 provider 实现，仅供无 key 时打通流式 + 工具循环）。
// 用法：node scripts/mock_provider.mjs
// 工具循环剧本：最后一条 user 消息含「读」或带 tools 参数 → 返回 read_file 工具调用；
// 请求里已含 tool 消息 → 返回最终回答。其余走普通流式回复。
import http from 'node:http';

const PORT = 9399;
const CHUNKS = ['你好，', '这是', '模拟渠道', '的流式回复。', '（中文测试 OK）'];

function sendSSE(res, obj) {
  res.write(`data: ${JSON.stringify(obj)}\n\n`);
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
        const loopForever = lastUser && /死循环/.test(lastUser.content || '');
        const wantsTool = hasToolsParam && (loopForever || !hasToolMsg);
        res.writeHead(200, {
          'Content-Type': 'text/event-stream',
          'Cache-Control': 'no-cache',
        });
        if (wantsTool) {
          // 返回 read_file 工具调用（分片模拟流式 delta.tool_calls）
          sendSSE(res, {
            choices: [
              {
                index: 0,
                delta: {
                  tool_calls: [
                    {
                      index: 0,
                      id: 'call-1',
                      type: 'function',
                      function: { name: 'read_file', arguments: '' },
                    },
                  ],
                },
              },
            ],
          });
          const pathMatch = (lastUser?.content || '').match(/读\s*([^\s，。]+)/);
          const args = JSON.stringify({ path: pathMatch ? pathMatch[1] : 'prompts/system.md' });
          for (let i = 0; i < args.length; i += 8) {
            sendSSE(res, {
              choices: [
                {
                  index: 0,
                  delta: {
                    tool_calls: [
                      { index: 0, function: { arguments: args.slice(i, i + 8) } },
                    ],
                  },
                },
              ],
            });
          }
          sendSSE(res, { choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] });
          res.write('data: [DONE]\n\n');
          res.end();
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
