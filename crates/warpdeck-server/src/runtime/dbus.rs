//! 每实例独立 D-Bus system bus（P2-005）。
//!
//! 设计（DESIGN §8.1 / §11.3）：
//! - 每个实例一个 dbus-daemon，socket 位于 `{runtime_dir}/instances/{id}/dbus/system_bus_socket`；
//! - `warp-svc` 与 `warp-cli` 均注入相同 `DBUS_SYSTEM_BUS_ADDRESS`，避免跨实例串接；
//! - 生命周期与实例绑定：start（建目录 → spawn → 等 socket 就绪）→ shutdown（kill → reap）。
//! - 不使用 `--fork`：进程由 spawner 直接管理，pid/handle 可信，退出可观测。

use std::path::Path;
use std::time::Duration;

use super::context::InstanceContext;
use super::process::{ProcessHandle, ProcessSpawner, ProcessStatus, SpawnCommand};

/// 等 socket 文件的默认上限。
const DBUS_SOCKET_WAIT: Duration = Duration::from_secs(10);
/// socket 轮询间隔。
const DBUS_SOCKET_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DbusError {
    #[error("dbus runtime dir could not be created: {0}")]
    DirCreateFailed(String),
    #[error("dbus-daemon spawn failed: {0}")]
    SpawnFailed(String),
    #[error("dbus socket not ready within {0:?}")]
    SocketNotReady(Duration),
}

/// 一个运行中的实例 D-Bus daemon。
///
/// 不实现 `Debug`：内部持有 `dyn ProcessHandle`（非 Debug 边界）。
pub struct DbusRuntime {
    process: Box<dyn ProcessHandle>,
    socket: std::path::PathBuf,
}

impl DbusRuntime {
    /// 以默认超时启动（生产路径）。
    pub async fn start(
        spawner: &dyn ProcessSpawner,
        ctx: &InstanceContext,
    ) -> Result<Self, DbusError> {
        Self::start_with_timeout(spawner, ctx, DBUS_SOCKET_WAIT).await
    }

    /// 启动流程：创建 runtime 目录 → spawn dbus-daemon → 等待 socket 就绪。
    pub async fn start_with_timeout(
        spawner: &dyn ProcessSpawner,
        ctx: &InstanceContext,
        socket_wait: Duration,
    ) -> Result<Self, DbusError> {
        let dbus_dir = ctx.paths.dbus_dir.clone();
        if let Err(e) = tokio::fs::create_dir_all(&dbus_dir).await {
            return Err(DbusError::DirCreateFailed(e.to_string()));
        }

        let socket = ctx.paths.dbus_socket.clone();
        let cmd = SpawnCommand {
            program: "dbus-daemon".to_string(),
            args: vec![
                "--system".to_string(),
                "--nofork".to_string(),
                "--nopidfile".to_string(),
                format!("--address=unix:path={}", socket.display()),
            ],
            envs: vec![],
            stderr_log_path: None,
            stdout_log_path: None,
        };
        let process = spawner
            .spawn(&cmd)
            .map_err(|e| DbusError::SpawnFailed(e.to_string()))?;

        match tokio::time::timeout(socket_wait, wait_for_socket(&socket)).await {
            Ok(Ok(())) => Ok(Self { process, socket }),
            Ok(Err(e)) => Err(DbusError::DirCreateFailed(e.to_string())),
            Err(_) => Err(DbusError::SocketNotReady(socket_wait)),
        }
    }

    /// 就绪的 socket 路径（与 `InstancePaths::dbus_socket` 一致）。
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// daemon 进程 PID（审计 / 调试用）。
    pub fn pid(&self) -> u32 {
        self.process.pid()
    }

    /// 停止 daemon：kill 后 reap 并返回退出状态。
    pub async fn shutdown(mut self) -> ProcessStatus {
        let _ = self.process.kill();
        self.process.wait().await
    }
}

/// 轮询等待 socket 文件出现（daemon 就绪信号）。
async fn wait_for_socket(socket: &Path) -> std::io::Result<()> {
    loop {
        if tokio::fs::try_exists(socket).await? {
            return Ok(());
        }
        tokio::time::sleep(DBUS_SOCKET_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeProcessSpawner;
    use crate::runtime::instance::InstanceId;

    async fn temp_ctx() -> (InstanceContext, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("warpdeck-dbus-{}", uuid::Uuid::new_v4()));
        let ctx = InstanceContext::new(&dir, &dir, InstanceId::from_db(0).unwrap()).unwrap();
        (ctx, dir)
    }

    #[tokio::test]
    async fn starts_daemon_with_instance_socket_address() {
        let spawner = FakeProcessSpawner::new();
        let (ctx, dir) = temp_ctx().await;

        // 预置 socket 文件模拟 daemon 就绪（Fake spawner 不产生真实进程）。
        tokio::fs::create_dir_all(&ctx.paths.dbus_dir)
            .await
            .unwrap();
        tokio::fs::write(&ctx.paths.dbus_socket, b"").await.unwrap();

        let runtime = DbusRuntime::start(&spawner, &ctx).await.unwrap();
        assert_eq!(runtime.socket(), &ctx.paths.dbus_socket);

        let calls = spawner.spawn_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "dbus-daemon");
        assert!(calls[0].args.contains(&format!(
            "--address=unix:path={}",
            ctx.paths.dbus_socket.display()
        )));

        runtime.shutdown().await;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn times_out_when_socket_never_appears() {
        let spawner = FakeProcessSpawner::new();
        // 关闭 fake 的 socket 自动就绪，模拟 daemon 永远不创建 socket。
        spawner.set_auto_socket(false);
        let (ctx, dir) = temp_ctx().await;

        let Err(err) =
            DbusRuntime::start_with_timeout(&spawner, &ctx, Duration::from_millis(100)).await
        else {
            panic!("expected SocketNotReady error");
        };
        assert!(matches!(err, DbusError::SocketNotReady(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn shutdown_kills_and_reaps_daemon() {
        let spawner = FakeProcessSpawner::new();
        let (ctx, dir) = temp_ctx().await;

        tokio::fs::create_dir_all(&ctx.paths.dbus_dir)
            .await
            .unwrap();
        tokio::fs::write(&ctx.paths.dbus_socket, b"").await.unwrap();

        let runtime = DbusRuntime::start(&spawner, &ctx).await.unwrap();
        let pid = runtime.pid();

        let status = runtime.shutdown().await;
        assert_eq!(status.exit_code, Some(137));
        assert!(spawner.was_killed(pid));

        std::fs::remove_dir_all(&dir).ok();
    }
}
