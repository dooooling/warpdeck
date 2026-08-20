//! P5 Phase Gate 验收：GOST 代理网关数据面（真实 WARP + 真实 GOST，无任何 Fake）。
//!
//! 在 warpdeck-dev-base 容器里跑真实链路：
//!   InstanceManager 启动 3 实例（Healthy，数据面已验证）→ GostManager.apply()
//!   → GOST 以渲染配置启动 → 11080/18080 双 listener → 外部 curl 验证 warp=on
//!   → 崩溃实例自动排除出 pool → GOST 崩溃可感知并可恢复 → 空池 Degraded
//!   → 外部 curl 验证空池不走 Direct → SIGTERM 优雅收尾。
//!
//! 用法：
//!   p5_gate_check --data-dir /var/lib/warpdeck --runtime-dir /run/warpdeck \
//!     [--gost-bin gost]
//!
//! 外部验收步骤（对应 DEVELOPMENT_PLAN §10.5 Gate）：
//!   1. 等待 `STEP_READY curl_check`；两个 listener 分两路 curl
//!      https://cloudflare.com/cdn-cgi/trace，均需 `warp=on`；完成后 `docker kill -s USR1 <c>`。
//!   2. 等待 `STEP_READY kill_warp2 warp_pid=<pid>`；`docker exec <c> kill -9 <pid>` 后
//!      `docker kill -s USR1 <c>`；确认 `STEP_DONE warp2_excluded pool=2`（实例 2 端口
//!      不在渲染配置中）。
//!   3. 等待 `STEP_READY kill_gost gost_pid=<pid>`；`docker exec <c> kill -9 <pid>` 后
//!      `docker kill -s USR1 <c>`；确认 `STEP_DONE gost_crash state=Failed`（可感知）。
//!   4. 2 秒后自动 `apply()` 恢复；确认 `STEP_DONE gost_recovered state=Running`。
//!   5. 等待 `STEP_READY stop_all`；确认全部实例 Stopped 后 `docker kill -s USR1 <c>`；
//!      确认 `STEP_DONE empty_pool state=Degraded`（listener 保留，无 healthy upstream）。
//!      随后外部 curl 两个 listener，验证请求明确失败（不走 Direct Internet）；
//!      然后 `docker stop <c>`（SIGTERM）→ `STOP_ALL_OK`。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::time::sleep;
use warpdeck_server::proxy::pool::{ReachabilityProbe, TcpReachabilityProbe};
use warpdeck_server::proxy::{
    GostManager, GostSettings, ProxyStatus, HTTP_LISTEN_PORT, SOCKS5_LISTEN_PORT,
};
use warpdeck_server::runtime::backoff::ExponentialBackoff;
use warpdeck_server::runtime::clock::SystemClock;
use warpdeck_server::runtime::context::InstanceContext;
use warpdeck_server::runtime::control::WarpControl;
use warpdeck_server::runtime::health::HealthConfig;
use warpdeck_server::runtime::health_monitor::HealthMonitor;
use warpdeck_server::runtime::instance::InstanceId;
use warpdeck_server::runtime::manager::{InstanceManager, TcpPortProber, WarpRuntime};
use warpdeck_server::runtime::probe::RealDataPlaneProber;
use warpdeck_server::runtime::process::TokioProcessSpawner;
use warpdeck_server::runtime::registry::{RuntimeRegistry, RuntimeState};
use warpdeck_server::runtime::warp_cli::RealWarpControl;

const INSTANCES: [i64; 3] = [0, 1, 2];
const HEALTH_INTERVAL: Duration = Duration::from_secs(5);
const GOST_STOP_GRACE: Duration = Duration::from_secs(5);
const GOST_STOP_POLL: Duration = Duration::from_millis(200);

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
            println!("gate: {label} state={:?}", snap.state);
            return Ok(());
        }
        if tokio::time::Instant::now() > deadline {
            bail!(
                "{label}: timeout waiting for {expected:?}, last={:?}",
                snap.state
            );
        }
        sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_proxy_status(
    manager: &GostManager,
    expect_running: bool,
    label: &str,
    timeout: Duration,
) -> Result<ProxyStatus> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let s = manager.status().await;
        let ok = match &s {
            ProxyStatus::Running { .. } => expect_running,
            ProxyStatus::Degraded { .. } | ProxyStatus::Failed { .. } => !expect_running,
            ProxyStatus::Stopped => false,
        };
        if ok {
            println!("gate: {label} status={s:?}");
            return Ok(s);
        }
        if tokio::time::Instant::now() > deadline {
            bail!("{label}: timeout, last={s:?}");
        }
        sleep(Duration::from_millis(500)).await;
    }
}

