//! 健康检查调度器（P4-007）。
//!
//! 周期驱动 `InstanceManager::collect_health_layers` → `assess_layers`
//! （health.rs 纯函数）→ `apply_health_verdict`（迁移状态 + 发布事件）。
//!
//! P4-007 要求：
//! - 有 cancellation：`spawn` 返回 cancel sender，drop 即停止；
//! - 避免所有实例同一毫秒 probe：tick 内实例串行探测 + 每两个探测间
//!   `probe_interval` 还夹了网络往返/CLI 进程时间，天然错开毫秒级；
//!   首轮相位固定为一个完整 interval（与真实轮询同相位，行为可预测）；
//! - manager shutdown 时停止：组合方在停止时 drop cancel sender。

use std::sync::Arc;
use std::time::Duration;

use super::clock::Clock;
use super::health::{assess_layers, HealthConfig, HealthCounters};
use super::instance::InstanceId;
use super::manager::InstanceManager;

/// 健康检查调度器（P4-007）。
pub struct HealthMonitor {
    manager: Arc<InstanceManager>,
    clock: Arc<dyn Clock>,
    config: HealthConfig,
    interval: Duration,
}

impl HealthMonitor {
    pub fn new(
        manager: Arc<InstanceManager>,
        clock: Arc<dyn Clock>,
        config: HealthConfig,
        interval: Duration,
    ) -> Self {
        assert!(!interval.is_zero(), "health interval must be non-zero");
        Self {
            manager,
            clock,
            config,
            interval,
        }
    }

    /// 单帧：对全部运行中（Healthy/Degraded）实例探测一轮，返回探测数。
    pub async fn tick(&self) -> usize {
        let ids = self.manager.all_ids();
        let mut probed = 0;
        for id in ids {
            if self.tick_instance(id).await {
                probed += 1;
            }
        }
        probed
    }

    /// 单实例单轮健康检查。
    async fn tick_instance(&self, id: InstanceId) -> bool {
        let Some((snapshot, report)) = self.manager.collect_health_layers(id).await else {
            return false;
        };
        let counters = HealthCounters {
            consecutive_failures: snapshot.consecutive_failures,
            consecutive_successes: snapshot.consecutive_successes,
        };
        let (verdict, next) = assess_layers(&report, &counters, &self.config, snapshot.state);
        self.manager
            .apply_health_verdict(id, verdict, next, &report)
            .await;
        true
    }

    /// 启动调度循环（P4-007：cancellation via sender drop）。
    pub fn spawn(self) -> (tokio::task::JoinHandle<()>, tokio::sync::watch::Sender<()>) {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(());
        let handle = tokio::spawn(async move { self.run_loop(cancel_rx).await });
        (handle, cancel_tx)
    }

