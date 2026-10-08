// 冒烟测试用 mock OAuth MCP server（不是产品代码）。
// 用法：node scripts/mock_oauth_mcp.mjs
// 实现一个最小 OAuth + MCP（Streamable HTTP）闭环：
//   /.well-known/oauth-protected-resource → 授权服务器指向自己
//   /.well-known/oauth-authorization-server → authorize/token/register 端点
//   POST /register                          → DCR 动态注册（发 client_id）
//   GET  /authorize                          → 自动同意页（meta 刷新 302 到 redirect_uri）
//   POST /token                              → 授权码换 token（校验 code_verifier）/ refresh
//   POST /mcp                                → Streamable HTTP：无有效 Bearer 401，否则 JSON-RPC
// 验证目标：agent 的 discovery→DCR→PKCE→loopback 回调→换 token→Bearer 挂载→MCP 握手全链路。
import http from 'node:http';

const PORT = 9400;
const VALID_CODE = 'mock_auth_code_123';
const CLIENT_ID = 'dcr-client-1';
const ACCESS_TOKEN = 'mock_access_token_xyz';

function json(res, status, obj) {
  res.writeHead(status, { 'Content-Type': 'application/json' });
  res.end(JSON.stringify(obj));
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url, `http://127.0.0.1:${PORT}`);

  if (req.method === 'GET' && url.pathname === '/.well-known/oauth-protected-resource') {
    return json(res, 200, {
      resource: `http://127.0.0.1:${PORT}/mcp`,
      authorization_servers: [`http://127.0.0.1:${PORT}`],
    });
  }

  if (req.method === 'GET' && url.pathname === '/.well-known/oauth-authorization-server') {
    return json(res, 200, {
      issuer: `http://127.0.0.1:${PORT}`,
      authorization_endpoint: `http://127.0.0.1:${PORT}/authorize`,
      token_endpoint: `http://127.0.0.1:${PORT}/token`,
      registration_endpoint: `http://127.0.0.1:${PORT}/register`,
      response_types_supported: ['code'],
      grant_types_supported: ['authorization_code', 'refresh_token'],
      code_challenge_methods_supported: ['S256'],
    });
  }

  if (req.method === 'POST' && url.pathname === '/register') {
    let body = '';
    req.on('data', (c) => (body += c));
    return req.on('end', () => {
      console.log('[mock-oauth] DCR:', body);
      json(res, 201, { client_id: CLIENT_ID, client_name: 'agent' });
    });
  }

  if (req.method === 'GET' && url.pathname === '/authorize') {
    // 模拟用户已同意：302 跳回 redirect_uri（等价真人点完「允许」；真授权码流程都是 302）
    const redirect = `${url.searchParams.get('redirect_uri')}?code=${VALID_CODE}&state=${url.searchParams.get('state')}`;
    console.log('[mock-oauth] authorize →', redirect);
    res.writeHead(302, { Location: redirect });
    return res.end();
  }

  if (req.method === 'POST' && url.pathname === '/token') {
    let body = '';
    req.on('data', (c) => (body += c));
    return req.on('end', () => {
      const p = new URLSearchParams(body);
      const grant = p.get('grant_type');
      console.log('[mock-oauth] token:', body);
      if (grant === 'authorization_code') {
        if (p.get('code') !== VALID_CODE) return json(res, 400, { error: 'invalid_grant' });
        if (!p.get('code_verifier')) return json(res, 400, { error: 'missing code_verifier（PKCE）' });
        return json(res, 200, {
          access_token: ACCESS_TOKEN,
          refresh_token: 'mock_refresh_token_456',
          expires_in: 3600,
          scope: 'mcp',
        });
      }
      if (grant === 'refresh_token') {
        if (p.get('refresh_token') !== 'mock_refresh_token_456') return json(res, 400, { error: 'invalid_grant' });
        return json(res, 200, { access_token: 'mock_access_token_v2', refresh_token: 'mock_refresh_token_456', expires_in: 3600 });
      }
      json(res, 400, { error: 'unsupported_grant_type' });
    });
  }

  if (req.method === 'POST' && url.pathname === '/mcp') {
    const auth = req.headers.authorization || '';
    let body = '';
    req.on('data', (c) => (body += c));
    return req.on('end', () => {
      if (auth !== `Bearer ${ACCESS_TOKEN}` && auth !== 'Bearer mock_access_token_v2') {
        res.writeHead(401, { 'Content-Type': 'application/json', 'WWW-Authenticate': 'Bearer realm="mock"' });
        return res.end(JSON.stringify({ jsonrpc: '2.0', error: { code: -32000, message: '未授权' }, id: null }));
      }
      const msg = JSON.parse(body);
      console.log('[mock-oauth] mcp:', JSON.stringify(msg).slice(0, 120));
      const reply = (result) =>
        json(res, 200, { jsonrpc: '2.0', id: msg.id, result });
      if (msg.method === 'initialize') {
        return reply({
          protocolVersion: '2025-03-26',
          capabilities: { tools: {} },
          serverInfo: { name: 'mock-oauth-mcp', version: '1.0' },
        });
      }
      if (msg.method === 'tools/list') {
        return reply({
          tools: [
            { name: 'echo', description: '回声工具（mock）', inputSchema: { type: 'object', properties: { text: { type: 'string' } } } },
          ],
        });
      }
      if (msg.method === 'tools/call') {
        return reply({ content: [{ type: 'text', text: `echo:${msg.params?.arguments?.text || ''}` }] });
      }
      json(res, 200, { jsonrpc: '2.0', id: msg.id, error: { code: -32601, message: 'method not found' } });
    });
  }

  res.writeHead(404);
  res.end();
});

server.listen(PORT, () => console.log(`[mock-oauth] OAuth MCP on http://127.0.0.1:${PORT}`));
