import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import vue from '@vitejs/plugin-vue';
import { defineConfig } from 'vite';

const pkg = JSON.parse(readFileSync(resolve(import.meta.dirname, '../../package.json'), 'utf8')) as { version?: string };

export default defineConfig({
  plugins: [vue()],
  define: { __APP_VERSION__: JSON.stringify(pkg.version || '0.0.0') },
  resolve: { alias: { 'streamdock/contract': resolve(import.meta.dirname, './src/types/contract.ts') } },
  build: { outDir: resolve(import.meta.dirname, '../../client'), emptyOutDir: false },
  publicDir: false,
  server: { host: '0.0.0.0', port: 5173, proxy: {
    '/api': 'http://localhost:6600', '/ws': { target: 'ws://localhost:6600', ws: true },
  } },
});
