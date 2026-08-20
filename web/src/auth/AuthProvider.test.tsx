// 认证状态机测试（P9 §14.4：auth redirect 依赖的会话恢复逻辑）。
//
// 覆盖 AuthProvider 的三个关键判定：
// - setup 未完成 → me 不执行（enabled=false）→ authReady=true（引导 /setup）；
// - me 401 → user 保持 null → 守卫跳 /login；
// - me 200 → user 落地 → 守卫放行受保护页。

import { describe, expect, it, vi } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import { QueryClientProvider } from '@tanstack/react-query'

import { AuthProvider } from './AuthProvider'
import { useAuth } from './useAuth'
import { createTestQueryClient } from '../test/setup'

function Probe() {
  const { setup, user, authReady, createAdmin } = useAuth()
  return (
    <div>
      <span data-testid="setup-loading">{String(setup.isLoading)}</span>
      <span data-testid="setup-initialized">{String(setup.data?.initialized ?? 'none')}</span>
      <span data-testid="user">{user?.username ?? 'none'}</span>
      <span data-testid="auth-ready">{String(authReady)}</span>
      <span data-testid="create-admin">{String(typeof createAdmin.mutate)}</span>
    </div>
  )
}

type FetchImpl = (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>

function renderProvider(fetchImpl: FetchImpl) {
  vi.stubGlobal('fetch', vi.fn<FetchImpl>(fetchImpl))
  const client = createTestQueryClient()
  return render(
    <QueryClientProvider client={client}>
      <AuthProvider>
        <Probe />
      </AuthProvider>
    </QueryClientProvider>,
  )
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })

describe('AuthProvider session recovery', () => {
  it('treats uninitialized setup as ready without hitting /auth/me', async () => {
    const fetchMock = vi.fn<FetchImpl>(async () => json({ initialized: false }))
    renderProvider(fetchMock)
    await waitFor(() => expect(screen.getByTestId('setup-initialized')).toHaveTextContent('false'))
    expect(screen.getByTestId('auth-ready')).toHaveTextContent('true')
    expect(screen.getByTestId('user')).toHaveTextContent('none')
    // me 不应被调用（未初始化不查会话）。
    expect(fetchMock.mock.calls.map((c) => String(c[0]))).not.toContain('/api/v1/auth/me')
  })

  it('keeps user null when the session is invalid (401)', async () => {
    const fetchMock = vi
      .fn<FetchImpl>(async () => json({ initialized: false }))
      .mockImplementationOnce(async () => json({ initialized: true }))
      .mockImplementationOnce(async () =>
        json(
          { error: { code: 'UNAUTHORIZED', message: 'authentication required', request_id: 'r' } },
          401,
        ),
      )
    renderProvider(fetchMock)
    await waitFor(() => expect(screen.getByTestId('auth-ready')).toHaveTextContent('true'))
    expect(screen.getByTestId('user')).toHaveTextContent('none')
  })

  it('restores the user from /auth/me when the session is valid', async () => {
    const fetchMock = vi
      .fn<FetchImpl>(async () => json({ initialized: false }))
      .mockImplementationOnce(async () => json({ initialized: true }))
      .mockImplementationOnce(async () =>
        json({ user: { id: 1, username: 'admin' }, 'x-csrf-token': 'csrf-1' }),
      )
    renderProvider(fetchMock)
    await waitFor(() => expect(screen.getByTestId('user')).toHaveTextContent('admin'))
    expect(screen.getByTestId('auth-ready')).toHaveTextContent('true')
  })

  it('holds authReady false while the session is still being restored', async () => {
    const fetchMock = vi
      .fn<FetchImpl>(async () => json({ initialized: false }))
      .mockImplementationOnce(async () => json({ initialized: true }))
      .mockImplementationOnce(() => new Promise<Response>(() => {})) // 永不 resolve → pending
    renderProvider(fetchMock)
    await waitFor(() => expect(screen.getByTestId('setup-initialized')).toHaveTextContent('true'))
    expect(screen.getByTestId('auth-ready')).toHaveTextContent('false')
  })
})