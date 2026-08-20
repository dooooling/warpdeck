import '@testing-library/jest-dom/vitest'
import { cleanup } from '@testing-library/react'

// i18n 必须在测试渲染前初始化（组件内部 useTranslation 依赖 react-i18next 实例）。
import '../i18n'

// 所有测试统一默认禁用 React Query 重试/轮询（避免超时与多余网络调用）。
import { QueryClient } from '@tanstack/react-query'

import { afterEach, vi } from 'vitest'

afterEach(() => {
  vi.unstubAllGlobals()
  cleanup()
})

export function createTestQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: {
        retry: 0,
        staleTime: 0,
        refetchInterval: false,
      },
    },
  })
}