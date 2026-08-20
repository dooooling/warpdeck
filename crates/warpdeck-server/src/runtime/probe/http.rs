//! 最小 HTTP 代理 CONNECT 客户端（P5 review 补强：HTTP-only 模式的数据面
//! 验证需要走 HTTP 代理建立隧道，不能对 18080 发 SOCKS5 握手）。
//!
//! 只实现了目标需要的 CONNECT 建立（RFC 7231 §4.3.6）：建连后升级为
//! 裸 TCP 隧道，后续 TLS/HTTP 由调用方（trace_probe）在同一连接上进行。
//!
//! 安全前提：CONNECT 成功后流量方向是我们 → 代理（TLS ClientHello 等），
//! 响应头之后不会有代理主动推送的数据，因此成功路径无需缓冲残余字节。

use std::io;
use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MAX_HEADER_BYTES: usize = 8192;

/// 通过 `proxy_addr` 上的 HTTP 代理向 `target_host:target_port` 建立 CONNECT 隧道。
/// 代理返回非 2xx 或头格式非法时视为失败（流已关闭）。
pub async fn http_connect(
    proxy_addr: SocketAddr,
    target_host: &str,
    target_port: u16,
) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect(proxy_addr).await?;
    let request = format!(
        "CONNECT {target_host}:{target_port} HTTP/1.1\r\nHost: {target_host}:{target_port}\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    stream.flush().await?;

    let mut head = Vec::with_capacity(256);
    let mut buf = [0u8; 1024];
    loop {
        if head.len() >= MAX_HEADER_BYTES {
            return Err(io::Error::other(
                "HTTP proxy CONNECT response header too large",
            ));
        }
        if let Some(pos) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            let status_line = String::from_utf8_lossy(&head[..pos])
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            if !status_line.starts_with("HTTP/1.") {
                return Err(io::Error::other(format!(
                    "invalid HTTP proxy CONNECT response: {status_line}"
                )));
            }
            let code: u16 = status_line
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse().ok())
                .ok_or_else(|| io::Error::other("malformed CONNECT status line"))?;
            if !(200..300).contains(&code) {
                return Err(io::Error::other(format!(
                    "HTTP proxy CONNECT rejected ({code}): {status_line}"
                )));
            }
            return Ok(stream);
        }
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Err(io::Error::other(
                "HTTP proxy closed connection during CONNECT",
            ));
        }
        head.extend_from_slice(&buf[..n]);
    }
}
