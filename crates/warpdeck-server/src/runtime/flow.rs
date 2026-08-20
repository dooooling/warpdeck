//! 实例启动流程（P2-009 Registration Flow）。
//!
//! 设计约束：
//! - DESIGN §11.5：`reg.json` 已存在则**不重复** `registration new`；
//!   不存在才执行注册，失败按 §11.6 backoff bounded 重试。
//! - ZeroTrust（v0.2）：注册由 `mdm.xml`（service token）在 warp-svc 启动后**异步**
//!   完成（E2E-08 实测约 3s；期间 `warp-cli connect` 报 `MissingRegistration`）。
//!   账号与 mode/代理端口同样由 mdm.xml 自动驱动，本流程**跳过**注册/
//!   `apply_account`/`mode proxy`/`proxy port`（headless 无限眼手段；managed
//!   账号禁止 CLI 改端口）。
//! - 注册完成后按序执行幂等配置：mode proxy → 内部代理端口 → connect →
//!   status 轮询验证（§25.7 就绪判据：connect 成功不算数，必须等数据面
//!   connected；`warp-cli connect` 是异步命令，daemon 需数秒完成握手）。
//! - ZeroTrust 的 connect 在注册完成前会瞬时失败（MissingRegistration）：按
//!   `ZT_REGISTRATION_WAIT_TIMEOUT` 预算有界重试，避免启动竞态误判为失败。
//! - 流程失败必须显式返回流程错误；HTTP/API 层不得把失败伪造成成功。

use std::sync::Arc;
use std::time::Duration;

use super::backoff::BackoffPolicy;
use super::clock::Clock;
use super::context::InstanceContext;
use super::control::{WarpControl, WarpControlError};
use super::credentials::InstanceCredentials;

/// warp-svc 的注册数据文件；存在即认为已注册（§11.5）。
pub const REGISTRATION_FILE: &str = "reg.json";

/// connect 后 status 轮询间隔。
pub const VERIFY_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// connect 后 status 轮询总超时（首次连接 QUIC+PQ 握手可能较慢）。
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(90);

/// ZeroTrust：mdm（service token）注册完成前重试 connect 的间隔。
/// 注册在 warp-svc 启动后异步完成（E2E-08 实测约 3s），轮询不必太快。
pub const ZT_CONNECT_RETRY_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// 等待 mdm 异步注册完成的总预算；超时按 ConnectFailed 上浮（bounded，
/// 绝不让单次启动流程无限重试）。
pub const ZT_REGISTRATION_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

/// 启动流程失败类别（给上层/审计用，不做堆栈级错误透传）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowError {
    /// 注册阶段：始终失败（attempts 已耗尽）。
    RegisterFailed { attempts: u32, last_error: String },
    /// 配置阶段（mode proxy / 内部端口）失败。
    ConfigureFailed { summary: String },
    /// connect 失败。
    ConnectFailed { summary: String },
    /// connect 执行成功但 status 验证未达 connected。
    VerifyFailed { raw_status: String },
}

/// 流程结果：本次是否执行了新注册、注册尝试次数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationOutcome {
    pub was_registered: bool,
    /// 实际注册尝试次数（已注册时恒为 0）。
    pub register_attempts: u32,
}

/// 启动流程：注册（按需）+ 配置 + 连接 + 验证。
pub struct RegistrationFlow {
    control: Arc<dyn WarpControl>,
    clock: Arc<dyn Clock>,
    backoff: Box<dyn BackoffPolicy>,
    max_register_attempts: u32,
}

impl RegistrationFlow {
    pub fn new(
        control: Arc<dyn WarpControl>,
        clock: Arc<dyn Clock>,
        backoff: Box<dyn BackoffPolicy>,
        max_register_attempts: u32,
    ) -> Self {
        assert!(
            max_register_attempts >= 1,
            "max_register_attempts must be >= 1"
        );
        Self {
            control,
            clock,
            backoff,
            max_register_attempts,
        }
    }

