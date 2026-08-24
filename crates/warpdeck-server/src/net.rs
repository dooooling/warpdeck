//! IP/CIDR 解析与匹配（P5-009 引入；P13-C 起为网关 allowlist 的唯一来源）。
//!
//! 严格语义：拒绝主机位非零的 CIDR（IPv4/IPv6 同等严格），非法条目在
//! 进入配置前即失败。原实现位于 `proxy::config`（GOST YAML 渲染层），
//! GOST 移除后迁至此处（DESIGN §35）。

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 已校验的 CIDR：裸 IP 视为 /32 或 /128。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpNetwork {
    V4 {
        net: Ipv4Addr,
        prefix: u8,
    },
    V6 {
        net: Ipv6Addr,
        prefix: u8,
    },
    /// 无前缀的裸 IP。
    Exact(IpAddr),
}

impl fmt::Display for IpNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpNetwork::V4 { net, prefix } => write!(f, "{net}/{prefix}"),
            IpNetwork::V6 { net, prefix } => write!(f, "{net}/{prefix}"),
            IpNetwork::Exact(ip) => write!(f, "{ip}"),
        }
    }
}

impl IpNetwork {
    /// `ip` 是否落在网络内（Exact = 精确相等）。
    pub fn contains(&self, ip: IpAddr) -> bool {
        match self {
            IpNetwork::Exact(exact) => *exact == ip,
            IpNetwork::V4 { net, prefix } => match ip {
                IpAddr::V4(v4) => {
                    let bits = u32::from(*prefix).min(32);
                    let mask = if bits == 0 {
                        0
                    } else {
                        u32::MAX << (32 - bits)
                    };
                    (u32::from(v4) & mask) == (u32::from(*net) & mask)
                }
                _ => false,
            },
            IpNetwork::V6 { net, prefix } => match ip {
                IpAddr::V6(v6) => {
                    let bits = u32::from(*prefix).min(128);
                    let mask = if bits == 0 {
                        0
                    } else {
                        u128::MAX << (128 - bits)
                    };
                    (u128::from(v6) & mask) == (u128::from(*net) & mask)
                }
                _ => false,
            },
        }
    }
}

/// CIDR 解析错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid CIDR `{input}`: {detail}")]
pub struct CidrError {
    pub input: String,
    pub detail: String,
}

