import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    // 开发时代理到后端容器内端口 9000（生产由同一 origin 提供静态资源）。
    proxy: {
      '/api': 'http://127.0.0.1:9000',
    },
  },
})
