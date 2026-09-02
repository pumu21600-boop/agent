// 冒烟测试用 mock 渠道（不是 provider 实现，仅供无 key 时打通流式链路）。
// 用法：node scripts/mock_provider.mjs
// 然后：DEEPSEEK_BASE_URL=http://127.0.0.1:9399 DEEPSEEK_API_KEY=sk-test cargo run
import http from 'node:http';

const PORT = 9399;
const CHUNKS = ['你好，', '这是', '模拟渠道', '的流式回复。', '（中文测试 OK）'];

http
  .createServer((req, res) => {
    if (req.method === 'GET' && req.url.includes('/models')) {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(
        JSON.stringify({
          object: 'list',
          data: [
            { id: 'mock-chat' },
            { id: 'mock-reasoner' },
            { id: 'DeepSeek-V3' },
          ],
        })
      );
    } else if (req.method === 'POST' && req.url.includes('/chat/completions')) {
      let body = '';
      req.on('data', (c) => (body += c));
      req.on('end', () => {
        console.log('[mock] request body:', body);
        res.writeHead(200, {
          'Content-Type': 'text/event-stream',
          'Cache-Control': 'no-cache',
        });
        let i = 0;
        const timer = setInterval(() => {
          if (i < CHUNKS.length) {
            const d = JSON.stringify({
              id: 'mock',
              choices: [{ index: 0, delta: { content: CHUNKS[i++] } }],
            });
            res.write(`data: ${d}\n\n`);
          } else {
            res.write('data: [DONE]\n\n');
            res.end();
            clearInterval(timer);
          }
        }, 120);
      });
    } else {
      res.writeHead(404);
      res.end();
    }
  })
  .listen(PORT, () => console.log(`[mock] provider on http://127.0.0.1:${PORT}`));
