//! 网关监督循环测试（P13-004 / DESIGN §35.2 / §35.7）：
//! serve 任务 panic 注入 → 监督循环捕获 → listener 重建 → 服务恢复。
//!
//! 语义对应旧 E2E-06「外部 GOST 进程 kill -9 → reconciler 拉起」，
//! builtin 下等价故障面是「网关任务崩溃 → 监督重启」（无外部进程可杀）。

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use warpdeck_server::gateway::pool::RoundRobinPool;
use warpdeck_server::gateway::BuiltinGateway;
use warpdeck_server::reconciler::{ProxyApplier, ProxySettings};
use warpdeck_server::runtime::registry::{RuntimeRegistry, RuntimeState};

/// 在固定端口上起一个 echo 型 SOCKS5 fake 上游（复用 gateway_e2e 的形态）。
async fn spawn_fake_upstream() -> std::net::SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut vn = [0u8; 2];
                if s.read_exact(&mut vn).await.is_err() {
                    return;
                }
                let mut methods = vec![0u8; vn[1] as usize];
                if s.read_exact(&mut methods).await.is_err() {
                    return;
                }
                let _ = s.write_all(&[0x05, 0x00]).await;
                let mut r = [0u8; 4];
                if s.read_exact(&mut r).await.is_err() {
                    return;
                }
                if r[3] != 0x01 {
                    return;
                }
                let mut o = [0u8; 4];
                if s.read_exact(&mut o).await.is_err() {
                    return;
                }
                let mut p = [0u8; 2];
                if s.read_exact(&mut p).await.is_err() {
                    return;
                }
                let _ = s
                    .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                    .await;
                let mut b = vec![0u8; 512];
                loop {
                    match s.read(&mut b).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let _ = s.write_all(b"UP:").await;
                            let _ = s.write_all(&b[..n]).await;
                        }
                    }
                }
            });
        }
    });
    addr
}

/// 预占一个 loopback 端口并释放（重建必须落在同一端口，见下）。
fn reserve_loopback_port() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

async fn full_session(addr: std::net::SocketAddr) -> bool {
    let Ok(mut c) = TcpStream::connect(addr).await else {
        return false;
    };
    if c.write_all(&[0x05, 0x01, 0x00]).await.is_err() {
        return false;
    }
    let mut greet = [0u8; 2];
    if c.read_exact(&mut greet).await.is_err() || greet != [0x05, 0x00] {
        return false;
    }
    if c.write_all(&[0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4, 0, 80])
        .await
        .is_err()
    {
        return false;
    }
    let mut reply = [0u8; 10];
    if c.read_exact(&mut reply).await.is_err() || reply[1] != 0x00 {
        return false;
    }
    c.write_all(b"ping").await.is_ok()
}

/// 打开一条已完成 CONNECT 的会话（占住，供后续读写断言）。
async fn open_session(addr: std::net::SocketAddr) -> TcpStream {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
    let mut greet = [0u8; 2];
    c.read_exact(&mut greet).await.unwrap();
    assert_eq!(greet, [0x05, 0x00]);
    c.write_all(&[0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4, 0, 80])
        .await
        .unwrap();
    let mut reply = [0u8; 10];
    c.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[1], 0x00);
    c
}

