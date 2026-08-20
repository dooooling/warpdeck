// 应用布局（P9-001）：侧边导航 + 用户区/登出 + 实时连接指示（P10-004）。

import { useState } from 'react'
import { NavLink, Outlet, useNavigate } from 'react-router'
import { useTranslation } from 'react-i18next'

import { useLogoutMutation } from '../api/queries'
import { useSseEvents } from '../api/sse'
import { ConfirmDialog } from './ConfirmDialog'
import { useNotify } from './useNotify'

const NAV_ITEMS: { to: string; key: 'dashboard' | 'instances' | 'account' | 'logs' | 'settings' }[] = [
  { to: '/dashboard', key: 'dashboard' },
  { to: '/instances', key: 'instances' },
  { to: '/account', key: 'account' },
  { to: '/logs', key: 'logs' },
  { to: '/settings', key: 'settings' },
]

/** 侧边栏底部 SSE 连接呼吸灯（P10-004：连接健康可见，颜色+aria 双通道）。 */
export function RealtimeStatusDot() {
  const { t } = useTranslation()
  const state = useSseEvents()
  const label = t(`conn.${state}`)
  return (
    <span
      className={`conn-badge conn-${state}`}
      role="status"
      aria-label={t(`conn.${state}`)}
      title={t('nav.sseTitle', { state: label })}
    />
  )
}

export function AppLayout() {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const notify = useNotify()
  const logout = useLogoutMutation()
  const [confirmingLogout, setConfirmingLogout] = useState(false)

  const doLogout = () => {
    logout.mutate(undefined, {
      onSuccess: () => navigate('/login'),
      onError: (e) => notify('error', e.message),
    })
  }

  return (
    <div className="layout">
      <aside className="sidebar">
        <div className="sidebar-brand">{t('nav.brand')}</div>
        <nav className="sidebar-nav" aria-label={t('nav.dashboard')}>
          {NAV_ITEMS.map((item) => (
            <NavLink
              key={item.to}
              to={item.to}
              className={({ isActive }) => `nav-link${isActive ? ' nav-link-active' : ''}`}
            >
              {t(`nav.${item.key}`)}
            </NavLink>
          ))}
        </nav>
        <div className="sidebar-footer">
          <RealtimeStatusDot />
          <button
            type="button"
            className="btn btn-ghost"
            onClick={() => setConfirmingLogout(true)}
          >
            {t('nav.logout')}
          </button>
        </div>
      </aside>
      <main className="content">
        <Outlet />
      </main>
      <ConfirmDialog
        open={confirmingLogout}
        title={t('nav.logoutTitle')}
        message={t('nav.logoutMessage')}
        confirmLabel={t('nav.logoutConfirm')}
        busy={logout.isPending}
        onConfirm={doLogout}
        onCancel={() => setConfirmingLogout(false)}
      />
    </div>
  )
}