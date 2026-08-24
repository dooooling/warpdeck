//! 会话限流（P13-C / DESIGN §35.4）：全局连接上限 + 可选令牌桶 RPS。
//!
//! 语义与旧 GOST climiters/rlimiters 对齐：
//! - 连接上限：并发会话数超过 `max_connections` 时拒绝新连接（非阻塞）；
//! - RPS：每秒新建连接数由令牌桶约束（容量 = `max_rps`，匀速补充）。
//!
//! 执行点在 allowlist → 认证**之后**（DESIGN §35.2 顺序）：未认证客户端
//! 无法消耗连接配额。许可随会话生命周期持有，结束自动归还。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::Semaphore;

/// 单条会话的限流许可；drop 即释放（连接上限计数 -1）。
#[derive(Debug)]
pub(crate) struct SessionPermit {
    _conn: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// 限流判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LimitRejection {
    /// 并发连接数已达上限。
    ConnFull,
    /// 新建速率超限（令牌不足）。
    RateLimited,
}

/// 令牌桶：容量 = rate（桶满即 max_rps 突发额度），按流逝时间线性补充。
struct TokenBucket {
    rate: f64,
    tokens: Mutex<f64>,
    last: Mutex<Instant>,
}

impl TokenBucket {
    fn new(rate: u32, now: Instant) -> Self {
        Self {
            rate: f64::from(rate),
            tokens: Mutex::new(f64::from(rate)),
            last: Mutex::new(now),
        }
    }

    fn try_take(&self, now: Instant) -> bool {
        let mut tokens = self.tokens.lock().unwrap();
        let mut last = self.last.lock().unwrap();
        let elapsed = now.saturating_duration_since(*last).as_secs_f64();
        // 补充量按流逝时间线性计算，封顶桶容量（容量 = rate，即 1 秒突发额度）。
        *tokens = (*tokens + elapsed * self.rate).min(self.rate);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}
/// 会话级限流器（apply 热重建时随 GatewayConfig 一起换新实例）。
pub struct SessionLimits {
    conn: Option<Arc<Semaphore>>,
    rps: Option<TokenBucket>,
}

impl std::fmt::Debug for SessionLimits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLimits")
            .field(
                "max_connections",
                &self.conn.as_ref().map(|s| s.available_permits()),
            )
            .field("max_rps", &self.rps.as_ref().map(|b| b.rate as u32))
            .finish()
    }
}

impl SessionLimits {
    pub(crate) fn new(max_connections: Option<u32>, max_rps: Option<u32>) -> Arc<Self> {
        Arc::new(Self {
            // permit 数 = 上限；0 视为关闭上游入口（校验层保证 ≥1，此处兜底）。
            conn: max_connections.map(|n| Arc::new(Semaphore::new(n.max(1) as usize))),
            rps: max_rps.map(|r| TokenBucket::new(r.max(1), Instant::now())),
        })
    }

    /// 尝试为一条新会话取得许可。`Ok` 的 guard 必须持有到会话结束。
    pub(crate) fn acquire(&self) -> Result<SessionPermit, LimitRejection> {
        if let Some(bucket) = &self.rps {
            if !bucket.try_take(Instant::now()) {
                return Err(LimitRejection::RateLimited);
            }
        }
        let conn = match &self.conn {
            Some(sem) => match Arc::clone(sem).try_acquire_owned() {
                Ok(p) => Some(p),
                Err(_) => return Err(LimitRejection::ConnFull),
            },
            None => None,
        };
        Ok(SessionPermit { _conn: conn })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn no_limits_always_admits() {
        let limits = SessionLimits::new(None, None);
        for _ in 0..1000 {
            assert!(limits.acquire().is_ok());
        }
    }

    #[tokio::test]
    async fn connection_cap_rejects_and_releases_on_drop() {
        let limits = SessionLimits::new(Some(2), None);
        let p1 = limits.acquire().unwrap();
        let p2 = limits.acquire().unwrap();
        assert_eq!(
            limits.acquire().unwrap_err(),
            LimitRejection::ConnFull,
            "third concurrent session must be rejected"
        );
        drop(p1);
        assert!(limits.acquire().is_ok(), "released permit must be reusable");
        drop(p2);
    }

    #[test]
    fn rps_bucket_allows_burst_then_throttles() {
        let now = Instant::now();
        let bucket = TokenBucket::new(3, now);
        // 满桶 = 3 个突发额度。
        assert!(bucket.try_take(now));
        assert!(bucket.try_take(now));
        assert!(bucket.try_take(now));
        assert!(!bucket.try_take(now), "empty bucket must throttle");

        // 时间推进 1s → 补充 3 个令牌。
        let later = now + Duration::from_secs(1);
        assert!(bucket.try_take(later));
        assert!(bucket.try_take(later));
        assert!(bucket.try_take(later));
        assert!(!bucket.try_take(later));
    }

    #[tokio::test]
    async fn combined_limits_apply_independently() {
        let limits = SessionLimits::new(Some(10), Some(1));
        assert!(limits.acquire().is_ok());
        assert_eq!(
            limits.acquire().unwrap_err(),
            LimitRejection::RateLimited,
            "second instant session exceeds max_rps=1"
        );
    }
}
