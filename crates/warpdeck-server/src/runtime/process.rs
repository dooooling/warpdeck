//! 外部进程启动抽象（P2-004）。
//!
//! 所有长生命周期子进程（D-Bus daemon、warp-svc）都经由 `ProcessSpawner`
//! 启动；测试使用 `FakeProcessSpawner` 验证启动参数 / 环境变量 / kill-reap / crash。

use std::time::Duration;

use async_trait::async_trait;

/// 进程启动请求：program + args + 环境变量注入。
/// 禁止在 domain 层拼 shell 字符串；参数逐项传递（AGENTS.md）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnCommand {
    pub program: String,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
    /// 将子进程 stderr 重定向到该文件（诊断 / 审计用，如实例日志）。
    pub stderr_log_path: Option<std::path::PathBuf>,
    /// 将子进程 stdout 重定向到该文件（P10-005 合并 stdout/stderr 到同一日志）。
    pub stdout_log_path: Option<std::path::PathBuf>,
}

impl SpawnCommand {
    /// 最小构造：无 stdout/stderr 重定向。
    pub fn simple(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: vec![],
            envs: vec![],
            stderr_log_path: None,
            stdout_log_path: None,
        }
    }

    /// builder：追加参数。
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    /// builder：追加环境变量。
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    /// builder：设置 stderr 重定向路径。
    pub fn with_stderr_log(mut self, path: std::path::PathBuf) -> Self {
        self.stderr_log_path = Some(path);
        self
    }

    /// builder：设置 stdout 重定向路径（与 stderr 同一路径时输出合并）。
    pub fn with_stdout_log(mut self, path: std::path::PathBuf) -> Self {
        self.stdout_log_path = Some(path);
        self
    }
}

/// 进程退出状态抽象（对 `std::process::ExitStatus` 的极简投影）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessStatus {
    /// `None` 表示被信号终止（无退出码）。
    pub exit_code: Option<i32>,
}

impl ProcessStatus {
    pub fn success(self) -> bool {
        self.exit_code == Some(0)
    }
}

/// 运行中子进程句柄（`async_trait` 保证 dyn 兼容，供 `Box<dyn ProcessHandle>` 分发）。
#[async_trait]
pub trait ProcessHandle: Send + std::fmt::Debug {
    fn pid(&self) -> u32;

    /// 请求优雅退出（SIGTERM 语义，§11.7 第 3 步；P2-010 引入）。
    /// 返回后进程可能仍在运行，随后应 `wait`（带超时）或 `kill`。
    fn terminate(&mut self) -> std::io::Result<()>;

    /// 终止进程（SIGKILL 语义）；返回后进程仍待 reap。
    fn kill(&mut self) -> std::io::Result<()>;

    /// 等待子进程终结（reap）。
    async fn wait(&mut self) -> ProcessStatus;

    /// 非阻塞探测退出（crash watcher 轮询用）。
    fn try_wait(&mut self) -> std::io::Result<Option<ProcessStatus>>;
}

/// 进程启动器抽象。
#[async_trait]
pub trait ProcessSpawner: Send + Sync {
    fn spawn(&self, cmd: &SpawnCommand) -> std::io::Result<Box<dyn ProcessHandle>>;
}

/// 提供给需要超时的等待场景的统一等待上限（P2-005/006 使用）。
pub const PROCESS_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// 生产实现：`tokio::process::Command` 启动真实子进程（P2-004）。
///
/// 生命周期语义与 `FakeProcessSpawner` 对齐：
/// - `terminate()` → SIGTERM（Linux）；Windows 下退化为 kill（测试环境无 WARP）。
/// - `kill()` → SIGKILL；`wait()` → reap 并返回退出状态。
/// - stderr 按 `SpawnCommand::stderr_log_path` 重定向（stdout 继承，避免阻塞）。
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioProcessSpawner;

