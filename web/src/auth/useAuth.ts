// useAuth hook（与 provider 拆分，满足 fast refresh 约束）。

import { createContext, useContext } from 'react'

import type { AuthContextValue } from './AuthProvider'

export const AuthContext = createContext<AuthContextValue | null>(null)

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext)
  if (!ctx) {
    throw new Error('useAuth must be used within AuthProvider')
  }
  return ctx
}