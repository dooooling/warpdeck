//! 实例优雅停止流程（P2-010）。
//!
//! 顺序完全遵循 DESIGN §11.7：
//! 1. `warp-cli disconnect`（尽力而为，失败不阻塞停止，§11.7"disconnect if possible"）；
//! 2. SIGTERM 给 warp-svc（`ProcessHandle::terminate`）；
//! 3. grace 期内轮询退出（`try_exited`，poll 步长内 sleep 由 Clock 注入）；
//! 4. grace 超时仍未退出 → SIGKILL（`force_kill`）；
//! 5. 停止实例 D-Bus daemon（§11.7 第 6 步）；
//! 6. 清理 runtime 临时目录（§11.7 第 7 步）；
//! 7. **保留** state 目录（注册数据不随普通 stop 删除，§11.7 第 8 步）。
//!
//! 本流程只负责停止；RuntimeState 迁移（Stopping -> Stopped）由上层状态机负责。

use std::sync::Arc;

use super::clock::Clock;
use super::context::InstanceContext;
use super::control::WarpControl;
use super::dbus::DbusRuntime;
use super::process::ProcessStatus;
use super::service::WarpService;

/// 停止结果：是否被迫强杀 + warp-svc 最终退出状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopOutcome {
    /// true = grace 期内未退出，最终 SIGKILL。
    pub kill_required: bool,
    pub exit_status: ProcessStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StopError {
    /// §11.7 第 7 步：runtime 目录清理失败（state 目录不受影响）。
    #[error("runtime dir cleanup failed: {0}")]
    CleanupFailed(String),
}

/// 优雅停止器：负责 §11.7 的完整停止顺序。
pub struct GracefulStop {
    control: Arc<dyn WarpControl>,
    clock: Arc<dyn Clock>,
    /// grace 总预算（§11.7 第 4 步）。
    grace: std::time::Duration,
    /// 轮询步长。
    poll: std::time::Duration,
}

impl GracefulStop {
    pub fn new(
        control: Arc<dyn WarpControl>,
        clock: Arc<dyn Clock>,
        grace: std::time::Duration,
        poll: std::time::Duration,
    ) -> Self {
        assert!(!poll.is_zero(), "poll interval must be non-zero");
        Self {
            control,
            clock,
            grace,
            poll,
        }
    }

    /// grace 预算换算为轮询次数（至少 1 次：grace=0 表示立即强杀前仍探一次）。
    fn max_polls(&self) -> u32 {
        let grace_ms = self.grace.as_millis();
        let poll_ms = self.poll.as_millis().max(1);
        ((grace_ms / poll_ms).min(u128::from(u32::MAX)) as u32).max(1)
    }