    /// 执行完整启动流程。
    pub async fn run(
        &self,
        ctx: &InstanceContext,
        credentials: &InstanceCredentials,
    ) -> Result<RegistrationOutcome, FlowError> {
        // v0.2 §11.2 / mdm.rs：ZeroTrust 由 mdm.xml（service token）在 warp-svc
        // 启动即自动完成注册、账号配置与 mode/代理端口（Teams managed 账号禁止
        // CLI 改这两项）——**不**执行 `registration new`、`teams-enroll` 或
        // `mode/proxy port`。其余模式：按需注册 + 账密应用 + CLI 配置。
        let is_zero_trust = credentials.mode == super::credentials::CredentialMode::ZeroTrust;
        let outcome = if is_zero_trust {
            RegistrationOutcome {
                was_registered: false,
                register_attempts: 0,
            }
        } else {
            self.ensure_registered(ctx).await?
        };

        if !is_zero_trust {
            // free 为 no-op；warp_plus 应用 license；失败上浮。
            self.control
                .apply_account(ctx, credentials)
                .await
                .map_err(|e| FlowError::ConfigureFailed {
                    summary: e.to_string(),
                })?;
            self.control
                .set_proxy_mode(ctx)
                .await
                .map_err(|e| FlowError::ConfigureFailed {
                    summary: e.to_string(),
                })?;
            self.control
                .set_proxy_port(ctx, ctx.internal_proxy_port)
                .await
                .map_err(|e| FlowError::ConfigureFailed {
                    summary: e.to_string(),
                })?;
        }
        // ZeroTrust 的 mdm 注册在 warp-svc 启动后异步完成：注册完成前 connect
        // 瞬时失败（MissingRegistration），按预算有界重试（E2E-08 实测）。
        self.connect(ctx, is_zero_trust).await?;

        // connect 是异步命令：daemon 建立连接需要时间，轮询 status 直到
        // connected 或超时（§25.7 就绪判据）。
        let deadline = self.clock.now() + VERIFY_TIMEOUT;
        let mut status = self
            .control
            .status(ctx)
            .await
            .map_err(|e| FlowError::VerifyFailed {
                raw_status: e.to_string(),
            })?;
        loop {
            if status.connected {
                break;
            }
            if self.clock.now() >= deadline {
                return Err(FlowError::VerifyFailed {
                    raw_status: status.raw_status,
                });
            }
            self.clock.sleep(VERIFY_POLL_INTERVAL).await;
            status = self
                .control
                .status(ctx)
                .await
                .map_err(|e| FlowError::VerifyFailed {
                    raw_status: e.to_string(),
                })?;
        }

        Ok(outcome)
    }

    /// DESIGN §11.5：已有 reg.json 直接跳过注册；否则 registration new +
    /// bounded backoff 重试。
    async fn ensure_registered(
        &self,
        ctx: &InstanceContext,
    ) -> Result<RegistrationOutcome, FlowError> {
        let reg_file = ctx.paths.state_dir.join(REGISTRATION_FILE);
        if tokio::fs::try_exists(&reg_file).await.unwrap_or(false) {
            return Ok(RegistrationOutcome {
                was_registered: false,
                register_attempts: 0,
            });
        }

        let mut last_error = String::new();
        let mut attempts = 0;
        for attempt in 1..=self.max_register_attempts {
            attempts = attempt;
            match self.control.register(ctx).await {
                Ok(()) => {
                    return Ok(RegistrationOutcome {
                        was_registered: true,
                        register_attempts: attempts,
                    });
                }
                Err(e) => last_error = e.to_string(),
            }

            if attempt < self.max_register_attempts {
                let delay = self.backoff.delay_for(attempt);
                self.clock.sleep(delay).await;
            }
        }

        Err(FlowError::RegisterFailed {
            attempts,
            last_error,
        })
    }

