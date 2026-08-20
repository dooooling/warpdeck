// Typed API client（P9-002 / DESIGN §22.1）。
//
// 集中处理 base URL、JSON、连续 CSRF、401 会话失效、统一错误契约。
// 页面与 hooks 不自行裸 `fetch()`。
//
// 依赖注入（不 import React）：CSRF token 与 401 处理由 AuthContext 注册，
// 避免 client 与路由组件循环依赖。

import type { ApiErrorBody, ApiErrorCode } from './types'

const API_BASE = '/api/v1'

/** CSRF 头名（与后端 `auth::CSRF_HEADER` 一致）。 */
export const CSRF_HEADER = 'x-csrf-token'

let csrfToken: string | null = null
let onUnauthorized: (() => void) | null = null

/** 由 AuthContext 在 login/me 成功后设置；logout/401 后清空。 */
export function setCsrfToken(token: string | null): void {
  csrfToken = token
}

/** 由 AuthContext 注册：收到 401 时清空会话并跳转登录页。 */
export function setUnauthorizedHandler(handler: (() => void) | null): void {
  onUnauthorized = handler
}

/** 带类型与统一错误契约的异常（组件可直接展示 message）。 */
export class ApiError extends Error {
  readonly status: number
  readonly code: ApiErrorCode | 'UNKNOWN'
  readonly requestId: string | null

  constructor(status: number, code: ApiErrorCode | 'UNKNOWN', message: string, requestId: string | null) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
    this.requestId = requestId
  }
}

async function parseError(response: Response): Promise<ApiError> {
  let code: ApiErrorCode | 'UNKNOWN' = 'UNKNOWN'
  let requestId: string | null = null
  let message = `request failed with status ${response.status}`
  try {
    const body = (await response.json()) as ApiErrorBody
    if (body.error) {
      code = body.error.code
      message = body.error.message
      requestId = body.error.request_id
    }
  } catch {
    // 非 JSON 错误体：保留默认 message。
  }
  return new ApiError(response.status, code, message, requestId)
}

/**
 * 统一请求入口。
 * - `credentials: 'include'`（HttpOnly session cookie）；
 * - Mutation 自动带 `x-csrf-token` 头（登录/setup 等 public 端点忽略即可）；
 * - 401 触发会话失效回调（AuthContext 负责清 session + 跳转）。
 */
export async function apiFetch<T>(
  path: string,
  init: Omit<RequestInit, 'body'> & { body?: unknown } = {},
): Promise<T> {
  const method = (init.method ?? 'GET').toUpperCase()
  const headers = new Headers(init.headers)
  const hasBody = init.body !== undefined
  if (hasBody) {
    headers.set('Content-Type', 'application/json')
  }
  if (method !== 'GET' && csrfToken) {
    headers.set(CSRF_HEADER, csrfToken)
  }

  const response = await fetch(`${API_BASE}${path}`, {
    ...init,
    method,
    headers,
    credentials: 'include',
    body: hasBody ? JSON.stringify(init.body) : undefined,
  })

  if (response.status === 401) {
    onUnauthorized?.()
  }
  if (!response.ok) {
    throw await parseError(response)
  }
  if (response.status === 204) {
    return undefined as T
  }
  return (await response.json()) as T
}

/** 便于测试替换（vitest 里 mock 全局 fetch 即可，无额外间接层）。 */
export function apiBase(): string {
  return API_BASE
}