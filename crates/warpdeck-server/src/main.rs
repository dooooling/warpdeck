//! warpdeck-server 二进制入口（P1-005，P6-008/P7-008 全量接线）。
//!
//! 启动顺序（DEVELOPMENT_PLAN §12.2 P7-008 / P6-008）：
//! config → 日志 → DB（migration）→ 仓储 → 运行时栈（manager/health）
//! → GOST 代理栈 → reconciler（持有 trigger + 关停信号）→ Web。
//! 关停顺序：SIGTERM → watch 置位（reconciler 退出）→ health cancel →
//! pool close。
//!
//! 所有组件经 trait 接缝组装：真实实现（本文件）；测试在 `app::TestApp`
//! 注入 fake（API 测试无需真实 WARP，AGENTS.md §开发纪律）。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, Notify};

use warpdeck_server::api::ApiState;
use warpdeck_server::auth::rate_limit::InMemoryLoginRateLimiter;
use warpdeck_server::auth::repos::SqliteUserRepository;
use warpdeck_server::auth::session::SqliteSessionRepository;
use warpdeck_server::crypto::master_key;
use warpdeck_server::crypto::secret_store::SqliteSecretStore;
use warpdeck_server::db::account::SqliteAccountRepository;
use warpdeck_server::db::credentials::SqliteCredentialResolver;
use warpdeck_server::db::profiles::SqliteAccountProfileRepository;
use warpdeck_server::db::repo::{
    ProxyConfigRepository, SqliteProxyConfigRepository, SqliteWarpInstanceRepository,
    WarpInstanceRepository,
};
use warpdeck_server::proxy::pool::TcpReachabilityProbe;
use warpdeck_server::proxy::GostManager;
use warpdeck_server::reconciler::{
    proxy_config_to_gost, ProxyApplier, Reconciler, DEFAULT_BACKOFF_BASE, DEFAULT_BACKOFF_MAX,
};
use warpdeck_server::runtime::backoff::ExponentialBackoff;
use warpdeck_server::runtime::clock::{Clock, SystemClock};
use warpdeck_server::runtime::control::WarpControl;
use warpdeck_server::runtime::events::EventBus;
use warpdeck_server::runtime::health::HealthConfig;
use warpdeck_server::runtime::health_monitor::HealthMonitor;
use warpdeck_server::runtime::logs::LogBus;
use warpdeck_server::runtime::manager::{InstanceManager, PortProber, TcpPortProber};
use warpdeck_server::runtime::probe::{DataPlaneProber, RealDataPlaneProber};
use warpdeck_server::runtime::process::{ProcessSpawner, TokioProcessSpawner};
use warpdeck_server::runtime::registry::RuntimeRegistry;
use warpdeck_server::runtime::warp_cli::{CommandExecutor, RealCommandExecutor, RealWarpControl};
use warpdeck_server::runtime::WarpRuntime;
use warpdeck_server::{app, config, db, observability, shutdown};

/// WARP 注册最大尝试次数（flow.rs 约束 ≥1）。
const MAX_REGISTER_ATTEMPTS: u32 = 3;
/// 健康检查轮询间隔。
const HEALTH_INTERVAL: Duration = Duration::from_secs(10);
/// GOST listener/节点探活超时。
const REACH_TIMEOUT: Duration = Duration::from_secs(2);
/// GOST 停止宽限/轮询。
const GOST_STOP_GRACE: Duration = Duration::from_secs(10);
const GOST_STOP_POLL: Duration = Duration::from_millis(500);
/// GOST 二进制路径（容器内固定，Compose 安装）。
const GOST_BINARY: &str = "gost";

