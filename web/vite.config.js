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
  build: {
    outDir: 'dist',
    // 2026-10-08：代码分包。原先单包 1.4MB（gzip 581KB）——文档类大依赖
    // （SheetJS xlsx / mammoth）和 svelte 运行时要跟应用代码一起重下。拆开后
    // 应用代码改动只失效自己的 chunk，vendor 大块靠长缓存。纯构建配置，
    // 不影响任何运行时行为。
    rollupOptions: {
      output: {
        manualChunks: {
          // key 要用实际导入的说明符（静态 import 的是完整路径，不是包名）
          svelte: ['svelte/internal'],
          xlsx: ['xlsx/dist/xlsx.full.min.js'],
          mammoth: ['mammoth/mammoth.browser.js'],
        },
      },
    },
  },
});
