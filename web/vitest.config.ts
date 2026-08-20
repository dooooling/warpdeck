import react from '@vitejs/plugin-react'
import { defineConfig } from 'vitest/config'

// 单元/组件测试：jsdom 环境 + jest-dom 断言。
export default defineConfig({
  plugins: [react()],
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test/setup.ts'],
    // e2e 属 Playwright（node 环境），vitest 不扫描。
    exclude: ['e2e/**', 'node_modules/**', 'dist/**'],
  },
})