/// 严格解析 IPv4/IPv6 裸地址与 CIDR。
///
/// 支持 `192.168.0.0/16`、`10.0.0.1`、`::1/128`、`fe80::1`；
/// 拒绝主机位非零的 CIDR（如 `192.168.0.1/16`，网络内主机位必须为零）。
pub fn parse_cidr(input: &str) -> Result<IpNetwork, CidrError> {
    let input = input.trim();
    let invalid = |detail: String| CidrError {
        input: input.to_string(),
        detail,
    };
    match input.split_once('/') {
        None => {
            let ip: IpAddr = input.parse().map_err(|_| {
                invalid("must be a bare IPv4/IPv6 address or CIDR like 192.168.0.0/16".into())
            })?;
            Ok(IpNetwork::Exact(ip))
        }
        Some((addr, prefix)) => {
            let prefix: u8 = prefix
                .parse()
                .map_err(|_| invalid("prefix must be a number".into()))?;
            let is_v4 = addr.contains('.');
            match addr.parse::<Ipv4Addr>() {
                Ok(net) if is_v4 => {
                    if prefix > 32 {
                        return Err(invalid(format!("IPv4 prefix must be <= 32, got {prefix}")));
                    }
                    // 主机位必须为零（严格 CIDR）。
                    let host_bits = 32u32.saturating_sub(u32::from(prefix));
                    let host_mask = if host_bits == 32 {
                        0xFFFF_FFFF
                    } else {
                        (1u32 << host_bits) - 1
                    };
                    if u32::from(net) & host_mask != 0 {
                        return Err(invalid(format!(
                            "host bits set: use the network address (e.g. {net}/{prefix} with host bits zeroed)"
                        )));
                    }
                    Ok(IpNetwork::V4 { net, prefix })
                }
                Ok(_) => Err(invalid("mixed IPv4 address and prefix".into())),
                Err(_) => match addr.parse::<Ipv6Addr>() {
                    Ok(net) => {
                        if prefix > 128 {
                            return Err(invalid(format!(
                                "IPv6 prefix must be <= 128, got {prefix}"
                            )));
                        }
                        // IPv6 与 IPv4 同等严格——主机位必须为零。
                        let host_bits = 128u32.saturating_sub(u32::from(prefix));
                        let net_bits = u128::from(net) >> host_bits << host_bits;
                        if u128::from(net) != net_bits {
                            let zeroed: Ipv6Addr = net_bits.into();
                            return Err(invalid(format!(
                                "host bits set: use the network address (e.g. {zeroed}/{prefix} with host bits zeroed)"
                            )));
                        }
                        Ok(IpNetwork::V6 { net, prefix })
                    }
                    Err(_) => Err(invalid("address part is not a valid IP".into())),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_bare_ips() {
        assert_eq!(
            parse_cidr("10.0.0.1").unwrap(),
            IpNetwork::Exact("10.0.0.1".parse().unwrap())
        );
        assert_eq!(
            parse_cidr("fe80::1").unwrap(),
            IpNetwork::Exact("fe80::1".parse().unwrap())
        );
    }

    #[test]
    fn accepts_valid_cidrs() {
        assert_eq!(
            parse_cidr("192.168.0.0/16").unwrap(),
            IpNetwork::V4 {
                net: "192.168.0.0".parse().unwrap(),
                prefix: 16
            }
        );
        assert_eq!(
            parse_cidr("::1/128").unwrap(),
            IpNetwork::V6 {
                net: "::1".parse().unwrap(),
                prefix: 128
            }
        );
    }

    #[test]
    fn rejects_ipv6_host_bits() {
        assert!(
            parse_cidr("2001:db8::1/64").is_err(),
            "host bits set must fail"
        );
        assert!(matches!(
            parse_cidr("2001:db8::/64"),
            Ok(IpNetwork::V6 { prefix: 64, .. })
        ));
        assert!(matches!(
            parse_cidr("::1/128"),
            Ok(IpNetwork::V6 { prefix: 128, .. })
        ));
    }

    #[test]
    fn rejects_host_bits_set() {
        let err = parse_cidr("192.168.0.1/16").unwrap_err();
        assert!(err.to_string().contains("host bits"));
    }

    #[test]
    fn rejects_bad_prefix() {
        assert!(parse_cidr("192.168.0.0/33").is_err());
        assert!(parse_cidr("10.0.0.0/abc").is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_cidr("not-an-ip").is_err());
        assert!(parse_cidr("300.1.1.0/24").is_err());
    }

    #[test]
    fn contains_matches_v4_v6_and_exact() {
        let v4 = parse_cidr("192.168.1.0/24").unwrap();
        assert!(v4.contains("192.168.1.7".parse().unwrap()));
        assert!(!v4.contains("192.168.2.7".parse().unwrap()));
        assert!(!v4.contains("2001:db8::1".parse().unwrap()));

        let v6 = parse_cidr("2001:db8::/32").unwrap();
        assert!(v6.contains("2001:db8:1234::1".parse().unwrap()));
        assert!(!v6.contains("2001:db9::1".parse().unwrap()));
        assert!(!v6.contains("10.0.0.1".parse().unwrap()));

        let exact = parse_cidr("10.0.0.1").unwrap();
        assert!(exact.contains("10.0.0.1".parse().unwrap()));
        assert!(!exact.contains("10.0.0.2".parse().unwrap()));

        // /0 全通配。
        let all = parse_cidr("0.0.0.0/0").unwrap();
        assert!(all.contains("203.0.113.9".parse().unwrap()));
    }

    #[test]
    fn display_roundtrip() {
        assert_eq!(
            parse_cidr("192.168.0.0/16").unwrap().to_string(),
            "192.168.0.0/16"
        );
        assert_eq!(parse_cidr("10.0.0.1").unwrap().to_string(), "10.0.0.1");
        assert_eq!(
            parse_cidr("2001:db8::/64").unwrap().to_string(),
            "2001:db8::/64"
        );
    }
}
