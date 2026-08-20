//! warp-cli 适配器（P2-007）。
//!
//! 设计约束（AGENTS.md / 计划 P2-007）：
//! - 命令一律 `Command::new` + `.arg`，禁止 shell 拼接；
//! - 每个命令有超时；捕获 stderr 作为 summary；typed errors；
//! - 执行 `warp-cli` 时必须注入与 `warp-svc` 相同的 `RUNTIME_DIRECTORY` /
//!   `DBUS_SYSTEM_BUS_ADDRESS`（DESIGN §11.3），避免连接错误实例；
//! - 通过 `CommandExecutor` trait 隔离真实子进程，测试注入 Fake。
//! - 账号凭据命令（v0.2）：WARP+ = `registration license <KEY>`；
//!   Zero Trust = mdm.xml 自动注册（service token，warp-svc 启动即完成；
//!   `teams-enroll` 是交互式 OAuth，headless 容器不可用，故不再使用）。

use std::time::Duration;

use async_trait::async_trait;

use super::context::InstanceContext;
use super::control::{WarpCliStatus, WarpControl, WarpControlError};
use super::credentials::InstanceCredentials;
use super::instance::InternalProxyPort;
use super::process::SpawnCommand;

/// warp-cli 单条命令默认超时。
pub const CLI_TIMEOUT: Duration = Duration::from_secs(10);

/// 命令执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

/// 命令执行错误（与子进程内容无关的 I/O / 超时）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandExecutorError {
    #[error("command spawn failed: {0}")]
    SpawnFailed(String),
    #[error("command timed out after {0:?}")]
    TimedOut(Duration),
}

/// 命令执行器：真实子进程与测试替身的边界。
#[async_trait]
pub trait CommandExecutor: Send + Sync {
    async fn run(
        &self,
        cmd: &SpawnCommand,
        timeout: Duration,
    ) -> Result<CommandOutput, CommandExecutorError>;
}

/// 真实实现：`tokio::process::Command`，捕获 stdout/stderr。
pub struct RealCommandExecutor;

#[async_trait]
impl CommandExecutor for RealCommandExecutor {
    async fn run(
        &self,
        cmd: &SpawnCommand,
        timeout: Duration,
    ) -> Result<CommandOutput, CommandExecutorError> {
        let mut child = tokio::process::Command::new(&cmd.program)
            .args(&cmd.args)
            .envs(cmd.envs.iter().map(|(k, v)| (k, v)))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| CommandExecutorError::SpawnFailed(e.to_string()))?;

        let child_ref = &mut child;
        let result = tokio::time::timeout(timeout, async {
            let stdout = read_pipe(child_ref.stdout.take());
            let stderr = read_pipe(child_ref.stderr.take());
            let (stdout, stderr) = tokio::join!(stdout, stderr);
            let status = child_ref.wait().await;
            (stdout, stderr, status)
        })
        .await;

        match result {
            Ok((stdout, stderr, status)) => Ok(CommandOutput {
                stdout,
                stderr,
                exit_code: status.ok().and_then(|s| s.code()),
            }),
            Err(_) => {
                let _ = child.kill().await;
                Err(CommandExecutorError::TimedOut(timeout))
            }
        }
    }
}

async fn read_pipe<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> String {
    use tokio::io::AsyncReadExt;

    match pipe {
        Some(mut p) => {
            let mut buf = String::new();
            let _ = p.read_to_string(&mut buf).await;
            buf
        }
        None => String::new(),
    }
}

/// 从 `warp-cli status` 输出解析连接状态（启发式，保留 raw）。
pub fn parse_status(stdout: &str) -> WarpCliStatus {
    let hay = stdout.to_ascii_lowercase();
    if hay.contains("disconnected") {
        return WarpCliStatus {
            connected: false,
            raw_status: stdout.to_string(),
        };
    }
    if hay.contains("connected") {
        return WarpCliStatus {
            connected: true,
            raw_status: stdout.to_string(),
        };
    }
    WarpCliStatus {
        connected: false,
        raw_status: stdout.to_string(),
    }
}

/// 生产适配器：`RealCommandExecutor` + 默认超时。
pub struct RealWarpControl {
    executor: Box<dyn CommandExecutor>,
}

