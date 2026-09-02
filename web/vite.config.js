import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [svelte()],
  server: {
    port: 5173,
    proxy: {
      '/api': 'http://127.0.0.1:8321',
      '/cancel': 'http://127.0.0.1:8321',
    },
  },
  build: { outDir: 'dist' },
});