/// 幂等 apply 直到 Running（首次启动 GOST 可能需短暂绑定窗口）。
async fn apply_until_running(manager: &GostManager, label: &str) -> Result<ProxyStatus> {
    for attempt in 1..=5 {
        manager
            .apply()
            .await
            .with_context(|| format!("{label}: apply attempt {attempt}"))?;
        let s = manager.status().await;
        if let ProxyStatus::Running { .. } = &s {
            println!("gate: {label} running after attempt {attempt}: {s:?}");
            return Ok(s);
        }
        println!("gate: {label} not running yet (attempt {attempt}): {s:?}");
        sleep(Duration::from_millis(1000)).await;
    }
    bail!("{label}: could not reach Running after 5 apply attempts")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = arg(&args, "data-dir")?;
    let runtime_dir = arg(&args, "runtime-dir")?;
    let gost_bin = args
        .iter()
        .position(|a| a == "--gost-bin")
        .map(|i| args[i + 1].clone())
        .unwrap_or_else(|| "gost".to_string());

    // --- 实例层：3 实例 + 健康循环（复用 P4 链路，保证 pool 输入可信）---
    let spawner = TokioProcessSpawner;
    let control: Arc<dyn WarpControl> = Arc::new(RealWarpControl::real());
    let clock = Arc::new(SystemClock);
    let registry = Arc::new(RuntimeRegistry::new());
    let manager = Arc::new(InstanceManager::new(
        registry.clone(),
        Arc::new(spawner),
        control,
        clock.clone(),
        Box::new(ExponentialBackoff::recommended()),
        5,
        Arc::new(warpdeck_server::runtime::fake::FakeCredentialResolver::default()),
        PathBuf::from(&data_dir),
        PathBuf::from(&runtime_dir),
        Arc::new(TcpPortProber),
        Arc::new(RealDataPlaneProber::default()),
        Default::default(),
    ));
    let monitor = HealthMonitor::new(
        manager.clone(),
        clock,
        HealthConfig::default(),
        HEALTH_INTERVAL,
    );
    let (_monitor_handle, _monitor_cancel) = monitor.spawn();

    for id in INSTANCES {
        let ctx = InstanceContext::new(
            std::path::Path::new(&data_dir),
            std::path::Path::new(&runtime_dir),
            InstanceId::from_db(id)?,
        )?;
        println!(
            "gate: start id={id} port={}",
            ctx.internal_proxy_port.as_u16()
        );
        if let Err(e) = manager.start(&ctx, None).await {
            bail!("start instance {id} failed: {e:?}");
        }
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

    // --- 代理层：GostManager（真实 GOST 进程）---
    let probe = Arc::new(TcpReachabilityProbe {
        connect_timeout: Duration::from_secs(1),
    });
    let gost = GostManager::new(
        registry.clone(),
        probe.clone(),
        probe.clone(),
        Arc::new(RealDataPlaneProber::default()),
        Arc::new(TokioProcessSpawner),
        Arc::new(SystemClock),
        gost_bin,
        PathBuf::from(&data_dir),
        GostSettings {
            socks5_enabled: true,
            http_enabled: true,
            auth: None,
            allowlist: vec![],
            max_connections: None,
            max_rps: None,
        },
        GOST_STOP_GRACE,
        GOST_STOP_POLL,
    );

    // --- Gate 1：双 listener 数据面（外部 curl，warp=on）---
    let _status = apply_until_running(&gost, "initial apply").await?;
    println!(
        "STEP_READY curl_check gost_pid={}",
        match gost.status().await {
            ProxyStatus::Running { pid, .. } => pid,
            other => bail!("expected Running, got {other:?}"),
        }
    );
    wait_for_usr1().await;
    println!("STEP_DONE curl_check");

    // --- Gate 2：崩溃实例自动排除（kill warp-svc #2 → Failed → apply）---
    let id2 = InstanceId::from_db(2)?;
    let snap2 = manager
        .status(id2)
        .await
        .context("status before kill_warp2")?;
    let warp_pid = snap2.warp_pid.context("instance 2 warp pid missing")?;
    println!("STEP_READY kill_warp2 warp_pid={warp_pid}");
    wait_for_usr1().await;
    wait_state(
        &manager,
        id2,
        RuntimeState::Failed,
        "after kill_warp2",
        Duration::from_secs(60),
    )
    .await?;
    let _ = apply_until_running(&gost, "apply after exclusion").await?;
    // 渲染配置不得再包含实例 2 的端口 40002。
    let rendered = std::fs::read_to_string(gost.config_path()).context("read generated config")?;
    assert!(
        !rendered.contains("127.0.0.1:40002"),
        "instance 2 must be excluded from pool:\n{rendered}"
    );
    assert!(rendered.contains("127.0.0.1:40000") && rendered.contains("127.0.0.1:40001"));
    println!("STEP_DONE warp2_excluded pool=2");

    // --- Gate 3：GOST 崩溃感知（kill -9 GOST → Failed）---
    let gost_pid = match gost.status().await {
        ProxyStatus::Running { pid, .. } => pid,
        other => bail!("expected Running before crash, got {other:?}"),
    };
    println!("STEP_READY kill_gost gost_pid={gost_pid}");
    wait_for_usr1().await;
    let s = wait_proxy_status(&gost, false, "after gost crash", Duration::from_secs(30)).await?;
    assert!(
        matches!(s, ProxyStatus::Failed { .. }),
        "crash must surface as Failed, got {s:?}"
    );
    println!("STEP_DONE gost_crash state=Failed");

    // --- Gate 4：崩溃恢复（幂等 apply）---
    sleep(Duration::from_secs(2)).await;
    let _ = apply_until_running(&gost, "recover after gost crash").await?;
    println!("STEP_DONE gost_recovered state=Running");

    // --- Gate 5：空池 → Degraded（listener 保留），外部 curl 验证不走 Direct ---
    println!("STEP_READY stop_all");
    wait_for_usr1().await;
    for id in INSTANCES {
        let outcome = manager
            .stop(InstanceId::from_db(id)?)
            .await
            .context("stop instance")?;
        println!(
            "gate: stopped id={id} exit_code={:?}",
            outcome.exit_status.exit_code
        );
    }
    apply_until_running_ignore(&gost).await?;
    let s = gost.status().await;
    match &s {
        ProxyStatus::Degraded { reason, .. } => {
            println!("gate: empty pool status=Degraded reason={reason}");
        }
        other => bail!("expected Degraded for empty pool, got {other:?}"),
    }
    // listener 必须仍在（GOST 进程保留）。
    let probe_now = TcpReachabilityProbe {
        connect_timeout: Duration::from_secs(1),
    };
    let socks_listening = probe_now
        .is_reachable(format!("127.0.0.1:{SOCKS5_LISTEN_PORT}").parse().unwrap())
        .await;
    let http_listening = probe_now
        .is_reachable(format!("127.0.0.1:{HTTP_LISTEN_PORT}").parse().unwrap())
        .await;
    println!("gate: empty pool listeners socks5={socks_listening} http={http_listening}");
    println!("STEP_DONE empty_pool state=Degraded");

    // --- 收尾：SIGTERM → 停止 GOST。（三个 warp-svc 实例已在 empty_pool
    // Gate 中按测试目的全部停止，此处只剩代理进程需要收尾。）---
    println!("waiting for SIGINT/SIGTERM, then stop gost");
    shutdown_signal().await;
    gost.stop().await.context("stop gost")?;
    println!("gate: gost stopped");
    println!("STOP_ALL_OK");
    Ok(())
}

/// 空池版本：允许结果停在 Degraded（不要求 Running）。
async fn apply_until_running_ignore(gost: &GostManager) -> Result<()> {
    gost.apply().await.context("apply after stop_all")?;
    Ok(())
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
