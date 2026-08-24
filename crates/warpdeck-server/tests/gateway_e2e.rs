//! 内置网关端到端集成测试（P13-A / DESIGN §35）。

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use warpdeck_server::gateway::pool::RoundRobinPool;
use warpdeck_server::gateway::BuiltinGateway;
use warpdeck_server::proxy::GostSettings;
use warpdeck_server::reconciler::ProxyApplier;
use warpdeck_server::runtime::registry::{RuntimeRegistry, RuntimeState};

struct FakeUpstream {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

async fn spawn_fake_upstream() -> FakeUpstream {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { break };
            tokio::spawn(async move {
                let mut h = [0u8; 2];
                if s.read_exact(&mut h).await.is_err() { return; }
                let _ = s.write_all(&[0x05, 0x00]).await;
                let mut r = [0u8; 4];
                if s.read_exact(&mut r).await.is_err() { return; }
                if r[3] != 0x01 { return; }
                let mut o = [0u8; 4];
                if s.read_exact(&mut o).await.is_err() { return; }
                let mut p = [0u8; 2];
                if s.read_exact(&mut p).await.is_err() { return; }
                let _ = s.write_all(&[0x05,0x00,0x00,0x01,0,0,0,0,0,0]).await;
                let mut b = vec![0u8; 512];
                loop {
                    match s.read(&mut b).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => { let _ = s.write_all(b"UP:").await; let _ = s.write_all(&b[..n]).await; }
                    }
                }
            });
        }
    });
    FakeUpstream { addr, task }
}

#[tokio::test]
async fn builtin_gateway_connect_end_to_end() {
    // fake 上游
    let up = spawn_fake_upstream().await;

    // registry：一个 Healthy 实例（id=1）
    let reg = Arc::new(RuntimeRegistry::new());
    let id = warpdeck_server::runtime::instance::InstanceId::from_db(1).unwrap();
    reg.insert(id);
    reg.set_state(id, RuntimeState::Healthy);

    // 池：基址 = fake 上游端口 - 1，实例 id=1 → 端口 = base+1 = fake 端口
    let base = up.addr.port() - 1;
    let pool = RoundRobinPool::with_upstream_base(reg.clone(), base);

    // 网关
    let gw = BuiltinGateway::with_pool(
        reg.clone(),
        pool,
        "127.0.0.1:0".parse().unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    );

    let settings = GostSettings {
        socks5_enabled: true,
        http_enabled: false,
        auth: None,
        allowlist: vec![],
        max_connections: None,
        max_rps: None,
    };
    gw.apply_config(&settings).await.unwrap();

    let (_stop_tx, shutdown) = tokio::sync::watch::channel(false);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let runner = gw.clone();
    tokio::spawn(async move { runner.run_with_ready(shutdown, Some(ready_tx)).await });
    let addr = ready_rx.await.expect("gateway ready");

    // 客户端 → 网关 → fake 上游 → 回显
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
    let mut greet = [0u8; 2];
    c.read_exact(&mut greet).await.unwrap();
    assert_eq!(greet, [0x05, 0x00]);

    c.write_all(&[0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4, 0, 80]).await.unwrap();
    let mut reply = [0u8; 10];
    c.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[1], 0x00);

    c.write_all(b"ping").await.unwrap();
    let mut echo = vec![0u8; 7]; // "UP:ping"
    c.read_exact(&mut echo).await.unwrap();
    assert_eq!(&echo, b"UP:ping");
}
