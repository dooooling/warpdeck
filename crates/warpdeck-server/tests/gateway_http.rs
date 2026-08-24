//! 内置网关 HTTP 入站集成测试（P13-B / DESIGN §35.3）。
//!
//! 此前 http::serve 无任何 L3 覆盖；本文件补齐：
//! - absolute-URI 转发（经 fake 上游回显断言）；
//! - CONNECT 隧道；
//! - Basic Auth 缺失 → 407，正确凭据 → 放行。

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use warpdeck_server::gateway::pool::RoundRobinPool;
use warpdeck_server::gateway::BuiltinGateway;
use warpdeck_server::reconciler::{ProxyApplier, ProxySettings};
use warpdeck_server::runtime::registry::{RuntimeRegistry, RuntimeState};

/// 支持 DOMAIN ATYP 的 fake SOCKS5 上游：握手后对所有字节回显 `UP:` 前缀。
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
                let mut head = [0u8; 4];
                if s.read_exact(&mut head).await.is_err() {
                    return;
                }
                // 按 ATYP 消费目标地址 + 端口（DOMAIN 必须支持）。
                match head[3] {
                    0x01 => {
                        let mut rest = [0u8; 6];
                        if s.read_exact(&mut rest).await.is_err() {
                            return;
                        }
                    }
                    0x03 => {
                        let mut l = [0u8; 1];
                        if s.read_exact(&mut l).await.is_err() {
                            return;
                        }
                        let mut rest = vec![0u8; l[0] as usize + 2];
                        if s.read_exact(&mut rest).await.is_err() {
                            return;
                        }
                    }
                    0x04 => {
                        let mut rest = [0u8; 18];
                        if s.read_exact(&mut rest).await.is_err() {
                            return;
                        }
                    }
                    _ => return,
                }
                let _ = s
                    .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                    .await;
                let mut b = vec![0u8; 1024];
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

fn registry_with_one_healthy() -> Arc<RuntimeRegistry> {
    let reg = Arc::new(RuntimeRegistry::new());
    let id = warpdeck_server::runtime::instance::InstanceId::from_db(1).unwrap();
    reg.insert(id);
    reg.set_state(id, RuntimeState::Healthy);
    reg
}

/// 启动网关（双 listener，固定预占端口），返回 (socks5_addr, http_addr)。
/// 第三个返回值是关停通道发送端——**必须由调用方持有**。
async fn start_gateway(
    auth: Option<(String, String)>,
) -> (
    std::net::SocketAddr,
    std::net::SocketAddr,
    tokio::sync::watch::Sender<bool>,
) {
    let up = spawn_fake_upstream().await;
    let reg = registry_with_one_healthy();
    let socks5_port = reserve_port();
    let http_port = reserve_port();
    let socks5_addr: std::net::SocketAddr = format!("127.0.0.1:{socks5_port}").parse().unwrap();
    let http_addr: std::net::SocketAddr = format!("127.0.0.1:{http_port}").parse().unwrap();

    let pool = RoundRobinPool::with_upstream_base(reg.clone(), up.port() - 1);
    let gw = BuiltinGateway::with_pool(reg, pool, socks5_addr, http_addr);
    gw.apply_config(&ProxySettings {
        socks5_enabled: true,
        http_enabled: true,
        auth: auth.map(
            |(username, password)| warpdeck_server::reconciler::ProxyAuth { username, password },
        ),
        allowlist: vec![],
        max_connections: None,
        max_rps: None,
    })
    .await
    .unwrap();

    let (tx, shutdown) = tokio::sync::watch::channel(false);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let runner = gw.clone();
    tokio::spawn(async move { runner.run_with_ready(shutdown, Some(ready_tx)).await });
    let bound = tokio::time::timeout(Duration::from_secs(5), ready_rx)
        .await
        .expect("gateway ready in time")
        .expect("ready channel");
    assert_eq!(bound, socks5_addr);
    (socks5_addr, http_addr, tx)
}

fn reserve_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn read_available(c: &mut TcpStream) -> String {
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), c.read(&mut buf))
        .await
        .expect("response within timeout")
        .expect("read response");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

#[tokio::test]
async fn http_absolute_uri_forwards_via_upstream() {
    let (_socks, http, _tx) = start_gateway(None).await;
    let mut c = TcpStream::connect(http).await.unwrap();
    c.write_all(b"GET http://example.com/path HTTP/1.1\r\nHost: example.com\r\n\r\n")
        .await
        .unwrap();
    let resp = read_available(&mut c).await;
    assert!(
        resp.starts_with("UP:GET http://example.com/path"),
        "request head must be forwarded through upstream, got: {resp:?}"
    );
}

#[tokio::test]
async fn http_connect_tunnel_establishes_and_relays() {
    let (_socks, http, _tx) = start_gateway(None).await;
    let mut c = TcpStream::connect(http).await.unwrap();
    c.write_all(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
        .await
        .unwrap();
    let resp = read_available(&mut c).await;
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "CONNECT must be accepted, got: {resp:?}"
    );
    c.write_all(b"ping").await.unwrap();
    let echoed = read_available(&mut c).await;
    assert_eq!(echoed, "UP:ping");
}

#[tokio::test]
async fn http_basic_auth_missing_gets_407_and_correct_passes() {
    let (_socks, http, _tx) = start_gateway(Some(("u".into(), "p".into()))).await;

    // 无凭据 → 407。
    let mut c = TcpStream::connect(http).await.unwrap();
    c.write_all(b"CONNECT example.com:443 HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let resp = read_available(&mut c).await;
    assert!(resp.starts_with("HTTP/1.1 407"), "got: {resp:?}");

    // 正确凭据（Basic base64("u:p") = dTpw）→ 隧道建立。
    let mut c = TcpStream::connect(http).await.unwrap();
    c.write_all(b"CONNECT example.com:443 HTTP/1.1\r\nProxy-Authorization: Basic dTpw\r\n\r\n")
        .await
        .unwrap();
    let resp = read_available(&mut c).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "got: {resp:?}");
}
