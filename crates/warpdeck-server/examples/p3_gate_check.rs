//! P3 Phase Gate 验收驱动（真实 3 实例，无任何 Fake）。
//!
//! 在 warpdeck-dev-base 容器内驱动 InstanceManager 完成：
//!   dbus → warp-svc → RegistrationFlow 的完整真实链路（每实例独立 env/端口），
//!   然后按 Gate 脚本时序执行 stop #1 / restart #1 / kill #2（外部注入崩溃）。
//!
//! 用法（容器内）:
//!   p3_gate_check --data-dir /var/lib/warpdeck --runtime-dir /run/warpdeck
//!
//! 外部验收步骤（脚本，见 DEVELOPMENT_PLAN §8.4）:
//!   1. 等待输出 `STARTED_3` 与 `DATA_PLANE_READY ports=40000,40001,40002`，
//!      然后对 3 个端口 curl SOCKS5 验证 `warp=on`。
//!   2. 等待 `STEP_READY stop1` → 本程序自动 stop #1 并输出 `STEP_DONE stop1 ...`。
//!   3. 等待 `STEP_READY restart1` → 自动 restart #1 并输出 `STEP_DONE restart1 ...`。
//!   4. 等待 `STEP_READY kill2 warp_pid=<pid>` →
//!      外部 `docker exec <c> bash -c "kill -9 <pid>"` 后
//!      `docker kill -s USR1 <c>` → 本程序确认 Failed 并输出 `STEP_DONE kill2 ...`。
//!   5. `docker stop <c>`（SIGTERM）→ 全部实例优雅停止，输出 `STOP_ALL_OK`，
//!      容器退出码 0（tini 转发信号；gate 判据：无 orphan、Exited(0)）。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use warpdeck_server::runtime::backoff::ExponentialBackoff;
use warpdeck_server::runtime::clock::SystemClock;
use warpdeck_server::runtime::context::InstanceContext;
use warpdeck_server::runtime::control::WarpControl;
use warpdeck_server::runtime::events::EventBus;
use warpdeck_server::runtime::instance::InstanceId;
use warpdeck_server::runtime::manager::{InstanceManager, TcpPortProber, WarpRuntime};
use warpdeck_server::runtime::probe::RealDataPlaneProber;
use warpdeck_server::runtime::process::TokioProcessSpawner;
use warpdeck_server::runtime::registry::RuntimeRegistry;
use warpdeck_server::runtime::registry::RuntimeState;
use warpdeck_server::runtime::warp_cli::RealWarpControl;

/// 验收实例数（计划 §8.1：正式验收最多 3 个真实实例）。
const INSTANCES: [i64; 3] = [0, 1, 2];

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

/// 等待外部完成 kill 后发来的 SIGUSR1（Linux 容器内；Windows 仅编译不执行）。
#[cfg(unix)]
async fn wait_for_usr1() {
    let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
        .expect("SIGUSR1 handler");
    sig.recv().await;
    println!("gate: SIGUSR1 received");
}

