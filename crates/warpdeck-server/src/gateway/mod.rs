//! 内置代理网关（P13 / DESIGN §35）：进程内替代 GOST 的统一入站。
//!
//! - supervised listener task：bind 失败指数退避；apply 触发热重建；
//!   listener 意外退出（含 handler panic 上抛）也走退避重建——不再依赖
//!   reconciler 周期性重复 apply 兜底
//! - SOCKS5 CONNECT 子集（RFC1928；认证子协商 user/pass 可选）
//! - HTTP CONNECT 隧道 + absolute-URI 转发 + Basic Auth（Phase B）
//! - RoundRobinPool：只消费 RuntimeRegistry 中 Healthy 实例
//! - allowlist 会话前置校验（crate::net 严格 CIDR）
//! - 连接/RPS 限流（Phase C，allowlist → 认证之后执行）
//! - `BuiltinGateway` 实现 `ProxyApplier`；外层 `supervise` 捕获任务
//!   panic 并指数退避重启（P13-004：网关崩溃 = 重启 + 状态上浮）

pub mod http;
pub mod limits;
pub mod pool;
pub mod socks5;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::net::parse_cidr;
use crate::reconciler::{ProxySettings, ProxyStatus};
use crate::runtime::backoff::BackoffPolicy;
use crate::runtime::registry::RuntimeRegistry;

use self::limits::SessionLimits;
use self::pool::RoundRobinPool;

/// 网关运行时快照（apply 写入，listener 任务读取）。
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub socks5_enabled: bool,
    pub http_enabled: bool,
    /// None = 匿名入站；(username, password) 明文仅存内存。
    pub auth: Option<(String, String)>,
    /// 允许的客户端 CIDR 文本（空 = 允许全部）。
    pub allowlist: Vec<String>,
    /// 连接/RPS 限流（None = 未配置任何上限）。
    pub limits: Option<Arc<SessionLimits>>,
}

impl GatewayConfig {
    pub fn from_settings(s: &ProxySettings) -> Self {
        Self {
            socks5_enabled: s.socks5_enabled,
            http_enabled: s.http_enabled,
            auth: s
                .auth
                .as_ref()
                .map(|a| (a.username.clone(), a.password.clone())),
            allowlist: s.allowlist.clone(),
            limits: Some(SessionLimits::new(s.max_connections, s.max_rps)),
        }
    }
}

/// 共享网关状态：apply/stop 写入，listener/status 读取。锁粒度极小、无 IO。
#[derive(Default)]
pub(crate) struct SharedState {
    config: Mutex<Option<GatewayConfig>>,
    stopped: AtomicBool,
    active: AtomicBool,
    last_error: Mutex<Option<String>>,
    conn_total: AtomicU64,
    /// 测试钩子（WARPDECK_GATEWAY_TEST_HOOKS=1 + SIGUSR1）：置位后 socks5
    /// serve 任务在下一轮 accept 前主动 panic（P13-004 注入用；默认恒 false）。
    pub(crate) panic_requested: AtomicBool,
    /// 唤醒监督循环（apply 重建 / stop 退出 共用单通道，waiter 只有 supervisor）。
    wake: Notify,
}

impl SharedState {
    fn set_config(&self, cfg: Option<GatewayConfig>) {
        *self.config.lock().unwrap() = cfg;
        self.wake.notify_waiters();
    }

    fn snapshot(&self) -> Option<GatewayConfig> {
        self.config.lock().unwrap().clone()
    }

    fn set_error(&self, err: Option<String>) {
        *self.last_error.lock().unwrap() = err;
    }

    pub(crate) fn take_panic_request(&self) -> bool {
        self.panic_requested.swap(false, Ordering::SeqCst)
    }
}

/// 内置网关。clone 廉价（Arc 共享）。
#[derive(Clone)]
pub struct BuiltinGateway {
    pub(crate) shared: Arc<SharedState>,
    registry: Arc<RuntimeRegistry>,
    pool: RoundRobinPool,
    pub(crate) socks5_addr: SocketAddr,
    pub(crate) http_addr: SocketAddr,
}

/// supervise 视为「稳定运行」的时长：超过后连续崩溃计数清零。
const SUPERVISE_STABLE_RESET: std::time::Duration = std::time::Duration::from_secs(60);
/// 连续快速崩溃达到该次数后把原因写入 last_error（status 上浮 Failed）。
const SUPERVISE_CRASH_ALERT_THRESHOLD: u32 = 3;

