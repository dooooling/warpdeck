//! API DTO 层（P7-001）。
//!
//! 设计：DTO 与领域模型分离——handler 只暴露此处定义的结构，
//! 领域类型（`WarpInstanceSpec`/`InstanceRuntime`/`ProxyConfig`）不直接序列化。
//! 序列化字段即对外契约（DESIGN §17.x），修改需同步计划 §12.2。

use serde::{Deserialize, Serialize};

use crate::db::profiles::AccountProfile;
use crate::db::repo::{ProxyConfig, WarpInstanceSpec};
use crate::runtime::registry::{InstanceRuntime, RuntimeState};

/// 实例状态视图（合并期望状态 + 实际运行状态；P7-004）。
#[derive(Debug, Clone, Serialize)]
pub struct InstanceView {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    /// 期望状态：`running` | `stopped`（数据库列，见 P6-003）。
    pub desired_state: String,
    pub auto_restart: bool,
    /// 运行时实际状态：九态字符串（DESIGN §10）。
    pub runtime_state: String,
    /// 主显示出口 IP（v4 优先，v6 兜底；兼容字段，P13-001 起由双字段驱动）。
    pub exit_ip: Option<String>,
    /// v4 出口 IP（双地址族探测，P13-001）。
    pub exit_ip_v4: Option<String>,
    /// v6 出口 IP（双地址族探测，P13-001）。
    pub exit_ip_v6: Option<String>,
    pub colo: Option<String>,
    pub latency_ms: Option<u32>,
    pub last_error: Option<String>,
    /// v0.2：绑定的账号档案摘要（NULL = 默认 free 档，展开其信息，§17.4）。
    pub account: Option<AccountRefView>,
}

/// 实例视角的档案摘要（§17.4 响应 `account` 字段；无任何 secret）。
#[derive(Debug, Clone, Serialize)]
pub struct AccountRefView {
    pub profile_id: i64,
    pub name: String,
    pub mode: String,
}

impl InstanceView {
    /// 由期望记录 + 实际快照合并；快照缺省（从未启动）视为 `stopped`。
    pub fn from_parts(spec: &WarpInstanceSpec, actual: Option<&InstanceRuntime>) -> Self {
        let (state, exit_v4, exit_v6, colo, latency_ms, last_error) = match actual {
            Some(r) => (
                r.state,
                r.exit_ip_v4.map(|ip| ip.to_string()),
                r.exit_ip_v6.map(|ip| ip.to_string()),
                r.colo.clone(),
                r.latency_ms,
                r.last_error.clone(),
            ),
            None => (RuntimeState::Stopped, None, None, None, None, None),
        };
        Self {
            id: spec.id.as_i64(),
            name: spec.name.clone(),
            enabled: spec.enabled,
            desired_state: spec.desired_state.as_str().to_string(),
            auto_restart: spec.auto_restart,
            runtime_state: state.as_str().to_string(),
            exit_ip: exit_v4.clone().or_else(|| exit_v6.clone()),
            exit_ip_v4: exit_v4,
            exit_ip_v6: exit_v6,
            colo,
            latency_ms,
            last_error,
            account: None,
        }
    }

    /// 填入实例绑定的档案摘要（批量加载的 profiles 由调用方传入）。
    pub fn with_account(
        mut self,
        profiles: &[AccountProfile],
        bound_profile_id: Option<i64>,
    ) -> Self {
        // NULL 绑定 = 默认 free 档（§16.9）：按 id=1 展开，与 resolver 语义一致。
        let pid = bound_profile_id.unwrap_or(1);
        self.account = profiles
            .iter()
            .find(|p| p.id == pid)
            .map(|p| AccountRefView {
                profile_id: p.id,
                name: p.name.clone(),
                mode: p.mode.as_str().to_string(),
            });
        self
    }
}

/// 创建实例请求（P7-005；v0.2 支持绑定档案）。
#[derive(Debug, Clone, Deserialize)]
pub struct CreateInstanceRequest {
    pub name: String,
    /// v0.2 §17.4：绑定的账号档案（缺省/NULL = 默认 free 档）。
    pub account_profile_id: Option<i64>,
}

impl CreateInstanceRequest {
    /// 名称校验：trim 后非空且 ≤64 字符（422 语义）。
    pub fn validate(&self) -> Result<String, String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("name must not be empty".to_string());
        }
        if name.chars().count() > 64 {
            return Err("name must be at most 64 characters".to_string());
        }
        Ok(name.to_string())
    }
}

/// 实例更新请求（v0.2 §17.4）：仅档案改绑（改绑在下次重启生效，
/// 后端置 restart_pending，UI 提示将触发重启）。
///
/// 区分「字段缺失」与「显式 null」：缺省 = 422（无字段可改）；
/// 显式 `null` = 解绑到默认 free 档。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PatchInstanceRequest {
    #[serde(default, deserialize_with = "deserialize_explicit_nullable")]
    pub account_profile_id: Option<Option<i64>>,
}

fn deserialize_explicit_nullable<'de, D>(de: D) -> Result<Option<Option<i64>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<i64>::deserialize(de)?))
}

