// MCP Streamable HTTP 冒烟 server（施工单 021）—— 本地 mini server，验证 http transport 全链路。
//
// 用法：
//   node evals/mcp-http-server.mjs            # 127.0.0.1:9401，JSON 响应
//   node evals/mcp-http-server.mjs --sse      # SSE 响应（event: message + data: 帧）
//   node evals/mcp-http-server.mjs --port 9402
//
// 接入：设置 → MCP → 添加 server，transport 选 http，url 填 http://127.0.0.1:9401/mcp
//
// 实现的是协议里最小的 tools 子集：initialize / notifications/initialized / tools/list / tools/call。
// 工具两个：echo（回吐 text 参数）、now（返回当前时间）。会话头按规范回带（首次 initialize 发放）。

import http from 'node:http';

const argv = process.argv.slice(2);
const useSse = argv.includes('--sse');
// 2026-10-08 修：--port 取值零校验——漏取值/非数字会 listen 在 NaN 上（日志还照印
// listening on :NaN），等号写法 --port=9402 也不被识别（indexOf 匹配不到）静默回落 9401。
function parsePort() {
  const eq = argv.find((a) => a.startsWith('--port='));
  if (eq) return Number(eq.slice('--port='.length));
  const i = argv.indexOf('--port');
  if (i < 0) return 9401;
  if (i + 1 >= argv.length) {
    console.error('[mini-mcp] --port 漏了取值（示例：--port 9402）');
    process.exit(2);
  }
  const n = Number(argv[i + 1]);
  if (!Number.isInteger(n) || n < 1 || n > 65535) {
    console.error(`[mini-mcp] --port 取值非法: ${argv[i + 1]}（要 1-65535 的整数）`);
    process.exit(2);
  }
  return n;
}
const PORT = parsePort();
const SESSION_ID = 'mini-http-' + Math.random().toString(36).slice(2, 8);
// 请求体上限（本地冒烟 server 也不做无界累加——一个 POST 把内存吃满太容易）
const MAX_BODY = 1024 * 1024;

const TOOLS = [
  {
    name: 'echo',
    description: '回吐传入的 text（冒烟用）',
    inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] },
  },
  {
    name: 'now',
    description: '返回本地当前时间（ISO 字符串）',
    inputSchema: { type: 'object' },
  },
];

function handle(msg) {
  const id = msg.id ?? null;
  switch (msg.method) {
    case 'initialize':
      return {
        result: {
          protocolVersion: '2024-11-05',
          // tools + resources + prompts（P17：资源与提示词子集）
          capabilities: { tools: {}, resources: {}, prompts: {} },
          serverInfo: { name: 'mini-http-mcp', version: '0.1.0' },
        },
      };
    case 'tools/list':
      return { result: { tools: TOOLS } };
    case 'resources/list':
      return {
        result: {
          resources: [
            { uri: 'notes://readme', name: 'README 摘要', mimeType: 'text/markdown' },
            { uri: 'notes://changelog', name: '变更记录', mimeType: 'text/markdown' },
          ],
        },
      };
    case 'resources/read': {
      const uri = msg.params?.uri;
      const text = uri === 'notes://readme' ? '# 迷你 MCP 资源\n这是 resources/read 返回的正文。' : '';
      if (!text) return { error: { code: -32602, message: `unknown resource: ${uri}` } };
      return { result: { contents: [{ uri, mimeType: 'text/markdown', text }] } };
    }
    case 'prompts/list':
      return { result: { prompts: [{ name: 'summarize', description: '把给定文本压成三句话' }] } };
    case 'prompts/get': {
      const name = msg.params?.name;
      if (name !== 'summarize') return { error: { code: -32602, message: `unknown prompt: ${name}` } };
      const text = msg.params?.arguments?.text || '(空)';
      return {
        result: {
          description: '把给定文本压成三句话',
          messages: [{ role: 'user', content: { type: 'text', text: `把下面内容压成三句话：\n${text}` } }],
        },
      };
    }
    case 'tools/call': {
      const name = msg.params?.name;
      const args = msg.params?.arguments || {};
      if (name === 'echo') {
        return { result: { content: [{ type: 'text', text: String(args.text ?? '') }] } };
      }
      if (name === 'now') {
        return { result: { content: [{ type: 'text', text: new Date().toISOString() }] } };
      }
      return { error: { code: -32602, message: `unknown tool: ${name}` } };
    }
    default:
      return { error: { code: -32601, message: `method not found: ${msg.method}` } };
  }
}

const server = http.createServer((req, res) => {
  if (req.method === 'GET') {
    res.writeHead(405, { 'Content-Type': 'text/plain' });
    res.end('use POST /mcp');
    return;
  }
  // 2026-10-08 修：请求体无上限 + 传输中途出错无错误臂——前者能把内存累满，
  // 后者让请求永远悬挂（无响应也无日志，只能看着客户端干等）。
  let body = '';
  let tooBig = false;
  req.on('error', (e) => {
    console.error(`[mini-mcp] 请求流传输出错: ${e.message}`);
    try {
      res.writeHead(400, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: { code: -32700, message: 'request stream error' } }));
    } catch {
      /* 响应可能已开始写，忽略 */
    }
  });
  req.on('data', (c) => {
    body += c;
    if (body.length > MAX_BODY) {
      tooBig = true;
      req.destroy();
    }
  });
  req.on('end', () => {
    if (tooBig) {
      res.writeHead(413, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: { code: -32600, message: `body too large (>${MAX_BODY})` } }));
      return;
    }
    let msg;
    try {
      msg = JSON.parse(body);
    } catch (e) {
      res.writeHead(400, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: { code: -32700, message: 'parse error' } }));
      return;
    }
    const isNotification = msg.id === undefined || msg.id === null;
    if (isNotification) {
      console.log(`[mini-mcp] notification: ${msg.method}`);
      res.writeHead(202, { 'Content-Type': 'application/json' });
      res.end('');
      return;
    }
    const out = { jsonrpc: '2.0', id: msg.id, ...handle(msg) };
    const headers = { 'Mcp-Session-Id': SESSION_ID };
    if (msg.method === 'initialize') {
      console.log(`[mini-mcp] initialize（发放会话 ${SESSION_ID}）`);
    } else {
      console.log(`[mini-mcp] ${msg.method}（请求带会话头: ${req.headers['mcp-session-id'] || '无'}）`);
    }
    if (useSse) {
      headers['Content-Type'] = 'text/event-stream';
      // 真实 server 会夹心跳/注释帧，这里照做——宿主必须能跳过它们
      res.writeHead(200, headers);
      res.write(': keep-alive\n\n');
      res.write(`event: message\ndata: ${JSON.stringify(out)}\n\n`);
      res.end();
    } else {
      headers['Content-Type'] = 'application/json';
      res.writeHead(200, headers);
      res.end(JSON.stringify(out));
    }
  });
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`[mini-mcp] listening on http://127.0.0.1:${PORT}/mcp （响应模式: ${useSse ? 'SSE' : 'JSON'}）`);
});
