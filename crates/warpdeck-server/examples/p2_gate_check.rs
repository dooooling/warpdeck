//! P2 Phase Gate 验收驱动（A 路径：真实 instance 0 可启动）。
//!
//! 在 warpdeck-dev-base 容器内跑完整真实链路（无任何 Fake）：
//!   dbus-daemon → warp-svc → ReadinessProbe（P2-008）→ RegistrationFlow
//!   （P2-009）→ 等待 SIGINT/SIGTERM → GracefulStop（P2-010）。
//!
//! 数据面（40000 端口 warp=on）由外部 `curl --socks5-hostname` 验证。
//!
//! 用法（容器内）:
//!   p2_gate_check --data-dir /var/lib/warpdeck --runtime-dir /run/warpdeck \
//!     --instance-id 0

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use warpdeck_server::runtime::backoff::ExponentialBackoff;
use warpdeck_server::runtime::clock::SystemClock;
use warpdeck_server::runtime::context::InstanceContext;
use warpdeck_server::runtime::control::WarpControl;
use warpdeck_server::runtime::dbus::DbusRuntime;
use warpdeck_server::runtime::flow::RegistrationFlow;
use warpdeck_server::runtime::instance::InstanceId;
use warpdeck_server::runtime::process::TokioProcessSpawner;
use warpdeck_server::runtime::readiness::ReadinessProbe;
use warpdeck_server::runtime::service::WarpService;
use warpdeck_server::runtime::stop::GracefulStop;
use warpdeck_server::runtime::warp_cli::RealWarpControl;

fn arg(args: &[String], name: &str) -> Result<String> {
    let key = format!("--{name}");
    let pos = args
        .iter()
        .position(|a| a == &key)
        .with_context(|| format!("missing --{name}"))?;
    args.get(pos + 1)
        .cloned()
        .with_context(|| format!("missing value for --{name}"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = arg(&args, "data-dir")?;
    let runtime_dir = arg(&args, "runtime-dir")?;
    let instance_id: i64 = arg(&args, "instance-id")?.parse()?;

    let ctx = InstanceContext::new(
        std::path::Path::new(&data_dir),
        std::path::Path::new(&runtime_dir),
        InstanceId::from_db(instance_id)?,
    )?;
    println!(
        "gate: instance={} port={} state={} runtime={}",
        ctx.id.as_i64(),
        ctx.internal_proxy_port.as_u16(),
        ctx.paths.state_dir.display(),
        ctx.paths.runtime_dir.display()
    );

    let spawner = TokioProcessSpawner;
    let control: Arc<dyn WarpControl> = Arc::new(RealWarpControl::real());
    let clock = Arc::new(SystemClock);

    // 1. 每实例独立 D-Bus（P2-005）。
    let mut dbus = DbusRuntime::start(&spawner, &ctx)
        .await
        .context("dbus-daemon start failed")?;
    println!("gate: dbus ok pid={}", dbus.pid());

    // 2. warp-svc（P2-006）。
    let mut svc = WarpService::start(&spawner, &ctx)
        .await
        .context("warp-svc start failed")?;
    println!("gate: warp-svc ok pid={}", svc.pid());

    // 3. 就绪探测（P2-008）：warp-cli status 成功一次即 ready。
    let probe = ReadinessProbe::new(
        control.clone(),
        clock.clone(),
        Box::new(ExponentialBackoff::recommended()),
        40,
    );
    let r = probe.probe(&ctx).await;
    if !r.ready {
        bail!(
            "control plane not ready after {} attempts: {:?}",
            r.attempts,
            r.last_error
        );
    }
    println!("gate: control-plane ready attempts={}", r.attempts);

    // 4. 注册 + 配置 + 连接 + 验证（P2-009）。
    let flow = RegistrationFlow::new(
        control.clone(),
        clock.clone(),
        Box::new(ExponentialBackoff::recommended()),
        5,
    );
    let outcome = flow
        .run(
            &ctx,
            &warpdeck_server::runtime::credentials::InstanceCredentials::free(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("registration flow failed: {e:?}"))?;
    println!(
        "gate: flow ok was_registered={} register_attempts={}",
        outcome.was_registered, outcome.register_attempts
    );

    // 5. 数据面就绪：外部 curl 验证 40000 端口 warp=on。
    println!("DATA_PLANE_READY port={}", ctx.internal_proxy_port.as_u16());
    println!("gate: waiting for SIGINT/SIGTERM, then graceful stop...");
    warpdeck_server::shutdown::shutdown_signal().await;
    println!("gate: signal received, running graceful stop");

    // 6. 优雅停止（P2-010）：disconnect → SIGTERM → grace → SIGKILL 兜底。
    let stop = GracefulStop::new(
        control.clone(),
        clock.clone(),
        Duration::from_secs(10),
        Duration::from_millis(100),
    );
    let outcome = stop
        .stop(&ctx, &mut svc, &mut dbus)
        .await
        .context("graceful stop failed")?;
    println!(
        "STOP_OK kill_required={} exit_code={:?}",
        outcome.kill_required, outcome.exit_status.exit_code
    );
    Ok(())
}
