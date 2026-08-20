// LoginGate 守卫测试：已登录用户访问 /login 必须被送进应用，
// 而不是停留在登录页（否则在 /login 上 F5 会一直"回到"登录页）。

import { describe, expect, it, vi } from 'vitest'
import { render, screen } from '@testing-library/react'
import { MemoryRouter, Route, Routes } from 'react-router'

import { LoginGate } from './App'
import type { AuthContextValue } from './auth/AuthProvider'

vi.mock('./auth/useAuth', () => ({
  useAuth: vi.fn(),
}))

import { useAuth } from './auth/useAuth'

const mockUseAuth = vi.mocked(useAuth)

function renderAtLogin(overrides: Partial<AuthContextValue>) {
  mockUseAuth.mockReturnValue({
    setup: { data: { initialized: true }, isLoading: false } as never,
    user: null,
    authReady: true,
    ...overrides,
  } as AuthContextValue)
  return render(
    <MemoryRouter initialEntries={['/login']}>
      <Routes>
        <Route
          path="/login"
          element={
            <LoginGate>
              <div>login-page</div>
            </LoginGate>
          }
        />
        <Route path="/dashboard" element={<div>dashboard-page</div>} />
        <Route path="/setup" element={<div>setup-page</div>} />
      </Routes>
    </MemoryRouter>,
  )
}

describe('LoginGate', () => {
  it('redirects a logged-in user to /dashboard', () => {
    renderAtLogin({ user: { id: 1, username: 'admin' } })
    expect(screen.queryByText('login-page')).not.toBeInTheDocument()
    expect(screen.getByText('dashboard-page')).toBeInTheDocument()
  })

  it('renders the login page when not authenticated', () => {
    renderAtLogin({ user: null })
    expect(screen.getByText('login-page')).toBeInTheDocument()
  })

  it('sends uninitialized setups to /setup', () => {
    renderAtLogin({
      setup: { data: { initialized: false }, isLoading: false } as never,
    })
    expect(screen.getByText('setup-page')).toBeInTheDocument()
  })

  it('shows splash while the session is still being restored', () => {
    renderAtLogin({ user: null, authReady: false })
    expect(screen.queryByText('login-page')).not.toBeInTheDocument()
    expect(screen.queryByText('dashboard-page')).not.toBeInTheDocument()
  })
})
