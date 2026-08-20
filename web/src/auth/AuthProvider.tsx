// 认证会话（P9-001）：setup 状态 + 当前用户 + CSRF token 的统一来源。
//
// 数据流：
// - 启动时（已初始化）`me` 查询恢复会话：200 → 用户 + CSRF；401 → 未登录；
// - 任意请求收到 401 时（client 回调）清空 user/CSRF → 守卫跳转登录页；
// - login 成功 / logout 维护 user 与 CSRF 同步。

import { useEffect, useState, type ReactNode } from 'react'
import { useMutation, useQuery, type UseMutationResult } from '@tanstack/react-query'

import { apiFetch, setCsrfToken, setUnauthorizedHandler } from '../api/client'
import { useSetupMutation, useSetupStatus } from '../api/queries'
import type { AuthResponse, SetupStatusResponse, UserInfo } from '../api/types'
import { AuthContext } from './useAuth'

export interface AuthContextValue {
  /** `{initialized}` 查询状态；loading 时页面应显示 splash。 */
  setup: ReturnType<typeof useSetupStatus>
  /** 当前用户；null = 未登录（守卫据此跳转 /login）。 */
  user: UserInfo | null
  /** 会话是否已恢复完成（未初始化或 me 已 settle）；false 时守卫显示 splash。 */
  authReady: boolean
  login: UseMutationResult<AuthResponse, Error, { username: string; password: string }, unknown>
  logout: UseMutationResult<void, Error, void, unknown>
  createAdmin: UseMutationResult<SetupStatusResponse, Error, { username: string; password: string }, unknown>
}

export function AuthProvider({ children }: { children: ReactNode }) {
  const setup = useSetupStatus()
  const [user, setUser] = useState<UserInfo | null>(null)

  useEffect(() => {
    setUnauthorizedHandler(() => {
      setUser(null)
      setCsrfToken(null)
    })
    return () => setUnauthorizedHandler(null)
  }, [])

  // 会话恢复：仅在「已初始化」后执行；401 时保持 user=null（无重试，
  // 不会因 invalidate 造成 401 循环）。v5 无 query 回调，结果经 effect 落地。
  const meQuery = useQuery({
    queryKey: ['me'],
    queryFn: () => apiFetch<AuthResponse>('/auth/me'),
    retry: 0,
    staleTime: 60_000,
    // 会话一旦恢复无需轮询（避免未登录时每 5s 触发一次 401）。
    refetchInterval: false,
    enabled: setup.data?.initialized === true,
  })

  useEffect(() => {
    if (meQuery.data) {
      setUser(meQuery.data.user)
      setCsrfToken(meQuery.data['x-csrf-token'])
    }
  }, [meQuery.data])

  // setup 未 settle 时 authReady 必须为 false：否则守卫会把 undefined
  // 误判为「未登录」提前踢去 /login（刷新时闪跳的根因）。
  const authReady = !setup.isLoading && (setup.data?.initialized !== true || !meQuery.isPending)

  const login = useMutation({
    mutationFn: (input: { username: string; password: string }) =>
      apiFetch<AuthResponse>('/auth/login', {
        method: 'POST',
        body: input,
      }),
    onSuccess: (data) => {
      setCsrfToken(data['x-csrf-token'])
      setUser(data.user)
    },
  })

  const logout = useMutation({
    mutationFn: () => apiFetch<void>('/auth/logout', { method: 'POST' }),
    onSuccess: () => {
      setCsrfToken(null)
      setUser(null)
    },
  })

  const createAdmin = useSetupMutation()

  return (
    <AuthContext.Provider value={{ setup, user, authReady, login, logout, createAdmin }}>
      {children}
    </AuthContext.Provider>
  )
}