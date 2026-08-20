//! warp-svc 进程包装（P2-006）。
//!
//! 设计（DESIGN §11.3 / 计划 P2-006）：
//! - 以实例独立环境启动：`STATE_DIRECTORY` / `RUNTIME_DIRECTORY` / `DBUS_SYSTEM_BUS_ADDRESS`；
//! - 必须记录 PID、start time、exit status、stderr summary；
//! - stderr 重定向到 `InstancePaths::log_path`，退出后可读取尾部摘要。

use std::time::Duration;

use super::context::InstanceContext;
use super::crash::CrashSource;
use super::instance::InstanceId;
use super::process::{ProcessHandle, ProcessSpawner, ProcessStatus, SpawnCommand};

/// stderr summary 最大读取字节数（尾部）。
const STDERR_SUMMARY_MAX_BYTES: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WarpServiceError {
    #[error("warp-svc state/runtime dir could not be created: {0}")]
    DirCreateFailed(String),
    #[error("warp-svc spawn failed: {0}")]
    SpawnFailed(String),
    #[error("warp-svc stderr log unreadable: {0}")]
    StderrUnreadable(String),
}

/// 一个运行中的 warp-svc 实例。
pub struct WarpService {
    process: Box<dyn ProcessHandle>,
    started_at: time::OffsetDateTime,
    ctx: InstanceContext,
}

impl WarpService {
    /// 创建 state / runtime 目录并启动 warp-svc。
    pub async fn start(
        spawner: &dyn ProcessSpawner,
        ctx: &InstanceContext,
    ) -> Result<Self, WarpServiceError> {
        for dir in [&ctx.paths.state_dir, &ctx.paths.runtime_dir] {
            if let Err(e) = tokio::fs::create_dir_all(dir).await {
                return Err(WarpServiceError::DirCreateFailed(e.to_string()));
            }
        }
        // stderr 重定向目标（§8.1 `{data_dir}/logs/instance-{id}.log`）的父目录；
        // 不建则真实 spawn 的 File::create 报 ENOENT（Fake spawner 不碰文件，注意覆盖）。
        if let Some(parent) = ctx.paths.log_path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return Err(WarpServiceError::DirCreateFailed(e.to_string()));
            }
        }

        let cmd = SpawnCommand {
            program: "warp-svc".to_string(),
            args: vec!["--accept-tos".to_string()],
            envs: vec![
                (
                    "STATE_DIRECTORY".to_string(),
                    ctx.paths.state_dir.display().to_string(),
                ),
                (
                    "RUNTIME_DIRECTORY".to_string(),
                    ctx.paths.runtime_dir.display().to_string(),
                ),
                (
                    "DBUS_SYSTEM_BUS_ADDRESS".to_string(),
                    ctx.paths.dbus_system_bus_address(),
                ),
            ],
            stderr_log_path: Some(ctx.paths.log_path.clone()),
            // P10-005：stdout 也并入同一文件（输出顺序合并）。
            stdout_log_path: Some(ctx.paths.log_path.clone()),
        };
        let process = spawner
            .spawn(&cmd)
            .map_err(|e| WarpServiceError::SpawnFailed(e.to_string()))?;

        Ok(Self {
            process,
            started_at: time::OffsetDateTime::now_utc(),
            ctx: ctx.clone(),
        })
    }

    /// PID（审计 / 调试）。
    pub fn pid(&self) -> u32 {
        self.process.pid()
    }

    /// 启动时间（UTC）。
    pub fn started_at(&self) -> time::OffsetDateTime {
        self.started_at
    }

    /// 非阻塞探测退出（crash watcher 用）。
    pub fn try_exited(&mut self) -> Option<ProcessStatus> {
        self.process.try_wait().ok().flatten()
    }

    /// 请求优雅退出（SIGTERM，§11.7 第 3 步）；随后由调用方等待/强杀。
    pub fn terminate(&mut self) -> std::io::Result<()> {
        self.process.terminate()
    }

    /// 强杀（SIGKILL，§11.7 第 5 步）并 reap，返回退出状态。
    pub async fn force_kill(&mut self) -> ProcessStatus {
        let _ = self.process.kill();
        self.process.wait().await
    }

    /// 停止：kill 后 reap 并返回退出状态（立即强杀，无 grace 期；内部/测试用）。
    pub async fn shutdown(mut self) -> ProcessStatus {
        let _ = self.process.kill();
        self.process.wait().await
    }

    /// stderr 尾部摘要（诊断用，不含 secret——真实 stderr 由 redactor 过滤）。
    pub async fn stderr_summary(&self) -> String {
        let path = self.ctx.paths.log_path.clone();
        read_log_tail(&path).await
    }

    /// 上下文（路径等，供后续 readiness 探测使用）。
    pub fn context(&self) -> &InstanceContext {
        &self.ctx
    }
}