#[async_trait]
impl ProcessSpawner for TokioProcessSpawner {
    fn spawn(&self, cmd: &SpawnCommand) -> std::io::Result<Box<dyn ProcessHandle>> {
        let mut builder = tokio::process::Command::new(&cmd.program);
        builder.args(&cmd.args);
        builder.envs(cmd.envs.iter().map(|(k, v)| (k, v)));

        // P1 审查 R3#7：子进程输出**不再直接落盘**——改为管道 + 行级脱敏泵
        // （scrub_line 后追加写入），原始输出（可能含 license/token/密码回显）
        // 不进入持久卷与备份。文件打开语义与旧实现一致：
        // - 双路径相同（合并）：create 截断一次，双 append 泵共享句柄副本；
        // - 单边重定向：truncate 创建 + 单泵；未重定向的一侧保持 inherit。
        enum PumpTarget {
            Stdout,
            Stderr,
        }
        let merged = cmd.stdout_log_path.is_some() && cmd.stdout_log_path == cmd.stderr_log_path;
        let mut pipes: Vec<(PumpTarget, std::fs::File)> = Vec::new();
        if let Some(path) = cmd
            .stdout_log_path
            .as_ref()
            .or(cmd.stderr_log_path.as_ref())
        {
            let file = if merged {
                std::fs::File::create(path)?;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?
            } else {
                std::fs::File::create(path)?
            };
            if cmd.stdout_log_path.is_some() {
                builder.stdout(std::process::Stdio::piped());
                pipes.push((PumpTarget::Stdout, file.try_clone()?));
            } else {
                builder.stdout(std::process::Stdio::inherit());
            }
            if cmd.stderr_log_path.is_some() {
                builder.stderr(std::process::Stdio::piped());
                pipes.push((PumpTarget::Stderr, file));
            } else {
                builder.stderr(std::process::Stdio::inherit());
            }
        } else {
            builder.stdout(std::process::Stdio::inherit());
            builder.stderr(std::process::Stdio::inherit());
        }
        let mut child = builder.spawn()?;

        // 每条被重定向的管道启动脱敏泵。
        for (target, file) in pipes {
            match target {
                PumpTarget::Stdout => {
                    if let Some(stdout) = child.stdout.take() {
                        tokio::spawn(pump_redacted(stdout, file));
                    }
                }
                PumpTarget::Stderr => {
                    if let Some(stderr) = child.stderr.take() {
                        tokio::spawn(pump_redacted(stderr, file));
                    }
                }
            }
        }

        Ok(Box::new(TokioProcess { child: Some(child) }))
    }
}

/// 行级脱敏泵：逐行读取子进程管道 → `scrub_line` → 追加写入日志文件。
/// 子进程退出后管道 EOF，泵自然结束。写入失败仅静默忽略（日志非关键路径，
/// 不得因磁盘问题反压拖垮数据面进程的 stdout 缓冲）。
async fn pump_redacted<R>(reader: R, mut file: std::fs::File)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use std::io::Write as _;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                // 去掉行尾换行后脱敏，再统一补写 \n。
                let trimmed = line.trim_end_matches(['\n', '\r']);
                let scrubbed = crate::observability::redactor::scrub_line(trimmed);
                let _ = writeln!(file, "{scrubbed}");
            }
        }
    }
}

/// 真实子进程句柄（tokio 实现）。
#[derive(Debug)]
pub struct TokioProcess {
    child: Option<tokio::process::Child>,
}

#[async_trait]
impl ProcessHandle for TokioProcess {
    fn pid(&self) -> u32 {
        self.child
            .as_ref()
            .and_then(|c| c.id())
            .expect("child already reaped")
    }

