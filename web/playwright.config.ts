// Playwright Mock E2E（P9 §14.4/24.4）：mock API 服务器 + 构建产物，
// 不依赖 Docker/WARP（AGENTS.md：UI PR 不触发完整 WARP Docker build）。

import { defineConfig, devices } from '@playwright/test'

export default defineConfig({
  testDir: './e2e',
  // mock server 单进程共享内存状态（测试前 resetMock），必须串行执行。
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  reporter: process.env.CI ? 'github' : 'list',
  use: {
    baseURL: 'http://127.0.0.1:8787',
    trace: 'on-first-retry',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
  webServer: {
    command: 'node mock/server.mjs',
    url: 'http://127.0.0.1:8787/api/v1/setup/status',
    // mock server 内存状态即测试基态；复用残留进程会污染状态，永远自起。
    reuseExistingServer: false,
    timeout: 30_000,
  },
})