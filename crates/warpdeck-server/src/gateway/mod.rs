//! 内置代理网关（P13 / DESIGN §35）：进程内替代 GOST 的 SOCKS5 入站（Phase A）。
//!
//! - supervised listener task：bind 失败指数退避；apply 触发热重建
//! - SOCKS5 CONNECT 子集（RFC1928；认证子协商 user/pass 可选）
//! - RoundRobinPool：只消费 RuntimeRegistry 中 Healthy 实例
//! - allowlist 会话前置校验（复用 proxy::config::parse_cidr）
//! - `BuiltinGateway` 实现 `ProxyApplier`
//!
//! HTTP :18080 入站为 Phase B（DESIGN §35.6）。

pub mod http;
pub mod pool;
pub mod socks5;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::proxy::{GostSettings, ProxyStatus};
use crate::runtime::backoff::BackoffPolicy;
use crate::runtime::registry::RuntimeRegistry;

use self::pool::RoundRobinPool;

/// 网关运行时快照（apply 写入，listener 任务读取）。
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub socks5_enabled: bool,
    pub http_enabled: bool,
    /// None = 匿名入站；(username, password) 明文仅存内存（与旧 gost.yaml 一致）。
    pub auth: Option<(String, String)>,
    /// 允许的客户端 CIDR 文本（空 = 允许全部）。
    pub allowlist: Vec<String>,
}

impl GatewayConfig {
    pub fn from_settings(s: &GostSettings) -> Self {
        Self {
            socks5_enabled: s.socks5_enabled,
            http_enabled: s.http_enabled,
            auth: s
                .auth
                .as_ref()
                .map(|a| (a.username.clone(), a.password.clone())),
            allowlist: s.allowlist.clone(),
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

    /// 监督循环（生产入口）。
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

            // Phase A 仅 SOCKS5；Phase B 加入 HTTP 入站。
            if cfg.http_enabled {
                tracing::debug!(
                    component = "gateway",
                    http = %self.http_addr,
                    "http inbound enabled (builtin)"
                );
            } else {
                tracing::debug!(
                    component = "gateway",
                    http = %self.http_addr,
                    "http inbound disabled"
                );
            }

            let pool = self.pool.clone();
            let shared = self.shared.clone();
            let cfg_task = cfg.clone();

            // bind 在监督循环内完成（便于向 ready 上报实际地址）；serve 任务
            // 仅负责 accept/handler，重建时被 abort 后由下一轮重新 bind。
            match tokio::net::TcpListener::bind(self.socks5_addr).await {
                Ok(listener) => {
                    if let Some(tx) = ready.take() {
                        let addr = listener.local_addr().unwrap_or(self.socks5_addr);
                        let _ = tx.send(addr);
                    }
                    self.shared.active.store(true, Ordering::SeqCst);
                    self.shared.set_error(None);

                    // Phase B：HTTP listener（cfg.http_enabled 控制）。
                    if cfg.http_enabled {
                        match tokio::net::TcpListener::bind(self.http_addr).await {
                            Ok(http_listener) => {
                                let http_shared = shared.clone();
                                let http_pool = pool.clone();
                                let http_cfg = cfg.clone();
                                tokio::spawn(async move {
                                    http::serve(http_listener, http_shared, http_pool, http_cfg)
                                        .await;
                                });
                            }
                            Err(e) => {
                                tracing::error!(component = "gateway", error = %e, "http bind failed");
                            }
                        }
                    }

                    let mut serve_task = tokio::spawn(async move {
                        socks5::serve(listener, shared, pool, cfg_task).await;
                    });

                    tokio::select! {
                        _ = &mut serve_task => {
                            tracing::warn!(component = "gateway", "socks5 listener exited unexpectedly");
                        }
                        _ = self.shared.wake.notified() => {}
                        _ = shutdown.changed() => {}
                    }
                    // 重建/退出前中止旧 accept 任务（listener 随之 drop）。
                    serve_task.abort();
                    let _ = serve_task.await;

                    // 停止 HTTP listener（如果已启动）。通过 drop TcpListener 实现。
                    // serve task 被 abort 后，其内部持有的 http listener 也会释放。
                }
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
            }

            if self.shared.stopped.load(Ordering::SeqCst) {
                break;
            }
            // 配置变化 → 立即重建；正常路径不退避。
            attempt = 0;
            tokio::select! {
                _ = self.shared.wake.notified() => {}
                _ = shutdown.changed() => break,
            }
        }
        self.shared.active.store(false, Ordering::SeqCst);
        tracing::info!(component = "gateway", "supervised loop exited");
    }
}

#[async_trait::async_trait]
impl crate::reconciler::ProxyApplier for BuiltinGateway {
    async fn apply_config(&self, settings: &GostSettings) -> Result<(), String> {
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
    allowlist.iter().any(|cidr| cidr_match(cidr, peer))
}

fn cidr_match(cidr: &str, ip: std::net::IpAddr) -> bool {
    use crate::proxy::config::IpNetwork as N;
    let Ok(net) = crate::proxy::config::parse_cidr(cidr) else {
        return false;
    };
    match net {
        N::Exact(exact) => exact == ip,
        N::V4 { net, prefix } => match ip {
            std::net::IpAddr::V4(v4) => {
                let bits = u32::from(prefix).min(32);
                let mask = if bits == 0 {
                    0
                } else {
                    u32::MAX << (32 - bits)
                };
                (u32::from(v4) & mask) == (u32::from(net) & mask)
            }
            _ => false,
        },
        N::V6 { net, prefix } => match ip {
            std::net::IpAddr::V6(v6) => {
                let bits = u32::from(prefix).min(128);
                let mask = if bits == 0 {
                    0
                } else {
                    u128::MAX << (128 - bits)
                };
                (u128::from(v6) & mask) == (u128::from(net) & mask)
            }
            _ => false,
        },
    }
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
}
