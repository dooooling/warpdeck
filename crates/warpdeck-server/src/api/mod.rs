//! REST API 层（P7 + P8 认证/CSRF）。
//!
//! 设计（AGENTS.md「HTTP handlers only mutate desired state and notify」、
//! DESIGN §17.x/§20）：handler 只读写 SQLite 期望状态 + 触发 reconciler，
//! 不直接监督进程；实际状态读 `RuntimeRegistry`（由 InstanceManager /
//! Crash Watcher / Health Monitor 更新）。
//!
//! 认证分层（P8-005）：public 路由（health/setup/login）不挂中间件；
//! 其余全部经 `middleware::auth_guard`（session cookie → 401；mutation
//! 额外 CSRF → 403）。

pub mod account;
pub mod accounts;
pub mod auth;
pub mod dto;
pub mod error;
pub mod events;
pub mod health;
pub mod instances;
pub mod logs;
pub mod middleware;
#[cfg(test)]
pub mod p8_tests;
pub mod proxy;
pub mod setup;
pub mod system;
#[cfg(test)]
pub mod tests;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::routing::{get, post};
use axum::Router;
use tokio::sync::Notify;

use crate::auth::rate_limit::LoginRateLimiter;
use crate::auth::repos::UserRepository;
use crate::auth::session::SessionRepository;
use crate::crypto::secret_store::SecretStore;
use crate::db::account::AccountRepository;
use crate::db::profiles::AccountProfileRepository;
use crate::db::repo::{ProxyConfigRepository, WarpInstanceRepository};
use crate::runtime::events::EventBus;
use crate::runtime::logs::LogBus;
use crate::runtime::registry::RuntimeRegistry;
use crate::runtime::WarpRuntime;

pub use error::{ApiError, ApiResult};

/// 当前 API 版本前缀（固定 `/api/v1`，破坏性变更才升级）。
pub const API_VERSION: &str = "v1";

/// API 共享状态：注入各 trait 接缝（生产 = sqlite repo + 真实 manager，
/// 测试 = fake/内存实现）。`trigger` 与 reconciler 共享同一个 `Notify`。
#[derive(Clone)]
pub struct ApiState {
    pub instances: Arc<dyn WarpInstanceRepository>,
    pub proxy: Arc<dyn ProxyConfigRepository>,
    pub registry: Arc<RuntimeRegistry>,
    pub runtime: Arc<dyn WarpRuntime>,
    /// P8：用户/会话/secret/账号仓储。
    pub users: Arc<dyn UserRepository>,
    pub sessions: Arc<dyn SessionRepository>,
    pub secrets: Arc<dyn SecretStore>,
    pub account: Arc<dyn AccountRepository>,
    /// v0.2 多账号档案（§16.9；task D)。
    pub profiles: Arc<dyn AccountProfileRepository>,
    pub login_limiter: Arc<dyn LoginRateLimiter>,
    /// P8-004：Secure cookie（HTTPS 部署置 true）。
    pub secure_cookie: bool,
    /// 状态事件总线（SSE `/events` 消费；manager/health 发布）。
    pub bus: EventBus,
    /// 实时日志行总线（P10-007；SSE `/events` 的 `log.line` 帧）。
    pub log_bus: LogBus,
    /// 持久化数据目录（日志源枚举/历史读取，P10-006）。
    pub data_dir: PathBuf,
    pub trigger: Arc<Notify>,
    pub started_at: Instant,
    pub version: String,
}

impl ApiState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        instances: Arc<dyn WarpInstanceRepository>,
        proxy: Arc<dyn ProxyConfigRepository>,
        registry: Arc<RuntimeRegistry>,
        runtime: Arc<dyn WarpRuntime>,
        users: Arc<dyn UserRepository>,
        sessions: Arc<dyn SessionRepository>,
        secrets: Arc<dyn SecretStore>,
        account: Arc<dyn AccountRepository>,
        profiles: Arc<dyn AccountProfileRepository>,
        login_limiter: Arc<dyn LoginRateLimiter>,
        secure_cookie: bool,
        bus: EventBus,
        log_bus: LogBus,
        data_dir: PathBuf,
        trigger: Arc<Notify>,
        version: String,
    ) -> Self {
        Self {
            instances,
            proxy,
            registry,
            runtime,
            users,
            sessions,
            secrets,
            account,
            profiles,
            login_limiter,
            secure_cookie,
            bus,
            log_bus,
            data_dir,
            trigger,
            started_at: Instant::now(),
            version,
        }
    }

    /// 期望状态变更后触发一轮 reconcile（reconciler 消费同一 Notify）。
    pub fn notify_change(&self) {
        self.trigger.notify_one();
    }
}

/// 组装 `/api/v1` 路由树（P7-001..008/012；P8 认证分层）。
///
/// public 区：setup（初始化）/login（登录取回会话）/health 不要求认证；
/// protected 区：其余全部（auth guard + mutation CSRF 校验）。
/// `state` 供 `from_fn_with_state` 挂载 auth guard（Clone 共享）。
pub fn router(state: ApiState) -> Router<ApiState> {
    let public = Router::new()
        .route("/setup/status", get(setup::status))
        .route("/setup", post(setup::create_admin))
        .route("/auth/login", post(auth::login));

    let protected = Router::new()
        .route("/auth/me", get(auth::me))
        .route("/auth/logout", post(auth::logout))
        .route("/system/status", get(system::status))
        .route("/system/version", get(system::version))
        .route("/account", get(account::get).put(account::update))
        .route("/accounts", get(accounts::list).post(accounts::create))
        .route(
            "/accounts/{id}",
            get(accounts::get)
                .delete(accounts::delete)
                .patch(accounts::update),
        )
        .route("/instances", get(instances::list).post(instances::create))
        .route(
            "/instances/{id}",
            get(instances::get)
                .patch(instances::update)
                .delete(instances::delete),
        )
        .route("/instances/{id}/start", post(instances::start))
        .route("/instances/{id}/stop", post(instances::stop))
        .route("/instances/{id}/restart", post(instances::restart))
        .route("/proxy", get(proxy::get).put(proxy::update))
        .route("/logs/sources", get(logs::sources))
        .route("/logs", get(logs::history))
        .route("/events", get(events::subscribe))
        .route_layer(axum::middleware::from_fn_with_state::<_, ApiState, _>(
            state,
            middleware::auth_guard,
        ));

    public.merge(protected)
}
