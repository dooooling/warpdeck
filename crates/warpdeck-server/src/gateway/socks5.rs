//! SOCKS5 入站 listener（Phase A：仅 CONNECT；认证子协商 user/pass 可选）。
//!
//! 会话流程：allowlist → 方法协商（0x00 / 0x02）→ [RFC1929 认证] →
//! CONNECT 请求 → 上游池选择 → warp-svc 内部 SOCKS5 握手 → 双向转发。

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{GatewayConfig, SharedState};
use crate::gateway::pool::RoundRobinPool;

const VER: u8 = 0x05;
const SUBNEG_VER: u8 = 0x01;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Method {
    NoAuth,
    UserPass,
}

/// 客户端目标地址（保留原始形态，向上游按原 ATYP 转发）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    V4([u8; 4]),
    Domain(String),
    V6([u8; 16]),
}

impl Target {
    fn push_into(&self, req: &mut Vec<u8>) {
        match self {
            Target::V4(o) => {
                req.push(0x01);
                req.extend_from_slice(o);
            }
            Target::Domain(d) => {
                req.push(0x03);
                req.push(d.len() as u8);
                req.extend_from_slice(d.as_bytes());
            }
            Target::V6(o) => {
                req.push(0x04);
                req.extend_from_slice(o);
            }
        }
    }
}

