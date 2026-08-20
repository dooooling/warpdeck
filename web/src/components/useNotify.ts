// useNotify hook（与 provider 拆分，满足 fast refresh 约束）。

import { createContext, useContext } from 'react'

import type { NoticeKind } from './Notifications'

export interface NotificationsContextValue {
  notify: (kind: NoticeKind, message: string) => void
}

export const NotificationsContext = createContext<NotificationsContextValue | null>(null)

export function useNotify(): NotificationsContextValue['notify'] {
  const ctx = useContext(NotificationsContext)
  if (!ctx) {
    throw new Error('useNotify must be used within NotificationsProvider')
  }
  return ctx.notify
}