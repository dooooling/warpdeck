//! 数据面探测（P4-004/005）：通过实例的 SOCKS5 内部端口请求 Cloudflare
//! trace，验证 `warp=on` 并记录出口 IP / colo / 延迟。
//!
//! DESIGN §14.3：只有 `warp=on` 或符合预期 WARP 状态时才认为数据面健康；
//! 探测必须走实例自己的代理端口（127.0.0.1:40000+id 回环内部端口）。
//!
//! `DataPlaneProber` trait 是领域接缝：真实实现走 SOCKS5 + TLS + HTTP/1.1
//! 最小客户端（不依赖外部 curl 程序，宿主/容器行为一致）；测试注入
//! `FakeDataPlaneProber`（fake.rs）覆盖 timeout / malformed / warp=off /
//! 缺字段 / 延迟等变体（FLUXING 时也无需真实出网）。

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

mod http;
mod socks5;
mod trace;

pub use trace::TraceResult;

use self::http::http_connect;
use self::socks5::socks5_connect;
use self::trace::{decode_http_body, parse_trace};

/// Cloudflare trace 目标。
pub const TRACE_HOST: &str = "cloudflare.com";
pub const TRACE_PATH: &str = "/cdn-cgi/trace";

/// 双地址族探测目标（P13-001）：用 Cloudflare 自家 anycast 的 IP 字面量直连，
/// 目标地址族决定出口地址族——v4 经 WARP CGNAT 出口，v6 为隧道原生地址。
/// 探测响应同样含 `warp=` 字段，健康判据与 hostname 路径一致。
pub const TRACE_V4_TARGET: &str = "1.1.1.1";
pub const TRACE_V6_TARGET: &str = "2606:4700:4700::1111";

/// 数据面验证走哪个代理协议（对应启用中的外部 listener）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeProto {
    Socks5,
    Http,
}

/// 一次双地址族探测报告（P13-001）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPlaneReport {
    /// v4 目标探测结果（`None` = 该族探测失败）。
    pub trace_v4: Option<TraceResult>,
    /// v6 目标探测结果（`None` = 该族探测失败）。
    pub trace_v6: Option<TraceResult>,
    /// 探测往返延迟（v4 优先；v4 失败时取 v6，秒级别精度，P4-005）。
    pub latency_ms: u64,
}

impl DataPlaneReport {
    /// 数据面健康判据：任一地址族探测成功且 `warp=on`
    /// （AGENTS.md：Healthy 需要真实数据面 `warp=on`，不只是 PID 存活）。
    pub fn warp_on(&self) -> bool {
        [&self.trace_v4, &self.trace_v6]
            .into_iter()
            .flatten()
            .any(|t| t.warp.as_deref() == Some("on"))
    }

    /// 接入的 Cloudflare 数据中心（v4 优先；两族同 egress 区域）。
    pub fn colo(&self) -> Option<String> {
        self.trace_v4
            .as_ref()
            .and_then(|t| t.colo.clone())
            .or_else(|| self.trace_v6.as_ref().and_then(|t| t.colo.clone()))
    }

    /// v4 出口 IP（v4 探测成功且响应含 `ip=` 时）。
    pub fn exit_ip_v4(&self) -> Option<IpAddr> {
        self.trace_v4.as_ref()?.ip.as_deref()?.parse().ok()
    }

    /// v6 出口 IP（v6 探测成功且响应含 `ip=` 时）。
    pub fn exit_ip_v6(&self) -> Option<IpAddr> {
        self.trace_v6.as_ref()?.ip.as_deref()?.parse().ok()
    }
}

/// 探测失败原因（透传给 registry.last_error，不含 secret）。
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProbeError {
    #[error("proxy connect failed: {0}")]
    ProxyConnect(String),
    #[error("socks5 handshake failed: {0}")]
    Socks5(String),
    #[error("http proxy connect failed: {0}")]
    HttpConnect(String),
    #[error("tls handshake failed: {0}")]
    Tls(String),
    #[error("http request failed: {0}")]
    Http(String),
    #[error("probe timed out after {0:?}")]
    Timeout(Duration),
}

/// 数据面探测接缝（P4-004）。测试注入 Fake。
#[async_trait]
pub trait DataPlaneProber: Send + Sync {
    /// 经 `proto` 协议（启用中的外部 listener）探测 `port` 上的代理数据面。
    /// 成功 = 完整拿到 trace 响应（是否 `warp=on` 由健康判定层决定，不在本 trait 内）。
    async fn probe(&self, proto: ProbeProto, port: u16) -> Result<DataPlaneReport, ProbeError>;
}

/// 真实探测：SOCKS5 隧道 → TLS → HTTP GET trace。
pub struct RealDataPlaneProber {
    timeout: Duration,
    connector: TlsConnector,
}

impl RealDataPlaneProber {
    pub fn new(timeout: Duration) -> Self {
        let mut roots = RootCertStore::empty();
        // Mozilla 根证书（与 curl 行为一致；容器内无需额外 CA 配置）。
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Self {
            timeout,
            connector: TlsConnector::from(Arc::new(config)),
        }
    }
}

impl Default for RealDataPlaneProber {
    fn default() -> Self {
        // 数据面探测通常 1-3s 内完成；P2 实测首连存在 10s+ 的建连窗口，
        // 启动期验证由 bounded retry 包住，单次探测超时给 10s。
        Self::new(Duration::from_secs(10))
    }
}