impl BuiltinGateway {
    /// 生产构造：内部端口基址 = FIRST_WARP_PORT（40000）。
    pub fn new(
        registry: Arc<RuntimeRegistry>,
        socks5_addr: SocketAddr,
        http_addr: SocketAddr,
    ) -> Arc<Self> {
        let pool = RoundRobinPool::new(registry.clone());
        Self::with_pool(registry, pool, socks5_addr, http_addr)
    }

    /// 测试/定制：注入外部池（上游基址可控，指向 fake 上游）。
    pub fn with_pool(
        registry: Arc<RuntimeRegistry>,
        pool: RoundRobinPool,
        socks5_addr: SocketAddr,
        http_addr: SocketAddr,
    ) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(SharedState::default()),
            registry,
            pool,
            socks5_addr,
            http_addr,
        })
    }

    /// 生产监督入口：捕获 `run` 任务边界 panic，指数退避重启（DESIGN §35.2）。
    ///
    /// 正常退出路径只有 stop/shutdown；panic 后按 BackoffPolicy 重启，
    /// 连续快速崩溃超阈值时把原因写入 last_error（status → Failed 上浮）。
    pub async fn supervise(
        self: std::sync::Arc<Self>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        #[cfg(unix)]
        self.spawn_fault_injection_hook();

        let backoff = crate::runtime::backoff::ExponentialBackoff::recommended();
        let mut crashes: u32 = 0;
        loop {
            let started = std::time::Instant::now();
            let runner = self.clone();
            let rx = shutdown.clone();
            let handle = tokio::spawn(async move { runner.run(rx).await });
            match handle.await {
                Ok(()) => break,
                Err(join_err) if join_err.is_panic() => {
                    if started.elapsed() >= SUPERVISE_STABLE_RESET {
                        crashes = 0;
                    }
                    crashes = crashes.saturating_add(1);
                    let delay = backoff.delay_for(crashes);
                    tracing::error!(
                        component = "gateway",
                        crashes,
                        delay = ?delay,
                        panic = %join_err,
                        "gateway task panicked; restarting under supervision"
                    );
                    if crashes >= SUPERVISE_CRASH_ALERT_THRESHOLD {
                        self.shared
                            .set_error(Some(format!("gateway task keeps crashing: {join_err}")));
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = shutdown.changed() => break,
                    }
                }
                Err(_) => break,
            }
        }
    }

    /// 测试钩子（仅 unix 容器）：`WARPDECK_GATEWAY_TEST_HOOKS=1` 时监听
    /// SIGUSR1，收到后请求 socks5 serve 任务注入 panic。生产镜像不设该 env，
    /// 钩子保持惰性（无信号监听、无行为差异）。
    #[cfg(unix)]
    fn spawn_fault_injection_hook(self: &std::sync::Arc<Self>) {
        let armed = std::env::var("WARPDECK_GATEWAY_TEST_HOOKS")
            .map(|v| v == "1")
            .unwrap_or(false);
        if !armed {
            return;
        }
        tracing::warn!(
            component = "gateway",
            "test hooks armed (SIGUSR1 injects serve-task panic)"
        );
        let hook = self.clone();
        tokio::spawn(async move {
            let mut sig =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
                    .expect("failed to install SIGUSR1 handler");
            while sig.recv().await.is_some() {
                tracing::warn!(
                    component = "gateway",
                    "SIGUSR1 received; requesting fault injection"
                );
                hook.request_fault_injection();
            }
        });
    }

    /// 测试钩子（P13-004 / E2E-06）：请求 socks5 serve 任务在下一轮
    /// accept 前注入 panic，验证监督循环的崩溃→重启→恢复链路。
    #[doc(hidden)]
    pub fn request_fault_injection(&self) {
        self.shared.panic_requested.store(true, Ordering::SeqCst);
    }

    /// 监督循环（测试入口；生产请用 `supervise`）。
    pub async fn run(self: std::sync::Arc<Self>, shutdown: tokio::sync::watch::Receiver<bool>) {
        self.run_with_ready(shutdown, None).await;
    }

    /// 监督循环 + 可选的「就绪地址」上报：绑定成功后把实际 listener 地址
    /// 发给调用方（集成测试用 :0 端口时依赖此通道拿真实端口）。
    pub async fn run_with_ready(
        self: std::sync::Arc<Self>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        mut ready: Option<tokio::sync::oneshot::Sender<SocketAddr>>,
    ) {
        let backoff = crate::runtime::backoff::ExponentialBackoff::recommended();
        let mut attempt: u32 = 0;
        loop {
            if self.shared.stopped.load(Ordering::SeqCst) {
                break;
            }
            let Some(cfg) = self.shared.snapshot() else {
                // 尚未 apply 过：等待第一次配置或停止。
                tokio::select! {
                    _ = self.shared.wake.notified() => continue,
                    _ = shutdown.changed() => break,
                }
            };

            // bind 在监督循环内完成（便于向 ready 上报实际地址）；serve 任务
            // 仅负责 accept/handler，重建时被 abort 后由下一轮重新 bind。
            let socks5_listener = match tokio::net::TcpListener::bind(self.socks5_addr).await {
                Ok(listener) => listener,
                Err(e) => {
                    let msg = format!("bind {}: {e}", self.socks5_addr);
                    tracing::error!(component = "gateway", error = %e, "socks5 bind failed");
                    self.shared.set_error(Some(msg));
                    // bind 失败退避后用同配置重试。
                    let d = backoff.delay_for(attempt.saturating_add(1));
                    attempt = attempt.saturating_add(1).min(8);
                    tokio::select! {
                        _ = tokio::time::sleep(d) => {}
                        _ = self.shared.wake.notified() => {}
                        _ = shutdown.changed() => break,
                    }
                    continue;
                }
            };

            if let Some(tx) = ready.take() {
                let addr = socks5_listener.local_addr().unwrap_or(self.socks5_addr);
                let _ = tx.send(addr);
            }
            self.shared.active.store(true, Ordering::SeqCst);
            self.shared.set_error(None);

            // HTTP listener（cfg.http_enabled 控制；bind 失败只降级该协议，
            // 不拖垮 SOCKS5——与旧 GOST 双 listener 语义一致）。
            let mut http_task: Option<tokio::task::JoinHandle<()>> = if cfg.http_enabled {
                match tokio::net::TcpListener::bind(self.http_addr).await {
                    Ok(http_listener) => {
                        let http_shared = self.shared.clone();
                        let http_pool = self.pool.clone();
                        let http_cfg = cfg.clone();
                        Some(tokio::spawn(async move {
                            http::serve(http_listener, http_shared, http_pool, http_cfg).await;
                        }))
                    }
                    Err(e) => {
                        tracing::error!(component = "gateway", error = %e, "http bind failed");
                        None
                    }
                }
            } else {
                None
            };

            let mut socks5_task = {
                let shared = self.shared.clone();
                let pool = self.pool.clone();
                let cfg_task = cfg.clone();
                tokio::spawn(async move {
                    socks5::serve(socks5_listener, shared, pool, cfg_task).await;
                })
            };

            // 任一 listener 意外退出（含 panic）都触发整代重建。
            #[derive(Debug)]
            enum GenerationExit {
                Socks(std::result::Result<(), tokio::task::JoinError>),
                Http(std::result::Result<(), tokio::task::JoinError>),
                Apply,
                Shutdown,
            }
            let exit = tokio::select! {
                r = &mut socks5_task => GenerationExit::Socks(r),
                r = async {
                    match http_task.as_mut() {
                        Some(t) => t.await,
                        None => std::future::pending().await,
                    }
                } => GenerationExit::Http(r),
                _ = self.shared.wake.notified() => GenerationExit::Apply,
                _ = shutdown.changed() => GenerationExit::Shutdown,
            };

            // 清理本代 listener 任务。注意：经 select 分支完成的那个
            // JoinHandle 输出已被消费，再 await 会 panic（polled after
            // completion）——只 abort/await 未自然结束的一方。
            if !matches!(exit, GenerationExit::Socks(_)) {
                socks5_task.abort();
                let _ = socks5_task.await;
            }
            if let Some(t) = http_task.as_mut() {
                if !matches!(exit, GenerationExit::Http(_)) {
                    t.abort();
                    let _ = t.await;
                }
            }

            match exit {
                GenerationExit::Shutdown => break,
                GenerationExit::Apply => {
                    // 配置热更新：立即以新快照重建，不退避。
                    attempt = 0;
                }
                GenerationExit::Socks(r) | GenerationExit::Http(r) => {
                    let detail = match &r {
                        Err(je) if je.is_panic() => format!("panicked: {je}"),
                        Err(je) => format!("cancelled: {je}"),
                        Ok(()) => "returned".to_string(),
                    };
                    tracing::warn!(
                        component = "gateway",
                        reason = %detail,
                        "listener task exited unexpectedly; rebuilding"
                    );
                    let d = backoff.delay_for(attempt.saturating_add(1));
                    attempt = attempt.saturating_add(1).min(8);
                    tokio::select! {
                        _ = tokio::time::sleep(d) => {}
                        _ = self.shared.wake.notified() => {}
                        _ = shutdown.changed() => break,
                    }
                }
            }
        }
        self.shared.active.store(false, Ordering::SeqCst);
        tracing::info!(component = "gateway", "supervised loop exited");
    }
}

