//! 时间抽象（计划 §7.2 要求的 `Clock` trait）。
//!
//! 生产路径使用 `SystemClock`（真实 tokio sleep）；测试注入 `ManualClock`（fake.rs），
//! 记录每次 sleep 而不真实等待，保证探测/重试逻辑的测试快速且确定。

use std::time::Duration;

use async_trait::async_trait;

/// 时钟抽象：读取当前时刻 + 可注入的睡眠（async_trait 保证 dyn 兼容）。
#[async_trait]
pub trait Clock: Send + Sync {
    fn now(&self) -> std::time::Instant;
    /// 当前 UTC 时刻的 RFC3339 文本（P6 reconciler backoff 持久化用）。
    /// 测试时钟返回同尺度虚拟时间，保证 `next_retry_at` 比较在测试中成立。
    fn now_utc_rfc3339(&self) -> String;
    async fn sleep(&self, duration: Duration);
}

/// 生产实现：真实系统时钟（tokio 调度 sleep）。
pub struct SystemClock;

#[async_trait]
impl Clock for SystemClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn now_utc_rfc3339(&self) -> String {
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn system_clock_roundtrip() {
        let clock = SystemClock;
        let before = clock.now();
        clock.sleep(Duration::from_millis(5)).await;
        let after = clock.now();
        assert!(after >= before);
    }
}