impl RealWarpControl {
    /// 自定义执行器（测试注入 Fake）。
    pub fn new(executor: Box<dyn CommandExecutor>) -> Self {
        Self { executor }
    }

    /// 生产构造：真实子进程。
    pub fn real() -> Self {
        Self::new(Box::new(RealCommandExecutor))
    }

    fn cli_command(
        &self,
        ctx: &InstanceContext,
        subcommand: &str,
        extra_args: &[String],
    ) -> SpawnCommand {
        let mut args = vec!["--accept-tos".to_string(), subcommand.to_string()];
        args.extend_from_slice(extra_args);
        SpawnCommand {
            program: "warp-cli".to_string(),
            args,
            envs: vec![
                (
                    "RUNTIME_DIRECTORY".to_string(),
                    ctx.paths.runtime_dir.display().to_string(),
                ),
                (
                    "DBUS_SYSTEM_BUS_ADDRESS".to_string(),
                    ctx.paths.dbus_system_bus_address(),
                ),
            ],
            stderr_log_path: None,
            stdout_log_path: None,
        }
    }

    async fn run_cli(
        &self,
        ctx: &InstanceContext,
        subcommand: &str,
        extra_args: &[String],
    ) -> Result<CommandOutput, WarpControlError> {
        let cmd = self.cli_command(ctx, subcommand, extra_args);
        match self.executor.run(&cmd, CLI_TIMEOUT).await {
            Ok(out) => Ok(out),
            Err(CommandExecutorError::TimedOut(_)) => Err(WarpControlError::CommandTimeout),
            Err(CommandExecutorError::SpawnFailed(msg)) => {
                Err(WarpControlError::CommandFailed { summary: msg })
            }
        }
    }
}

#[async_trait]
impl WarpControl for RealWarpControl {
    async fn status(&self, ctx: &InstanceContext) -> Result<WarpCliStatus, WarpControlError> {
        let out = self.run_cli(ctx, "status", &[]).await?;
        if out.exit_code != Some(0) {
            return Err(WarpControlError::CommandFailed {
                summary: summarize(&out),
            });
        }
        Ok(parse_status(&out.stdout))
    }

    async fn apply_account(
        &self,
        ctx: &InstanceContext,
        credentials: &InstanceCredentials,
    ) -> Result<(), WarpControlError> {
        match credentials.mode {
            // license 属于 secret，但只在子进程 argv 中出现，与 stderr 捕获
            // 路径隔离：`summarize` 不打印 argv（redactor 兜底，AGENTS.md）。
            super::credentials::CredentialMode::Free => return Ok(()),
            super::credentials::CredentialMode::WarpPlus => {
                let license =
                    credentials
                        .license
                        .as_deref()
                        .ok_or(WarpControlError::CommandFailed {
                            summary: "warp_plus profile missing license secret".into(),
                        })?;
                let out = self
                    .run_cli(
                        ctx,
                        "registration",
                        &["license".to_string(), license.to_string()],
                    )
                    .await?;
                if out.exit_code != Some(0) {
                    return Err(WarpControlError::CommandFailed {
                        summary: format!("license apply failed: {}", summarize(&out)),
                    });
                }
            }
            super::credentials::CredentialMode::ZeroTrust => {
                // 注册由 mdm.xml（service token）在 warp-svc 启动时自动完成；
                // `teams-enroll` 为交互式 OAuth，headless 容器不可用，且重复
                // 注册会破坏 service-token 注册。这里必须 no-op。
                return Ok(());
            }
        }
        Ok(())
    }

    async fn register(&self, ctx: &InstanceContext) -> Result<(), WarpControlError> {
        let out = self
            .run_cli(ctx, "registration", &["new".to_string()])
            .await?;
        if out.exit_code != Some(0) {
            return Err(WarpControlError::CommandFailed {
                summary: summarize(&out),
            });
        }
        Ok(())
    }

    async fn set_proxy_mode(&self, ctx: &InstanceContext) -> Result<(), WarpControlError> {
        let out = self.run_cli(ctx, "mode", &["proxy".to_string()]).await?;
        if out.exit_code != Some(0) {
            return Err(WarpControlError::CommandFailed {
                summary: summarize(&out),
            });
        }
        Ok(())
    }