#[async_trait::async_trait]
impl crate::reconciler::ProxyApplier for BuiltinGateway {
    async fn apply_config(&self, settings: &ProxySettings) -> Result<(), String> {
        self.shared
            .set_config(Some(GatewayConfig::from_settings(settings)));
        Ok(())
    }

    async fn status(&self) -> Option<ProxyStatus> {
        let healthy_upstreams = self
            .registry
            .list()
            .iter()
            .filter(|(_, r)| r.state == crate::runtime::registry::RuntimeState::Healthy)
            .count();
        if self.shared.stopped.load(Ordering::SeqCst) {
            return Some(ProxyStatus::Stopped);
        }
        if !self.shared.active.load(Ordering::SeqCst) {
            return Some(match self.shared.last_error.lock().unwrap().clone() {
                Some(reason) => ProxyStatus::Failed {
                    reason,
                    exit_code: None,
                },
                None => ProxyStatus::Stopped,
            });
        }
        Some(ProxyStatus::Running {
            pid: std::process::id(),
            healthy_upstreams,
            applied_at: String::new(),
        })
    }

    async fn stop(&self) -> Result<(), String> {
        self.shared.stopped.store(true, Ordering::SeqCst);
        self.shared.wake.notify_waiters();
        self.shared.active.store(false, Ordering::SeqCst);
        Ok(())
    }
}

