// Playwright Mock API 服务器（P9 §14.4/24.4）：
// 一个进程同时 serve `dist` 静态产物与 `/api/v1` 假后端（内存状态），
// 无 CORS/Docker/WARP 依赖。前端契约与后端实现保持一致
// （统一错误体、CSRF 校验、cookie 会话、secret 不回填）。

import { createServer } from 'node:http'
import { readFile, stat } from 'node:fs/promises'
import { join, extname } from 'node:path'
import { randomBytes } from 'node:crypto'
import { fileURLToPath } from 'node:url'

const PORT = Number(process.env.WARPDECK_MOCK_PORT ?? 8787)
const DIST = fileURLToPath(new URL('../dist/', import.meta.url))

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript',
  '.css': 'text/css',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.json': 'application/json',
  '.ico': 'image/x-icon',
  '.woff2': 'font/woff2',
}

// ---------- 内存状态 ----------

const state = {
  initialized: false,
  admin: null, // { username, password }
  instances: [], // { id, name, desired_state, runtime_state, exit_ip, colo, latency_ms, last_error }
  nextInstanceId: 1,
  nextSessionId: 1,
  sessions: new Map(), // sid -> { user_id, csrf }
  // v0.2 账号档案（P1 审查 R3#5：e2e 恢复覆盖 profiles 流程）
  profiles: [
    {
      id: 1,
      name: 'default',
      mode: 'free',
      zero_trust_org: null,
      license_configured: false,
      client_id_configured: false,
      client_secret_configured: false,
      instance_count: 0,
      default: true,
    },
  ],
  nextProfileId: 2,
  proxy: {
    socks5_enabled: true,
    http_enabled: true,
    auth_enabled: false,
    auth_configured: false,
    proxy_username: null,
    proxy_password: null,
    allowed_ips: [],
    max_connections: null,
    max_rps: null,
  },
  account: {
    mode: 'free',
    license: null,
    license_present: false,
    zero_trust_org: null,
    zero_trust_client_id: null,
    zero_trust_client_secret: null,
    zero_trust_configured: false,
  },
}

function reset() {
  state.initialized = false
  state.admin = null
  state.instances = []
  state.nextInstanceId = 1
  state.nextSessionId = 1
  state.sessions.clear()
  state.profiles = [
    {
      id: 1,
      name: 'default',
      mode: 'free',
      zero_trust_org: null,
      license_configured: false,
      client_id_configured: false,
      client_secret_configured: false,
      instance_count: 0,
      default: true,
    },
  ]
  state.nextProfileId = 2
  state.proxy = {
    socks5_enabled: true,
    http_enabled: true,
    auth_enabled: false,
    auth_configured: false,
    proxy_username: null,
    proxy_password: null,
    allowed_ips: [],
    max_connections: null,
    max_rps: null,
  }
  state.account = {
    mode: 'free',
    license: null,
    license_present: false,
    zero_trust_org: null,
    zero_trust_client_id: null,
    zero_trust_client_secret: null,
    zero_trust_configured: false,
  }
}

// ---------- helpers ----------

const errorBody = (code, message, requestId = 'mock') =>
  JSON.stringify({ error: { code, message, request_id: requestId } })

function sendJson(res, status, body) {
  const payload = typeof body === 'string' ? body : JSON.stringify(body)
  res.writeHead(status, { 'content-type': 'application/json' })
  res.end(payload)
}

function sendNoContent(res) {
  res.writeHead(204)
  res.end()
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    let data = ''
    req.on('data', (chunk) => {
      data += chunk
    })
    req.on('end', () => {
      try {
        resolve(data.length ? JSON.parse(data) : {})
      } catch {
        reject(new Error('invalid JSON'))
      }
    })
    req.on('error', reject)
  })
}

function parseCookies(req) {
  const header = req.headers.cookie ?? ''
  const out = {}
  for (const part of header.split(';')) {
    const idx = part.indexOf('=')
    if (idx > 0) out[part.slice(0, idx).trim()] = part.slice(idx + 1).trim()
  }
  return out
}

function currentSession(req) {
  const sid = parseCookies(req).warpdeck_session
  if (!sid) return null
  const session = state.sessions.get(sid)
  if (!session) return null
  if (session.expires < Date.now()) {
    state.sessions.delete(sid)
    return null
  }
  return session
}

function requireAuth(req, res, checkCsrf = false) {
  const session = currentSession(req)
  if (!session) {
    sendJson(res, 401, errorBody('UNAUTHORIZED', 'authentication required'))
    return null
  }
  if (checkCsrf) {
    const header = req.headers['x-csrf-token']
    if (header !== session.csrf) {
      sendJson(res, 403, errorBody('FORBIDDEN', 'invalid CSRF token'))
      return null
    }
  }
  return session
}