    /// 发出 connect（§25.7 就绪判据的第一步）。
    ///
    /// `retry_missing_registration`（ZeroTrust）：mdm 注册在 warp-svc 启动后异步
    /// 完成，注册前 `warp-cli connect` 报 `MissingRegistration`。仅对该失败签名在
    /// `ZT_REGISTRATION_WAIT_TIMEOUT` 预算内按 `ZT_CONNECT_RETRY_POLL_INTERVAL`
    /// 重试；非该签名（真实失败）立即上浮。
    async fn connect(
        &self,
        ctx: &InstanceContext,
        retry_missing_registration: bool,
    ) -> Result<(), FlowError> {
        if !retry_missing_registration {
            self.control
                .connect(ctx)
                .await
                .map_err(|e| FlowError::ConnectFailed {
                    summary: e.to_string(),
                })?;
            return Ok(());
        }

        let deadline = self.clock.now() + ZT_REGISTRATION_WAIT_TIMEOUT;
        loop {
            match self.control.connect(ctx).await {
                Ok(()) => return Ok(()),
                Err(WarpControlError::ConnectFailure { summary })
                    if is_missing_registration(&summary) =>
                {
                    if self.clock.now() >= deadline {
                        return Err(FlowError::ConnectFailed { summary });
                    }
                    self.clock.sleep(ZT_CONNECT_RETRY_POLL_INTERVAL).await;
                }
                Err(e) => {
                    return Err(FlowError::ConnectFailed {
                        summary: e.to_string(),
                    });
                }
            }
        }
    }
}