/// 恒定时间字符串比较（认证口令比对）。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub(crate) fn verify_credentials(
    auth: &Option<(String, String)>,
    user: &[u8],
    pass: &[u8],
) -> bool {
    match auth {
        None => true,
        Some((u, p)) => ct_eq(u.as_bytes(), user) && ct_eq(p.as_bytes(), pass),
    }
}

/// allowlist 匹配：空表 = 全部允许；条目解析失败视为不可匹配（跳过）。
pub(crate) fn client_allowed(peer: std::net::IpAddr, allowlist: &[String]) -> bool {
    if allowlist.is_empty() {
        return true;
    }
    allowlist.iter().any(|cidr| match parse_cidr(cidr) {
        Ok(net) => net.contains(peer),
        Err(_) => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_basic() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn client_allowlist_matches_v4_v6_and_skips_bad_entries() {
        let ok = vec![
            "192.168.1.0/24".to_string(),
            "2001:db8::/32".to_string(),
            "not-a-cidr".to_string(), // 解析失败跳过
        ];
        assert!(client_allowed(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 7)),
            &ok
        ));
        assert!(!client_allowed(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
            &ok
        ));
        assert!(client_allowed("2001:db8:1234::1".parse().unwrap(), &ok));
        // host bits 非零的条目解析失败 → 跳过（不 panic）。
        let bad = vec!["2001:db8::1/64".to_string()];
        assert!(!client_allowed("2001:db8::1".parse().unwrap(), &bad));
    }

    #[test]
    fn empty_allowlist_allows_everything() {
        assert!(client_allowed("10.1.2.3".parse().unwrap(), &[]));
    }

    #[test]
    fn gateway_config_carries_limits() {
        let settings = ProxySettings {
            socks5_enabled: true,
            http_enabled: true,
            auth: None,
            allowlist: vec![],
            max_connections: Some(7),
            max_rps: Some(3),
        };
        let cfg = GatewayConfig::from_settings(&settings);
        let limits = cfg.limits.expect("limits must be built");
        // 行为由 limits 单元测试覆盖；此处验证装配链路。
        assert!(Arc::strong_count(&limits) >= 1);

        let unlimited = GatewayConfig::from_settings(&ProxySettings {
            max_connections: None,
            max_rps: None,
            ..settings
        });
        // SessionLimits::new(None, None) 仍返回实例（内部两个 None 分支直通）。
        assert!(unlimited.limits.is_some());
    }
}
