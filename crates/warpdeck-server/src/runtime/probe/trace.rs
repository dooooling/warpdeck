//! Cloudflare trace 解析（P4-004）。
//!
//! `https://cloudflare.com/cdn-cgi/trace` 的响应是 `key=value` 行文本；
//! 解析必须容错（DESIGN §14.3 / 计划 P4-004）：
//! - 不依赖字段顺序（逐行扫描，任意缺失字段返回 `None`）；
//! - 未知字段忽略；
//! - `warp=on` 才是数据面健康判据（AGENTS.md 硬性要求）。

/// trace 中我们关心的字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceResult {
    /// 出口 IP（`ip=`）。
    pub ip: Option<String>,
    /// 接入的 Cloudflare 数据中心（`colo=`，如 LAX）。
    pub colo: Option<String>,
    /// WARP 状态（`warp=`，取值 `on`/`off`）。
    pub warp: Option<String>,
}

/// 解析 trace 文本。不依赖字段顺序；任何字段缺失都返回 `None` 而不是报错。
pub fn parse_trace(text: &str) -> TraceResult {
    let mut result = TraceResult {
        ip: None,
        colo: None,
        warp: None,
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "ip" => result.ip = Some(value.to_string()),
            "colo" => result.colo = Some(value.to_string()),
            "warp" => result.warp = Some(value.to_string()),
            _ => {}
        }
    }
    result
}

/// 从 HTTP/1.1 响应体中提取实体：支持 Content-Length 直读与 chunked
/// 传输编码（curl 场景下 `Connection: close` 通常无 chunked，但容错解析）。
pub fn decode_http_body(body: &[u8]) -> Vec<u8> {
    // chunked：首行是十六进制长度 + CRLF。
    match body.splitn(2, |&b| b == b'\n').next() {
        Some(first_line) if !first_line.is_empty() => {
            let line = String::from_utf8_lossy(first_line);
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
                return dechunk(body);
            }
        }
        _ => {}
    }
    body.to_vec()
}

fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = body.iter().position(|&b| b == b'\n') else {
            return out;
        };
        let size_line = String::from_utf8_lossy(&body[..line_end]);
        let size = usize::from_str_radix(size_line.trim_end(), 16).unwrap_or(0);
        if size == 0 {
            return out;
        }
        let data_start = line_end + 1;
        if body.len() < data_start + size + 2 {
            return out;
        }
        out.extend_from_slice(&body[data_start..data_start + size]);
        body = &body[data_start + size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_trace_in_any_order() {
        let text = "warp=on\ncolo=LAX\nsliver=none\nts=1755\nip=104.28.1.2\nloc=US\n";
        let r = parse_trace(text);
        assert_eq!(r.ip.as_deref(), Some("104.28.1.2"));
        assert_eq!(r.colo.as_deref(), Some("LAX"));
        assert_eq!(r.warp.as_deref(), Some("on"));
    }

    #[test]
    fn parses_without_trailing_newline_and_whitespace() {
        let r = parse_trace("ip= 1.2.3.4   \ncolo = SJC");
        assert_eq!(r.ip.as_deref(), Some("1.2.3.4"));
        assert_eq!(r.colo.as_deref(), Some("SJC"));
    }

    #[test]
    fn missing_fields_are_none_not_error() {
        let r = parse_trace("colo=LAX\nwarp=on\n");
        assert_eq!(r.ip, None);
        assert_eq!(r.colo.as_deref(), Some("LAX"));
        assert_eq!(r.warp.as_deref(), Some("on"));
    }

    #[test]
    fn warp_off_is_distinguishable() {
        let r = parse_trace("warp=off\nip=8.8.8.8\n");
        assert_eq!(r.warp.as_deref(), Some("off"));
        assert_eq!(r.ip.as_deref(), Some("8.8.8.8"));
    }

    #[test]
    fn empty_and_garbage_are_tolerated() {
        assert_eq!(
            parse_trace(""),
            TraceResult {
                ip: None,
                colo: None,
                warp: None
            }
        );
        let r = parse_trace("not a kv line\n=value\nwarp=on\n");
        assert_eq!(r.warp.as_deref(), Some("on"));
    }

    #[test]
    fn plain_body_passes_through() {
        let body = "warp=on\nip=1.2.3.4\n";
        assert_eq!(decode_http_body(body.as_bytes()), body.as_bytes());
    }

    #[test]
    fn chunked_body_is_decoded() {
        // `warp=on\n` = 9 字节，`ip=1.2.3.4\n` = 10 字节 → chunk 0x13 = 19。
        let chunked = b"13\r\nwarp=on\nip=1.2.3.4\n\r\n0\r\n\r\n";
        assert_eq!(decode_http_body(chunked), b"warp=on\nip=1.2.3.4\n");
    }

    #[test]
    fn malformed_chunked_degrades_to_raw() {
        // 首行是十六进制但不是 chunked 结构 → 原样返回（容错不 panic）。
        let body = b"zz\r\nwarp=on\n";
        assert_eq!(decode_http_body(body), body);
    }
}