/// `warp-cli connect` 在注册完成前的失败签名（daemon IPC 实测：
/// "Failed to connect err=MissingRegistration"）。只有该签名才触发 ZeroTrust
/// 重试；其余 connect 错误一律立即上浮。
fn is_missing_registration(summary: &str) -> bool {
    let hay = summary.to_ascii_lowercase();
    hay.contains("missingregistration")
        || (hay.contains("no registration") && hay.contains("found"))
        || hay.contains("not registered")
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::runtime::backoff::ExponentialBackoff;
    use crate::runtime::context::InstanceContext;
    use crate::runtime::control::WarpControlError;
    use crate::runtime::credentials::{CredentialMode, InstanceCredentials};
    use crate::runtime::fake::{FakeWarpControl, ManualClock};
    use crate::runtime::instance::InstanceId;

    struct TempState {
        _dir: tempfile::TempDir,
    }

    fn ctx_with_state(reg_exists: bool) -> (InstanceContext, TempState) {
        let dir = tempfile::TempDir::new().unwrap();
        let ctx = InstanceContext::new(
            dir.path(),
            Path::new("/run/warpdeck"),
            InstanceId::from_db(1).unwrap(),
        )
        .unwrap();
        if reg_exists {
            std::fs::create_dir_all(&ctx.paths.state_dir).unwrap();
            std::fs::write(ctx.paths.state_dir.join(REGISTRATION_FILE), b"{}").unwrap();
        }
        (ctx, TempState { _dir: dir })
    }

    fn flow(control: Arc<dyn WarpControl>, clock: Arc<dyn Clock>) -> RegistrationFlow {
        RegistrationFlow::new(
            control,
            clock,
            Box::new(ExponentialBackoff::new(
                Duration::from_millis(100),
                2,
                Duration::from_secs(10),
            )),
            5,
        )
    }

    /// ZeroTrust 凭据（mdm service token 场景）。
    fn zt_creds() -> InstanceCredentials {
        InstanceCredentials {
            mode: CredentialMode::ZeroTrust,
            zero_trust_org: Some("acme-corp".into()),
            zt_client_id: Some("cid.access".into()),
            zt_client_secret: Some("csec".into()),
            ..InstanceCredentials::free()
        }
    }

    #[tokio::test]
    async fn already_registered_skips_registration_command() {
        let fake = Arc::new(FakeWarpControl::new());
        // reg.json 已存在等价于 warp-svc 已完成注册（§11.5）。
        fake.set_registered(true);
        fake.set_connected(true);
        let (ctx, _keep) = ctx_with_state(true);

        let clock = Arc::new(ManualClock::new());
        let outcome = flow(fake.clone(), clock.clone())
            .run(&ctx, &InstanceCredentials::free())
            .await
            .unwrap();

        assert_eq!(
            outcome,
            RegistrationOutcome {
                was_registered: false,
                register_attempts: 0,
            }
        );
        // 关键：reg.json 存在时绝不 invoke `registration new`（§11.5）。
        assert_eq!(fake.register_calls(), 0);
        assert!(fake.is_proxy_mode());
        assert_eq!(fake.proxy_port(), Some(ctx.internal_proxy_port));
        assert!(fake.is_connected());
        assert!(clock.slept().is_empty());
    }

    #[tokio::test]
    async fn fresh_instance_registers_then_configures_and_connects() {
        let fake = Arc::new(FakeWarpControl::new());
        fake.set_connected(true);
        let (ctx, _keep) = ctx_with_state(false);

        let clock = Arc::new(ManualClock::new());
        let outcome = flow(fake.clone(), clock.clone())
            .run(&ctx, &InstanceCredentials::free())
            .await
            .unwrap();

        assert_eq!(
            outcome,
            RegistrationOutcome {
                was_registered: true,
                register_attempts: 1,
            }
        );
        assert!(fake.is_registered());
        assert!(fake.is_proxy_mode());
        assert_eq!(fake.proxy_port(), Some(ctx.internal_proxy_port));
        assert!(fake.is_connected());
        assert!(clock.slept().is_empty());
    }

    #[tokio::test]
    async fn registration_retries_with_backoff_then_succeeds() {
        let fake = Arc::new(FakeWarpControl::new());
        fake.set_connected(true);
        fake.fail_next(WarpControlError::CommandTimeout);
        fake.fail_next(WarpControlError::CommandTimeout);
        let (ctx, _keep) = ctx_with_state(false);

        let clock = Arc::new(ManualClock::new());
        let outcome = flow(fake.clone(), clock.clone())
            .run(&ctx, &InstanceCredentials::free())
            .await
            .unwrap();

        assert_eq!(
            outcome,
            RegistrationOutcome {
                was_registered: true,
                register_attempts: 3,
            }
        );
        // 失败 2 次 → 重试等待 100ms 与 200ms（指数），之后成功。
        assert_eq!(
            clock.slept(),
            vec![Duration::from_millis(100), Duration::from_millis(200)]
        );
        assert!(fake.is_connected());
    }

    #[tokio::test]
    async fn registration_exhausts_attempts_and_fails() {
        let fake = Arc::new(FakeWarpControl::new());
        for _ in 0..5 {
            fake.fail_next(WarpControlError::CommandTimeout);
        }
        let (ctx, _keep) = ctx_with_state(false);

        let clock = Arc::new(ManualClock::new());
        let err = flow(fake.clone(), clock.clone())
            .run(&ctx, &InstanceCredentials::free())
            .await
            .unwrap_err();

        assert!(
            matches!(err, FlowError::RegisterFailed { attempts: 5, .. }),
            "unexpected: {err:?}"
        );
        assert!(!fake.is_registered());
        assert!(!fake.is_connected());
        assert_eq!(clock.slept().len(), 4);
        // 等退避后仍失败，不会触发后续配置步骤。
        assert!(!fake.is_proxy_mode());
        assert_eq!(fake.proxy_port(), None);
    }

    #[tokio::test]
    async fn connect_failure_surfaces_as_flow_error() {
        let fake = Arc::new(FakeWarpControl::new());
        // 已注册：mode / port 各成功，connect 第一次调用直接失败。
        fake.set_registered(true);
        fake.set_connected(true);
        fake.fail_connect(WarpControlError::CommandTimeout);
        let (ctx, _keep) = ctx_with_state(true);

        let err = flow(fake.clone(), Arc::new(ManualClock::new()))
            .run(&ctx, &InstanceCredentials::free())
            .await
            .unwrap_err();

        assert!(
            matches!(err, FlowError::ConnectFailed { .. }),
            "unexpected: {err:?}"
        );
    }

    #[tokio::test]
    async fn connect_ok_but_status_not_connected_fails_verification() {
        let fake = Arc::new(FakeWarpControl::new());
        // connect 命令执行成功，但 WARP 数据面一直未连接（license 无效等）。
        fake.set_registered(true);
        fake.set_connected(true);
        fake.set_status_override(Some(false));
        let (ctx, _keep) = ctx_with_state(true);

        let clock = Arc::new(ManualClock::new());
        let err = flow(fake.clone(), clock.clone())
            .run(&ctx, &InstanceCredentials::free())
            .await
            .unwrap_err();

        assert!(
            matches!(err, FlowError::VerifyFailed { .. }),
            "unexpected: {err:?}"
        );
        // 轮询持续到超时（虚拟时间），期间按间隔 sleep。
        let total: Duration = clock.slept().iter().sum();
        assert!(total >= VERIFY_TIMEOUT, "polled {total:?} < timeout");
    }

    #[tokio::test]
    async fn verify_polls_until_connected_after_async_connect() {
        let fake = Arc::new(FakeWarpControl::new());
        fake.set_registered(true);
        fake.set_connected(true);
        // connect 是异步命令：前 3 次 status 仍报告未连接（握手进行中）。
        fake.set_status_pending(3);
        let (ctx, _keep) = ctx_with_state(true);

        let clock = Arc::new(ManualClock::new());
        flow(fake.clone(), clock.clone())
            .run(&ctx, &InstanceCredentials::free())
            .await
            .unwrap();

        // 前 3 次轮询各自等待一个间隔后第 4 次 status 达到 connected。
        assert_eq!(clock.slept(), vec![VERIFY_POLL_INTERVAL; 3]);
    }

    #[tokio::test]
    async fn flow_verifies_connected_state_from_real_status() {
        let fake = Arc::new(FakeWarpControl::new());
        fake.set_registered(true);
        fake.set_connected(true);
        let (ctx, _keep) = ctx_with_state(true);

        flow(fake, Arc::new(ManualClock::new()))
            .run(&ctx, &InstanceCredentials::free())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn free_credentials_never_invoke_apply_account() {
        let fake = Arc::new(FakeWarpControl::new());
        fake.set_registered(true);
        fake.set_connected(true);
        let (ctx, _keep) = ctx_with_state(true);

        flow(fake.clone(), Arc::new(ManualClock::new()))
            .run(&ctx, &InstanceCredentials::free())
            .await
            .unwrap();

        assert_eq!(fake.apply_account_calls(), 0, "free 档必须 no-op");
    }

    #[tokio::test]
    async fn warp_plus_credentials_drive_apply_account_license() {
        let fake = Arc::new(FakeWarpControl::new());
        fake.set_registered(true);
        fake.set_connected(true);
        let (ctx, _keep) = ctx_with_state(true);

        let creds = InstanceCredentials {
            mode: CredentialMode::WarpPlus,
            license: Some("WPL-XYZ".into()),
            ..InstanceCredentials::free()
        };
        flow(fake.clone(), Arc::new(ManualClock::new()))
            .run(&ctx, &creds)
            .await
            .unwrap();

        let calls = fake.applied_credentials().unwrap();
        assert_eq!(calls.mode, CredentialMode::WarpPlus);
        assert_eq!(calls.license.as_deref(), Some("WPL-XYZ"));
    }

    #[tokio::test]
    async fn zero_trust_skips_registration_apply_account_and_mode_port_but_connects() {
        // 注册/账号/mode/代理端口全部由 mdm.xml（service token）在 warp-svc
        // 启动时自动完成（managed 账号禁止 CLI 改端口）：flow 不得执行
        // registration new / apply_account / mode proxy / proxy port，但 connect
        // 与 status 验证照常。
        let fake = Arc::new(FakeWarpControl::new());
        fake.set_registered(true); // 模拟 mdm 已自动注册
        fake.set_connected(true);
        let (ctx, _keep) = ctx_with_state(false); // reg.json 不存在也不触发注册

        let creds = InstanceCredentials {
            mode: CredentialMode::ZeroTrust,
            zero_trust_org: Some("acme-corp".into()),
            zt_client_id: Some("cid.access".into()),
            zt_client_secret: Some("csec".into()),
            ..InstanceCredentials::free()
        };
        flow(fake.clone(), Arc::new(ManualClock::new()))
            .run(&ctx, &creds)
            .await
            .unwrap();

        assert_eq!(
            fake.register_calls(),
            0,
            "ZeroTrust 绝不执行 registration new"
        );
        assert_eq!(
            fake.apply_account_calls(),
            0,
            "ZeroTrust 不执行 teams-enroll"
        );
        assert!(!fake.is_proxy_mode(), "mode 由 mdm.xml 驱动，不走 CLI");
        assert_eq!(
            fake.proxy_port(),
            None,
            "proxy port 由 mdm.xml 驱动，不走 CLI"
        );
        assert!(fake.is_connected());
    }

    #[tokio::test]
    async fn zero_trust_connect_retries_until_mdm_registration_completes() {
        // E2E-08 实测：mdm（service token）注册在 warp-svc 启动后异步完成
        // （~3s），期间 `warp-cli connect` 报 MissingRegistration。流程必须
        // 有界重试 connect，注册完成后自然连接成功，而不是误判 ConnectFailed。
        let fake = Arc::new(FakeWarpControl::new());
        fake.set_connected(true);
        fake.connect_missing_registration(3);
        let (ctx, _keep) = ctx_with_state(false);

        let clock = Arc::new(ManualClock::new());
        flow(fake.clone(), clock.clone())
            .run(&ctx, &zt_creds())
            .await
            .unwrap();

        assert!(fake.is_registered(), "mdm 注册应在重试期间完成");
        assert!(fake.is_connected());
        // 前 3 次失败各等待一个间隔后第 4 次 connect 成功。
        assert_eq!(clock.slept(), vec![ZT_CONNECT_RETRY_POLL_INTERVAL; 3]);
        assert_eq!(fake.register_calls(), 0, "注册仍由 mdm 驱动");
    }

    #[tokio::test]
    async fn zero_trust_registration_wait_is_bounded_and_surfaces_failure() {
        // 注册长时间不完成（如 token 失效）：预算耗尽后必须按 ConnectFailed
        // 上浮，绝不无限重试。
        let fake = Arc::new(FakeWarpControl::new());
        fake.connect_missing_registration(1000);
        let (ctx, _keep) = ctx_with_state(false);

        let clock = Arc::new(ManualClock::new());
        let err = flow(fake.clone(), clock.clone())
            .run(&ctx, &zt_creds())
            .await
            .unwrap_err();

        assert!(
            matches!(err, FlowError::ConnectFailed { .. }),
            "unexpected: {err:?}"
        );
        let total: Duration = clock.slept().iter().sum();
        assert!(
            total >= ZT_REGISTRATION_WAIT_TIMEOUT,
            "waited {total:?} < budget"
        );
        assert!(!fake.is_connected());
    }

    #[tokio::test]
    async fn non_registration_connect_failure_is_not_retried() {
        // 只有 MissingRegistration 签名才触发 ZT 重试；其他 connect 错误
        // 立即上浮，不得消耗等待预算。
        let fake = Arc::new(FakeWarpControl::new());
        fake.set_registered(true);
        fake.set_connected(true);
        fake.fail_connect(WarpControlError::ConnectFailure {
            summary: "connect: refused".into(),
        });
        let (ctx, _keep) = ctx_with_state(false);

        let clock = Arc::new(ManualClock::new());
        let err = flow(fake.clone(), clock.clone())
            .run(&ctx, &zt_creds())
            .await
            .unwrap_err();

        assert!(
            matches!(err, FlowError::ConnectFailed { .. }),
            "unexpected: {err:?}"
        );
        assert!(clock.slept().is_empty(), "非注册签名不得重试");
    }

    #[test]
    fn missing_registration_signature_detection() {
        assert!(is_missing_registration(
            "Failed to connect err=MissingRegistration"
        ));
        assert!(is_missing_registration("No registration found"));
        assert!(is_missing_registration("not registered"));
        assert!(!is_missing_registration("connect: refused"));
        assert!(!is_missing_registration("warp-cli failed: exit 1"));
        assert!(!is_missing_registration(""));
    }

    #[tokio::test]
    async fn apply_account_failure_surfaces_and_aborts_rest_of_flow() {
        let fake = Arc::new(FakeWarpControl::new());
        fake.set_registered(true);
        fake.set_connected(true);
        fake.fail_next(WarpControlError::CommandTimeout);
        let (ctx, _keep) = ctx_with_state(true);

        let creds = InstanceCredentials {
            mode: CredentialMode::WarpPlus,
            license: Some("WPL-BAD".into()),
            ..InstanceCredentials::free()
        };
        let err = flow(fake.clone(), Arc::new(ManualClock::new()))
            .run(&ctx, &creds)
            .await
            .unwrap_err();

        assert!(
            matches!(err, FlowError::ConfigureFailed { .. }),
            "unexpected: {err:?}"
        );
        // 失败后绝不继续配置/连接（AGENTS.md：应用失败必须上浮）。
        assert!(!fake.is_proxy_mode());
        assert_eq!(fake.proxy_port(), None);
    }
}
