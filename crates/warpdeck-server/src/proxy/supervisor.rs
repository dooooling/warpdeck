//! P5-004 GOST 进程监管。
//!
//! 职责：spawn（`gost -C config.yaml`）、优雅停止（SIGTERM → grace → SIGKILL）、
//! 重启、非阻塞退出探测、stderr 尾部（与 warp-svc 同样的 log 文件 + tail 模式）。
//! 崩溃感知：GOST 是网关单进程，不套用实例级 CrashWatcher（后者绑定 InstanceId），
//! 由 `GostManager` 用本模块的 `try_exited` 轮询即可。
//!
//! stderr 重定向：`stderr_log_path` 由调用方给定（`<data_dir>/logs/gost.stderr.log`），
//! 退出后可读尾部摘要；真实内容经中心 redactor（P8）过滤。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::runtime::clock::Clock;
use crate::runtime::process::{ProcessHandle, ProcessSpawner, ProcessStatus, SpawnCommand};

const STDERR_SUMMARY_MAX_BYTES: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GostSupervisorError {
    #[error("gost spawn failed: {0}")]
    SpawnFailed(String),
    #[error("stderr log directory could not be created: {0}")]
    LogDirFailed(String),
}

/// 一个运行中的 GOST 进程。
#[derive(Debug)]
pub struct GostProcess {
    process: Box<dyn ProcessHandle>,
    started_at: time::OffsetDateTime,
    log_path: PathBuf,
}

impl GostProcess {
    pub fn pid(&self) -> u32 {
        self.process.pid()
    }

    pub fn started_at(&self) -> time::OffsetDateTime {
        self.started_at
    }

    /// 非阻塞退出探测（崩溃感知轮询用）。
    pub fn try_exited(&mut self) -> Option<ProcessStatus> {
        self.process.try_wait().ok().flatten()
    }

    /// SIGTERM。
    pub fn terminate(&mut self) -> std::io::Result<()> {
        self.process.terminate()
    }

    /// SIGKILL + reap。
    pub async fn force_kill(&mut self) -> ProcessStatus {
        let _ = self.process.kill();
        self.process.wait().await
    }

    /// GOST stderr 尾部摘要。
    pub async fn stderr_summary(&self) -> String {
        read_log_tail(&self.log_path).await
    }

    /// 日志路径拷贝（供调用方避免跨 await 持有 `&GostProcess`）。
    pub fn log_path(&self) -> PathBuf {
        self.log_path.clone()
    }
}

/// GOST 进程监管器。
pub struct GostSupervisor {
    spawner: Arc<dyn ProcessSpawner>,
    clock: Arc<dyn Clock>,
    /// GOST 可执行文件名或绝对路径（默认 `gost`，走 PATH）。
    binary: String,
    config_path: PathBuf,
    log_path: PathBuf,
    /// grace 停止总预算与轮询步长。
    grace: Duration,
    poll: Duration,
}

impl GostSupervisor {
    pub fn new(
        spawner: Arc<dyn ProcessSpawner>,
        clock: Arc<dyn Clock>,
        binary: String,
        config_path: PathBuf,
        log_path: PathBuf,
        grace: Duration,
        poll: Duration,
    ) -> Self {
        assert!(!poll.is_zero(), "poll must be non-zero");
        Self {
            spawner,
            clock,
            binary,
            config_path,
            log_path,
            grace,
            poll,
        }
    }

    /// 启动 GOST（配置必须已原子落盘）。stderr 落到 log_path。
    pub async fn start(&self) -> Result<GostProcess, GostSupervisorError> {
        if let Some(parent) = self.log_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| GostSupervisorError::LogDirFailed(e.to_string()))?;
        }
        let cmd = SpawnCommand {
            program: self.binary.clone(),
            args: vec!["-C".to_string(), self.config_path.display().to_string()],
            envs: vec![],
            stderr_log_path: Some(self.log_path.clone()),
            // P10-005：GOST 日志（stdout，如 access log）并入同一文件。
            stdout_log_path: Some(self.log_path.clone()),
        };
        let process = self
            .spawner
            .spawn(&cmd)
            .map_err(|e| GostSupervisorError::SpawnFailed(e.to_string()))?;
        Ok(GostProcess {
            process,
            started_at: time::OffsetDateTime::now_utc(),
            log_path: self.log_path.clone(),
        })
    }

    /// 优雅停止：SIGTERM → grace 期轮询 → SIGKILL（与 GracefulStop 同策略，
    /// 无实例上下文版本）。
    pub async fn stop(&self, proc: &mut GostProcess) -> Result<ProcessStatus, GostSupervisorError> {
        let _ = proc.terminate();
        let mut exit_status = None;
        for _ in 0..max_polls(self.grace, self.poll) {
            if let Some(status) = proc.try_exited() {
                exit_status = Some(status);
                break;
            }
            self.clock.sleep(self.poll).await;
        }
        let status = match exit_status {
            Some(status) => status,
            None => proc.force_kill().await,
        };
        Ok(status)
    }
}

