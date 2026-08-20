//! 最小 SOCKS5 CONNECT 客户端（P4-004 数据面探测用）。
//!
//! 只实现了本项目需要的子集：TCP over SOCKS5，无认证，支持域名目标
//! （ATYP=0x03）与 IP 字面量目标（ATYP=0x01 IPv4 / 0x04 IPv6，P13-001
//! 双地址族探测用：IP 直连无 DNS 参与，地址族由目标字面量决定）。
//! 校验 agent：本模块是领域代码探测路径，不代表用户流量。
//! 协议细节见 RFC 1928。

use std::io;
use std::net::{IpAddr, SocketAddr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 建立到 `target_host` 的 SOCKS5 CONNECT 隧道（通过 `proxy_addr`）。
///
/// `target_host` 为 IP 字面量时走 ATYP IP 直连，否则按域名（ATYP 0x03）。
/// 返回已握手的隧道流；失败时流已关闭，调用方无需清理。
pub async fn socks5_connect(
    proxy_addr: SocketAddr,
    target_host: &str,
    target_port: u16,
) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect(proxy_addr).await?;
    handshake(&mut stream, target_host, target_port).await?;
    Ok(stream)
}

async fn handshake(stream: &mut TcpStream, host: &str, port: u16) -> io::Result<()> {
    // 协商：仅支持无认证方式。
    stream
        .write_all(&[0x05, 0x01, 0x00])
        .await
        .map_err(io::Error::other)?;
    let mut method = [0u8; 2];
    stream.read_exact(&mut method).await?;
    if method[0] != 0x05 || method[1] != 0x00 {
        return Err(io::Error::other(format!(
            "SOCKS5 no-auth not accepted (method reply {:#04x})",
            method[1]
        )));
    }

    // CONNECT 请求：ATYP 按目标类型（IP 字面量 → 0x01/0x04，域名 → 0x03）。
    let mut request = Vec::with_capacity(16);
    request.extend_from_slice(&[0x05, 0x01, 0x00]);
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            request.push(0x01);
            request.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            request.push(0x04);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            if host.len() > 255 {
                return Err(io::Error::other("SOCKS5 target host too long"));
            }
            request.push(0x03);
            request.push(host.len() as u8);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await?;

    // 响应头：VER REP RSV ATYP（4 字节）+ 可变 BND.ADDR。
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != 0x05 {
        return Err(io::Error::other("invalid SOCKS5 reply version"));
    }
    if head[1] != 0x00 {
        return Err(io::Error::other(format!(
            "SOCKS5 CONNECT rejected (rep {:#04x})",
            head[1]
        )));
    }
    let bnd_len = match head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            len[0] as usize
        }
        atyp => return Err(io::Error::other(format!("unsupported ATYP {atyp:#04x}"))),
    };
    let mut bnd = vec![0u8; bnd_len + 2];
    stream.read_exact(&mut bnd).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// 兜底 SOCKS5 服务器：记录握手，回复 NO-AUTH + CONNECT success。
    async fn run_fake_socks5(listener: TcpListener) -> (String, u16) {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut greeting = [0u8; 3];
        sock.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [0x05, 0x01, 0x00]);
        sock.write_all(&[0x05, 0x00]).await.unwrap();
        // CONNECT 固定部分 7 字节 = VER CMD RSV ATYP LEN + port(2)。
        let expected = 7usize;
        let mut conn = [0u8; 512];
        let mut used = 0;
        while used < expected {
            let n = sock.read(&mut conn[used..]).await.unwrap();
            used += n;
        }
        assert_eq!(&conn[..4], &[0x05, 0x01, 0x00, 0x03]);
        let host_len = conn[4] as usize;
        while used < expected + host_len {
            let n = sock.read(&mut conn[used..]).await.unwrap();
            used += n;
        }
        let host = String::from_utf8_lossy(&conn[5..5 + host_len]).to_string();
        let port = u16::from_be_bytes([conn[5 + host_len], conn[6 + host_len]]);
        // BND.ADDR = 0.0.0.0:0。
        sock.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        (host, port)
    }

    /// 记录 IP 字面量 CONNECT（ATYP 0x01/0x04）的兜底服务器：返回 (atyp, ip_bytes, port)。
    async fn run_fake_socks5_ip(listener: TcpListener) -> (u8, Vec<u8>, u16) {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut greeting = [0u8; 3];
        sock.read_exact(&mut greeting).await.unwrap();
        sock.write_all(&[0x05, 0x00]).await.unwrap();
        let mut conn = [0u8; 32];
        let mut used = 0;
        while used < 4 {
            let n = sock.read(&mut conn[used..]).await.unwrap();
            used += n;
        }
        let atyp = conn[3];
        let addr_len = match atyp {
            0x01 => 4usize,
            0x04 => 16usize,
            other => panic!("unexpected ATYP {other:#04x}"),
        };
        while used < 4 + addr_len + 2 {
            let n = sock.read(&mut conn[used..]).await.unwrap();
            used += n;
        }
        let ip = conn[4..4 + addr_len].to_vec();
        let port = u16::from_be_bytes([conn[4 + addr_len], conn[5 + addr_len]]);
        sock.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        (atyp, ip, port)
    }

    #[tokio::test]
    async fn connects_via_no_auth_and_forwards_domain() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(run_fake_socks5(listener));

        let mut stream = socks5_connect(addr, "cloudflare.com", 443).await.unwrap();
        stream.write_all(b"hi").await.unwrap();
        let (host, port) = server.await.unwrap();
        assert_eq!(host, "cloudflare.com");
        assert_eq!(port, 443);
    }

    #[tokio::test]
    async fn ipv4_literal_uses_atyp_1() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(run_fake_socks5_ip(listener));

        socks5_connect(addr, "1.1.1.1", 443).await.unwrap();
        let (atyp, ip, port) = server.await.unwrap();
        assert_eq!(atyp, 0x01);
        assert_eq!(ip, vec![1, 1, 1, 1]);
        assert_eq!(port, 443);
    }

    #[tokio::test]
    async fn ipv6_literal_uses_atyp_4() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(run_fake_socks5_ip(listener));

        socks5_connect(addr, "2606:4700:4700::1111", 443)
            .await
            .unwrap();
        let (atyp, ip, port) = server.await.unwrap();
        assert_eq!(atyp, 0x04);
        assert_eq!(
            ip,
            vec![0x26, 0x06, 0x47, 0x00, 0x47, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0x11, 0x11]
        );
        assert_eq!(port, 443);
    }

    #[tokio::test]
    async fn rejects_when_no_auth_not_offered() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut greeting = [0u8; 3];
            sock.read_exact(&mut greeting).await.unwrap();
            sock.write_all(&[0x05, 0xFF]).await.unwrap();
        });

        let err = socks5_connect(addr, "cloudflare.com", 443)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no-auth not accepted"));
    }

    #[tokio::test]
    async fn reports_connect_rejection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut greeting = [0u8; 3];
            sock.read_exact(&mut greeting).await.unwrap();
            sock.write_all(&[0x05, 0x00]).await.unwrap();
            let mut conn = [0u8; 512];
            let mut used = 0;
            while used < 7 {
                let n = sock.read(&mut conn[used..]).await.unwrap();
                used += n;
            }
            sock.write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
        });

        let err = socks5_connect(addr, "cloudflare.com", 443)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("CONNECT rejected (rep 0x05)"));
    }

    #[tokio::test]
    async fn reports_connection_refused() {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let err = socks5_connect(addr, "cloudflare.com", 443)
            .await
            .unwrap_err();
        assert!(err.kind() == io::ErrorKind::ConnectionRefused);
    }
}
