//! P4 Phase Gate 验收：真实 3 实例 + 健康检查（无任何 Fake）。
//!
//! 在 warpdeck-dev-base 容器里用 InstanceManager + HealthMonitor 跑真实链路：
//!   dbus → warp-svc → 注册 → 连接 → 启动尾部数据面验证（warp=on 才算 Healthy）
//!   → 健康循环周期探测（SOCKS5 → TLS → trace，记录 exit_ip/colo/latency）
//!   → 事件总线（StateChanged / HealthChanged / ExitIpChanged）。
//!
//! 用法：
//!   p4_gate_check --data-dir /var/lib/warpdeck --runtime-dir /run/warpdeck
//!
//! 外部验收步骤（对应 DEVELOPMENT_PLAN §9.4 Gate）：
//!   1. 等待 `HEALTHY_3` 与 `STEP_READY curl_check`；对 3 端口 curl SOCKS5
//!      https://cloudflare.com/cdn-cgi/trace 验证 `warp=on`（Healthy = 真实
//!      数据面证据，非 PID 存活）；完成后 `docker kill -s USR1 <c>`。
//!   2. 等待 `STEP_READY kill2 warp_pid=<pid>`；`docker exec <c> kill -9 <pid>`
//!      后 `docker kill -s USR1 <c>`；确认 `STEP_DONE kill2 state=Failed`。
//!   3. 等待 `STEP_READY restart2`；`docker kill -s USR1 <c>`；确认
//!      `STEP_DONE restart2 state=Healthy`（崩溃后恢复路径）。
//!   4. `docker stop <c>`（SIGTERM）→ 全部优雅停止 → `STOP_ALL_OK`。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use warpdeck_server::runtime::backoff::ExponentialBackoff;
use warpdeck_server::runtime::clock::SystemClock;
use warpdeck_server::runtime::context::InstanceContext;
use warpdeck_server::runtime::control::WarpControl;
use warpdeck_server::runtime::events::{EventBus, HealthEvent};
use warpdeck_server::runtime::health::HealthConfig;
use warpdeck_server::runtime::health_monitor::HealthMonitor;
use warpdeck_server::runtime::instance::InstanceId;
use warpdeck_server::runtime::manager::{InstanceManager, TcpPortProber, WarpRuntime};
use warpdeck_server::runtime::probe::RealDataPlaneProber;
use warpdeck_server::runtime::process::TokioProcessSpawner;
use warpdeck_server::runtime::registry::{RuntimeRegistry, RuntimeState};
use warpdeck_server::runtime::warp_cli::RealWarpControl;

const INSTANCES: [i64; 3] = [0, 1, 2];
/// 健康检查间隔（验收加速；生产默认 30s 见 DESIGN settings）。
const HEALTH_INTERVAL: Duration = Duration::from_secs(5);

fn arg(args: &[String], name: &str) -> Result<String> {
    let flag = format!("--{name}");
    let pos = args
        .iter()
        .position(|a| a == &flag)
        .with_context(|| format!("missing --{name}"))?;
    args.get(pos + 1)
        .cloned()
        .with_context(|| format!("missing value for --{name}"))
}

#[cfg(unix)]
async fn wait_for_usr1() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sig = signal(SignalKind::user_defined1()).expect("install SIGUSR1 handler");
    sig.recv().await;
    println!("gate: SIGUSR1 received");
}

#[cfg(not(unix))]
async fn wait_for_usr1() {
    std::future::pending().await
}

