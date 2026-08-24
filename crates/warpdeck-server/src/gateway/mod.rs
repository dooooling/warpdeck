//! 内置代理网关（P13 / DESIGN §35）：进程内替代 GOST 的 SOCKS5 入站（Phase A）。
//!
//! - supervised listener task：bind 失败指数退避；apply 触发热重建
//! - SOCKS5 CONNECT 子集（RFC1928；认证子协商 user/pass 可选）
//! - RoundRobinPool：只消费 RuntimeRegistry 中 Healthy 实例
//! - allowlist 会话前置校验（复用 proxy::config::parse_cidr）
//! - `BuiltinGateway` 实现 `ProxyApplier`
//!
//! HTTP :18080 入站为 Phase B（DESIGN §35.6）。

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
    pub(crate) socks5_addr: SocketAddr,
    pub(crate) http_addr: SocketAddr,
}

impl BuiltinGateway {
    pub fn new(
        registry: Arc<RuntimeRegistry>,
        socks5_addr: SocketAddr,
        http_addr: SocketAddr,
    ) -> Self {
        Self {
            shared: Arc::new(SharedState::default()),
            registry,
            socks5_addr,
            http_addr,
        }
    }

    /// 监督循环：按共享配置维护 listener；bind 失败指数退避；
    /// apply 变化触发热重建；stop/shutdown 退出。由 main spawn。
    pub async fn run(self: std::sync::Arc<Self>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
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

            // Phase A 仅 SOCKS5；HTTP 入站为 Phase B。
            if !cfg.http_enabled {
                tracing::debug!(
                    component = "gateway",
                    "http inbound not implemented in Phase A"
                );
            } else {
                tracing::debug!(component = "gateway", http = %self.http_addr, "http inbound enabled (Phase B)");
            }

            let pool = RoundRobinPool::new(self.registry.clone());
            let shared = self.shared.clone();
            let cfg_task = cfg.clone();
            let socks5_addr = self.socks5_addr;

            let socks5_task = tokio::spawn(async move {
                match tokio::net::TcpListener::bind(socks5_addr).await {
                    Ok(listener) => {
                        socks5::serve(listener, shared, pool, cfg_task).await;
                    }
                    Err(e) => {
                        tracing::error!(
                            component = "gateway",
                            addr = %socks5_addr,
                            error = %e,
                            "socks5 bind failed"
                        );
                        shared.set_error(Some(format!("bind {socks5_addr}: {e}")));
                    }
                }
            });

            self.shared.active.store(true, Ordering::SeqCst);
            self.shared.set_error(None);

            tokio::select! {
                _ = socks5_task => {
                    tracing::warn!(component = "gateway", "socks5 listener exited unexpectedly");
                }
                _ = self.shared.wake.notified() => {}
                _ = shutdown.changed() => {}
            }

            if self.shared.stopped.load(Ordering::SeqCst) {
                break;
            }
            // 重建间小退避：避免外部快速抖动导致的 accept 风暴。
            let d = backoff.delay_for(attempt.saturating_add(1));
            attempt = attempt.saturating_add(1).min(8);
            tokio::select! {
                _ = tokio::time::sleep(d) => {}
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
