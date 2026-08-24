//! HTTP 入站 listener（Phase B / DESIGN §35.3）：
//! CONNECT 隧道 + absolute-URI 转发 + Basic Auth。

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::pool::RoundRobinPool;
use super::{GatewayConfig, SharedState};

pub(crate) async fn serve(
    listener: TcpListener,
    shared: Arc<SharedState>,
    pool: RoundRobinPool,
    cfg: GatewayConfig,
) {
    loop {
        let (mut stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(component = "gateway", error = %e, "http accept error");
                continue;
            }
        };
        let shared = shared.clone();
        let pool = pool.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let _ = handle_conn(&mut stream, peer, shared, pool, cfg).await;
        });
    }
}

async fn handle_conn(
    stream: &mut TcpStream,
    peer: SocketAddr,
    shared: Arc<SharedState>,
    pool: RoundRobinPool,
    cfg: GatewayConfig,
) {
    shared.conn_total.fetch_add(1, Ordering::Relaxed);

    // allowlist。
    if !super::client_allowed(peer.ip(), &cfg.allowlist) {
        tracing::debug!(component = "gateway", %peer, "client rejected by allowlist");
        return;
    }

    // 读取请求头（直到 \r\n\r\n，上限 16 KiB）。
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 4096];
    loop {
        let n =
            match tokio::time::timeout(std::time::Duration::from_secs(30), stream.read(&mut tmp))
                .await
            {
                Ok(Ok(n)) => n,
                _ => return,
            };
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).rposition(|w| w == b"\r\n\r\n").is_some() || n == 0 {
            break;
        }
        if buf.len() > 16384 {
            return;
        }
    }
    let request_head = String::from_utf8_lossy(&buf);

    // Proxy-Authorization 检查（Basic Auth 启用时必须匹配）。
    if let Some((expect_user, expect_pass)) = &cfg.auth {
        let expected_token = format!(
            "Basic {}",
            base64_encode(format!("{expect_user}:{expect_pass}").as_bytes())
        );
        let ok = request_head.lines().any(|l| {
            let lower = l.to_ascii_lowercase();
            lower.starts_with("proxy-authorization:")
                && l.trim_start_matches(|c: char| !c.is_ascii_whitespace())
                    .trim()
                    .trim_end_matches('\r')
                    == expected_token
        });
        if !ok {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"warpdeck\"\r\nConnection: close\r\n\r\n"
                )
                .await;
            tracing::debug!(component = "gateway", %peer, "proxy auth failed");
            return;
        }
    }

    // ---- 连接/RPS 限流（DESIGN §35.2：allowlist → 认证 → conn-limit）----
    // 超限回 503；许可持有到会话结束。
    let _permit = match cfg.limits.as_deref() {
        Some(limits) => match limits.acquire() {
            Ok(permit) => Some(permit),
            Err(rejection) => {
                tracing::debug!(component = "gateway", %peer, ?rejection, "session rejected by limits");
                let _ = stream
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }
        },
        None => None,
    };

    // 解析请求行。
    let first_line = request_head.lines().next().unwrap_or("");
    let is_connect = first_line.starts_with("CONNECT ");

    // 提取目标 host:port。
    let target = if is_connect {
        first_line
            .strip_prefix("CONNECT ")
            .and_then(|rest| rest.split_whitespace().next())
            .map(|s| s.to_string())
    } else {
        extract_absolute_uri_host_port(first_line)
    };

    let Some(host_port) = target else {
        let _ = client_400(stream).await;
        return;
    };

    let Some(upstream_addr) = pool.pick().map(|u| u.addr) else {
        let _ = client_502(stream).await;
        return;
    };

    let (host, port) = split_host_port(&host_port);
    let mut up = match dial_socks5(upstream_addr, &host, port).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(component = "gateway", error = %e, "upstream dial failed");
            let _ = client_502(stream).await;
            return;
        }
    };

    if is_connect {
        // 隧道：告知客户端后进入双向转发。原始头不转发给上游。
        let _ = stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await;
        let _ = tokio::io::copy_bidirectional(stream, &mut up).await;
    } else {
        // 非 CONNECT：将客户端的完整请求头 + 已读数据写入上游，
        // 然后双向转发（处理 body / chunked 等）。
        let _ = up.write_all(request_head.as_bytes()).await;
        let _ = tokio::io::copy_bidirectional(stream, &mut up).await;
    }
}

fn client_400<'a>(
    stream: &'a mut TcpStream,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let _ = stream
            .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
            .await;
    })
}

fn client_502<'a>(
    stream: &'a mut TcpStream,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let _ = stream
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
            .await;
    })
}

/// 从 HTTP absolute-URI 请求行提取 host:port（如 GET http://example.com/path）。
fn extract_absolute_uri_host_port(first_line: &str) -> Option<String> {
    let after_method = first_line.split_once(' ')?.1;
    let uri = after_method.split_once(' ')?.0;
    let after_scheme = uri
        .strip_prefix("http://")
        .or_else(|| uri.strip_prefix("https://"))?;
    Some(after_scheme.split('/').next()?.to_string())
}

fn split_host_port(s: &str) -> (String, u16) {
    if let Ok(addr) = s.parse::<SocketAddr>() {
        let ip = addr.ip().to_string();
        return (ip, addr.port());
    }
    if let Some((host, port_str)) = s.rsplit_once(':') {
        if let Ok(port) = port_str.parse::<u16>() {
            let h = host.trim_start_matches('[').trim_end_matches(']');
            return (h.to_string(), port);
        }
    }
    (s.to_string(), 80)
}

async fn dial_socks5(
    addr: SocketAddr,
    target_host: &str,
    target_port: u16,
) -> Result<TcpStream, std::io::Error> {
    use tokio::io::AsyncReadExt as _;

    let mut up = TcpStream::connect(addr).await?;
    up.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut resp = [0u8; 2];
    up.read_exact(&mut resp).await?;
    if resp[1] != 0x00 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!("upstream socks5 method rejected: {resp:?}"),
        ));
    }

    if let Ok(v4) = target_host.parse::<std::net::Ipv4Addr>() {
        let mut req = vec![0x05, 0x01, 0x00, 0x01];
        req.extend_from_slice(&v4.octets());
        req.extend_from_slice(&target_port.to_be_bytes());
        up.write_all(&req).await?;
    } else if let Ok(v6) = target_host.parse::<std::net::Ipv6Addr>() {
        let mut req = vec![0x05, 0x01, 0x00, 0x04];
        req.extend_from_slice(&v6.octets());
        req.extend_from_slice(&target_port.to_be_bytes());
        up.write_all(&req).await?;
    } else {
        let mut req = vec![0x05, 0x01, 0x00, 0x03, target_host.len() as u8];
        req.extend_from_slice(target_host.as_bytes());
        req.extend_from_slice(&target_port.to_be_bytes());
        up.write_all(&req).await?;
    }

    let mut head = [0u8; 4];
    up.read_exact(&mut head).await?;
    if head[1] != 0x00 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!("upstream socks5 connect failed: rep={:#x}", head[1]),
        ));
    }
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

// ---- base64 编码（Proxy-Authorization 头构造用；仅编码，无解码需求）----

const B64_TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        out.push(B64_TABLE[(b[0] >> 2) as usize] as char);
        out.push(B64_TABLE[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64_TABLE[(((b[1] & 0x0F) << 2) | (b[2] >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64_TABLE[(b[2] & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
