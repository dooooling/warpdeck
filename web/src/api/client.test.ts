// API client 单元测试（P9 §14.4：错误映射 / CSRF / 401 回调 / 序列化）。

import { afterEach, describe, expect, it, vi } from 'vitest'

import { ApiError, apiFetch, setCsrfToken, setUnauthorizedHandler } from './client'

const jsonResponse = (body: unknown, status: number) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  })

type FetchMock = (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>

/** 取最后一次 fetch 调用并解构出 url/init（vi.fn 泛型化后类型安全）。 */
function lastCall(fetchMock: ReturnType<typeof vi.fn>): { url: string; init: RequestInit } {
  const [url, init] = fetchMock.mock.calls.at(-1) as unknown as [
    RequestInfo | URL,
    RequestInit | undefined,
  ]
  return { url: String(url), init: init ?? {} }
}

function stubFetch(impl: FetchMock) {
  vi.stubGlobal('fetch', vi.fn<FetchMock>(impl))
}

afterEach(() => {
  vi.unstubAllGlobals()
  setCsrfToken(null)
  setUnauthorizedHandler(null)
})

describe('apiFetch error mapping', () => {
  it('parses the unified error contract (P7-002)', async () => {
    stubFetch(async () =>
      jsonResponse(
        { error: { code: 'VALIDATION', message: 'name must not be empty', request_id: 'rid-1' } },
        422,
      ),
    )
    const err = await apiFetch('/instances', { method: 'POST', body: {} }).catch((e: unknown) => e)
    expect(err).toBeInstanceOf(ApiError)
    expect(err).toMatchObject({
      status: 422,
      code: 'VALIDATION',
      message: 'name must not be empty',
      requestId: 'rid-1',
    })
  })

  it('falls back to UNKNOWN for non-JSON error bodies', async () => {
    stubFetch(async () => new Response('<html>oops</html>', { status: 502 }))
    const err = await apiFetch('/system/status').catch((e: unknown) => e)
    expect(err).toMatchObject({ status: 502, code: 'UNKNOWN', requestId: null })
  })

  it('maps 401 to UNAUTHORIZED and fires the handler', async () => {
    const handler = vi.fn()
    setUnauthorizedHandler(handler)
    stubFetch(async () =>
      jsonResponse(
        { error: { code: 'UNAUTHORIZED', message: 'authentication required', request_id: 'r' } },
        401,
      ),
    )
    await expect(apiFetch('/instances')).rejects.toMatchObject({ code: 'UNAUTHORIZED' })
    expect(handler).toHaveBeenCalledTimes(1)
  })

  it('does not fire the handler for other statuses', async () => {
    const handler = vi.fn()
    setUnauthorizedHandler(handler)
    stubFetch(async () =>
      jsonResponse(
        { error: { code: 'CONFLICT', message: 'not running', request_id: 'r' } },
        409,
      ),
    )
    await expect(apiFetch('/instances/1/restart', { method: 'POST' })).rejects.toMatchObject({
      code: 'CONFLICT',
    })
    expect(handler).not.toHaveBeenCalled()
  })
})

describe('apiFetch request construction', () => {
  it('serializes JSON bodies and sends session cookie credentials', async () => {
    const fetchMock = vi.fn<FetchMock>(async () => jsonResponse({ ok: true }, 200))
    vi.stubGlobal('fetch', fetchMock)
    await apiFetch('/instances', { method: 'POST', body: { name: 'warp-1' } })
    const { url, init } = lastCall(fetchMock)
    expect(url).toBe('/api/v1/instances')
    expect(init.method).toBe('POST')
    expect(init.credentials).toBe('include')
    expect(init.body).toBe(JSON.stringify({ name: 'warp-1' }))
    expect(new Headers(init.headers).get('content-type')).toBe('application/json')
  })

  it('attaches the CSRF header to mutations when a token is known', async () => {
    setCsrfToken('tok-123')
    const fetchMock = vi.fn<FetchMock>(async () => new Response(null, { status: 204 }))
    vi.stubGlobal('fetch', fetchMock)
    await apiFetch('/auth/logout', { method: 'POST' })
    const { init } = lastCall(fetchMock)
    expect(new Headers(init.headers).get('x-csrf-token')).toBe('tok-123')
  })

  it('does not attach CSRF to GET requests', async () => {
    setCsrfToken('tok-123')
    const fetchMock = vi.fn<FetchMock>(async () => jsonResponse([], 200))
    vi.stubGlobal('fetch', fetchMock)
    await apiFetch('/instances')
    const { init } = lastCall(fetchMock)
    expect(new Headers(init.headers).get('x-csrf-token')).toBeNull()
  })

  it('returns undefined for 204 responses', async () => {
    stubFetch(async () => new Response(null, { status: 204 }))
    await expect(apiFetch('/instances/1/stop', { method: 'POST' })).resolves.toBeUndefined()
  })
})