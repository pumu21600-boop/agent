import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [svelte()],
  server: {
    // 热更服务器：5174（5173 已让给主服务，见 config.toml [server].port）
    port: 5174,
    proxy: {
      '/api': 'http://127.0.0.1:5173',
      '/cancel': 'http://127.0.0.1:5173',
    },
  },
  build: { outDir: 'dist' },
});