    fn terminate(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            // 真实 SIGTERM（§11.7 第 3 步：warp-svc 收到后自行优雅退出）。
            // 不引入 libc/nix 的 kill(2)：crate 有 `unsafe_code = "forbid"`（审计约束），
            // 且外部 `kill -TERM <pid>` 与 warp-cli 同款 Command::new + .arg 模式，
            // dev-base 镜像自带 procps。投递失败（如进程已退出）返回 Err，由调用方
            // 走 grace 超时 → SIGKILL 路径兜底（stop.rs 忽略 terminate 错误）。
            let Some(child) = self.child.as_ref() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "child already reaped",
                ));
            };
            let pid = child.id().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "child already reaped")
            })?;
            let status = std::process::Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status()?;
            if status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "kill -TERM {} exited with {status:?}",
                    pid
                )))
            }
        }
        #[cfg(not(unix))]
        {
            // Windows 开发机仅跑单元测试（Fake spawner）；真实进程路径只在
            // Linux 容器内使用，这里退化为 SIGKILL 语义以保持接口自洽。
            self.kill()
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        match self.child.as_mut() {
            Some(child) => child.start_kill(),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "child already reaped",
            )),
        }
    }

    async fn wait(&mut self) -> ProcessStatus {
        match self.child.take() {
            Some(mut child) => match child.wait().await {
                Ok(status) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        ProcessStatus {
                            exit_code: status.code().or_else(|| status.signal()),
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        ProcessStatus {
                            exit_code: status.code().or(Some(1)),
                        }
                    }
                }
                Err(_) => ProcessStatus { exit_code: None },
            },
            None => ProcessStatus { exit_code: Some(0) },
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ProcessStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(Some(ProcessStatus { exit_code: Some(0) }));
        };
        match child.try_wait()? {
            Some(status) => Ok(Some(status_to_process_status(status))),
            None => Ok(None),
        }
    }
}

