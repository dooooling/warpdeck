// 全局通知（toast）：context + 渲染层（P9-001 Global notifications）。

import { useCallback, useMemo, useRef, useState, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'

import { NotificationsContext } from './useNotify'

export type NoticeKind = 'success' | 'error' | 'info'

interface Notice {
  id: number
  kind: NoticeKind
  message: string
}

const AUTO_HIDE_MS = 5_000

export function NotificationsProvider({ children }: { children: ReactNode }) {
  const { t } = useTranslation()
  const [notices, setNotices] = useState<Notice[]>([])
  const nextId = useRef(1)

  const notify = useCallback((kind: NoticeKind, message: string) => {
    const id = nextId.current++
    setNotices((prev) => [...prev, { id, kind, message }])
    window.setTimeout(() => {
      setNotices((prev) => prev.filter((n) => n.id !== id))
    }, AUTO_HIDE_MS)
  }, [])

  const value = useMemo(() => ({ notify }), [notify])

  return (
    <NotificationsContext.Provider value={value}>
      {children}
      <div className="notifications" aria-live="polite" aria-atomic="false">
        {notices.map((n) => (
          <div key={n.id} className={`notice notice-${n.kind}`} role="status">
            {n.message}
            <button
              type="button"
              className="notice-dismiss"
              aria-label={t('notifications.dismiss')}
              onClick={() =>
                setNotices((prev) => prev.filter((x) => x.id !== n.id))
              }
            >
              ×
            </button>
          </div>
        ))}
      </div>
    </NotificationsContext.Provider>
  )
}