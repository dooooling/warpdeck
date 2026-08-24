//! 内置网关限流集成测试（P13-C / DESIGN §35.4）。
//!
//! 断言顺序契约（DESIGN §35.2）：allowlist → 认证 → 连接/RPS 限制；
//! 超限会话在方法协商后被直接关闭（SOCKS5）/ 回 503（HTTP）。

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use warpdeck_server::gateway::pool::RoundRobinPool;
use warpdeck_server::gateway::BuiltinGateway;
use warpdeck_server::reconciler::{ProxyApplier, ProxySettings};
use warpdeck_server::runtime::registry::{RuntimeRegistry, RuntimeState};

#[allow(dead_code)]
struct FakeUpstream {
    addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

async fn spawn_fake_upstream() -> FakeUpstream {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
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
    FakeUpstream { addr, task }
}

/// 启动网关（SOCKS5 only），返回实际监听地址。
/// 第二个返回值是关停通道的发送端——**必须由调用方持有**，
/// 否则 drop 后网关监督循环立即视为收到关停信号并退出。
async fn start_gateway(
    reg: Arc<RuntimeRegistry>,
    upstream_port: u16,
    settings: ProxySettings,
) -> (std::net::SocketAddr, tokio::sync::watch::Sender<bool>) {
    let base = upstream_port - 1;
    let pool = RoundRobinPool::with_upstream_base(reg.clone(), base);
    let gw = BuiltinGateway::with_pool(
        reg,
        pool,
        "127.0.0.1:0".parse().unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    );
    gw.apply_config(&settings).await.unwrap();
    let (tx, shutdown) = tokio::sync::watch::channel(false);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let runner = gw.clone();
    tokio::spawn(async move { runner.run_with_ready(shutdown, Some(ready_tx)).await });
    (ready_rx.await.expect("gateway ready"), tx)
}

fn registry_with_one_healthy() -> Arc<RuntimeRegistry> {
    let reg = Arc::new(RuntimeRegistry::new());
    let id = warpdeck_server::runtime::instance::InstanceId::from_db(1).unwrap();
    reg.insert(id);
    reg.set_state(id, RuntimeState::Healthy);
    reg
}

/// 完成一次 SOCKS5 方法协商（无认证）；返回后可继续发送 CONNECT。
async fn socks5_greet(c: &mut TcpStream) {
    c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
    let mut greet = [0u8; 2];
    c.read_exact(&mut greet).await.unwrap();
    assert_eq!(greet, [0x05, 0x00]);
}

/// 发送 CONNECT 并等待回执；`None` = 连接被关闭（超限拒绝路径）。
async fn socks5_connect(c: &mut TcpStream) -> Option<[u8; 10]> {
    c.write_all(&[0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4, 0, 80])
        .await
        .unwrap();
    let mut reply = [0u8; 10];
    match c.read_exact(&mut reply).await {
        Ok(_) => Some(reply),
        Err(_) => None,
    }
}

/// 断言服务端已关闭该连接（拒绝路径）。
/// Linux 关闭未读缓冲的 socket 是干净 FIN（EOF）；Windows 则发 RST
/// （ConnectionReset）。两种形态在此语义等价。
async fn assert_closed_by_server(c: &mut TcpStream) {
    let mut buf = [0u8; 1];
    let res = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
        .await
        .expect("no timeout: server must close promptly");
    match res {
        Ok(0) => {}
        Err(e)
            if e.kind() == std::io::ErrorKind::ConnectionReset
                || e.kind() == std::io::ErrorKind::ConnectionAborted => {}
        other => panic!("expected connection close, got {other:?}"),
    }
}

#[tokio::test]
async fn max_connections_rejects_second_concurrent_session() {
    let up = spawn_fake_upstream().await;
    let reg = registry_with_one_healthy();
    let (addr, _shutdown_tx) = start_gateway(
        reg,
        up.addr.port(),
        ProxySettings {
            socks5_enabled: true,
            http_enabled: false,
            auth: None,
            allowlist: vec![],
            max_connections: Some(1),
            max_rps: None,
        },
    )
    .await;

    // 会话 A：完成 CONNECT，保持打开（占住唯一许可）。
    let mut a = TcpStream::connect(addr).await.unwrap();
    socks5_greet(&mut a).await;
    assert_eq!(socks5_connect(&mut a).await.unwrap()[1], 0x00);
    a.write_all(b"ping").await.unwrap();
    let mut echo = vec![0u8; 7];
    a.read_exact(&mut echo).await.unwrap();
    assert_eq!(&echo, b"UP:ping");

    // 会话 B：协商通过但被连接上限拒绝（连接关闭，读端 EOF）。
    let mut b = TcpStream::connect(addr).await.unwrap();
    socks5_greet(&mut b).await;
    b.write_all(&[0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4, 0, 80])
        .await
        .unwrap();
    assert_closed_by_server(&mut b).await;

    // 会话 A 结束释放许可后，新会话恢复放行。
    drop(a);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut c = TcpStream::connect(addr).await.unwrap();
    socks5_greet(&mut c).await;
    assert_eq!(socks5_connect(&mut c).await.unwrap()[1], 0x00);
}

#[tokio::test]
async fn max_rps_throttles_burst_then_recovers() {
    let up = spawn_fake_upstream().await;
    let reg = registry_with_one_healthy();
    let (addr, _shutdown_tx) = start_gateway(
        reg,
        up.addr.port(),
        ProxySettings {
            socks5_enabled: true,
            http_enabled: false,
            auth: None,
            allowlist: vec![],
            max_connections: None,
            max_rps: Some(1),
        },
    )
    .await;

    // 第 1 条：消耗桶内唯一令牌。
    let mut a = TcpStream::connect(addr).await.unwrap();
    socks5_greet(&mut a).await;
    assert_eq!(socks5_connect(&mut a).await.unwrap()[1], 0x00);
    drop(a);

    // 立刻第 2 条：令牌不足 → 拒绝（EOF）。
    let mut b = TcpStream::connect(addr).await.unwrap();
    socks5_greet(&mut b).await;
    b.write_all(&[0x05, 0x01, 0x00, 0x01, 1, 2, 3, 4, 0, 80])
        .await
        .unwrap();
    assert_closed_by_server(&mut b).await;

    // >1s 后令牌补充 → 恢复放行。
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let mut c = TcpStream::connect(addr).await.unwrap();
    socks5_greet(&mut c).await;
    assert_eq!(socks5_connect(&mut c).await.unwrap()[1], 0x00);
}