/// 读取日志尾部摘要（仅捕获局部 path 借用 → future 恒 Send）。
async fn read_log_tail(path: &std::path::Path) -> String {
    let Ok(content) = tokio::fs::read_to_string(path).await else {
        return String::new();
    };
    tail_summary(&content, STDERR_SUMMARY_MAX_BYTES)
}

#[async_trait::async_trait]
impl CrashSource for WarpService {
    fn probe_exit(&mut self) -> Option<ProcessStatus> {
        self.try_exited()
    }

    fn instance_id(&self) -> InstanceId {
        self.ctx.id
    }

    async fn stderr_summary(&mut self) -> String {
        let path = self.ctx.paths.log_path.clone();
        read_log_tail(&path).await
    }

    fn into_warp_service(self: Box<Self>) -> Option<WarpService> {
        Some(*self)
    }
}

/// stderr summary 从文件尾部截取的辅助函数（独立可测）。
pub fn tail_summary(content: &str, max_bytes: u64) -> String {
    if content.len() as u64 <= max_bytes {
        content.to_string()
    } else {
        content[content.len() - max_bytes as usize..].to_string()
    }
}

/// 启动后的最大存活探测等待（P2-008 readiness 使用，此处先统一常量）。
pub const SERVICE_READY_WAIT: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeProcessSpawner;
    use crate::runtime::instance::InstanceId;

    fn temp_ctx() -> (InstanceContext, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("warpdeck-svc-{}", uuid::Uuid::new_v4()));
        let ctx = InstanceContext::new(&dir, &dir, InstanceId::from_db(0).unwrap()).unwrap();
        (ctx, dir)
    }

    #[tokio::test]
    async fn spawns_with_instance_isolated_environment() {
        let spawner = FakeProcessSpawner::new();
        let (ctx, dir) = temp_ctx();

        let svc = WarpService::start(&spawner, &ctx).await.unwrap();

        let calls = spawner.spawn_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "warp-svc");
        assert_eq!(calls[0].args, vec!["--accept-tos"]);
        assert_eq!(
            calls[0].envs,
            vec![
                (
                    "STATE_DIRECTORY".to_string(),
                    ctx.paths.state_dir.display().to_string()
                ),
                (
                    "RUNTIME_DIRECTORY".to_string(),
                    ctx.paths.runtime_dir.display().to_string()
                ),
                (
                    "DBUS_SYSTEM_BUS_ADDRESS".to_string(),
                    ctx.paths.dbus_system_bus_address()
                ),
            ]
        );
        assert_eq!(calls[0].stderr_log_path, Some(ctx.paths.log_path.clone()));

        assert!(svc.started_at() <= time::OffsetDateTime::now_utc());
        let _ = svc.shutdown().await;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn records_pid_and_start_time() {
        let spawner = FakeProcessSpawner::new();
        let (ctx, dir) = temp_ctx();

        let svc = WarpService::start(&spawner, &ctx).await.unwrap();
        assert_eq!(svc.pid(), 1);

        let _ = svc.shutdown().await;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn stderr_summary_reads_log_tail() {
        let spawner = FakeProcessSpawner::new();
        let (ctx, dir) = temp_ctx();

        tokio::fs::create_dir_all(&ctx.paths.log_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&ctx.paths.log_path, "boom: license invalid\n")
            .await
            .unwrap();

        let svc = WarpService::start(&spawner, &ctx).await.unwrap();
        assert_eq!(svc.stderr_summary().await, "boom: license invalid\n");

        let _ = svc.shutdown().await;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tail_summary_truncates_from_end() {
        let content = "x".repeat(100);
        let tail = tail_summary(&content, 10);
        assert_eq!(tail.len(), 10);
        assert_eq!(tail, "x".repeat(10));
        assert_eq!(tail_summary("short", 10), "short");
    }

    #[tokio::test]
    async fn shutdown_kills_and_reaps() {
        let spawner = FakeProcessSpawner::new();
        let (ctx, dir) = temp_ctx();

        let svc = WarpService::start(&spawner, &ctx).await.unwrap();
        let pid = svc.pid();
        let status = svc.shutdown().await;
        assert_eq!(status.exit_code, Some(137));
        assert!(spawner.was_killed(pid));

        std::fs::remove_dir_all(&dir).ok();
    }
}
