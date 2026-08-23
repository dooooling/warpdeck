// 后端 API 契约类型（与 crates/warpdeck-server/src/api/dto.rs 保持同步）。

export type RuntimeState =
  | 'disabled'
  | 'stopped'
  | 'starting'
  | 'registering'
  | 'connecting'
  | 'healthy'
  | 'degraded'
  | 'stopping'
  | 'failed'

export interface InstanceView {
  id: number
  name: string
  enabled: boolean
  /** 期望状态：`running` | `stopped`（数据库列）。 */
  desired_state: 'running' | 'stopped'
  auto_restart: boolean
  /** 运行时实际状态（九态）。 */
  runtime_state: RuntimeState
  /** 主显示出口 IP（v4 优先，v6 兜底）。 */
  exit_ip: string | null
  /** v4 出口 IP（双地址族探测，P13-001）。 */
  exit_ip_v4: string | null
  /** v6 出口 IP（双地址族探测，P13-001）。 */
  exit_ip_v6: string | null
  colo: string | null
  latency_ms: number | null
  last_error: string | null
  /** 实际重启次数（RuntimeRegistry；未运行 = 0，P1 审查 R4）。 */
  restarts: number
  /** v0.2 §17.4：绑定的账号档案摘要（NULL 绑定按默认 free 档展开）。 */
  account: AccountRefView | null
}

/** 实例视角的档案摘要（§17.4 响应 `account` 字段；无任何 secret）。 */
export interface AccountRefView {
  profile_id: number
  name: string
  mode: AccountMode
}

/** 创建实例请求（P7-005：MVP 只需 name；v0.2 支持绑定档案）。 */
export interface CreateInstanceRequest {
  name: string
  /** 缺省/NULL = 默认 free 档（§17.4）。 */
  account_profile_id?: number | null
}

/** 实例更新请求（v0.2 §17.4）：`account_profile_id` 仅档案改绑（显式 null = 解绑默认档）。 */
export interface PatchInstanceRequest {
  account_profile_id: number | null
}

/** GOST 数据面实际状态（P1 审查 #4：desired ≠ actual 必须可见）。 */
export interface ProxyActual {
  status: 'running' | 'stopped' | 'degraded' | 'failed'
  pid?: number
  exit_code?: number
  reason?: string
}

export interface ProxyConfigView {
  socks5_enabled: boolean
  http_enabled: boolean
  auth_enabled: boolean
  /** secret store 中是否存在代理密码。 */
  auth_configured: boolean
  allowed_ips: string[]
  max_connections: number | null
  max_rps: number | null
  /** GOST 实际状态（后端未追踪时缺省）。 */
  actual?: ProxyActual
}

/** 代理配置部分更新（P8 语义：undefined = 保持；password "" = 清除，非空 = 设置/轮换）。 */
export interface UpdateProxyRequest {
  socks5_enabled?: boolean
  http_enabled?: boolean
  auth_enabled?: boolean
  username?: string
  password?: string
  allowed_ips?: string[]
  max_connections?: number | null
  max_rps?: number | null
}

export type AccountMode = 'free' | 'warp_plus' | 'zero_trust'

/** v0.2 §17.6：账号档案视图（永不包含 secret 明文，仅 mask 状态）。 */
export interface AccountProfileView {
  id: number
  name: string
  mode: AccountMode
  /** Zero Trust org 名（非 secret）。 */
  zero_trust_org: string | null
  license_configured: boolean
  client_id_configured: boolean
  client_secret_configured: boolean
  /** 绑定该档案的实例数（NULL 绑定计入默认 free 档）。 */
  instance_count: number
  /** 内置默认档（id=1；不可删除）。 */
  default: boolean
}

/** 档案创建/更新（undefined = 保持；"" = 清除；非空 = 设置/轮换）。 */
export interface AccountProfileWriteRequest {
  name?: string
  mode?: AccountMode
  zero_trust_org?: string
  license?: string
  client_id?: string
  client_secret?: string
}

export interface InstanceCounts {
  total: number
  running: number
  healthy: number
  degraded: number
  failed: number
  stopped: number
}

/** 组件 operational 状态（P1 审查 #4）。 */
export interface SystemComponents {
  gost: string
  gost_reason?: string
  secret_store: string
}

export interface SystemStatusView {
  status: string
  version: string
  uptime_secs: number
  instances: InstanceCounts
  components?: SystemComponents
  last_apply_error?: { error: string; at_rfc3339: string }
}

export interface UserInfo {
  id: number
  username: string
}

export interface AuthResponse {
  user: UserInfo
  'x-csrf-token': string
}

export interface SetupStatusResponse {
  initialized: boolean
}

export type ApiErrorCode =
  | 'VALIDATION'
  | 'UNAUTHORIZED'
  | 'FORBIDDEN'
  | 'NOT_FOUND'
  | 'CONFLICT'
  | 'INTERNAL'

/** 统一错误契约（P7-002）：`{"error": {"code", "message", "request_id"}}`。 */
export interface ApiErrorBody {
  error: {
    code: ApiErrorCode
    message: string
    request_id: string
  }
}

// ---------- Logs (P10-006/007) ----------

export interface LogSourceView {
  source: string
  kind: 'manager' | 'gost' | 'instance'
  instance_id: number | null
  exists: boolean
}

export interface LogHistoryResponse {
  source: string
  offset: number
  next_offset: number
  has_more: boolean
  lines: string[]
}

export interface LogLineEvent {
  source: string
  seq: number
  line: string
}