impl PatchInstanceRequest {
    /// 校验：至少提供一个可改字段（空 body 无意义）。
    pub fn validate(&self) -> Result<(), String> {
        if self.account_profile_id.is_none() {
            return Err("no updateable field provided (account_profile_id expected)".to_string());
        }
        Ok(())
    }
}

/// 账号档案视图（v0.2 §17.6；绝不包含 secret 明文，仅 mask 状态）。
#[derive(Debug, Clone, Serialize)]
pub struct AccountProfileView {
    pub id: i64,
    pub name: String,
    /// `free` | `warp_plus` | `zero_trust`。
    pub mode: String,
    /// Zero Trust org 名（非 secret；None = 未设置）。
    pub zero_trust_org: Option<String>,
    pub license_configured: bool,
    pub client_id_configured: bool,
    pub client_secret_configured: bool,
    /// 绑定该档案的实例数（NULL 绑定计入默认 free 档）。
    pub instance_count: usize,
    /// 是否为内置默认档（id=1；不可删除）。
    pub default: bool,
}

impl AccountProfileView {
    /// 由档案行 + 凭据存在性 mask + 绑定计数组装。
    pub fn from_parts(
        p: &AccountProfile,
        license_configured: bool,
        client_id_configured: bool,
        client_secret_configured: bool,
        instance_count: usize,
    ) -> Self {
        Self {
            id: p.id,
            name: p.name.clone(),
            mode: p.mode.as_str().to_string(),
            zero_trust_org: p.zero_trust_org.clone(),
            license_configured,
            client_id_configured,
            client_secret_configured,
            instance_count,
            default: p.id == 1,
        }
    }
}

/// 账号档案写入请求（POST 全量 / PATCH 部分更新共用）。
/// 秘密字段（license/client_secret）提交即入密文库，永不回显。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccountProfileWriteRequest {
    /// PATCH 可省略；POST 必填（name 校验由 handler 做）。
    pub name: Option<String>,
    pub mode: Option<String>,
    pub zero_trust_org: Option<String>,
    pub license: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

/// 代理配置视图（P7-008；P8 接 secret store）。
///
/// 秘密边界（AGENTS.md / DESIGN §16.4）：不返回 `proxy_username` 明文，
/// 只给 `auth_configured` 布尔（由 secret store 是否存在密码决定）。
#[derive(Debug, Clone, Serialize)]
pub struct ProxyConfigView {
    pub socks5_enabled: bool,
    pub http_enabled: bool,
    pub auth_enabled: bool,
    pub auth_configured: bool,
    pub allowed_ips: Vec<String>,
    pub max_connections: Option<u32>,
    pub max_rps: Option<u32>,
    /// GOST 实际状态（P1 审查 #4：desired ≠ actual 必须可见）。
    /// None = 实现未追踪（测试 fake）；生产恒 Some。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<ProxyActualView>,
}

impl ProxyConfigView {
    /// `auth_configured` = secret store 中是否存在代理密码（P8-009）。
    pub fn from_config(cfg: &ProxyConfig, auth_configured: bool) -> Self {
        Self {
            socks5_enabled: cfg.socks5_enabled,
            http_enabled: cfg.http_enabled,
            auth_enabled: cfg.auth_enabled,
            auth_configured,
            allowed_ips: cfg.allowed_ips.clone(),
            max_connections: cfg.max_connections,
            max_rps: cfg.max_rps,
            actual: None,
        }
    }

    /// 附加实际状态（GET/PUT handler 在查询 GostManager 后调用）。
    pub fn with_actual(mut self, actual: Option<ProxyActualView>) -> Self {
        self.actual = actual;
        self
    }
}

/// GOST 数据面实际状态视图。
/// `status`: `running` / `stopped` / `degraded` / `failed`。
#[derive(Debug, Clone, Serialize)]
pub struct ProxyActualView {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ProxyActualView {
    pub fn from_status(status: &crate::proxy::ProxyStatus) -> Self {
        use crate::proxy::ProxyStatus as S;
        match status {
            S::Stopped => Self {
                status: "stopped".into(),
                pid: None,
                exit_code: None,
                reason: None,
            },
            S::Running { pid, .. } => Self {
                status: "running".into(),
                pid: Some(*pid),
                exit_code: None,
                reason: None,
            },
            S::Degraded { reason, pid } => Self {
                status: "degraded".into(),
                pid: *pid,
                exit_code: None,
                reason: Some(reason.clone()),
            },
            S::Failed { reason, exit_code } => Self {
                status: "failed".into(),
                pid: None,
                exit_code: *exit_code,
                reason: Some(reason.clone()),
            },
        }
    }
}

/// 代理配置更新请求（P7-008；Option 字段 = 部分更新）。
/// 端口（11080/18080）不属于可改字段：host 映射归 Compose `.env`（AGENTS.md）。
///
/// P8 扩展（DESIGN §20.6）：
/// - `username`: 用户名（None = 保持，Some 非空 = 设置）；
/// - `password`: 密码（None = 保持，Some("") = 清除，Some 非空 = 设置/轮换）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateProxyRequest {
    pub socks5_enabled: Option<bool>,
    pub http_enabled: Option<bool>,
    pub auth_enabled: Option<bool>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub allowed_ips: Option<Vec<String>>,
    pub max_connections: Option<u32>,
    pub max_rps: Option<u32>,
}

