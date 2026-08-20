//! 实例进程崩溃监视（P2-011 Crash Watcher）。
//!
//! DESIGN §25.9 Crash 要求 / 计划 P2-011：
//! - warp-svc 意外退出 → 探测并发出 `CrashEvent`（manager 自身不退出，因为
//!   watcher 是实例级独立任务）；
//! - 记录 last error（退出状态 + stderr 尾部摘要），结构化日志打点；
//! - 本阶段不自动重启：是否 restart 由后续 Reconciler 决策。
//!
//! 受控停止（GracefulStop）必须先取消 watcher？否——`cancel` 信号由停止流程
//! 发出，watcher 收到后静默结束，避免把受控退出误报为 crash。

use std::sync::Arc;

use async_trait::async_trait;

use super::clock::Clock;
use super::instance::InstanceId;
use super::process::ProcessStatus;
use super::service::WarpService;

/// 崩溃事件：进程身份 + 退出状态 + stderr 摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashEvent {
    pub instance_id: InstanceId,
    pub exit_status: ProcessStatus,
    /// 崩溃前的 stderr 尾部摘要（诊断，不含 secret）。
    pub stderr_summary: String,
}

/// 崩溃监视所需的进程探测面（由 `WarpService` 实现；测试可注入 fake）。
#[async_trait]
pub trait CrashSource: Send {
    /// 非阻塞探测：进程已退出则返回状态（reap 语义由调用方保证）。
    fn probe_exit(&mut self) -> Option<ProcessStatus>;
    fn instance_id(&self) -> InstanceId;
    async fn stderr_summary(&mut self) -> String;
    /// 取回 `WarpService` 所有权（watcher 结束后归还进程句柄，供 GracefulStop
    /// 继续使用；唯一真实实现是 `WarpService`，测试 fake 返回 None）。
    fn into_warp_service(self: Box<Self>) -> Option<WarpService> {
        None
    }
}

/// 崩溃监视器：轮询探测退出，受控取消或崩溃时结束。
pub struct CrashWatcher {
    clock: Arc<dyn Clock>,
    poll: std::time::Duration,
}

impl CrashWatcher {
    pub fn new(clock: Arc<dyn Clock>, poll: std::time::Duration) -> Self {
        assert!(!poll.is_zero(), "poll must be non-zero");
        Self { clock, poll }
    }

