#!/usr/bin/env node
// MCP 冒烟用 mini server（施工单 016）——零依赖，stdio JSON-RPC 2.0
// 提供 2 个工具：echo（回显）、now（当前时间）。仅用于协议链路验证。

let buf = '';

process.stdout.write(
  JSON.stringify({ jsonrpc: '2.0', method: 'notifications/log', params: { level: 'info', data: 'mini server up' } }) + '\n'
);

process.stdin.on('data', (d) => {
  buf += d.toString('utf8');
  let idx;
  while ((idx = buf.indexOf('\n')) !== -1) {
    const line = buf.slice(0, idx).trim();
    buf = buf.slice(idx + 1);
    if (!line) continue;
    let req;
    try { req = JSON.parse(line); } catch { continue; }
    handle(req);
  }
});

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

function handle(req) {
  const { id, method } = req;
  if (id === undefined || id === null) return; // notification
  if (method === 'initialize') {
    send({
      jsonrpc: '2.0', id,
      result: {
        protocolVersion: '2024-11-05',
        capabilities: { tools: {} },
        serverInfo: { name: 'mini', version: '0.1.0' },
      },
    });
    return;
  }
  if (method === 'tools/list') {
    send({
      jsonrpc: '2.0', id,
      result: {
        tools: [
          {
            name: 'echo',
            description: '原样返回 text 参数（冒烟用）',
            inputSchema: { type: 'object', properties: { text: { type: 'string' } }, required: ['text'] },
          },
          {
            name: 'now',
            description: '返回当前本地时间字符串',
            inputSchema: { type: 'object', properties: {} },
          },
        ],
      },
    });
    return;
  }
  if (method === 'tools/call') {
    const name = req.params?.name;
    const args = req.params?.arguments || {};
    let text;
    if (name === 'echo') text = `echo: ${args.text ?? ''}`;
    else if (name === 'now') text = `now: ${new Date().toISOString()}`;
    else {
      send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text: `未知工具 ${name}` }], isError: true } });
      return;
    }
    send({ jsonrpc: '2.0', id, result: { content: [{ type: 'text', text }] } });
    return;
  }
  send({ jsonrpc: '2.0', id, error: { code: -32601, message: `未知方法 ${method}` } });
}
