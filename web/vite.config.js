import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// 主服务地址（config.toml [server].port）。代理转发时把 Origin 改写成它——
// 主服务的来源校验要求 Origin 与 Host 同源（防 DNS rebinding），
// 而浏览器发来的是热更端口 5174，不改写会被 403。代理的意义正在于此。
const SERVICE = 'http://127.0.0.1:5173';

const proxyToService = {
  target: SERVICE,
  changeOrigin: true,
  configure: (proxy) => {
    proxy.on('proxyReq', (proxyReq) => proxyReq.setHeader('Origin', SERVICE));
  },
};

export default defineConfig({
  plugins: [svelte()],
  server: {
    // 热更服务器：5174（5173 已让给主服务，见 config.toml [server].port）
    port: 5174,
    proxy: {
      '/api': proxyToService,
      '/cancel': proxyToService,
      '/ui-ext': proxyToService,
    },
  },
  build: { outDir: 'dist' },
});
