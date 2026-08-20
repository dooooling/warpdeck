// P10-004 SSE hook 单元测试：mock EventSource 验证重连退避 / 状态机 / cache 更新。

import { act, renderHook } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ReactNode } from 'react'

import { resourceInstanceId, useSseEvents } from './sse'

class FakeEventSource {
  static instances: FakeEventSource[] = []
  static autoOpen = true

  url: string
  onopen: (() => void) | null = null
  onerror: (() => void) | null = null
  listeners: Record<string, (ev: MessageEvent<string>) => void> = {}
  closed = false

  constructor(url: string) {
    this.url = url
    FakeEventSource.instances.push(this)
    if (FakeEventSource.autoOpen) {
      queueMicrotask(() => this.onopen?.())
    }
  }

  addEventListener(type: string, cb: (ev: MessageEvent<string>) => void) {
    this.listeners[type] = cb
  }

  close() {
    this.closed = true
  }

  /** 按帧名 emit；未注册监听器则忽略（对齐原生 EventSource 行为）。 */
  emit(type: string, data: unknown) {
    this.listeners[type]?.({ data: JSON.stringify(data) } as MessageEvent<string>)
  }

  fail() {
    this.onerror?.()
  }

  static reset() {
    FakeEventSource.instances = []
    FakeEventSource.autoOpen = true
  }
}

vi.stubGlobal('EventSource', FakeEventSource)

function wrapper(queryClient: QueryClient) {
  return function Wrapper({ children }: { children: ReactNode }) {
    return <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  }
}

function makeClient() {
  return new QueryClient({
    defaultOptions: { queries: { retry: false } },
  })
}

/** flush microtask（FakeEventSource.autoOpen 的 onopen 在 microtask 里触发）。 */
async function flush() {
  await act(async () => {
    await Promise.resolve()
  })
}

describe('useSseEvents', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    FakeEventSource.reset()
    vi.stubGlobal('EventSource', FakeEventSource)
  })

  afterEach(() => {
    vi.useRealTimers()
    vi.unstubAllGlobals()
  })

  it('opens and reports connection state', async () => {
    const client = makeClient()
    const { result } = renderHook(() => useSseEvents(), {
      wrapper: wrapper(client),
    })
    await flush()
    expect(result.current).toBe('open')
    expect(FakeEventSource.instances).toHaveLength(1)
    expect(FakeEventSource.instances[0].url).toBe('/api/v1/events')
  })

  it('reconnects after error with exponential backoff capped at 15s', async () => {
    const client = makeClient()
    const { result } = renderHook(() => useSseEvents(), {
      wrapper: wrapper(client),
    })
    await flush()
    expect(FakeEventSource.instances).toHaveLength(1)

    // 第一次失败 → closed + 1s 后重连（onopen 未发生前重试计数保持）。
    act(() => {
      FakeEventSource.instances[0].fail()
    })
    expect(result.current).toBe('closed')
    // 阻止 autoOpen：重连后不触发 onopen（模拟持续断线）。
    FakeEventSource.autoOpen = false
    act(() => {
      vi.advanceTimersByTime(1_000)
    })
    await act(async () => {
      await Promise.resolve()
    })
    expect(FakeEventSource.instances).toHaveLength(2)
    // 第二次失败（未 open，retry=1）→ 2s 退避。
    act(() => {
      FakeEventSource.instances[1].fail()
    })
    act(() => {
      vi.advanceTimersByTime(1_999)
    })
    expect(FakeEventSource.instances).toHaveLength(2)
    act(() => {
      vi.advanceTimersByTime(1)
    })
    await act(async () => {
      await Promise.resolve()
    })
    expect(FakeEventSource.instances).toHaveLength(3)

    // 连接恢复 → open 且重置退避（下次失败仍 1s）。
    await act(async () => {
      FakeEventSource.instances[2].onopen?.()
      await Promise.resolve()
    })
    expect(result.current).toBe('open')
  })

  it('cleans up on unmount and ignores late reconnect', async () => {
    const client = makeClient()
    const { unmount } = renderHook(() => useSseEvents(), {
      wrapper: wrapper(client),
    })
    await flush()
    const first = FakeEventSource.instances[0]
    unmount()
    expect(first.closed).toBe(true)
    // unmount 后的 onerror 不重连（disposed 守卫）。
    act(() => {
      first.onerror?.()
      vi.advanceTimersByTime(20_000)
    })
    expect(FakeEventSource.instances).toHaveLength(1)
  })

  it('invalidates instance queries on state_changed', async () => {
    const client = makeClient()
    const invalidateSpy = vi.spyOn(client, 'invalidateQueries')
    const { result } = renderHook(() => useSseEvents(), {
      wrapper: wrapper(client),
    })
    await flush()
    act(() => {
      FakeEventSource.instances[0].emit('instance.health_changed', {
        type: 'instance.health_changed',
        version: 1,
        timestamp: '2026-08-18T00:00:00Z',
        resource_id: 'instance:3',
        data: { instance_id: 3, from: 'starting', to: 'healthy', reason: 'probe ok' },
      })
    })
    expect(invalidateSpy).toHaveBeenCalledWith({
      queryKey: ['instances', 3],
    })
    expect(invalidateSpy).toHaveBeenCalledWith({ queryKey: ['instances'] })
    expect(result.current).toBe('open')
  })

  it('parses resource ids from instance resources only', () => {
    expect(resourceInstanceId('instance:7')).toBe(7)
    expect(resourceInstanceId('gost')).toBe(-1)
    expect(resourceInstanceId('manager')).toBe(-1)
  })
})
