import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// 开发期:`npm run dev` 时把 /ws 代理到本地中转(cargo run 于 relay-server),
// 生产构建产物由中转直接托管(同源,无需代理)。
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/ws': {
        target: 'ws://localhost:8080',
        ws: true,
      },
    },
  },
});
