// 应用入口（P9-001）：QueryClient + Auth + Notifications + ErrorBoundary + 路由守卫。
//
// 守卫流程（DESIGN §20.1 / §19.1）：
// - 未初始化 → /setup；已初始化 → 登录 → /dashboard；
// - 会话恢复期间显示 splash，避免误踢到登录页。

import type { ReactNode } from 'react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import {
  createBrowserRouter,
  Navigate,
  RouterProvider,
  useLocation,
} from 'react-router'
import { useTranslation } from 'react-i18next'

import { AuthProvider } from './auth/AuthProvider'
import { useAuth } from './auth/useAuth'
import { ErrorBoundary } from './components/ErrorBoundary'
import { NotificationsProvider } from './components/Notifications'
import { Spinner } from './components/Feedback'
import { AppLayout } from './components/AppLayout'
import { SetupPage } from './pages/SetupPage'
import { LoginPage } from './pages/LoginPage'
import { DashboardPage } from './pages/DashboardPage'
import { InstancesPage } from './pages/InstancesPage'
import { InstanceDetailPage } from './pages/InstanceDetailPage'
import { AccountPage } from './pages/AccountPage'
import { LogsPage } from './pages/LogsPage'
import { SettingsPage } from './pages/SettingsPage'

// P9 无 SSE（P10）：5s 轮询提供准实时（见 api/queries.ts）。
const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 1,
      refetchOnWindowFocus: true,
      staleTime: 5_000,
      refetchInterval: 5_000,
    },
  },
})

function FullscreenSplash() {
  const { t } = useTranslation()
  return (
    <div className="auth-shell">
      <Spinner label={t('app.splashLoading')} />
    </div>
  )
}

/** /setup：已初始化则永久跳转 /login（DESIGN §20.1「创建后永久关闭」）。 */
function SetupGate({ children }: { children: ReactNode }) {
  const { setup } = useAuth()
  if (setup.isLoading) {
    return <FullscreenSplash />
  }
  if (setup.data?.initialized) {
    return <Navigate to="/login" replace />
  }
  return children
}

/** /login：已登录直接进应用（尊重 RequireAuth 带来的 from；无则 /dashboard）；未初始化引导去 /setup。 */
export function LoginGate({ children }: { children: ReactNode }) {
  const location = useLocation()
  const { setup, user, authReady } = useAuth()
  if (setup.isLoading || !authReady) {
    return <FullscreenSplash />
  }
  if (!setup.data?.initialized) {
    return <Navigate to="/setup" replace />
  }
  if (user !== null) {
    const from =
      typeof location.state === 'object' &&
      location.state !== null &&
      'from' in location.state &&
      typeof location.state.from === 'string'
        ? location.state.from
        : '/dashboard'
    return <Navigate to={from} replace />
  }
  return children
}

/** 受保护区域：会话恢复中显示 splash；未登录跳 /login（携带来源页，登录后回跳）。 */
function RequireAuth({ children }: { children: ReactNode }) {
  const { authReady, user } = useAuth()
  const location = useLocation()
  if (!authReady) {
    return <FullscreenSplash />
  }
  if (user === null) {
    return (
      <Navigate
        to="/login"
        replace
        state={{ from: `${location.pathname}${location.search}` }}
      />
    )
  }
  return children
}

/** `/`：按初始化/登录状态重定向。 */
function RootRedirect() {
  const { authReady, setup, user } = useAuth()
  if (setup.isLoading || !authReady) {
    return <FullscreenSplash />
  }
  if (!setup.data?.initialized) {
    return <Navigate to="/setup" replace />
  }
  if (user === null) {
    return <Navigate to="/login" replace />
  }
  return <Navigate to="/dashboard" replace />
}

const router = createBrowserRouter([
  {
    path: '/setup',
    element: (
      <SetupGate>
        <SetupPage />
      </SetupGate>
    ),
  },
  {
    path: '/login',
    element: (
      <LoginGate>
        <LoginPage />
      </LoginGate>
    ),
  },
  { path: '/', element: <RootRedirect /> },
  {
    path: '/',
    element: (
      <RequireAuth>
        <AppLayout />
      </RequireAuth>
    ),
    children: [
      { path: 'dashboard', element: <DashboardPage /> },
      { path: 'instances', element: <InstancesPage /> },
      { path: 'instances/:id', element: <InstanceDetailPage /> },
      { path: 'proxy', element: <Navigate to="/settings" replace /> },
      { path: 'account', element: <AccountPage /> },
      { path: 'logs', element: <LogsPage /> },
      { path: 'settings', element: <SettingsPage /> },
    ],
  },
  { path: '*', element: <Navigate to="/" replace /> },
])

export function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <AuthProvider>
        <NotificationsProvider>
          <ErrorBoundary>
            <RouterProvider router={router} />
          </ErrorBoundary>
        </NotificationsProvider>
      </AuthProvider>
    </QueryClientProvider>
  )
}