    /// 监视直到：`cancel` 通道关闭（受控停止 → `None`）或探测到退出（→ `Some`）。
    /// 探测到崩溃后返回事件；无论何种结局都归还 `source` 所有权（manager 用
    /// `into_warp_service` 取回进程句柄）。
    ///
    /// `biased` 令 cancel 分支优先：ManualClock 下 sleep 即时完成，若两分支同时
    /// 就绪随机选择会造成既无 cancel 也无崩溃的无限轮询（挂起测试）；受控停止
    /// 语义上 cancel 先于崩溃到来（GracefulStop 流程），故 cancel 优先不会吞掉
    /// 真实崩溃。
    pub async fn watch(
        &self,
        mut source: Box<dyn CrashSource>,
        mut cancel: tokio::sync::watch::Receiver<()>,
    ) -> (Option<CrashEvent>, Box<dyn CrashSource>) {
        loop {
            tokio::select! {
                biased;
                _ = cancel.changed() => return (None, source),
                _ = self.clock.sleep(self.poll) => {
                    if let Some(exit_status) = source.probe_exit() {
                        let event = CrashEvent {
                            instance_id: source.instance_id(),
                            exit_status,
                            stderr_summary: source.stderr_summary().await,
                        };
                        tracing::warn!(
                            component = "crash_watcher",
                            instance_id = %event.instance_id,
                            exit_code = ?event.exit_status.exit_code,
                            event = "instance_crashed",
                            "warp-svc exited unexpectedly"
                        );
                        return (Some(event), source);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::runtime::context::InstanceContext;
    use crate::runtime::fake::{FakeProcessSpawner, ManualClock};
    use crate::runtime::service::WarpService;

    fn temp_ctx() -> (InstanceContext, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("warpdeck-crash-{}", uuid::Uuid::new_v4()));
        let ctx = InstanceContext::new(&dir, &dir, InstanceId::from_db(2).unwrap()).unwrap();
        (ctx, dir)
    }

    fn cancel_pair() -> (
        tokio::sync::watch::Sender<()>,
        tokio::sync::watch::Receiver<()>,
    ) {
        tokio::sync::watch::channel(())
    }

    fn watcher(clock: Arc<dyn Clock>) -> CrashWatcher {
        CrashWatcher::new(clock, Duration::from_millis(100))
    }

    #[tokio::test]
    async fn detects_crash_and_reports_event() {
        let spawner = FakeProcessSpawner::new();
        let (ctx, dir) = temp_ctx();
        // stderr 摘要预置（模拟日志文件已有内容）。
        std::fs::create_dir_all(ctx.paths.log_path.parent().unwrap()).unwrap();
        std::fs::write(&ctx.paths.log_path, "boom: license invalid\n").unwrap();

        let svc = WarpService::start(&spawner, &ctx).await.unwrap();
        let pid = svc.pid();
        let clock = Arc::new(ManualClock::new());
        let (_tx, cancel) = cancel_pair();

        // 一轮 poll 后进程崩溃。
        let watch = tokio::spawn({
            let clock = clock.clone();
            async move { watcher(clock).watch(Box::new(svc), cancel).await }
        });
        tokio::task::yield_now().await;
        spawner.crash_process(pid);

        let (event, source) = watch.await.unwrap();
        let event = event.expect("crash must be detected");
        assert_eq!(event.instance_id, InstanceId::from_db(2).unwrap());
        assert_eq!(event.exit_status, ProcessStatus { exit_code: Some(1) });
        assert!(event.stderr_summary.contains("license invalid"));
        assert_eq!(clock.slept().len(), 1);
        // 崩溃后所有权归还：可还原为 WarpService（进程已死，try_exited 可见）。
        let mut svc = source
            .into_warp_service()
            .expect("real CrashSource must be recoverable");
        assert_eq!(svc.try_exited(), Some(ProcessStatus { exit_code: Some(1) }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn cancel_returns_none_silently() {
        let spawner = FakeProcessSpawner::new();
        let (ctx, dir) = temp_ctx();
        let svc = WarpService::start(&spawner, &ctx).await.unwrap();
        let clock = Arc::new(ManualClock::new());
        let (tx, cancel) = cancel_pair();

        let watch = tokio::spawn({
            let clock = clock.clone();
            async move { watcher(clock).watch(Box::new(svc), cancel).await }
        });
        drop(tx);

        let (event, source) = watch.await.unwrap();
        assert!(event.is_none());
        // 受控取消：0 次轮询。
        assert!(clock.slept().is_empty());
        // 受控取消同样归还所有权：进程存活，可继续 GracefulStop。
        let mut svc = source
            .into_warp_service()
            .expect("real CrashSource must be recoverable");
        assert!(svc.try_exited().is_none());
        let _ = svc.shutdown().await;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn keeps_watching_until_crash_across_polls() {
        let spawner = FakeProcessSpawner::new();
        let (ctx, dir) = temp_ctx();
        let svc = WarpService::start(&spawner, &ctx).await.unwrap();
        let pid = svc.pid();
        let clock = Arc::new(ManualClock::new());
        let (_tx, cancel) = cancel_pair();

        let watch = tokio::spawn({
            let clock = clock.clone();
            async move { watcher(clock).watch(Box::new(svc), cancel).await }
        });
        // 两轮 poll 后崩溃，期间 watcher 存活。
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        spawner.crash_process(pid);

        let (event, _source) = watch.await.unwrap();
        let event = event.expect("crash must be detected");
        assert!(event.exit_status.exit_code.is_some());
        assert!(clock.slept().len() >= 2, "expected >=2 polls");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn other_instances_keep_running_after_one_crash() {
        let spawner = FakeProcessSpawner::new();
        let (ctx_a, dir_a) = temp_ctx();
        let dir_b = std::env::temp_dir().join(format!("warpdeck-crash-b-{}", uuid::Uuid::new_v4()));
        let ctx_b = InstanceContext::new(&dir_b, &dir_b, InstanceId::from_db(0).unwrap()).unwrap();

        let svc_a = WarpService::start(&spawner, &ctx_a).await.unwrap();
        let mut svc_b = WarpService::start(&spawner, &ctx_b).await.unwrap();
        let pid_a = svc_a.pid();
        // sender 保活到测试结束：drop 会关闭 cancel 通道使 watcher 静默退出。
        let (_tx_a, cancel_a) = cancel_pair();
        let _tx_hold = _tx_a;
        let clock = Arc::new(ManualClock::new());

        let watch_a = tokio::spawn({
            let clock = clock.clone();
            async move { watcher(clock).watch(Box::new(svc_a), cancel_a).await }
        });
        tokio::task::yield_now().await;
        spawner.crash_process(pid_a);

        // watcher A 报告崩溃；B 的进程句柄仍可正常巡检（互不影响）。
        let (event, _source) = watch_a.await.unwrap();
        let event = event.expect("crash A must be detected");
        assert_eq!(event.instance_id, ctx_a.id);
        assert!(svc_b.try_exited().is_none());
        assert!(!clock.slept().is_empty());

        std::fs::remove_dir_all(&dir_a).ok();
        std::fs::remove_dir_all(&dir_b).ok();
    }
}