fn max_polls(grace: Duration, poll: Duration) -> u32 {
    let grace_ms = grace.as_millis();
    let poll_ms = poll.as_millis().max(1);
    ((grace_ms / poll_ms).min(u128::from(u32::MAX)) as u32).max(1)
}

/// 读取日志尾部（与 service.rs 同源实现；GOST 保留独立副本避免导出内部细节）。
pub(crate) async fn read_log_tail(path: &PathBuf) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let len = bytes.len() as u64;
    let start = len.saturating_sub(STDERR_SUMMARY_MAX_BYTES) as usize;
    String::from_utf8_lossy(&bytes[start..]).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::{FakeProcessSpawner, ManualClock};

    fn temp_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    fn supervisor(
        spawner: Arc<FakeProcessSpawner>,
        config_path: PathBuf,
        log_path: PathBuf,
    ) -> GostSupervisor {
        GostSupervisor::new(
            spawner.clone(),
            Arc::new(ManualClock::new()),
            "gost".into(),
            config_path,
            log_path,
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
    }

    #[tokio::test]
    async fn starts_gost_with_config_flag_and_stderr_log() {
        let spawner = Arc::new(FakeProcessSpawner::new());
        let dir = temp_dir();
        let cfg = dir.path().join("gost.yaml");
        let log = dir.path().join("logs/gost.stderr.log");

        let sup = supervisor(spawner.clone(), cfg.clone(), log.clone());
        let proc = sup.start().await.unwrap();
        assert_eq!(proc.pid(), 1);

        let calls = spawner.spawn_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "gost");
        assert_eq!(
            calls[0].args,
            vec!["-C".to_string(), cfg.display().to_string()]
        );
        assert_eq!(calls[0].stderr_log_path, Some(log));
    }

    #[tokio::test]
    async fn stop_terminates_and_waits_grace() {
        let spawner = Arc::new(FakeProcessSpawner::new());
        let dir = temp_dir();
        let sup = supervisor(
            spawner.clone(),
            dir.path().join("gost.yaml"),
            dir.path().join("gost.stderr.log"),
        );
        let mut proc = sup.start().await.unwrap();
        let pid = proc.pid();
        spawner.exit_on_terminate(pid, 0);

        let status = sup.stop(&mut proc).await.unwrap();
        assert_eq!(status, ProcessStatus { exit_code: Some(0) });
        assert!(spawner.was_terminated(pid));
        assert!(!spawner.was_killed(pid));
    }

    #[tokio::test]
    async fn stop_forces_kill_when_grace_expires() {
        let spawner = Arc::new(FakeProcessSpawner::new());
        let dir = temp_dir();
        let sup = supervisor(
            spawner.clone(),
            dir.path().join("gost.yaml"),
            dir.path().join("gost.stderr.log"),
        );
        let mut proc = sup.start().await.unwrap();
        let pid = proc.pid();
        // 不注入优雅退出 → 强杀。
        let status = sup.stop(&mut proc).await.unwrap();
        assert!(status.exit_code.is_some());
        assert!(spawner.was_killed(pid));
    }

    #[tokio::test]
    async fn stderr_summary_reads_log_tail() {
        let spawner = Arc::new(FakeProcessSpawner::new());
        let dir = temp_dir();
        let log = dir.path().join("logs/gost.stderr.log");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, b"line1\nline2\n").unwrap();
        let sup = supervisor(spawner.clone(), dir.path().join("gost.yaml"), log);
        let proc = sup.start().await.unwrap();
        let summary = proc.stderr_summary().await;
        assert!(summary.contains("line2"));
    }

    #[tokio::test]
    async fn spawn_failure_surfaces_error() {
        struct FailSpawner;
        #[async_trait::async_trait]
        impl ProcessSpawner for FailSpawner {
            fn spawn(&self, _cmd: &SpawnCommand) -> std::io::Result<Box<dyn ProcessHandle>> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such binary",
                ))
            }
        }
        let dir = temp_dir();
        let sup = GostSupervisor::new(
            Arc::new(FailSpawner),
            Arc::new(ManualClock::new()),
            "gost".into(),
            dir.path().join("gost.yaml"),
            dir.path().join("gost.stderr.log"),
            Duration::from_secs(5),
            Duration::from_millis(50),
        );
        let err = sup.start().await.unwrap_err();
        assert!(matches!(err, GostSupervisorError::SpawnFailed(_)));
    }
}