    /// 执行完整停止顺序。
    ///
    /// `ctx` 由调用方传入（manager 在 crash watcher 归还 svc 后仍持有上下文；
    /// svc 本身也保有同一 ctx，但显式传参让本方法不依赖 svc 具体类型的
    /// `context()` 能力，解耦停止器与进程包装类型）。
    pub async fn stop(
        &self,
        ctx: &InstanceContext,
        svc: &mut WarpService,
        dbus: &mut DbusRuntime,
    ) -> Result<StopOutcome, StopError> {
        // 1. disconnect if possible（失败不阻塞）。
        let _ = self.control.disconnect(ctx).await;

        // 2. SIGTERM。
        let _ = svc.terminate();

        // 3+4. grace 期轮询，超时强杀。
        let mut exit_status = None;
        for _ in 0..self.max_polls() {
            if let Some(status) = svc.try_exited() {
                exit_status = Some(status);
                break;
            }
            self.clock.sleep(self.poll).await;
        }
        let kill_required = exit_status.is_none();
        let exit_status = match exit_status {
            Some(status) => status,
            None => svc.force_kill().await,
        };

        // 5. 停止 D-Bus。
        let _ = dbus.shutdown().await;

        // 6. 清理 runtime 目录（含 dbus/、warp/；state 目录不动）。
        if let Err(e) = tokio::fs::remove_dir_all(&ctx.paths.runtime_dir).await {
            return Err(StopError::CleanupFailed(e.to_string()));
        }

        Ok(StopOutcome {
            kill_required,
            exit_status,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::runtime::context::InstanceContext;
    use crate::runtime::fake::{FakeProcessSpawner, FakeWarpControl, ManualClock};
    use crate::runtime::instance::InstanceId;

    fn temp_ctx() -> (InstanceContext, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("warpdeck-stop-{}", uuid::Uuid::new_v4()));
        let ctx = InstanceContext::new(&dir, &dir, InstanceId::from_db(0).unwrap()).unwrap();
        (ctx, dir)
    }

    /// 组合一个已启动的 warp-svc + dbus 环境（spawner 记录全部进程）。
    async fn setup() -> (
        FakeProcessSpawner,
        FakeWarpControl,
        InstanceContext,
        std::path::PathBuf,
    ) {
        let spawner = FakeProcessSpawner::new();
        let (ctx, dir) = temp_ctx();
        let control = FakeWarpControl::new();
        // 预置 dbus socket 文件模拟 daemon 就绪（Fake spawner 不产生真实进程）。
        std::fs::create_dir_all(&ctx.paths.dbus_dir).unwrap();
        std::fs::write(&ctx.paths.dbus_socket, b"").unwrap();
        (spawner, control, ctx, dir)
    }

    fn stop(control: Arc<dyn WarpControl>, clock: Arc<dyn Clock>) -> GracefulStop {
        GracefulStop::new(
            control,
            clock,
            Duration::from_secs(10),
            Duration::from_millis(100),
        )
    }

    #[tokio::test]
    async fn graceful_exit_without_force_kill() {
        let (spawner, control, ctx, dir) = setup().await;
        let mut svc = WarpService::start(&spawner, &ctx).await.unwrap();
        let mut dbus = DbusRuntime::start(&spawner, &ctx).await.unwrap();
        let warp_pid = svc.pid();
        let dbus_pid = dbus.pid();
        // warp-svc 收到 SIGTERM 后自行优雅退出。
        spawner.exit_on_terminate(warp_pid, 0);
        // 制造 runtime 目录与 state 目录（正常启动已建）。
        let clock = Arc::new(ManualClock::new());

        let outcome = stop(Arc::new(control), clock.clone())
            .stop(&ctx, &mut svc, &mut dbus)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            StopOutcome {
                kill_required: false,
                exit_status: ProcessStatus { exit_code: Some(0) },
            }
        );
        assert!(spawner.was_terminated(warp_pid));
        assert!(!spawner.was_killed(warp_pid));
        assert!(spawner.was_killed(dbus_pid));
        // 优雅退出：无需任何轮询 sleep。
        assert!(clock.slept().is_empty());
        // state 保留、runtime 清理。
        assert!(ctx.paths.state_dir.exists());
        assert!(!ctx.paths.runtime_dir.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn force_kill_when_grace_expires() {
        let (spawner, control, ctx, dir) = setup().await;
        let mut svc = WarpService::start(&spawner, &ctx).await.unwrap();
        let mut dbus = DbusRuntime::start(&spawner, &ctx).await.unwrap();
        let warp_pid = svc.pid();
        // 不注入优雅退出：SIGTERM 后仍存活。
        let clock = Arc::new(ManualClock::new());

        let outcome = stop(Arc::new(control), clock.clone())
            .stop(&ctx, &mut svc, &mut dbus)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            StopOutcome {
                kill_required: true,
                exit_status: ProcessStatus {
                    exit_code: Some(137)
                },
            }
        );
        assert!(spawner.was_terminated(warp_pid));
        assert!(spawner.was_killed(warp_pid));
        // grace 10s / poll 100ms = 100 次轮询，每次失败后 sleep 100ms。
        assert_eq!(clock.slept().len(), 100);
        assert!(!ctx.paths.runtime_dir.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn zero_grace_means_immediate_force_kill() {
        let (spawner, control, ctx, dir) = setup().await;
        let mut svc = WarpService::start(&spawner, &ctx).await.unwrap();
        let mut dbus = DbusRuntime::start(&spawner, &ctx).await.unwrap();
        let warp_pid = svc.pid();
        let clock = Arc::new(ManualClock::new());

        let stop = GracefulStop::new(
            Arc::new(control),
            clock.clone(),
            Duration::ZERO,
            Duration::from_millis(100),
        );
        let outcome = stop.stop(&ctx, &mut svc, &mut dbus).await.unwrap();

        assert!(outcome.kill_required);
        assert!(spawner.was_killed(warp_pid));
        // grace=0 -> 至少探测一次即强杀（1 次 sleep 后 kill）。
        assert_eq!(clock.slept().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn disconnect_failure_does_not_block_stop() {
        let (spawner, control, ctx, dir) = setup().await;
        control.fail_next(crate::runtime::control::WarpControlError::CommandTimeout);
        let mut svc = WarpService::start(&spawner, &ctx).await.unwrap();
        let mut dbus = DbusRuntime::start(&spawner, &ctx).await.unwrap();
        spawner.exit_on_terminate(svc.pid(), 0);
        let clock = Arc::new(ManualClock::new());

        let outcome = stop(Arc::new(control), clock)
            .stop(&ctx, &mut svc, &mut dbus)
            .await
            .unwrap();

        assert!(!outcome.kill_required);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn state_dir_is_preserved_after_stop() {
        let (spawner, control, ctx, dir) = setup().await;
        // 模拟已注册数据：state_dir 下存在 reg.json。
        std::fs::create_dir_all(&ctx.paths.state_dir).unwrap();
        std::fs::write(ctx.paths.state_dir.join("reg.json"), b"{}").unwrap();
        let mut svc = WarpService::start(&spawner, &ctx).await.unwrap();
        let mut dbus = DbusRuntime::start(&spawner, &ctx).await.unwrap();
        spawner.exit_on_terminate(svc.pid(), 0);

        stop(Arc::new(control), Arc::new(ManualClock::new()))
            .stop(&ctx, &mut svc, &mut dbus)
            .await
            .unwrap();

        // §11.7 第 8 步：注册数据保留。
        assert!(ctx.paths.state_dir.join("reg.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
