// P10-004 SSE 客户端：事件订阅 + 指数退避重连 + 连接状态 + React Query cache 更新。
//
// 事件帧契约（P10-003）：{type, version, timestamp, resource_id, data}。
// - instance.* → invalidate 对应实例查询（list + detail），refetch 拿完整状态；
// - log.line → 不触发 refetch（LogsPage 自己消费 live 行）。

import { useEffect, useRef, useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'

import { instanceKeys } from './queries'

export type SseConnectionState = 'connecting' | 'open' | 'closed'

export type SseEnvelope = {
  type: string
  version: number
  timestamp: string
  resource_id: string
  data: Record<string, unknown>
}

/** 后端事件帧名（与 events.rs 的 event: 帧保持一致）。
 * EventSource 的 onmessage 只收未命名帧，命名事件必须逐个 addEventListener。 */
const EVENT_TYPES = [
  'instance.state_changed',
  'instance.health_changed',
  'instance.exit_ip_changed',
  'log.line',
] as const

/** 从 resource_id（`instance:7`）提取 id；非实例资源返回 -1。 */
export function resourceInstanceId(resourceId: string): number {
  const m = /^instance:(\d+)$/.exec(resourceId)
  return m ? Number(m[1]) : -1
}

/**
 * 订阅 `/api/v1/events`（同源 + cookie 认证）。
 *
 * 断线退避：1s → 2s → 4s … 上限 15s（指数退避）。
 * html 原生 EventSource 自动重连策略与自定义退避互斥，onerror 后手动重连。
 */
export function useSseEvents(onEvent?: (env: SseEnvelope) => void): SseConnectionState {
  const queryClient = useQueryClient()
  const [state, setState] = useState<SseConnectionState>('connecting')
  const onEventRef = useRef(onEvent)
  onEventRef.current = onEvent

  useEffect(() => {
    let es: EventSource | null = null
    let timer: ReturnType<typeof setTimeout> | null = null
    let retry = 0
    let disposed = false

    const handleEnvelope = (env: SseEnvelope) => {
      onEventRef.current?.(env)
      switch (env.type) {
        case 'instance.state_changed':
        case 'instance.health_changed':
        case 'instance.exit_ip_changed': {
          const id = resourceInstanceId(env.resource_id)
          if (id >= 0) {
            void queryClient.invalidateQueries({ queryKey: instanceKeys.detail(id) })
            void queryClient.invalidateQueries({ queryKey: instanceKeys.all })
          }
          break
        }
        default:
          break // log.line 等：页面自行消费，不 refetch
      }
    }

    const connect = () => {
      if (disposed) return
      setState('connecting')
      const source = new EventSource('/api/v1/events')
      es = source
      source.onopen = () => {
        retry = 0
        setState('open')
      }
      for (const name of EVENT_TYPES) {
        source.addEventListener(name, (ev) => {
          try {
            const env = JSON.parse((ev as MessageEvent<string>).data) as SseEnvelope
            handleEnvelope(env)
          } catch {
            // 忽略畸形帧（契约外内容不处理）
          }
        })
      }
      source.onerror = () => {
        source.close()
        if (disposed) return
        setState('closed')
        const delay = Math.min(1_000 * 2 ** retry, 15_000)
        retry += 1
        timer = setTimeout(connect, delay)
      }
    }

    connect()

    return () => {
      disposed = true
      if (timer) clearTimeout(timer)
      es?.close()
    }
  }, [queryClient])

  return state
}