    async fn run_loop(&self, mut cancel: tokio::sync::watch::Receiver<()>) {
        loop {
            tokio::select! {
                // biased：cancel 优先（ManualClock 下 sleep 立即就绪时不空转 tick）。
                biased;
                _ = cancel.changed() => break,
                _ = self.clock.sleep(self.interval) => {
                    let count = self.tick().await;
                    tracing::debug!(component = "health_monitor", probed = count, "health tick");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::runtime::backoff::ExponentialBackoff;
    use crate::runtime::context::InstanceContext;
    use crate::runtime::events::EventBus;
    use crate::runtime::fake::{
        FakeDataPlaneProber, FakeProcessSpawner, FakeWarpControl, ManualClock,
    };
    use crate::runtime::manager::{PortProber, WarpRuntime};
    use crate::runtime::registry::RuntimeState;

    fn testing_prober() -> Arc<dyn PortProber> {
        #[derive(Debug)]
        struct AlwaysFree;
        impl PortProber for AlwaysFree {
            fn is_free(&self, _port: u16) -> bool {
                true
            }
        }
        Arc::new(AlwaysFree)
    }

    struct Harness {
        manager: Arc<InstanceManager>,
        registry: Arc<super::super::registry::RuntimeRegistry>,
        prober: Arc<FakeDataPlaneProber>,
        clock: Arc<ManualClock>,
        data_dir: PathBuf,
        runtime_base: PathBuf,
        _keep: Vec<tempfile::TempDir>,
    }

    impl Harness {
        fn new() -> Self {
            let registry = Arc::new(super::super::registry::RuntimeRegistry::new());
            let spawner = Arc::new(FakeProcessSpawner::new());
            let control = Arc::new(FakeWarpControl::new());
            control.set_registered(true);
            control.set_connected(true);
            let clock = Arc::new(ManualClock::new());
            let prober = Arc::new(FakeDataPlaneProber::new());
            let data = tempfile::TempDir::new().unwrap();
            let runtime = tempfile::TempDir::new().unwrap();
            let manager = Arc::new(InstanceManager::new(
                registry.clone(),
                spawner,
                control.clone(),
                clock.clone(),
                Box::new(ExponentialBackoff::new(
                    Duration::from_millis(10),
                    2,
                    Duration::from_millis(100),
                )),
                5,
                Arc::new(crate::runtime::fake::FakeCredentialResolver::default()),
                data.path().to_path_buf(),
                runtime.path().to_path_buf(),
                testing_prober(),
                prober.clone(),
                EventBus::new(16),
            ));
            Self {
                manager,
                registry,
                prober,
                clock,
                data_dir: data.path().to_path_buf(),
                runtime_base: runtime.path().to_path_buf(),
                _keep: vec![data, runtime],
            }
        }

        fn ctx(&self, id: i64) -> InstanceContext {
            InstanceContext::new(
                &self.data_dir,
                &self.runtime_base,
                InstanceId::from_db(id).unwrap(),
            )
            .unwrap()
        }

        async fn start_instance(&self, id: i64) {
            let ctx = self.ctx(id);
            std::fs::create_dir_all(&ctx.paths.state_dir).unwrap();
            self.manager.start(&ctx, None).await.unwrap();
        }
    }

    #[tokio::test]
    async fn healthy_instance_stays_healthy_and_records_metrics() {
        let h = Harness::new();
        h.start_instance(0).await;
        h.prober.push_ok("104.28.7.5", "LAX", 42);

        let monitor = HealthMonitor::new(
            h.manager.clone(),
            h.clock.clone(),
            HealthConfig::default(),
            Duration::from_secs(5),
        );
        let probed = monitor.tick().await;
        assert_eq!(probed, 1);

        let snap = h.registry.get(InstanceId::from_db(0).unwrap()).unwrap();
        assert_eq!(snap.state, RuntimeState::Healthy);
        assert_eq!(snap.exit_ip_v4.unwrap().to_string(), "104.28.7.5");
        assert_eq!(snap.colo.as_deref(), Some("LAX"));
        assert_eq!(snap.latency_ms, Some(42));
        assert_eq!(snap.consecutive_successes, 1);
        assert!(snap.last_error.is_none());
    }

    #[tokio::test]
    async fn transient_failure_moves_to_degraded_then_recovers() {
        let h = Harness::new();
        h.start_instance(0).await;

        let monitor = HealthMonitor::new(
            h.manager.clone(),
            h.clock.clone(),
            HealthConfig::default(),
            Duration::from_secs(5),
        );

        // 1 次失败 → Degraded。
        h.prober
            .push_err(crate::runtime::probe::ProbeError::Timeout(
                Duration::from_secs(10),
            ));
        monitor.tick().await;
        let snap = h.registry.get(InstanceId::from_db(0).unwrap()).unwrap();
        assert_eq!(snap.state, RuntimeState::Degraded);
        assert_eq!(snap.consecutive_failures, 1);

        // 恢复探测 1：仍 Degraded（未满 2 连成功）。
        h.prober.push_ok("104.28.7.5", "LAX", 33);
        monitor.tick().await;
        let snap = h.registry.get(InstanceId::from_db(0).unwrap()).unwrap();
        assert_eq!(snap.state, RuntimeState::Degraded);
        assert_eq!(snap.consecutive_successes, 1);

        // 恢复探测 2：Healthy。
        monitor.tick().await;
        let snap = h.registry.get(InstanceId::from_db(0).unwrap()).unwrap();
        assert_eq!(snap.state, RuntimeState::Healthy);
        assert_eq!(snap.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn repeated_failures_reach_failed() {
        let h = Harness::new();
        h.start_instance(0).await;

        let monitor = HealthMonitor::new(
            h.manager.clone(),
            h.clock.clone(),
            HealthConfig::default(),
            Duration::from_secs(5),
        );
        for _ in 0..3 {
            h.prober
                .push_err(crate::runtime::probe::ProbeError::Timeout(
                    Duration::from_secs(10),
                ));
            monitor.tick().await;
        }
        let snap = h.registry.get(InstanceId::from_db(0).unwrap()).unwrap();
        assert_eq!(snap.state, RuntimeState::Failed);
        assert_eq!(snap.consecutive_failures, 3);
        assert_eq!(snap.last_error.as_deref(), Some("data-plane probe failed"));
    }

    #[tokio::test]
    async fn warp_off_degrades_even_when_probe_succeeds() {
        let h = Harness::new();
        h.start_instance(0).await;
        h.prober.push_warp_off("8.8.8.8", "DFW", 7);

        let monitor = HealthMonitor::new(
            h.manager.clone(),
            h.clock.clone(),
            HealthConfig::default(),
            Duration::from_secs(5),
        );
        monitor.tick().await;
        let snap = h.registry.get(InstanceId::from_db(0).unwrap()).unwrap();
        assert_eq!(snap.state, RuntimeState::Degraded);
        assert_eq!(snap.consecutive_failures, 1);
    }

    #[tokio::test]
    async fn degraded_by_startup_verify_recovers_via_monitor() {
        // 启动尾部数据面验证失败（注满脚本）→ Degraded（start 返回成功），
        // 健康循环拉回 Healthy（恢复需 2 连成功）。
        let h = Harness::new();
        for _ in 0..12 {
            h.prober
                .push_err(crate::runtime::probe::ProbeError::Timeout(
                    Duration::from_secs(10),
                ));
        }
        h.start_instance(0).await;
        let id = InstanceId::from_db(0).unwrap();
        let snap = h.registry.get(id).unwrap();
        assert_eq!(snap.state, RuntimeState::Degraded);
        assert_eq!(snap.consecutive_failures, 1);

        // 进程/控制面均正常；默认 fake prober 无脚本时返回成功。
        let monitor = HealthMonitor::new(
            h.manager.clone(),
            h.clock.clone(),
            HealthConfig::default(),
            Duration::from_secs(5),
        );
        monitor.tick().await;
        assert_eq!(
            h.registry.get(id).unwrap().state,
            RuntimeState::Degraded,
            "恢复需连续 2 次成功"
        );
        monitor.tick().await;
        assert_eq!(h.registry.get(id).unwrap().state, RuntimeState::Healthy);
    }

    #[tokio::test]
    async fn does_not_probe_failed_or_stopped_instances() {
        let h = Harness::new();
        h.start_instance(0).await;
        let id = InstanceId::from_db(0).unwrap();
        // 打爆 3 次 → Failed。
        let monitor = HealthMonitor::new(
            h.manager.clone(),
            h.clock.clone(),
            HealthConfig::default(),
            Duration::from_secs(5),
        );
        for _ in 0..3 {
            h.prober
                .push_err(crate::runtime::probe::ProbeError::Timeout(
                    Duration::from_secs(10),
                ));
            monitor.tick().await;
        }
        assert_eq!(h.registry.get(id).unwrap().state, RuntimeState::Failed);
        let ports_before = h.prober.probed_ports().len();
        monitor.tick().await;
        assert_eq!(
            h.prober.probed_ports().len(),
            ports_before,
            "Failed 实例不应再被探测"
        );
    }

    #[tokio::test]
    async fn cancel_stops_loop() {
        let h = Harness::new();
        h.start_instance(0).await;
        let monitor = HealthMonitor::new(
            h.manager.clone(),
            h.clock.clone(),
            HealthConfig::default(),
            Duration::from_millis(50),
        );
        let (handle, cancel) = monitor.spawn();
        drop(cancel);
        let _ = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .unwrap();
    }
}
