//! 实例就绪探测（P2-008）。
//!
//! 设计约束（DESIGN §25.7 启动要求 / 计划 P2-008）：
//! - spawn 后**不能仅凭进程存在**判定 Ready；
//! - 就绪判据：`warp-cli status` 成功执行（control plane 已响应）；
//! - bounded retry + backoff：探测次数与等待都有上限，失败不无限重试。

use std::sync::Arc;

use super::backoff::BackoffPolicy;
use super::clock::Clock;
use super::context::InstanceContext;
use super::control::{WarpControl, WarpControlError};

/// 探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    pub ready: bool,
    /// 实际消耗的探测次数（>= 1）。
    pub attempts: u32,
    /// 最后一次探测失败原因（`ready == false` 时通常有值）。
    pub last_error: Option<WarpControlError>,
}

/// 就绪探测器：status 成功一次即为 ready，失败则按 backoff 重试。
pub struct ReadinessProbe {
    control: Arc<dyn WarpControl>,
    clock: Arc<dyn Clock>,
    backoff: Box<dyn BackoffPolicy>,
    max_attempts: u32,
}

impl ReadinessProbe {
    pub fn new(
        control: Arc<dyn WarpControl>,
        clock: Arc<dyn Clock>,
        backoff: Box<dyn BackoffPolicy>,
        max_attempts: u32,
    ) -> Self {
        assert!(max_attempts >= 1, "max_attempts must be >= 1");
        Self {
            control,
            clock,
            backoff,
            max_attempts,
        }
    }

    /// 执行一次 bounded 探测循环。
    pub async fn probe(&self, ctx: &InstanceContext) -> ProbeResult {
        let mut last_error: Option<WarpControlError> = None;

        for attempt in 1..=self.max_attempts {
            match self.control.status(ctx).await {
                Ok(_) => {
                    return ProbeResult {
                        ready: true,
                        attempts: attempt,
                        last_error: None,
                    };
                }
                Err(e) => last_error = Some(e),
            }

            if attempt < self.max_attempts {
                let delay = self.backoff.delay_for(attempt);
                self.clock.sleep(delay).await;
            }
        }

        ProbeResult {
            ready: false,
            attempts: self.max_attempts,
            last_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::runtime::backoff::ExponentialBackoff;
    use crate::runtime::control::WarpCliStatus;
    use crate::runtime::fake::{FakeWarpControl, ManualClock};
    use crate::runtime::instance::{InstanceId, InternalProxyPort};

    fn ctx() -> InstanceContext {
        InstanceContext::new(
            std::path::Path::new("/var/lib/warpdeck"),
            std::path::Path::new("/run/warpdeck"),
            InstanceId::from_db(0).unwrap(),
        )
        .unwrap()
    }

    /// 前 `fail_times` 次以 `err` 失败，之后成功的探针包装。
    struct FlakyControl {
        inner: FakeWarpControl,
        fail_times: u32,
        calls: AtomicU32,
    }

    impl FlakyControl {
        fn new(inner: FakeWarpControl, fail_times: u32) -> Self {
            Self {
                inner,
                fail_times,
                calls: AtomicU32::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl WarpControl for FlakyControl {
        async fn status(&self, ctx: &InstanceContext) -> Result<WarpCliStatus, WarpControlError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call <= self.fail_times {
                Err(WarpControlError::CommandTimeout)
            } else {
                self.inner.status(ctx).await
            }
        }

        async fn register(&self, ctx: &InstanceContext) -> Result<(), WarpControlError> {
            self.inner.register(ctx).await
        }

        async fn apply_account(
            &self,
            ctx: &InstanceContext,
            credentials: &crate::runtime::credentials::InstanceCredentials,
        ) -> Result<(), WarpControlError> {
            self.inner.apply_account(ctx, credentials).await
        }

        async fn set_proxy_mode(&self, ctx: &InstanceContext) -> Result<(), WarpControlError> {
            self.inner.set_proxy_mode(ctx).await
        }

        async fn set_proxy_port(
            &self,
            ctx: &InstanceContext,
            port: InternalProxyPort,
        ) -> Result<(), WarpControlError> {
            self.inner.set_proxy_port(ctx, port).await
        }

        async fn connect(&self, ctx: &InstanceContext) -> Result<(), WarpControlError> {
            self.inner.connect(ctx).await
        }

        async fn disconnect(&self, ctx: &InstanceContext) -> Result<(), WarpControlError> {
            self.inner.disconnect(ctx).await
        }
    }

    #[tokio::test]
    async fn ready_when_status_succeeds_on_first_attempt() {
        let warp = Arc::new(FakeWarpControl::new());
        let clock = Arc::new(ManualClock::new());
        let probe = ReadinessProbe::new(
            warp,
            clock.clone(),
            Box::new(ExponentialBackoff::recommended()),
            5,
        );

        let result = probe.probe(&ctx()).await;
        assert!(result.ready);
        assert_eq!(result.attempts, 1);
        assert_eq!(result.last_error, None);
        // 一次成功无需任何 sleep。
        assert_eq!(clock.slept().len(), 0);
    }

    #[tokio::test]
    async fn retries_with_backoff_until_success() {
        let warp = Arc::new(FlakyControl::new(FakeWarpControl::new(), 2));
        let clock = Arc::new(ManualClock::new());
        let probe = ReadinessProbe::new(
            warp,
            clock.clone(),
            Box::new(ExponentialBackoff::new(
                Duration::from_millis(100),
                2,
                Duration::from_secs(1),
            )),
            5,
        );

        let result = probe.probe(&ctx()).await;
        assert!(result.ready);
        assert_eq!(result.attempts, 3);
        // 前两次失败后各睡一次：2 次 sleep，时长 100ms、200ms。
        assert_eq!(
            clock.slept(),
            vec![Duration::from_millis(100), Duration::from_millis(200)]
        );
    }

    #[tokio::test]
    async fn gives_up_after_bounded_attempts() {
        let warp = Arc::new(FlakyControl::new(FakeWarpControl::new(), u32::MAX));
        let clock = Arc::new(ManualClock::new());
        let probe = ReadinessProbe::new(
            warp,
            clock.clone(),
            Box::new(ExponentialBackoff::recommended()),
            3,
        );

        let result = probe.probe(&ctx()).await;
        assert!(!result.ready);
        assert_eq!(result.attempts, 3);
        assert!(matches!(
            result.last_error,
            Some(WarpControlError::CommandTimeout)
        ));
        // 3 次尝试 -> 2 次等待（最后一次不睡）。
        assert_eq!(clock.slept().len(), 2);
    }
}