    async fn set_proxy_port(
        &self,
        ctx: &InstanceContext,
        port: InternalProxyPort,
    ) -> Result<(), WarpControlError> {
        // 官方 CLI 子命令是 `warp-cli proxy port <port>`（`proxy` 下挂 `port`）；不存在
        // 顶层 `set-proxy-port`（2026.6.880.0 实测报 unrecognized subcommand）。
        let out = self
            .run_cli(
                ctx,
                "proxy",
                &["port".to_string(), port.as_u16().to_string()],
            )
            .await?;
        if out.exit_code != Some(0) {
            return Err(WarpControlError::CommandFailed {
                summary: summarize(&out),
            });
        }
        Ok(())
    }

    async fn connect(&self, ctx: &InstanceContext) -> Result<(), WarpControlError> {
        let out = self.run_cli(ctx, "connect", &[]).await?;
        if out.exit_code != Some(0) {
            return Err(WarpControlError::ConnectFailure {
                summary: summarize(&out),
            });
        }
        Ok(())
    }

    async fn disconnect(&self, ctx: &InstanceContext) -> Result<(), WarpControlError> {
        let out = self.run_cli(ctx, "disconnect", &[]).await?;
        if out.exit_code != Some(0) {
            return Err(WarpControlError::CommandFailed {
                summary: summarize(&out),
            });
        }
        Ok(())
    }
}