const instanceView = (inst) => ({
  id: inst.id,
  name: inst.name,
  enabled: true,
  desired_state: inst.desired_state,
  auto_restart: true,
  runtime_state: inst.runtime_state,
  exit_ip: inst.exit_ip,
  colo: inst.colo,
  latency_ms: inst.latency_ms,
  last_error: inst.last_error,
})

// v0.2 §17.6 档案视图（masked：永不回显 secret 明文）。
const profileView = (p) => ({
  id: p.id,
  name: p.name,
  mode: p.mode,
  zero_trust_org: p.zero_trust_org,
  license_configured: p.license_configured,
  client_id_configured: p.client_id_configured,
  client_secret_configured: p.client_secret_configured,
  instance_count: p.instance_count,
  default: Boolean(p.default),
})

// ---------- routes ----------

async function handleApi(req, res, url, method, requestId) {
  // 测试隔离：重置内存状态。
  if (url.pathname === '/__mock/reset' && method === 'POST') {
    reset()
    sendNoContent(res)
    return
  }

  if (url.pathname === '/api/v1/setup/status' && method === 'GET') {
    sendJson(res, 200, { initialized: state.initialized })
    return
  }

  if (url.pathname === '/api/v1/setup' && method === 'POST') {
    if (state.initialized) {
      sendJson(res, 409, errorBody('CONFLICT', 'setup already completed', requestId))
      return
    }
    const body = await readBody(req)
    if (String(body.password ?? '').length < 8) {
      sendJson(res, 422, errorBody('VALIDATION', 'password must be at least 8 characters', requestId))
      return
    }
    state.initialized = true
    state.admin = { username: String(body.username), password: String(body.password) }
    sendJson(res, 200, { initialized: true })
    return
  }

  if (url.pathname === '/api/v1/auth/login' && method === 'POST') {
    const body = await readBody(req)
    if (!state.admin || body.username !== state.admin.username || body.password !== state.admin.password) {
      sendJson(res, 401, errorBody('UNAUTHORIZED', 'invalid username or password', requestId))
      return
    }
    const sid = `mock-session-${state.nextSessionId++}`
    state.sessions.set(sid, {
      user_id: 1,
      csrf: randomBytes(16).toString('hex'),
      expires: Date.now() + 30 * 24 * 60 * 60 * 1000,
    })
    const session = state.sessions.get(sid)
    res.writeHead(200, {
      'content-type': 'application/json',
      'set-cookie': `warpdeck_session=${sid}; HttpOnly; Path=/; SameSite=Lax; Max-Age=2592000`,
    })
    res.end(
      JSON.stringify({
        user: { id: 1, username: state.admin.username },
        'x-csrf-token': session.csrf,
      }),
    )
    return
  }

  if (url.pathname === '/api/v1/auth/me' && method === 'GET') {
    const session = requireAuth(req, res)
    if (!session) return
    sendJson(res, 200, {
      user: { id: 1, username: state.admin.username },
      'x-csrf-token': session.csrf,
    })
    return
  }

  if (url.pathname === '/api/v1/auth/logout' && method === 'POST') {
    const cookies = parseCookies(req)
    state.sessions.delete(cookies.warpdeck_session)
    res.writeHead(204, {
      'set-cookie': 'warpdeck_session=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0',
    })
    res.end()
    return
  }

  if (url.pathname === '/api/v1/system/status' && method === 'GET') {
    const session = requireAuth(req, res)
    if (!session) return
    const counts = state.instances.reduce(
      (acc, i) => {
        acc.total += 1
        if (i.runtime_state === 'healthy') {
          acc.healthy += 1
          acc.running += 1
        }
        if (i.runtime_state === 'degraded') acc.degraded += 1
        if (i.runtime_state === 'failed') acc.failed += 1
        if (i.runtime_state === 'stopped') acc.stopped += 1
        return acc
      },
      { total: 0, running: 0, healthy: 0, degraded: 0, failed: 0, stopped: 0 },
    )
    sendJson(res, 200, {
      status: 'ok',
      version: '0.9.0-mock',
      uptime_secs: 123,
      instances: counts,
    })
    return
  }

  if (url.pathname === '/api/v1/instances' && method === 'GET') {
    const session = requireAuth(req, res)
    if (!session) return
    sendJson(res, 200, state.instances.map(instanceView))
    return
  }

  if (url.pathname === '/api/v1/instances' && method === 'POST') {
    const session = requireAuth(req, res, true)
    if (!session) return
    const body = await readBody(req)
    const name = String(body.name ?? '').trim()
    if (!name) {
      sendJson(res, 422, errorBody('VALIDATION', 'name must not be empty', requestId))
      return
    }
    const inst = {
      id: state.nextInstanceId++,
      name,
      desired_state: 'running',
      runtime_state: 'starting',
      exit_ip: null,
      colo: null,
      latency_ms: null,
      last_error: null,
    }
    state.instances.push(inst)
    // 模拟收敛：starting -> healthy。
    setTimeout(() => {
      inst.runtime_state = 'healthy'
      inst.exit_ip = '104.28.1.2'
      inst.colo = 'SJC'
      inst.latency_ms = 38
    }, 600)
    sendJson(res, 201, instanceView(inst))
    return
  }

  const instanceMatch = url.pathname.match(/^\/api\/v1\/instances\/(\d+)\/(start|stop|restart)$/)
  if (instanceMatch && method === 'POST') {
    const session = requireAuth(req, res, true)
    if (!session) return
    const inst = state.instances.find((i) => i.id === Number(instanceMatch[1]))
    if (!inst) {
      sendJson(res, 404, errorBody('NOT_FOUND', `instance ${instanceMatch[1]} not found`, requestId))
      return
    }
    const action = instanceMatch[2]
    if (action === 'start') {
      inst.desired_state = 'running'
      inst.runtime_state = 'starting'
      setTimeout(() => {
        inst.runtime_state = 'healthy'
        inst.exit_ip = '104.28.1.2'
        inst.colo = 'SJC'
        inst.latency_ms = 38
      }, 600)
    } else if (action === 'stop') {
      inst.desired_state = 'stopped'
      inst.runtime_state = 'stopped'
    } else {
      inst.runtime_state = 'starting'
      setTimeout(() => {
        inst.runtime_state = 'healthy'
      }, 600)
    }
    sendNoContent(res)
    return
  }

  const detailMatch = url.pathname.match(/^\/api\/v1\/instances\/(\d+)$/)
  if (detailMatch && method === 'GET') {
    const session = requireAuth(req, res)
    if (!session) return
    const inst = state.instances.find((i) => i.id === Number(detailMatch[1]))
    if (!inst) {
      sendJson(res, 404, errorBody('NOT_FOUND', `instance ${detailMatch[1]} not found`, requestId))
      return
    }
    sendJson(res, 200, instanceView(inst))
    return
  }

  const deleteMatch = url.pathname.match(/^\/api\/v1\/instances\/(\d+)$/)
  if (deleteMatch && method === 'DELETE') {
    const session = requireAuth(req, res, true)
    if (!session) return
    const idx = state.instances.findIndex((i) => i.id === Number(deleteMatch[1]))
    if (idx === -1) {
      sendJson(res, 404, errorBody('NOT_FOUND', `instance ${deleteMatch[1]} not found`, requestId))
      return
    }
    state.instances.splice(idx, 1)
    sendNoContent(res)
    return
  }

  // ---------- v0.2 账号档案（§17.6；masked 视图） ----------
  if (url.pathname === '/api/v1/accounts' && method === 'GET') {
    const session = requireAuth(req, res)
    if (!session) return
    sendJson(res, 200, state.profiles.map(profileView))
    return
  }

  if (url.pathname === '/api/v1/accounts' && method === 'POST') {
    const session = requireAuth(req, res, true)
    if (!session) return
    const body = await readBody(req)
    const name = String(body.name ?? '').trim()
    const mode = String(body.mode ?? '')
    const org = body.zero_trust_org ? String(body.zero_trust_org) : null
    if (!name) {
      sendJson(res, 422, errorBody('VALIDATION', 'Name is required', requestId))
      return
    }
    if (state.profiles.some((p) => p.name === name)) {
      sendJson(res, 409, errorBody('CONFLICT', 'profile name already exists', requestId))
      return
    }
    if (!['free', 'warp_plus', 'zero_trust'].includes(mode)) {
      sendJson(res, 422, errorBody('VALIDATION', 'invalid mode', requestId))
      return
    }
    // 与后端一致：warp_plus 必须 license；zero_trust 必须三件套（§16.9 校验）。
    if (mode === 'warp_plus' && !body.license) {
      sendJson(res, 422, errorBody('VALIDATION', 'WARP+ requires a license key', requestId))
      return
    }
    if (mode === 'zero_trust' && (!org || !body.client_id || !body.client_secret)) {
      sendJson(res, 422, errorBody('VALIDATION', 'zero_trust requires organization/client id/secret', requestId))
      return
    }
    if (mode === 'free' && state.profiles.some((p) => p.mode === 'free')) {
      sendJson(res, 409, errorBody('CONFLICT', 'free profile is unique and reserved', requestId))
      return
    }
    const profile = {
      id: state.nextProfileId++,
      name,
      mode,
      zero_trust_org: org,
      license_configured: Boolean(body.license),
      client_id_configured: Boolean(body.client_id),
      client_secret_configured: Boolean(body.client_secret),
      instance_count: 0,
      default: false,
    }
    state.profiles.push(profile)
    sendJson(res, 201, profileView(profile))
    return
  }

  const profileMatch = url.pathname.match(/^\/api\/v1\/accounts\/(\d+)$/)
  if (profileMatch && (method === 'PATCH' || method === 'DELETE')) {
    const session = requireAuth(req, res, method !== 'GET')
    if (!session) return
    const pid = Number(profileMatch[1])
    const profile = state.profiles.find((p) => p.id === pid)
    if (!profile) {
      sendJson(res, 404, errorBody('NOT_FOUND', `profile ${pid} not found`, requestId))
      return
    }
    if (method === 'DELETE') {
      if (profile.default || profile.instance_count > 0) {
        sendJson(res, 409, errorBody('CONFLICT', 'profile is protected or still bound', requestId))
        return
      }
      state.profiles = state.profiles.filter((p) => p.id !== pid)
      sendNoContent(res)
      return
    }
    const body = await readBody(req)
    // masked 语义：undefined/空 = 保持现有凭据。
    if (body.name !== undefined) profile.name = String(body.name)
    if (body.mode !== undefined) profile.mode = String(body.mode)
    if (body.zero_trust_org !== undefined) profile.zero_trust_org = body.zero_trust_org || null
    if (body.license) profile.license_configured = true
    if (body.client_id) profile.client_id_configured = true
    if (body.client_secret) profile.client_secret_configured = true
    sendJson(res, 200, profileView(profile))
    return
  }

  if (url.pathname === '/api/v1/proxy' && method === 'GET') {
    const session = requireAuth(req, res)
    if (!session) return
    sendJson(res, 200, {
      socks5_enabled: state.proxy.socks5_enabled,
      http_enabled: state.proxy.http_enabled,
      auth_enabled: state.proxy.auth_enabled,
      auth_configured: state.proxy.auth_configured,
      allowed_ips: state.proxy.allowed_ips,
      max_connections: state.proxy.max_connections,
      max_rps: state.proxy.max_rps,
    })
    return
  }

  if (url.pathname === '/api/v1/proxy' && method === 'PUT') {
    const session = requireAuth(req, res, true)
    if (!session) return
    const body = await readBody(req)
    for (const key of ['socks5_enabled', 'http_enabled', 'auth_enabled']) {
      if (typeof body[key] === 'boolean') state.proxy[key] = body[key]
    }
    if (typeof body.username === 'string' && body.username.trim()) {
      state.proxy.proxy_username = body.username.trim()
    }
    if (typeof body.password === 'string') {
      if (body.password === '') {
        state.proxy.proxy_password = null
        state.proxy.auth_configured = false
      } else {
        state.proxy.proxy_password = body.password
        state.proxy.auth_configured = true
      }
    }
    if (Array.isArray(body.allowed_ips)) {
      if (body.allowed_ips.some((ip) => typeof ip !== 'string')) {
        sendJson(res, 422, errorBody('VALIDATION', 'allowed_ips must be strings', requestId))
        return
      }
      state.proxy.allowed_ips = body.allowed_ips
    }
    if (body.max_connections !== undefined) {
      if (body.max_connections !== null && body.max_connections < 1) {
        sendJson(res, 422, errorBody('VALIDATION', 'max_connections must be >= 1', requestId))
        return
      }
      state.proxy.max_connections = body.max_connections
    }
    if (body.max_rps !== undefined) {
      if (body.max_rps !== null && body.max_rps < 1) {
        sendJson(res, 422, errorBody('VALIDATION', 'max_rps must be >= 1', requestId))
        return
      }
      state.proxy.max_rps = body.max_rps
    }
    sendJson(res, 200, {
      socks5_enabled: state.proxy.socks5_enabled,
      http_enabled: state.proxy.http_enabled,
      auth_enabled: state.proxy.auth_enabled,
      auth_configured: state.proxy.auth_configured,
      allowed_ips: state.proxy.allowed_ips,
      max_connections: state.proxy.max_connections,
      max_rps: state.proxy.max_rps,
    })
    return
  }

  if (url.pathname === '/api/v1/account' && method === 'GET') {
    const session = requireAuth(req, res)
    if (!session) return
    // 与后端一致：永不回填 secret 明文。
    sendJson(res, 200, {
      mode: state.account.mode,
      configured: state.account.license_present || state.account.zero_trust_configured,
      license_present: state.account.license_present,
      zero_trust_configured: state.account.zero_trust_configured,
      zero_trust_org: state.account.zero_trust_org,
    })
    return
  }

  if (url.pathname === '/api/v1/account' && method === 'PUT') {
    const session = requireAuth(req, res, true)
    if (!session) return
    const body = await readBody(req)
    const mode = body.mode ?? state.account.mode
    const licensePresent =
      body.license === undefined
        ? state.account.license_present
        : String(body.license ?? '').length > 0
    const ztIdPresent =
      body.client_id === undefined
        ? state.account.zero_trust_client_id !== null
        : String(body.client_id ?? '').length > 0
    const ztSecretPresent =
      body.client_secret === undefined
        ? state.account.zero_trust_client_secret !== null
        : String(body.client_secret ?? '').length > 0
    const org = body.zero_trust_org === undefined ? state.account.zero_trust_org : String(body.zero_trust_org ?? '').trim() || null

    if (mode === 'warp_plus' && !licensePresent) {
      sendJson(res, 422, errorBody('VALIDATION', 'warp_plus mode requires a license', requestId))
      return
    }
    if (mode === 'zero_trust' && !(ztIdPresent && ztSecretPresent && org)) {
      sendJson(res, 422, errorBody('VALIDATION', 'zero_trust mode requires org, client id and client secret', requestId))
      return
    }

    state.account.mode = mode
    if (body.license !== undefined) {
      state.account.license = body.license === '' ? null : body.license
      state.account.license_present = Boolean(state.account.license)
    }
    if (body.zero_trust_org !== undefined) {
      state.account.zero_trust_org = org
    }
    if (body.client_id !== undefined) {
      state.account.zero_trust_client_id = body.client_id === '' ? null : String(body.client_id)
    }
    if (body.client_secret !== undefined) {
      state.account.zero_trust_client_secret =
        body.client_secret === '' ? null : String(body.client_secret)
    }
    state.account.zero_trust_configured =
      state.account.zero_trust_client_id !== null && state.account.zero_trust_client_secret !== null

    sendJson(res, 200, {
      mode: state.account.mode,
      configured: state.account.license_present || state.account.zero_trust_configured,
      license_present: state.account.license_present,
      zero_trust_configured: state.account.zero_trust_configured,
      zero_trust_org: state.account.zero_trust_org,
    })
    return
  }

  if (url.pathname === '/api/v1/audit/logs' && method === 'GET') {
    const session = requireAuth(req, res)
    if (!session) return
    sendJson(res, 200, [])
    return
  }

  sendJson(res, 404, errorBody('NOT_FOUND', `no such endpoint: ${method} ${url.pathname}`))
}