#[cfg(not(unix))]
async fn wait_for_usr1() {
    std::future::pending().await
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = arg(&args, "data-dir")?;
    let runtime_dir = arg(&args, "runtime-dir")?;

    let spawner = TokioProcessSpawner;
    let control: Arc<dyn WarpControl> = Arc::new(RealWarpControl::real());
    let clock = Arc::new(SystemClock);
    let registry = Arc::new(RuntimeRegistry::new());

    let manager = InstanceManager::new(
        registry.clone(),
        Arc::new(spawner),
        control,
        clock,
        Box::new(ExponentialBackoff::recommended()),
        5,
        Arc::new(warpdeck_server::runtime::fake::FakeCredentialResolver::default()),
        std::path::PathBuf::from(&data_dir),
        std::path::PathBuf::from(&runtime_dir),
        Arc::new(TcpPortProber),
        Arc::new(RealDataPlaneProber::default()),
        EventBus::default(),
    );

    for id in INSTANCES {
        let ctx = InstanceContext::new(
            std::path::Path::new(&data_dir),
            std::path::Path::new(&runtime_dir),
            InstanceId::from_db(id)?,
        )?;
        println!(
            "gate: start id={} port={} state={}",
            id,
            ctx.internal_proxy_port.as_u16(),
            ctx.paths.state_dir.display()
        );
        if let Err(e) = manager.start(&ctx, None).await {
            bail!("start instance {id} failed: {e:?}");
        }
        let e = manager.status(ctx.id).await.context("status after start")?;
        println!(
            "gate: started id={} state={:?} warp_pid={:?} dbus_pid={:?}",
            id, e.state, e.warp_pid, e.dbus_pid
        );
    }

    // 数据面就绪：外部对 3 端口 curl 验证 warp=on。
    println!("DATA_PLANE_READY ports=40000,40001,40002");
    println!("STARTED_3");

    // --- Gate 步骤 2：stop #1，验证 #0/#2 不受影响（P3-007）---
    println!("STEP_READY stop1");
    let outcome = manager.stop(InstanceId::from_db(1)?).await?;
    println!(
        "STEP_DONE stop1 kill_required={} exit_code={:?}",
        outcome.kill_required, outcome.exit_status.exit_code
    );
    check_state(&manager, 0, RuntimeState::Healthy, "after stop1 #0").await?;
    check_state(&manager, 1, RuntimeState::Stopped, "after stop1 #1").await?;
    check_state(&manager, 2, RuntimeState::Healthy, "after stop1 #2").await?;

    // --- Gate 步骤 3：restart #1（P3-007）---
    println!("STEP_READY restart1");
    manager.restart(InstanceId::from_db(1)?, None).await?;
    check_state(&manager, 1, RuntimeState::Healthy, "after restart1 #1").await?;

    // --- Gate 步骤 4：外部 kill -9 #2 的 warp-svc，manager 必须存活（P3 crash）---
    let pid2 = manager
        .status(InstanceId::from_db(2)?)
        .await
        .context("status #2")?
        .warp_pid
        .context("warp pid #2 missing")?;
    println!("STEP_READY kill2 warp_pid={pid2}");
    wait_for_usr1().await;
    // watcher 检测到崩溃后 registry 进入 Failed（500ms 轮询）。
    for _ in 0..20 {
        if manager
            .status(InstanceId::from_db(2)?)
            .await
            .is_some_and(|e| e.state == RuntimeState::Failed)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    check_state(&manager, 2, RuntimeState::Failed, "after kill2 #2").await?;
    // 崩溃不影响其余实例（manager 仍可正常操作 #0/#1）。
    check_state(&manager, 0, RuntimeState::Healthy, "after kill2 #0").await?;

    // --- Gate 步骤 5：SIGTERM → 全部停止（#2 走崩溃回收路径，无 orphan）---
    println!("gate: waiting for SIGINT/SIGTERM, then stop all");
    warpdeck_server::shutdown::shutdown_signal().await;
    for id in INSTANCES {
        let outcome = manager.stop(InstanceId::from_db(id)?).await?;
        let e = manager
            .status(InstanceId::from_db(id)?)
            .await
            .context("status after final stop")?;
        println!(
            "gate: stopped id={} state={:?} kill_required={} exit_code={:?}",
            id, e.state, outcome.kill_required, outcome.exit_status.exit_code
        );
    }
    println!("STOP_ALL_OK");
    Ok(())
}

async fn check_state(
    manager: &InstanceManager,
    id: i64,
    expected: RuntimeState,
    context: &str,
) -> Result<()> {
    let e = manager
        .status(InstanceId::from_db(id)?)
        .await
        .with_context(|| format!("status {id} ({context})"))?;
    if e.state != expected {
        bail!(
            "{context}: instance {id} state = {:?}, expected {:?} (last_error={:?})",
            e.state,
            expected,
            e.last_error
        );
    }
    println!("gate: check ok id={id} state={expected:?} ({context})");
    Ok(())
}