impl UpdateProxyRequest {
    /// 应用到当前配置（None = 保持原值）。
    pub fn apply(&self, current: &ProxyConfig) -> ProxyConfig {
        ProxyConfig {
            socks5_enabled: self.socks5_enabled.unwrap_or(current.socks5_enabled),
            http_enabled: self.http_enabled.unwrap_or(current.http_enabled),
            auth_enabled: self.auth_enabled.unwrap_or(current.auth_enabled),
            proxy_username: self
                .username
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .or_else(|| current.proxy_username.clone()),
            allowed_ips: self
                .allowed_ips
                .clone()
                .unwrap_or_else(|| current.allowed_ips.clone()),
            max_connections: self.max_connections.or(current.max_connections),
            max_rps: self.max_rps.or(current.max_rps),
        }
    }

    /// 校验：max_connections/max_rps 必须 ≥1（0/None = 不限制的表达由存储层处理）。
    pub fn validate(&self) -> Result<(), String> {
        for (field, v) in [
            ("max_connections", self.max_connections),
            ("max_rps", self.max_rps),
        ] {
            if let Some(v) = v {
                if v == 0 {
                    return Err(format!("{field} must be >= 1 (omit to leave unchanged)"));
                }
            }
        }
        if let Some(u) = &self.username {
            if u.trim().is_empty() {
                return Err("username must not be empty (omit to keep existing)".to_string());
            }
        }
        Ok(())
    }
}

/// 账号状态视图（P8-009；无任何明文 secret）。
#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    /// `free` | `warp_plus` | `zero_trust`。
    pub mode: String,
    /// 是否存在任意凭据（license / zero trust）。
    pub configured: bool,
    pub license_present: bool,
    pub zero_trust_configured: bool,
    /// Zero Trust org 名（非 secret；None = 未设置）。
    pub zero_trust_org: Option<String>,
}

/// 系统状态（P7-003；P1 审查 #4 起含组件 operational 状态）。
#[derive(Debug, Clone, Serialize)]
pub struct SystemStatusView {
    pub status: &'static str,
    pub version: String,
    pub uptime_secs: u64,
    pub instances: InstanceCountsView,
    /// 组件级实际状态（liveness 之外的 readiness 信息）。
    pub components: SystemComponentsView,
    /// 最近一次代理配置应用失败（None = 当前配置已成功应用/停止）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_apply_error: Option<LastApplyErrorView>,
}

/// 组件 operational 视图（P1 审查 #4）。
#[derive(Debug, Clone, Serialize)]
pub struct SystemComponentsView {
    /// GOST 数据面：`running` / `stopped` / `degraded` / `failed`。
    pub gost: String,
    /// GOST 非 Running 时的人类可读原因（Failed/Degraded 携带）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gost_reason: Option<String>,
    /// secret store 可用性（读探针；`ok` / `unavailable`）。
    pub secret_store: &'static str,
}

/// 最近一次代理应用失败（P1 审查 #3：绝不伪装成功）。
#[derive(Debug, Clone, Serialize)]
pub struct LastApplyErrorView {
    pub error: String,
    pub at_rfc3339: String,
}

impl LastApplyErrorView {
    pub fn from_slot(slot: &crate::reconciler::ApplyErrorSlot) -> Option<Self> {
        slot.lock().unwrap().as_ref().map(|e| Self {
            error: e.error.clone(),
            at_rfc3339: e.at_rfc3339.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct InstanceCountsView {
    pub total: usize,
    /// 运行中（starting/registering/connecting/healthy/degraded/stopping）。
    pub running: usize,
    pub healthy: usize,
    pub degraded: usize,
    pub failed: usize,
    pub stopped: usize,
}

impl InstanceCountsView {
    /// 由实际快照列表统计。
    ///
    /// 语义注记：`total` = registry 当前跟踪的实例数（已启动过/正在运行）；
    /// DB 中“期望存在但从未启动”的实例不计入（P7 明确：状态视图反映实际，
    /// 期望列表见 `GET /api/v1/instances`）。
    pub fn from_runtime(
        snapshots: &[(crate::runtime::instance::InstanceId, InstanceRuntime)],
    ) -> Self {
        let mut counts = Self::default();
        for (_, r) in snapshots {
            counts.total += 1;
            match r.state {
                RuntimeState::Healthy => {
                    counts.healthy += 1;
                    counts.running += 1;
                }
                RuntimeState::Degraded => {
                    counts.degraded += 1;
                    counts.running += 1;
                }
                RuntimeState::Failed => counts.failed += 1,
                RuntimeState::Stopped | RuntimeState::Disabled => counts.stopped += 1,
                RuntimeState::Starting
                | RuntimeState::Registering
                | RuntimeState::Connecting
                | RuntimeState::Stopping => counts.running += 1,
            }
        }
        counts
    }
}
