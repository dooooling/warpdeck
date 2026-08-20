//! 重试退避策略（计划 §7.2 要求的 `BackoffPolicy` trait）。
//!
//! 设计依据 DESIGN §11.6：base / factor / max 可配置，重试与退避必须 bounded。
//! jitter（§11.6 建议 0~30%）留待 P12 加固阶段引入——当前是启动探测路径，
//! 单一实例场景惊群收益低，且 jitter 会破坏确定性测试。

use std::time::Duration;

/// 重试等待策略：第 `attempt` 次重试前等待的时长（attempt 从 1 开始）。
pub trait BackoffPolicy: Send + Sync {
    fn delay_for(&self, attempt: u32) -> Duration;
}

/// 指数退避，封顶 `max`。
pub struct ExponentialBackoff {
    base: Duration,
    factor: u32,
    max: Duration,
}

impl ExponentialBackoff {
    pub fn new(base: Duration, factor: u32, max: Duration) -> Self {
        debug_assert!(factor >= 2, "factor must be >= 2");
        Self { base, factor, max }
    }

    /// DESIGN §11.6 建议值：base 2s / factor 2 / max 120s。
    pub fn recommended() -> Self {
        Self::new(Duration::from_secs(2), 2, Duration::from_secs(120))
    }
}

impl BackoffPolicy for ExponentialBackoff {
    fn delay_for(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1);
        // saturating 防止法溢出：超过范围直接封顶。
        let growth = self
            .base
            .checked_mul(self.factor.saturating_pow(exponent))
            .unwrap_or(self.max);
        growth.min(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ExponentialBackoff {
        ExponentialBackoff::new(Duration::from_secs(2), 2, Duration::from_secs(120))
    }

    #[test]
    fn grows_exponentially_from_base() {
        let p = policy();
        assert_eq!(p.delay_for(1), Duration::from_secs(2));
        assert_eq!(p.delay_for(2), Duration::from_secs(4));
        assert_eq!(p.delay_for(3), Duration::from_secs(8));
        assert_eq!(p.delay_for(4), Duration::from_secs(16));
    }

    #[test]
    fn caps_at_max() {
        let p = policy();
        assert_eq!(
            p.delay_for(7),
            Duration::from_secs(128).min(Duration::from_secs(120))
        );
        assert_eq!(p.delay_for(100), Duration::from_secs(120));
    }

    #[test]
    fn never_exceeds_max() {
        let p = policy();
        for attempt in 1..=200 {
            assert!(p.delay_for(attempt) <= Duration::from_secs(120));
        }
    }

    #[test]
    fn recommended_matches_design_116() {
        let p = ExponentialBackoff::recommended();
        assert_eq!(p.delay_for(1), Duration::from_secs(2));
        assert_eq!(p.delay_for(2), Duration::from_secs(4));
    }
}
