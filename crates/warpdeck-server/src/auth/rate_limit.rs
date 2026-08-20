//! 登录限流（P8-011）。
//!
//! 设计：per-IP 失败计数滑动窗口；窗口内失败达到阈值后该 IP 被锁定
//! 直到窗口过期。内存实现（单进程部署足够），不依赖额外存储。
//! 成功登录清空该 IP 计数（正常用户不受影响）。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;

/// 默认阈值与窗口（DESIGN §29 登录速率限制）。
pub const DEFAULT_MAX_FAILURES: u32 = 5;
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(15 * 60);

/// 限流判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// 允许尝试。
    Allow,
    /// 已锁定：失败次数超阈值且窗口未过期。
    Blocked,
}

/// 登录限流接缝（测试可注入小窗口/内存实现）。
#[async_trait]
pub trait LoginRateLimiter: Send + Sync {
    /// 记录一次失败并返回当前判定（记录后可能变为 Blocked）。
    async fn record_failure(&self, ip: IpAddr) -> RateDecision;
    /// 记录成功（清空该 IP 计数）。
    async fn record_success(&self, ip: IpAddr);
    /// 当前是否允许尝试。
    async fn check(&self, ip: IpAddr) -> RateDecision;
}

/// 进程内实现：`HashMap<IpAddr, (失败数, 窗口起点)>`。
pub struct InMemoryLoginRateLimiter {
    inner: Mutex<HashMap<IpAddr, FailureState>>,
    max_failures: u32,
    window: Duration,
}

#[derive(Clone)]
struct FailureState {
    count: u32,
    window_start: Instant,
}

impl InMemoryLoginRateLimiter {
    pub fn new(max_failures: u32, window: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_failures,
            window,
        }
    }

    fn snapshot(&self, ip: IpAddr) -> Option<FailureState> {
        let mut map = self.inner.lock().unwrap();
        // 惰性清理过期窗口（避免无限增长）。
        let expired: Vec<IpAddr> = map
            .iter()
            .filter(|(_, s)| s.window_start.elapsed() >= self.window)
            .map(|(k, _)| *k)
            .collect();
        for k in expired {
            map.remove(&k);
        }
        map.get(&ip).cloned()
    }
}

impl Default for InMemoryLoginRateLimiter {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FAILURES, DEFAULT_WINDOW)
    }
}

#[async_trait]
impl LoginRateLimiter for InMemoryLoginRateLimiter {
    async fn record_failure(&self, ip: IpAddr) -> RateDecision {
        let mut map = self.inner.lock().unwrap();
        let state = map.entry(ip).or_insert(FailureState {
            count: 0,
            window_start: Instant::now(),
        });
        // 窗口过期则重置计数。
        if state.window_start.elapsed() >= self.window {
            state.count = 0;
            state.window_start = Instant::now();
        }
        state.count += 1;
        if state.count >= self.max_failures {
            RateDecision::Blocked
        } else {
            RateDecision::Allow
        }
    }

    async fn record_success(&self, ip: IpAddr) {
        self.inner.lock().unwrap().remove(&ip);
    }

    async fn check(&self, ip: IpAddr) -> RateDecision {
        match self.snapshot(ip) {
            Some(state) if state.count >= self.max_failures => RateDecision::Blocked,
            _ => RateDecision::Allow,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip() -> IpAddr {
        "10.0.0.1".parse().unwrap()
    }

    #[tokio::test]
    async fn allows_until_threshold_then_blocks() {
        let limiter = InMemoryLoginRateLimiter::new(3, Duration::from_secs(60));
        assert_eq!(limiter.check(ip()).await, RateDecision::Allow);
        assert_eq!(limiter.record_failure(ip()).await, RateDecision::Allow);
        assert_eq!(limiter.record_failure(ip()).await, RateDecision::Allow);
        assert_eq!(limiter.record_failure(ip()).await, RateDecision::Blocked);
        assert_eq!(limiter.check(ip()).await, RateDecision::Blocked);
    }

    #[tokio::test]
    async fn success_resets_count() {
        let limiter = InMemoryLoginRateLimiter::new(2, Duration::from_secs(60));
        limiter.record_failure(ip()).await;
        limiter.record_success(ip()).await;
        assert_eq!(limiter.record_failure(ip()).await, RateDecision::Allow);
    }

    #[tokio::test]
    async fn per_ip_isolation() {
        let limiter = InMemoryLoginRateLimiter::new(2, Duration::from_secs(60));
        let other: IpAddr = "10.0.0.2".parse().unwrap();
        assert_eq!(limiter.record_failure(ip()).await, RateDecision::Allow);
        assert_eq!(limiter.record_failure(ip()).await, RateDecision::Blocked);
        assert_eq!(
            limiter.record_failure(other).await,
            RateDecision::Allow,
            "second IP must have its own counter"
        );
    }
}