// ---------- static SPA ----------

async function serveStatic(req, res, url) {
  let pathname = decodeURIComponent(url.pathname)
  if (pathname === '/') pathname = '/index.html'
  const filePath = join(DIST, pathname)
  try {
    const info = await stat(filePath)
    if (!info.isFile()) throw new Error('not a file')
    const type = MIME[extname(filePath).toLowerCase()] ?? 'application/octet-stream'
    const content = await readFile(filePath)
    res.writeHead(200, { 'content-type': type })
    res.end(content)
  } catch {
    // SPA fallback：路由路径返回 index.html。
    try {
      const content = await readFile(join(DIST, 'index.html'))
      res.writeHead(200, { 'content-type': MIME['.html'] })
      res.end(content)
    } catch {
      res.writeHead(500)
      res.end('dist not built — run `pnpm build` before e2e')
    }
  }
}

// ---------- server ----------

const server = createServer(async (req, res) => {
  const url = new URL(req.url ?? '/', `http://${req.headers.host ?? 'localhost'}`)
  try {
    if (url.pathname.startsWith('/api/') || url.pathname === '/__mock/reset') {
      await handleApi(req, res, url, req.method ?? 'GET', 'mock-request')
    } else {
      await serveStatic(req, res, url)
    }
  } catch (err) {
    res.writeHead(400, { 'content-type': 'application/json' })
    res.end(errorBody('VALIDATION', err instanceof Error ? err.message : 'bad request'))
  }
})

server.listen(PORT, '127.0.0.1', () => {
  console.log(`warpdeck mock server: http://127.0.0.1:${PORT} (dist: ${DIST})`)
})

process.on('SIGTERM', () => server.close(() => process.exit(0)))
process.on('SIGINT', () => server.close(() => process.exit(0)))