#[tokio::test]
async fn injected_serve_panic_is_recovered_by_supervision() {
    let _ = warpdeck_server::observability::init_tracing("debug", None);
    let up = spawn_fake_upstream().await;
    let reg = Arc::new(RuntimeRegistry::new());
    let id = warpdeck_server::runtime::instance::InstanceId::from_db(1).unwrap();
    reg.insert(id);
    reg.set_state(id, RuntimeState::Healthy);

    // 固定端口：serve 任务 panic 后监督循环按**同一地址**重建 listener
    // （生产为固定 11080/18080；测试用预占端口模拟该不变量）。
    let socks5_addr = reserve_loopback_port();
    let base = up.port();
    let pool = RoundRobinPool::with_upstream_base(reg.clone(), base - 1);
    let gw = BuiltinGateway::with_pool(reg, pool, socks5_addr, "127.0.0.1:0".parse().unwrap());

    gw.apply_config(&ProxySettings {
        socks5_enabled: true,
        http_enabled: false,
        auth: None,
        allowlist: vec![],
        max_connections: None,
        max_rps: None,
    })
    .await
    .unwrap();

    let (_tx, shutdown) = tokio::sync::watch::channel(false);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let runner = gw.clone();
    tokio::spawn(async move { runner.run_with_ready(shutdown, Some(ready_tx)).await });
    // 就绪通道：首次 bind 完成后才继续（否则 CI 上 connect 可能早于 bind）。
    let bound = ready_rx.await.expect("gateway ready");
    assert_eq!(bound, socks5_addr, "rebuild must reuse the reserved port");

    // 1) 正常服务。
    assert!(full_session(socks5_addr).await, "session before fault");
    // 2) 注入 serve 任务 panic。
    gw.request_fault_injection();
    // 触发一次 accept 循环迭代以命中注入点：连接本身可能被 RST，忽略结果。
    let _ = TcpStream::connect(socks5_addr).await;
    // 3) 有界等待监督循环重建后恢复（退避 base 2s + 重绑窗口）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut recovered = false;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if full_session(socks5_addr).await {
            recovered = true;
            break;
        }
    }
    assert!(recovered, "gateway must recover after serve-task panic");
}

/// 幂等 diff-skip（E2E-03 回归）：reconciler 每轮重放同一份期望配置，
/// 内容未变化时**不得**重建 listener——否则在途会话每 reconcile 周期
/// 被拆一次。变更配置仍必须触发重建（与 GOST restart 语义一致）。
#[tokio::test]
async fn identical_reapply_keeps_sessions_alive_and_change_rebuilds() {
    let up = spawn_fake_upstream().await;
    let reg = Arc::new(RuntimeRegistry::new());
    let id = warpdeck_server::runtime::instance::InstanceId::from_db(1).unwrap();
    reg.insert(id);
    reg.set_state(id, RuntimeState::Healthy);

    let socks5_addr = reserve_loopback_port();
    let pool = RoundRobinPool::with_upstream_base(reg.clone(), up.port() - 1);
    let gw = BuiltinGateway::with_pool(reg, pool, socks5_addr, "127.0.0.1:0".parse().unwrap());

    let settings = ProxySettings {
        socks5_enabled: true,
        http_enabled: false,
        auth: None,
        allowlist: vec![],
        max_connections: None,
        max_rps: None,
    };
    gw.apply_config(&settings).await.unwrap();

    let (_tx, shutdown) = tokio::sync::watch::channel(false);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let runner = gw.clone();
    tokio::spawn(async move { runner.run_with_ready(shutdown, Some(ready_tx)).await });
    let _ = ready_rx.await.expect("gateway ready");

    // 在途会话 + 同配置重放 ×3（模拟 reconciler 周期）。
    let mut c = open_session(socks5_addr).await;
    for _ in 0..3 {
        gw.apply_config(&settings).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.write_all(b"ping")
        .await
        .expect("session must survive identical re-applies");
    let mut echo = vec![0u8; 7];
    c.read_exact(&mut echo).await.unwrap();
    assert_eq!(&echo, b"UP:ping");

    // 变更配置（allowlist 收紧）→ 重建后**新会话**必须按新配置拒绝
    // （既有会话由独立 handler 任务服务，允许优雅存活到自然结束）。
    let changed = ProxySettings {
        allowlist: vec!["10.0.0.0/8".to_string()],
        ..settings.clone()
    };
    gw.apply_config(&changed).await.unwrap();
    // 有界等待重建完成：新连接来自 127.0.0.1，不在 10.0.0.0/8 内 → 被拒。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut rejected = false;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(mut n) = TcpStream::connect(socks5_addr).await {
            // 允许短暂的旧代 listener：继续探测直到新代生效。
            n.write_all(&[0x05, 0x01, 0x00]).await.ok();
            let mut greet = [0u8; 2];
            match tokio::time::timeout(Duration::from_secs(2), n.read_exact(&mut greet)).await {
                Ok(Ok(_)) if greet != [0x05, 0x00] => {}
                Ok(Ok(_)) => continue, // 旧代仍放行，继续等
                Ok(Err(_)) | Err(_) => {
                    rejected = true;
                    break;
                }
            }
        }
    }
    assert!(rejected, "config change must take effect for new sessions");
}