#[async_trait]
impl DataPlaneProber for RealDataPlaneProber {
    async fn probe(&self, proto: ProbeProto, port: u16) -> Result<DataPlaneReport, ProbeError> {
        // 双地址族并行探测（P13-001）。任一成功即整体成功；两者皆败返回
        // v4 侧错误（主路径），与单探时代的错误语义一致。
        let (v4, v6) = tokio::join!(
            trace_probe_one(proto, self.timeout, &self.connector, port, TRACE_V4_TARGET),
            trace_probe_one(proto, self.timeout, &self.connector, port, TRACE_V6_TARGET),
        );
        let latency_ms = match (&v4, &v6) {
            (Ok(a), _) => a.1,
            (Err(_), Ok(b)) => b.1,
            (Err(e), Err(_)) => return Err(e.clone()),
        };
        Ok(DataPlaneReport {
            trace_v4: v4.ok().map(|(t, _)| t),
            trace_v6: v6.ok().map(|(t, _)| t),
            latency_ms,
        })
    }
}

/// 单目标探测（`target` 决定地址族：IP 字面量 → SOCKS5 ATYP 直连，无 DNS）。
/// 成功返回该族的 trace 结果与往返延迟。
async fn trace_probe_one(
    proto: ProbeProto,
    timeout: Duration,
    connector: &TlsConnector,
    port: u16,
    target: &str,
) -> Result<(TraceResult, u64), ProbeError> {
    let started = tokio::time::Instant::now();
    let server_name = tokio_rustls::rustls::pki_types::ServerName::try_from(target)
        .map_err(|e| ProbeError::Socks5(e.to_string()))?
        .to_owned();
    let proxy_addr = std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port);
    let raw = tokio::time::timeout(timeout, async {
        let connect = match proto {
            ProbeProto::Socks5 => socks5_connect(proxy_addr, target, 443).await,
            ProbeProto::Http => http_connect(proxy_addr, target, 443).await,
        };
        let sock = connect.map_err(|e| {
            if e.kind() == std::io::ErrorKind::ConnectionRefused {
                ProbeError::ProxyConnect(format!("{e} (port {port})"))
            } else {
                match proto {
                    ProbeProto::Socks5 => ProbeError::Socks5(e.to_string()),
                    ProbeProto::Http => ProbeError::HttpConnect(e.to_string()),
                }
            }
        })?;
        let stream = connector
            .connect(
                server_name.clone(),
                sock,
            )
            .await
            .map_err(|e| ProbeError::Tls(e.to_string()))?;
        let mut stream = stream;
        // Host 头必须与目标一致（RFC 7230：IPv6 字面量加方括号）：
        // IP 字面量目标不能复用固定域名 Host，否则 CF 边缘拒绝
        // （421 Misdirected / 403）。
        let request = format!(
            "GET {TRACE_PATH} HTTP/1.1\r\nHost: {}\r\nUser-Agent: warpdeck-health/0.1\r\nConnection: close\r\nAccept: */*\r\n\r\n",
            host_header(target)
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| ProbeError::Http(e.to_string()))?;
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .map_err(|e| ProbeError::Http(e.to_string()))?;
        Ok::<_, ProbeError>(response)
    })
    .await
    .map_err(|_| ProbeError::Timeout(timeout))??;

    let (headers, body) = match raw.iter().position(|&b| b == b'\n') {
        Some(_) if raw.len() >= 4 => split_headers(&raw),
        _ => return Err(ProbeError::Http("empty or malformed response".into())),
    };
    if !status_ok(headers) {
        return Err(ProbeError::Http(format!(
            "unexpected status: {}",
            String::from_utf8_lossy(headers)
        )));
    }
    let decoded = decode_http_body(body);
    let text = String::from_utf8_lossy(&decoded).to_string();
    let duration = started.elapsed().as_millis() as u64;
    Ok((parse_trace(&text), duration))
}

/// Host 头值：IPv6 字面量按 RFC 7230 §5.4 加方括号，其余原样。
fn host_header(target: &str) -> String {
    if target.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv6()) {
        format!("[{target}]")
    } else {
        target.to_string()
    }
}

/// 拆响应头/实体（首个空行分隔）。
fn split_headers(raw: &[u8]) -> (&[u8], &[u8]) {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .unwrap_or(raw.len());
    (&raw[..sep], raw.get(sep..).unwrap_or(&[]))
}

/// 判断状态行是否为 2xx。
fn status_ok(headers: &[u8]) -> bool {
    String::from_utf8_lossy(headers)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|code| (200..300).contains(&code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_target_gets_bracketed_host_header() {
        assert_eq!(host_header("1.1.1.1"), "1.1.1.1");
        assert_eq!(
            host_header("2606:4700:4700::1111"),
            "[2606:4700:4700::1111]"
        );
        assert_eq!(host_header("cloudflare.com"), "cloudflare.com");
    }

    #[test]
    fn split_headers_and_status_ok() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nwarp=on\n";
        let (headers, body) = split_headers(raw);
        assert!(status_ok(headers));
        assert_eq!(body, b"warp=on\n");
    }

    #[test]
    fn non_2xx_status_is_flagged() {
        let raw = b"HTTP/1.1 503 Service Unavailable\r\n\r\n";
        let (headers, _) = split_headers(raw);
        assert!(!status_ok(headers));
    }

    #[test]
    fn malformed_response_degrades_to_all_raw() {
        let raw = b"garbage";
        let (headers, body) = split_headers(raw);
        assert!(!status_ok(headers));
        assert_eq!(body, b"");
        assert!(!headers.is_empty());
    }
}