/// 等待实例到达期望状态（真实轮询）。
async fn wait_state(
    manager: &InstanceManager,
    id: InstanceId,
    expected: RuntimeState,
    label: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snap = manager
            .status(id)
            .await
            .with_context(|| format!("status after {label}"))?;
        if snap.state == expected {
            println!(
                "gate: {label} state={:?} exit_ip_v4={:?} exit_ip_v6={:?} colo={:?} latency_ms={:?}",
                snap.state,
                snap.exit_ip_v4,
                snap.exit_ip_v6,
                snap.colo,
                snap.latency_ms
            );
            return Ok(());
        }
        if tokio::time::Instant::now() > deadline {
            bail!(
                "{label}: timeout waiting for {expected:?}, last={:?}",
                snap.state
            );
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
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
    let bus = EventBus::new(256);
    let mut events = bus.subscribe();
    // 事件消费者：打印供外部观察（含 crash/恢复事件）。
    let event_printer = tokio::spawn(async move {
        while let Ok(ev) = events.recv().await {
            match ev {
                HealthEvent::StateChanged(t) => println!(
                    "gate: event state_changed id={} {} -> {} ({})",
                    t.instance_id.as_i64(),
                    transition_name(t.from),
                    transition_name(t.to),
                    t.reason
                ),
                HealthEvent::HealthChanged(t) => println!(
                    "gate: event health_changed id={} {} -> {} ({})",
                    t.instance_id.as_i64(),
                    transition_name(t.from),
                    transition_name(t.to),
                    t.reason
                ),
                HealthEvent::ExitIpChanged {
                    instance_id,
                    exit_ip_v4,
                    exit_ip_v6,
                    colo,
                    latency_ms,
                } => println!(
                    "gate: event exit_ip_changed id={} ip_v4={:?} ip_v6={:?} colo={:?} latency={:?}ms",
                    instance_id.as_i64(),
                    exit_ip_v4,
                    exit_ip_v6,
                    colo,
                    latency_ms
                ),
            }
        }
    });

    let manager = Arc::new(InstanceManager::new(
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
        bus.clone(),
    ));

    // 健康循环：周期数据面探测 + 阈值判定 + 事件发布。
    let monitor = HealthMonitor::new(
        manager.clone(),
        Arc::new(SystemClock),
        HealthConfig::default(),
        HEALTH_INTERVAL,
    );
    let (_monitor_handle, _monitor_cancel) = monitor.spawn();

    // --- 启动 3 实例（Healthy = 启动尾部真实数据面验证通过）---
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
        // start 返回后可能仍是 Degraded（数据面建连窗口）：等健康循环拉回 Healthy。
        wait_state(
            &manager,
            ctx.id,
            RuntimeState::Healthy,
            &format!("started id={id}"),
            Duration::from_secs(180),
        )
        .await?;
    }
    println!("HEALTHY_3");

    // --- Gate 1：数据面 curl 验证（外部对 3 端口 warp=on）+ 指标记录 ---
    println!("STEP_READY curl_check");
    wait_for_usr1().await;
    for id in INSTANCES {
        let snap = manager
            .status(InstanceId::from_db(id)?)
            .await
            .context("status curl_check")?;
        assert_eq!(snap.state, RuntimeState::Healthy, "id={id} must be Healthy");
        assert!(
            snap.exit_ip_v4.is_some() || snap.exit_ip_v6.is_some(),
            "id={id} missing exit_ip"
        );
        assert!(snap.colo.is_some(), "id={id} missing colo");
        assert!(snap.latency_ms.is_some(), "id={id} missing latency");
        println!(
            "gate: verified id={} exit_ip_v4={:?} exit_ip_v6={:?} colo={:?} latency_ms={:?}",
            id, snap.exit_ip_v4, snap.exit_ip_v6, snap.colo, snap.latency_ms
        );
    }
    println!("STEP_DONE curl_check");

    // --- Gate 2：kill #2 → watcher Failed（健康循环不再探测 Failed）---
    let id2 = InstanceId::from_db(2)?;
    let snap = manager.status(id2).await.context("status before kill2")?;
    let warp_pid = snap.warp_pid.context("id=2 warp pid missing")?;
    println!("STEP_READY kill2 warp_pid={warp_pid}");
    wait_for_usr1().await;
    wait_state(
        &manager,
        id2,
        RuntimeState::Failed,
        "after kill2",
        Duration::from_secs(30),
    )
    .await?;
    println!("STEP_DONE kill2");

    // --- Gate 3：restart #2 → 崩溃恢复（数据面验证 + 健康循环保持 Healthy）---
    println!("STEP_READY restart2");
    wait_for_usr1().await;
    manager.restart(id2, None).await.context("restart id=2")?;
    wait_state(
        &manager,
        id2,
        RuntimeState::Healthy,
        "after restart2",
        Duration::from_secs(180),
    )
    .await?;
    println!("STEP_DONE restart2");

    // --- 收尾：SIGTERM → 全部优雅停止 ---
    println!("waiting for SIGINT/SIGTERM, then stop all");
    shutdown_signal().await;
    for id in INSTANCES {
        let outcome = manager
            .stop(InstanceId::from_db(id)?)
            .await
            .context("stop")?;
        println!(
            "gate: stopped id={id} state=Stopped kill_required={} exit_code={:?}",
            outcome.kill_required, outcome.exit_status.exit_code
        );
    }
    event_printer.abort();
    println!("STOP_ALL_OK");
    Ok(())
}

fn transition_name(s: RuntimeState) -> &'static str {
    match s {
        RuntimeState::Healthy => "healthy",
        RuntimeState::Degraded => "degraded",
        RuntimeState::Failed => "failed",
        RuntimeState::Stopping => "stopping",
        RuntimeState::Stopped => "stopped",
        RuntimeState::Starting => "starting",
        RuntimeState::Registering => "registering",
        RuntimeState::Connecting => "connecting",
        RuntimeState::Disabled => "disabled",
    }
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => println!("gate: SIGINT received"),
        _ = sigterm.recv() => println!("gate: SIGTERM received"),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    tokio::signal::ctrl_c().await.expect("ctrl_c");
}