fn summarize(out: &CommandOutput) -> String {
    let stderr = out.stderr.trim();
    let stdout = out.stdout.trim();
    if !stderr.is_empty() {
        stderr.to_string()
    } else if !stdout.is_empty() {
        stdout.to_string()
    } else {
        format!("exit {:?}", out.exit_code)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex;

    use super::*;
    use crate::runtime::instance::InstanceId;

    fn ctx(id: i64) -> InstanceContext {
        InstanceContext::new(
            std::path::Path::new("/var/lib/warpdeck"),
            std::path::Path::new("/run/warpdeck"),
            InstanceId::from_db(id).unwrap(),
        )
        .unwrap()
    }

    struct ScriptedExecutor {
        calls: Mutex<Vec<SpawnCommand>>,
        results: Mutex<VecDeque<Result<CommandOutput, CommandExecutorError>>>,
    }

    impl ScriptedExecutor {
        fn new(results: Vec<Result<CommandOutput, CommandExecutorError>>) -> Self {
            Self {
                calls: Mutex::new(vec![]),
                results: Mutex::new(results.into()),
            }
        }
        fn calls(&self) -> Vec<SpawnCommand> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommandExecutor for ScriptedExecutor {
        async fn run(
            &self,
            cmd: &SpawnCommand,
            _timeout: Duration,
        ) -> Result<CommandOutput, CommandExecutorError> {
            self.calls.lock().unwrap().push(cmd.clone());
            self.results.lock().unwrap().pop_front().unwrap()
        }
    }

    #[async_trait]
    impl CommandExecutor for Arc<ScriptedExecutor> {
        async fn run(
            &self,
            cmd: &SpawnCommand,
            timeout: Duration,
        ) -> Result<CommandOutput, CommandExecutorError> {
            (**self).run(cmd, timeout).await
        }
    }

    fn ok_out(stdout: &str) -> Result<CommandOutput, CommandExecutorError> {
        Ok(CommandOutput {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }

    fn failed_out(stderr: &str, code: i32) -> Result<CommandOutput, CommandExecutorError> {
        Ok(CommandOutput {
            stdout: String::new(),
            stderr: stderr.to_string(),
            exit_code: Some(code),
        })
    }

    #[tokio::test]
    async fn status_parses_connected() {
        let exec = Arc::new(ScriptedExecutor::new(vec![ok_out("Status: Connected")]));
        let warp = RealWarpControl::new(Box::new(exec.clone()));

        let status = warp.status(&ctx(0)).await.unwrap();
        assert!(status.connected);
        assert_eq!(status.raw_status, "Status: Connected");
        assert!(status_invokes_proper_command(&exec.calls()));
    }

    #[tokio::test]
    async fn status_parses_disconnected() {
        let exec = Arc::new(ScriptedExecutor::new(vec![ok_out("Status: Disconnected")]));
        let warp = RealWarpControl::new(Box::new(exec.clone()));

        let status = warp.status(&ctx(0)).await.unwrap();
        assert!(!status.connected);
    }

    #[tokio::test]
    async fn commands_carry_isolated_runtime_and_bus_env() {
        let exec = Arc::new(ScriptedExecutor::new(vec![ok_out("ok"), ok_out("ok")]));
        let warp = RealWarpControl::new(Box::new(exec.clone()));
        let c = ctx(3);

        warp.register(&c).await.unwrap();
        warp.set_proxy_port(&c, crate::runtime::instance::instance_port(c.id).unwrap())
            .await
            .unwrap();

        for call in exec.calls() {
            assert_eq!(call.program, "warp-cli");
            assert_eq!(call.args[0], "--accept-tos");
            assert!(call.envs.contains(&(
                "RUNTIME_DIRECTORY".to_string(),
                c.paths.runtime_dir.display().to_string(),
            )));
            assert!(call.envs.contains(&(
                "DBUS_SYSTEM_BUS_ADDRESS".to_string(),
                c.paths.dbus_system_bus_address(),
            )));
        }
        let calls = exec.calls();
        assert_eq!(calls[0].args, vec!["--accept-tos", "registration", "new"]);
        assert_eq!(
            calls[1].args,
            vec!["--accept-tos", "proxy", "port", "40003"]
        );
    }

    fn status_invokes_proper_command(calls: &[SpawnCommand]) -> bool {
        calls.len() == 1 && calls[0].args.contains(&"status".to_string())
    }

    #[tokio::test]
    async fn timeout_maps_to_command_timeout() {
        let exec = Arc::new(ScriptedExecutor::new(vec![Err(
            CommandExecutorError::TimedOut(CLI_TIMEOUT),
        )]));
        let warp = RealWarpControl::new(Box::new(exec.clone()));

        assert!(matches!(
            warp.status(&ctx(0)).await,
            Err(WarpControlError::CommandTimeout)
        ));
    }

    #[tokio::test]
    async fn nonzero_exit_maps_to_typed_errors_with_stderr_summary() {
        let exec = Arc::new(ScriptedExecutor::new(vec![
            failed_out("license required", 1),
            failed_out("connect: refused", 2),
        ]));
        let warp = RealWarpControl::new(Box::new(exec.clone()));
        let c = ctx(0);

        assert!(matches!(
            warp.register(&c).await,
            Err(WarpControlError::CommandFailed { summary }) if summary == "license required"
        ));
        assert!(matches!(
            warp.connect(&c).await,
            Err(WarpControlError::ConnectFailure { summary }) if summary == "connect: refused"
        ));
    }

    #[tokio::test]
    async fn parse_status_is_case_insensitive_and_safe() {
        let cases = [
            ("Connected", true),
            ("connected", true),
            ("Disconnected", false),
            ("disconnected after restart", false),
            ("WoRrIrEe", false),
        ];
        for (text, expected) in cases {
            assert_eq!(parse_status(text).connected, expected, "for {text:?}");
        }
        let raw = parse_status("Status: Connected");
        assert_eq!(raw.raw_status, "Status: Connected");
    }

    #[tokio::test]
    async fn disconnect_is_idempotent_command() {
        let exec = Arc::new(ScriptedExecutor::new(vec![ok_out("")]));
        let warp = RealWarpControl::new(Box::new(exec.clone()));
        warp.disconnect(&ctx(0)).await.unwrap();
        assert_eq!(exec.calls()[0].args[1], "disconnect");
    }

    #[tokio::test]
    async fn zero_trust_apply_account_is_noop_without_cli_invocation() {
        // ZeroTrust 注册由 mdm.xml（service token）在 warp-svc 启动时自动完成；
        // apply_account 不得派生任何 warp-cli 命令（teams-enroll 无法 headless）。
        use crate::runtime::credentials::CredentialMode;

        let exec = Arc::new(ScriptedExecutor::new(vec![]));
        let warp = RealWarpControl::new(Box::new(exec.clone()));
        let creds = InstanceCredentials {
            mode: CredentialMode::ZeroTrust,
            zero_trust_org: Some("team-x".into()),
            zt_client_id: Some("cid.access".into()),
            zt_client_secret: Some("csec".into()),
            ..InstanceCredentials::free()
        };
        warp.apply_account(&ctx(1), &creds).await.unwrap();
        assert!(
            exec.calls().is_empty(),
            "ZeroTrust apply_account 必须 zero CLI 调用"
        );
    }
}
