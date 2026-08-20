// React Query hooks（P9-002 / DESIGN §22.2）：server state 统一管理。
//
// P9 无 SSE（P10 再做），关键列表用轮询（refetchInterval 5s）提供准实时；
// mutation 成功后 invalidate 相关 key，SSE 接入时改走 queryClient.setQueryData。

import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

import { apiFetch, setCsrfToken } from './client'
import type {
  AccountProfileView,
  AccountProfileWriteRequest,
  AuthResponse,
  CreateInstanceRequest,
  InstanceView,
  LogHistoryResponse,
  LogSourceView,
  PatchInstanceRequest,
  ProxyConfigView,
  SetupStatusResponse,
  SystemStatusView,
  UpdateProxyRequest,
} from './types'

export const instanceKeys = {
  all: ['instances'] as const,
  detail: (id: number) => ['instances', id] as const,
}

/** 轮询间隔：实例/代理等实际状态（P9 无 SSE，见模块注释）。 */
export const POLL_INTERVAL_MS = 5_000

export function useSetupStatus() {
  return useQuery({
    queryKey: ['setup-status'],
    queryFn: () => apiFetch<SetupStatusResponse>('/setup/status'),
    retry: 0,
    staleTime: 30_000,
  })
}

export function useSetupMutation() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (input: { username: string; password: string }) =>
      apiFetch<SetupStatusResponse>('/setup', {
        method: 'POST',
        body: input,
      }),
    // setup 完成后立即刷新状态（守卫据此跳 /login，不能等 staleTime 过期）。
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['setup-status'] })
    },
  })
}

export function useLoginMutation() {
  return useMutation({
    mutationFn: (input: { username: string; password: string }) =>
      apiFetch<AuthResponse>('/auth/login', {
        method: 'POST',
        body: input,
      }),
  })
}

export function useLogoutMutation() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: () => apiFetch<void>('/auth/logout', { method: 'POST' }),
    onSuccess: () => {
      setCsrfToken(null)
      queryClient.clear()
    },
  })
}

export function useSystemStatus() {
  return useQuery({
    queryKey: ['system-status'],
    queryFn: () => apiFetch<SystemStatusView>('/system/status'),
    refetchInterval: POLL_INTERVAL_MS,
  })
}

export function useInstances() {
  return useQuery({
    queryKey: instanceKeys.all,
    queryFn: () => apiFetch<InstanceView[]>('/instances'),
    refetchInterval: POLL_INTERVAL_MS,
  })
}

export function useInstance(id: number) {
  return useQuery({
    queryKey: instanceKeys.detail(id),
    queryFn: () => apiFetch<InstanceView>(`/instances/${id}`),
    refetchInterval: POLL_INTERVAL_MS,
  })
}

function useInstanceAction(pathSuffix: string, onSuccess: () => void) {
  return useMutation({
    mutationFn: (id: number) => apiFetch<void>(`/instances/${id}${pathSuffix}`, { method: 'POST' }),
    onSuccess,
  })
}

export function useCreateInstanceMutation() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (input: CreateInstanceRequest) =>
      apiFetch<InstanceView>('/instances', {
        method: 'POST',
        body: input,
      }),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: instanceKeys.all })
      void queryClient.invalidateQueries({ queryKey: ['system-status'] })
    },
  })
}

export function useStartInstanceMutation() {
  const queryClient = useQueryClient()
  return useInstanceAction('/start', () => {
    void queryClient.invalidateQueries({ queryKey: instanceKeys.all })
    void queryClient.invalidateQueries({ queryKey: ['system-status'] })
  })
}

export function useStopInstanceMutation() {
  const queryClient = useQueryClient()
  return useInstanceAction('/stop', () => {
    void queryClient.invalidateQueries({ queryKey: instanceKeys.all })
    void queryClient.invalidateQueries({ queryKey: ['system-status'] })
  })
}

export function useRestartInstanceMutation() {
  const queryClient = useQueryClient()
  return useInstanceAction('/restart', () => {
    void queryClient.invalidateQueries({ queryKey: instanceKeys.all })
  })
}

export function useDeleteInstanceMutation() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => apiFetch<void>(`/instances/${id}`, { method: 'DELETE' }),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: instanceKeys.all })
      void queryClient.invalidateQueries({ queryKey: ['system-status'] })
    },
  })
}

export function useProxyConfig() {
  return useQuery({
    queryKey: ['proxy'],
    queryFn: () => apiFetch<ProxyConfigView>('/proxy'),
    refetchInterval: POLL_INTERVAL_MS,
  })
}

export function useUpdateProxyMutation() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (input: UpdateProxyRequest) =>
      apiFetch<ProxyConfigView>('/proxy', { method: 'PUT', body: input }),
    onSuccess: (view) => {
      queryClient.setQueryData(['proxy'], view)
    },
  })
}

// ---------- Account profiles (v0.2 §17.6) ----------

export const profileKeys = {
  all: ['profiles'] as const,
}

export function useAccountProfiles() {
  return useQuery({
    queryKey: profileKeys.all,
    queryFn: () => apiFetch<AccountProfileView[]>('/accounts'),
    refetchInterval: POLL_INTERVAL_MS,
  })
}

export function useCreateAccountProfileMutation() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (input: AccountProfileWriteRequest) =>
      apiFetch<AccountProfileView>('/accounts', { method: 'POST', body: input }),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: profileKeys.all })
    },
  })
}

export function useUpdateAccountProfileMutation(profileId: number) {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (input: AccountProfileWriteRequest) =>
      apiFetch<AccountProfileView>(`/accounts/${profileId}`, { method: 'PATCH', body: input }),
    onSuccess: (view) => {
      queryClient.setQueryData(profileKeys.all, (old: AccountProfileView[] | undefined) =>
        old?.map((p) => (p.id === view.id ? view : p)),
      )
    },
  })
}

export function useDeleteAccountProfileMutation(profileId: number) {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: () => apiFetch<void>(`/accounts/${profileId}`, { method: 'DELETE' }),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: profileKeys.all })
    },
  })
}

/** `PATCH /api/v1/instances/{id}`：改绑账号档案（改绑在下次重启生效）。 */
export function useRebindInstanceMutation() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, input }: { id: number; input: PatchInstanceRequest }) =>
      apiFetch<InstanceView>(`/instances/${id}`, { method: 'PATCH', body: input }),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: instanceKeys.all })
      void queryClient.invalidateQueries({ queryKey: profileKeys.all })
    },
  })
}

// ---------- Logs (P10-006/007) ----------

export const logKeys = {
  sources: ['logs', 'sources'] as const,
}

export function useLogSources() {
  return useQuery({
    queryKey: logKeys.sources,
    queryFn: () => apiFetch<LogSourceView[]>('/logs/sources'),
    staleTime: 30_000,
  })
}

export function useLogHistory(source: string, offset: number, limit = 200) {
  return useQuery({
    queryKey: ['logs', source, offset],
    queryFn: () =>
      apiFetch<LogHistoryResponse>(
        `/logs?source=${encodeURIComponent(source)}&limit=${limit}&offset=${offset}`,
      ),
    staleTime: 5_000,
  })
}