/// 平台无关的 `ExitStatus` → `ProcessStatus` 转换（信号终止保留 exit_code=None）。
fn status_to_process_status(status: std::process::ExitStatus) -> ProcessStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ProcessStatus {
            exit_code: status.code().or_else(|| status.signal()),
        }
    }
    #[cfg(not(unix))]
    {
        ProcessStatus {
            exit_code: status.code(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 一个"会持续运行约 n 秒"的进程命令（跨平台：unix 用 sleep，windows 用 ping）。
    fn long_running(n: u32) -> SpawnCommand {
        #[cfg(unix)]
        {
            SpawnCommand::simple("sleep").with_args(vec![n.to_string()])
        }
        #[cfg(not(unix))]
        {
            SpawnCommand::simple("cmd")
                .with_args(vec!["/c".into(), format!("ping -n {} 127.0.0.1 >nul", n)])
        }
    }

    #[tokio::test]
    async fn real_spawner_launches_and_reaps() {
        let spawner = TokioProcessSpawner;
        let mut handle = spawner.spawn(&long_running(5)).unwrap();

        assert!(handle.pid() > 0);
        assert!(handle.try_wait().unwrap().is_none());

        handle.kill().unwrap();
        let status = handle.wait().await;
        // SIGKILL → 无退出码（None）或信号码（Linux signal()=9；shell 场景 137；
        // Windows 平台码 1）；仅断言进程已结束（reap 完成）。
        assert!(matches!(
            status.exit_code,
            None | Some(9) | Some(137) | Some(1)
        ));
    }

    #[tokio::test]
    async fn real_spawner_redirects_stderr_and_waits_exit() {
        let dir = std::env::temp_dir().join(format!("warpdeck-proc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("stderr.log");

        #[cfg(unix)]
        let cmd = SpawnCommand::simple("sh")
            .with_args(vec!["-c".into(), "echo boom 1>&2".into()])
            .with_stderr_log(log.clone());
        #[cfg(not(unix))]
        let cmd = SpawnCommand::simple("cmd")
            .with_args(vec!["/c".into(), "echo boom 1>&2".into()])
            .with_stderr_log(log.clone());

        let spawner = TokioProcessSpawner;
        let mut handle = spawner.spawn(&cmd).unwrap();
        let status = handle.wait().await;
        assert_eq!(status.exit_code, Some(0));
        let content = std::fs::read_to_string(&log).unwrap();
        assert!(content.contains("boom"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn real_spawner_merges_stdout_and_stderr_into_one_file() {
        let dir = std::env::temp_dir().join(format!("warpdeck-proc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("combined.log");

        #[cfg(unix)]
        let cmd = SpawnCommand::simple("sh")
            .with_args(vec!["-c".into(), "echo out && echo err 1>&2".into()])
            .with_stderr_log(log.clone())
            .with_stdout_log(log.clone());
        #[cfg(not(unix))]
        let cmd = SpawnCommand::simple("cmd")
            .with_args(vec!["/c".into(), "echo out && echo err 1>&2".into()])
            .with_stderr_log(log.clone())
            .with_stdout_log(log.clone());

        let spawner = TokioProcessSpawner;
        let mut handle = spawner.spawn(&cmd).unwrap();
        let status = handle.wait().await;
        assert_eq!(status.exit_code, Some(0));
        let content = std::fs::read_to_string(&log).unwrap();
        assert!(content.contains("out"), "stdout captured: {content}");
        assert!(content.contains("err"), "stderr captured: {content}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn merged_output_never_clobbers_stderr_bytes() {
        // 回归（review）：合并场景下 stdout 曾用 File::create（独立 offset），
        // stderr 用 O_APPEND——stdout 第二轮写入会落在 stderr 已写位置并覆盖。
        // 修复：合并时两个句柄都必须 append（写入原子性地交错追加）。
        // Windows 上由主句柄截断后双句柄 append（FILE_APPEND_DATA：每次写固定
        // 落在 EOF，父子句柄偏移不互踩）。
        let dir = std::env::temp_dir().join(format!("warpdeck-proc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("combined.log");

        #[cfg(unix)]
        // stdout 300×a → stderr 300×b → stdout 300×c：旧实现下 c 覆盖 b 全部字节。
        let script = "i=0; while [ $i -lt 300 ]; do printf a; i=$((i+1)); done; \
                      i=0; while [ $i -lt 300 ]; do printf b 1>&2; i=$((i+1)); done; \
                      i=0; while [ $i -lt 300 ]; do printf c; i=$((i+1)); done";
        #[cfg(unix)]
        let cmd = SpawnCommand::simple("sh")
            .with_args(vec!["-c".into(), script.into()])
            .with_stderr_log(log.clone())
            .with_stdout_log(log.clone());
        #[cfg(not(unix))]
        // 三连输出：err 插在两次 stdout 之间；旧实现下第二个 stdout 写覆盖 err。
        let cmd = SpawnCommand::simple("cmd")
            .with_args(vec![
                "/c".into(),
                "echo out1 && echo err 1>&2 && echo out2".into(),
            ])
            .with_stderr_log(log.clone())
            .with_stdout_log(log.clone());

        let spawner = TokioProcessSpawner;
        let mut handle = spawner.spawn(&cmd).unwrap();
        let status = handle.wait().await;
        assert_eq!(status.exit_code, Some(0), "脚本正常退出");
        let content = std::fs::read_to_string(&log).unwrap();
        #[cfg(unix)]
        {
            // 回归核心：三段各 300 字节必须全部完整落盘（旧实现下 c 段从 b 段
            // 起始处覆写，总长只剩 600 且 b 全丢）。不强制段序——sh 对重定向
            // stdout 的缓冲策略（dash 逐写 / bash 块缓冲）会改变段间顺序，
            // 与「是否覆盖」正交。
            // P1 审查 R3#7：输出经行级脱敏泵落盘——无换行的三段在 EOF 时合并为
            // 一行并补写 \n，故总长 900+1；三段内容必须完整（防覆盖语义不变）。
            assert_eq!(content.len(), 901, "总字节数（防覆盖，含补写换行）: {content}");
            assert!(content.contains(&"a".repeat(300)), "stdout 首段: {content}");
            assert!(
                content.contains(&"b".repeat(300)),
                "stderr 不得被后续 stdout 覆盖: {content}"
            );
            assert!(content.contains(&"c".repeat(300)), "stdout 末段: {content}");
        }
        #[cfg(not(unix))]
        {
            assert!(content.contains("out1"), "stdout 首批写入: {content}");
            assert!(
                content.contains("err"),
                "stderr 不得被后续 stdout 覆盖（双 append 修复）: {content}"
            );
            assert!(content.contains("out2"), "stdout 末批写入: {content}");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn try_wait_polls_running_child_without_blocking() {
        let spawner = TokioProcessSpawner;
        let mut handle = spawner.spawn(&long_running(30)).unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        if let Some(status) = handle.try_wait().unwrap() {
            panic!("child exited too early: {status:?}");
        }
        tokio::time::sleep(deadline - tokio::time::Instant::now()).await;
        assert!(
            handle.try_wait().unwrap().is_none(),
            "child still running past deadline is fine"
        );
        handle.kill().unwrap();
        let _ = handle.wait().await;
    }
}