fn main() {
    let cfg = config::AppConfig::from_env().expect("invalid bootstrap configuration");
    observability::init_tracing(&cfg.log_level, Some(&cfg.data_dir))
        .expect("failed to init tracing");
    tracing::info!(
        version = %app_version(),
        "warpdeck-server starting"
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    rt.block_on(serve(cfg));
}

fn app_version() -> String {
    warpdeck_server::version::app_version()
}

/// 完整启动路径（P6-008 / P7-008）：
/// db → repos → runtime stack → gost → reconciler → web → graceful shutdown。
async fn serve(cfg: config::AppConfig) {
    let pool = db::connect(&cfg.database_url)
        .await
        .expect("failed to initialize database");
    tracing::info!(database_url = %cfg.database_url, "database ready, migrations applied");

    // --- 仓储（desired state 单一事实来源）---
    let instances: Arc<dyn WarpInstanceRepository> =
        Arc::new(SqliteWarpInstanceRepository::new(pool.clone()));
    let proxy_repo: Arc<dyn ProxyConfigRepository> =
        Arc::new(SqliteProxyConfigRepository::new(pool.clone()));

    // --- P8 认证与 secret 栈 ---
    let master_key = master_key::load_or_create(cfg.master_key_env.as_deref(), &cfg.data_dir)
        .expect("failed to load or create master key");
    let users: Arc<dyn warpdeck_server::auth::repos::UserRepository> =
        Arc::new(SqliteUserRepository::new(pool.clone()));
    let sessions: Arc<dyn warpdeck_server::auth::session::SessionRepository> =
        Arc::new(SqliteSessionRepository::new(pool.clone()));
    let secrets: Arc<dyn warpdeck_server::crypto::secret_store::SecretStore> =
        Arc::new(SqliteSecretStore::new(pool.clone(), master_key));
    let account: Arc<dyn warpdeck_server::db::account::AccountRepository> =
        Arc::new(SqliteAccountRepository::new(pool.clone()));
    let profiles: Arc<dyn warpdeck_server::db::profiles::AccountProfileRepository> =
        Arc::new(SqliteAccountProfileRepository::new(pool.clone()));

    // --- 运行时栈（WARP 实例 + 健康监控）---
    let registry = Arc::new(RuntimeRegistry::new());
    let bus = EventBus::default();
    let log_bus = LogBus::default();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let spawner: Arc<dyn ProcessSpawner> = Arc::new(TokioProcessSpawner);
    let executor: Box<dyn CommandExecutor> = Box::new(RealCommandExecutor);
    let control: Arc<dyn WarpControl> = Arc::new(RealWarpControl::new(executor));
    let prober: Arc<dyn PortProber> = Arc::new(TcpPortProber);
    let dplane: Arc<dyn DataPlaneProber> = Arc::new(RealDataPlaneProber::default());

    // 注意：HealthMonitor 需要具体 `Arc<InstanceManager>`，故保留具体类型，
    // reconciler 与 API state 再各自克隆为 `Arc<dyn WarpRuntime>`。
    let credential_resolver = Arc::new(SqliteCredentialResolver::new(
        profiles.clone(),
        secrets.clone(),
    ));
    let manager = Arc::new(InstanceManager::new(
        registry.clone(),
        spawner.clone(),
        control,
        clock.clone(),
        Box::new(ExponentialBackoff::new(
            DEFAULT_BACKOFF_BASE,
            2,
            DEFAULT_BACKOFF_MAX,
        )),
        MAX_REGISTER_ATTEMPTS,
        credential_resolver,
        cfg.data_dir.clone(),
        cfg.runtime_dir.clone(),
        prober,
        dplane.clone(),
        bus.clone(),
    ));
    let runtime: Arc<dyn WarpRuntime> = manager.clone();

    // 健康监控（P4-007）：独立循环；`health_cancel` drop 时停止。
    let (_health_handle, health_cancel) = HealthMonitor::new(
        manager.clone(),
        clock.clone(),
        HealthConfig::default(),
        HEALTH_INTERVAL,
    )
    .spawn();

    // --- GOST 代理栈（读期望配置，P6 reconciler 驱动 apply）---
    let proxy_cfg = proxy_repo.get().await.expect("failed to load proxy config");
    // 保留具体引用：优雅关停时显式 stop（容器退出不留孤儿进程）。
    let gost_manager = Arc::new(GostManager::new(
        registry.clone(),
        Arc::new(TcpReachabilityProbe {
            connect_timeout: REACH_TIMEOUT,
        }),
        Arc::new(TcpReachabilityProbe {
            connect_timeout: REACH_TIMEOUT,
        }),
        dplane,
        spawner,
        clock.clone(),
        GOST_BINARY.to_string(),
        cfg.data_dir.clone(),
        proxy_config_to_gost(&proxy_cfg),
        GOST_STOP_GRACE,
        GOST_STOP_POLL,
    ));
    let gost: Arc<dyn ProxyApplier> = gost_manager.clone();

    // --- reconciler（唯一写者：收敛 desired → actual）---
    let trigger = Arc::new(Notify::new());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let apply_error = warpdeck_server::reconciler::new_apply_error_slot();
    let mut reconciler = Reconciler::new(
        instances.clone(),
        proxy_repo.clone(),
        runtime.clone(),
        registry.clone(),
        gost.clone(),
        secrets.clone(),
        clock,
        cfg.data_dir.clone(),
        cfg.runtime_dir.clone(),
        DEFAULT_BACKOFF_BASE,
        DEFAULT_BACKOFF_MAX,
        trigger.clone(),
        shutdown_rx,
        bus.clone(),
        apply_error.clone(),
    );
    tokio::spawn(async move {
        reconciler.run().await;
    });

    // --- 实时日志 tail watcher（P10-007）：新行经 redactor 推 LogBus ---
    // 句柄 drop 不 abort；任务与进程同生命周期。
    let _tail_handles =
        warpdeck_server::runtime::log_tail::spawn_tail_watchers(&cfg.data_dir, log_bus.clone());

    // --- Web/API（handler 只写 desired + trigger；实际状态读 registry）---
    let state = ApiState::new(
        instances,
        proxy_repo,
        registry,
        runtime,
        users,
        sessions,
        secrets,
        account,
        profiles,
        Arc::new(InMemoryLoginRateLimiter::default()),
        cfg.secure_cookie,
        bus.clone(),
        log_bus.clone(),
        cfg.data_dir.clone(),
        trigger,
        app_version(),
        gost,
        apply_error,
    );
    let router = app::router(state, cfg.ui_dir.clone());
    let listener = tokio::net::TcpListener::bind(cfg.web_bind)
        .await
        .expect("failed to bind web port");
    tracing::info!(bind = %cfg.web_bind, "listening");

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown::shutdown_signal().await;
        tracing::info!("shutdown: signalling reconciler, stopping gost");
        let _ = shutdown_tx.send(true);
        drop(health_cancel);
        if let Err(e) = gost_manager.stop().await {
            tracing::warn!(error = %e, "gost stop failed during shutdown");
        }
    })
    .await
    .expect("server error");

    pool.close().await;
    tracing::info!("database closed");
}