/// 启动 accept 循环。由监督任务管理生命周期（abort 即停止）。
pub(crate) async fn serve(
    listener: TcpListener,
    shared: Arc<SharedState>,
    pool: RoundRobinPool,
    cfg: GatewayConfig,
) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(component = "gateway", error = %e, "accept error");
                continue;
            }
        };
        let shared = shared.clone();
        let pool = pool.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let _ = handle_conn(stream, peer, shared, pool, cfg).await;
        });
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    peer: SocketAddr,
    shared: Arc<SharedState>,
    pool: RoundRobinPool,
    cfg: GatewayConfig,
) {
    shared.conn_total.fetch_add(1, Ordering::Relaxed);

    // allowlist 前置（会话建立前）。
    if !super::client_allowed(peer.ip(), &cfg.allowlist) {
        tracing::debug!(component = "gateway", %peer, "client rejected by allowlist");
        return;
    }

    // ---- 方法协商 ----
    let mut hdr = [0u8; 2];
    if stream.read_exact(&mut hdr).await.is_err() || hdr[0] != VER {
        return;
    }
    let nmethods = hdr[1] as usize;
    let mut methods = vec![0u8; nmethods];
    if stream.read_exact(&mut methods).await.is_err() {
        return;
    }
    let want_auth = cfg.auth.is_some();
    let chosen = if want_auth && methods.contains(&0x02) {
        Method::UserPass
    } else if !want_auth && methods.contains(&0x00) {
        Method::NoAuth
    } else {
        let _ = stream.write_all(&[VER, 0xFF]).await;
        return;
    };
    let reply_method = match chosen {
        Method::NoAuth => 0x00u8,
        Method::UserPass => 0x02,
    };
    if stream.write_all(&[VER, reply_method]).await.is_err() {
        return;
    }

    // ---- RFC1929 认证子协商 ----
    if chosen == Method::UserPass {
        let Some((expect_user, expect_pass)) = cfg.auth.clone() else {
            return;
        };
        let mut ver = [0u8; 1];
        if stream.read_exact(&mut ver).await.is_err() || ver[0] != SUBNEG_VER {
            return;
        }
        let mut ulen = [0u8; 1];
        if stream.read_exact(&mut ulen).await.is_err() {
            return;
        }
        let mut user = vec![0u8; ulen[0] as usize];
        if stream.read_exact(&mut user).await.is_err() {
            return;
        }
        let mut plen = [0u8; 1];
        if stream.read_exact(&mut plen).await.is_err() {
            return;
        }
        let mut pass = vec![0u8; plen[0] as usize];
        if stream.read_exact(&mut pass).await.is_err() {
            return;
        }
        if !super::verify_credentials(&Some((expect_user, expect_pass)), &user, &pass) {
            let _ = stream.write_all(&[SUBNEG_VER, 0x01]).await;
            return;
        }
        let _ = stream.write_all(&[SUBNEG_VER, 0x00]).await;
    }

    // ---- CONNECT 请求 ----
    let mut head = [0u8; 4];
    if stream.read_exact(&mut head).await.is_err() || head[0] != VER {
        return;
    }
    if head[1] != 0x01 {
        // 仅支持 CONNECT → 回 CMD not supported(0x07)。
        let _ = stream
            .write_all(&[VER, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await;
        return;
    }
    let Some(target) = read_target_addr(&mut stream).await else {
        return;
    };
    let mut port_buf = [0u8; 2];
    if stream.read_exact(&mut port_buf).await.is_err() {
        return;
    }
    let port = u16::from_be_bytes(port_buf);

    // ---- 上游选择与连接 ----
    let Some(upstream_addr) = pool.pick().map(|u| u.addr) else {
        tracing::debug!(component = "gateway", %peer, "no healthy upstream");
        // general SOCKS server failure；语义同旧 gost「无健康上游即失败」。
        let _ = stream
            .write_all(&[VER, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await;
        return;
    };

    let mut up = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        dial_socks5_connect(upstream_addr, &target, port),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => {
            let _ = stream
                .write_all(&[VER, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await;
            return;
        }
    };

    // 成功回执 + 双向转发。
    let _ = stream
        .write_all(&[VER, 0x00, 0x00, 0x01, 127, 0, 0, 1, 0, 0])
        .await;
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut up).await;
}

async fn read_target_addr(stream: &mut TcpStream) -> Option<Target> {
    let mut atyp = [0u8; 1];
    stream.read_exact(&mut atyp).await.ok()?;
    match atyp[0] {
        0x01 => {
            let mut o = [0u8; 4];
            stream.read_exact(&mut o).await.ok()?;
            Some(Target::V4(o))
        }
        0x03 => {
            let mut l = [0u8; 1];
            stream.read_exact(&mut l).await.ok()?;
            let mut d = vec![0u8; l[0] as usize];
            stream.read_exact(&mut d).await.ok()?;
            Some(Target::Domain(String::from_utf8_lossy(&d).into_owned()))
        }
        0x04 => {
            let mut o = [0u8; 16];
            stream.read_exact(&mut o).await.ok()?;
            Some(Target::V6(o))
        }
        _ => None,
    }
}

/// 连接 warp-svc 内部 SOCKS5（无认证）并完成 CONNECT 握手，
/// 返回已建立隧道的原始字节流。目标按原始 ATYP 形态转发。
async fn dial_socks5_connect(
    addr: SocketAddr,
    target: &Target,
    port: u16,
) -> std::io::Result<TcpStream> {
    let mut up = TcpStream::connect(addr).await?;
    // greeting：无认证。
    up.write_all(&[VER, 1, 0x00]).await?;
    let mut resp = [0u8; 2];
    up.read_exact(&mut resp).await?;
    if resp[1] != 0x00 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!("upstream socks5 method rejected: {resp:?}"),
        ));
    }
    let mut req = vec![VER, 0x01, 0x00];
    target.push_into(&mut req);
    req.extend_from_slice(&port.to_be_bytes());
    up.write_all(&req).await?;
    let mut head = [0u8; 4];
    up.read_exact(&mut head).await?;
    if head[1] != 0x00 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!("upstream socks5 connect failed: rep={:#x}", head[1]),
        ));
    }
    // 消费 BND.ADDR/BND.PORT（warp-svc 回 IPv4 占位）。
    let skip = match head[3] {
        0x01 => 6,
        0x03 => {
            let mut l = [0u8; 1];
            up.read_exact(&mut l).await?;
            l[0] as usize + 2
        }
        0x04 => 18,
        _ => 6,
    };
    if skip > 4 {
        let mut discard = vec![0u8; skip - 4];
        up.read_exact(&mut discard).await?;
    }
    Ok(up)
}
