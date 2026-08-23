//! P5-001 GostConfig 领域模型 + P5-002 渲染（手动 YAML 输出，无序列化依赖）+ P5-009 IP Allowlist 严格校验。
//!
//! GOST v3 配置结构（参考 go-gost/gost `gost.yml` 与 gost.run 文档）：
//! - `services`：socks5（:11080）/ http（:18080）两个 listener；
//! - `handler.auther` 引用 `authers`（P5-008 认证）；
//! - `service.admission` 引用 `admissions`（whitelist 模式 = P5-009 allowlist）；
//! - `service.climiter` / `service.rlimiter` 引用 `climiters` / `rlimiters`（P5-010）；
//! - `handler.chain` 引用 `chains`（所有 listener 共享 chain-0 = Healthy 轮询池）；
//! - `chain.hops[].nodes` + `hop.selector`（strategy: round = P5-003 轮询池）。
//!
//! 渲染为手写字符串而非 serde_yaml：GOST 配置项多且带特殊语法（如
//! `- '$ 1000'` 的限制区间隔），序列化库会引入歧义；类型模型 + 格式化函数
//! 的输出由快照测试锁定，改动有测试兜底。

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::config::{HTTP_PORT, SOCKS5_PORT};

/// 节点池路由策略（DESIGN §13.2：MVP 只实现 round_robin）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingStrategy {
    RoundRobin,
}

impl fmt::Display for RoutingStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RoutingStrategy::RoundRobin => write!(f, "round"),
        }
    }
}

/// 上游节点：一个 Healthy WARP 实例的内部 SOCKS5 端点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyNode {
    pub name: String,
    /// `127.0.0.1:40000+i`（实例内部端口，loopback only）。
    pub addr: String,
}

/// YAML 双引号标量转义（P1 审查 R1#8）：凭据渲染进 GOST 配置的唯一安全形态。
/// 处理 `"` `\` 与控制字符（含换行——上游虽已拒绝，此处纵深防御），
/// 防止 `#` 行内注释、前导 `*/&/!` 等截断或误解裸标量。
fn yaml_double_quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 代理认证（P5-008）。明文仅存在于配置写入瞬间与内存中；
/// HTTP API 侧由 P7/P8 加密存储，GET 永不回显。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyAuth {
    pub username: String,
    pub password: String,
}

/// 生成参数。静态部分（认证/allowlist/limit）由应用层配置注入，
/// 动态部分（healthy 节点池）由 HealthyPoolBuilder 提供。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GostConfig {
    /// SOCKS5 listener 开关（P5-006）。
    pub socks5_enabled: bool,
    /// HTTP listener 开关（P5-007）。
    pub http_enabled: bool,
    pub auth: Option<ProxyAuth>,
    /// 白名单 CIDR（P5-009）：非空时只放行这些网段；空 = 不限制。
    pub allowlist: Vec<IpNetwork>,
    /// 最大并发连接（P5-010）。
    pub max_connections: Option<u32>,
    /// 最大每秒请求数/连接数（P5-010）。
    pub max_rps: Option<u32>,
    pub nodes: Vec<ProxyNode>,
    pub routing: RoutingStrategy,
}

/// 已校验的 CIDR（P5-009）：非法 CIDR 在进入 config 前即失败。
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
    /// 无前缀的裸 IP（/32 或 /128）。
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

/// 配置错误（渲染侧输入问题；不含 GOST 进程错误）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("invalid CIDR `{input}`: {detail}")]
    InvalidCidr { input: String, detail: String },
    #[error("password must not be empty when auth is enabled")]
    EmptyPassword,
    #[error("password must not appear in plaintext except in the config file")]
    SecretInPlaintext,
    #[error("username must not contain newline (would corrupt the generated YAML)")]
    UsernameWithNewline,
    #[error("no listeners enabled (socks5 and http both disabled)")]
    NoListeners,
}

/// 严格解析 IPv4/IPv6 裸地址与 CIDR（P5-009：非法 CIDR 在保存前失败）。
///
/// 支持 `192.168.0.0/16`、`10.0.0.1`、`::1/128`、`fe80::1`；
/// 拒绝主机位非零的 CIDR（如 `192.168.0.1/16`，网络内主机位必须为零）。
pub fn parse_cidr(input: &str) -> Result<IpNetwork, ConfigError> {
    let input = input.trim();
    let invalid = |detail: String| ConfigError::InvalidCidr {
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
                            "host bits set: use the network address (e.g. {}/{} with host bits zeroed)",
                            net, prefix
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
                        Ok(IpNetwork::V6 { net, prefix })
                    }
                    Err(_) => Err(invalid("address part is not a valid IP".into())),
                },
            }
        }
    }
}

impl GostConfig {
    /// P5-009：以严格解析器校验 allowlist；同时拒绝明显会泄漏到日志的明文 secret。
    pub fn new(
        socks5_enabled: bool,
        http_enabled: bool,
        auth: Option<ProxyAuth>,
        allowlist: &[String],
        max_connections: Option<u32>,
        max_rps: Option<u32>,
        nodes: Vec<ProxyNode>,
    ) -> Result<Self, ConfigError> {
        if !socks5_enabled && !http_enabled {
            return Err(ConfigError::NoListeners);
        }
        if let Some(a) = &auth {
            if a.password.is_empty() {
                return Err(ConfigError::EmptyPassword);
            }
            // P5-008：密码不得在配置外出现（渲染时仅写入配置文件）。
            if a.password.contains('\n') {
                return Err(ConfigError::SecretInPlaintext);
            }
            // review 补强：username 与 password 一样是渲染进 YAML 的裸标量，
            // 换行会破坏 authers 结构（YAML 注入），保存前拒绝。
            if a.username.contains('\n') {
                return Err(ConfigError::UsernameWithNewline);
            }
        }
        let allowlist = allowlist
            .iter()
            .map(|s| parse_cidr(s))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            socks5_enabled,
            http_enabled,
            auth,
            allowlist,
            max_connections,
            max_rps,
            nodes,
            routing: RoutingStrategy::RoundRobin,
        })
    }

    /// 渲染为 GOST v3 YAML（P5-002 输出格式）。
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(2048);
        out.push_str("services:\n");
        if self.socks5_enabled {
            out.push_str(&self.render_service("socks5", &format!(":{SOCKS5_PORT}")));
        }
        if self.http_enabled {
            out.push_str(&self.render_service("http", &format!(":{HTTP_PORT}")));
        }
        if let Some(a) = &self.auth {
            out.push_str("authers:\n- name: auther-0\n  auths:\n");
            // P1 审查 R1#8：凭据必须双引号转义——裸标量遇 `#`（行内注释）、
            // 前导 `*/&/!` 等会被截断或误解，破坏 auther 结构。
            out.push_str(&format!(
                "  - username: {}\n    password: {}\n",
                yaml_double_quoted(&a.username),
                yaml_double_quoted(&a.password)
            ));
        }
        if !self.allowlist.is_empty() {
            out.push_str("admissions:\n- name: allowlist-0\n  whitelist: true\n  matchers:\n");
            for net in &self.allowlist {
                out.push_str(&format!("  - {net}\n"));
            }
        }
        if let Some(m) = self.max_connections {
            out.push_str(&format!(
                "climiters:\n- name: climiter-0\n  limits:\n  - '$ {}'\n",
                m
            ));
        }
        if let Some(r) = self.max_rps {
            out.push_str(&format!(
                "rlimiters:\n- name: rlimiter-0\n  limits:\n  - '$ {}'\n",
                r
            ));
        }
        out.push_str("log:\n  output: stderr\n  level: warn\n");
        out.push_str(&self.render_chain());
        out
    }

    fn render_service(&self, handler: &str, addr: &str) -> String {
        let mut s = String::new();
        s.push_str(&format!("- name: {handler}\n  addr: \"{addr}\"\n"));
        if !self.allowlist.is_empty() {
            s.push_str("  admission: allowlist-0\n");
        }
        s.push_str("  handler:\n");
        if self.auth.is_some() {
            s.push_str("    type: auto\n    auther: auther-0\n");
        } else {
            s.push_str(&format!("    type: {handler}\n"));
        }
        // GOST v3：socks5/http handler 的转发必须用 chain（service.handler.chain），
        // `forwarder` 字段只对 tcp/udp 端口转发生效——E2E 实测（2026-08）误用
        // forwarder 会导致直连而非走 upstream（P5 gate check 发现并修复）。
        s.push_str("    chain: chain-0\n");
        s.push_str("  listener:\n    type: tcp\n");
        if self.max_connections.is_some() {
            s.push_str("  climiter: climiter-0\n");
        }
        if self.max_rps.is_some() {
            s.push_str("  rlimiter: rlimiter-0\n");
        }
        s
    }

    /// chain 段：所有 listener 共享一个 chain（Healthy upstream 列表）。
    /// 空节点池（P5-005）：写入不可达占位节点 `no-upstream`——E2E 实测（2026-08）
    /// GOST 空 chain 会 fallback 直连（违反"空池不走 Direct"）；占位节点让
    /// listener 保持、请求明确失败（SOCKS5 error 97）。
    fn render_chain(&self) -> String {
        let mut s = String::new();
        s.push_str("chains:\n- name: chain-0\n  hops:\n  - name: hop-0\n");
        s.push_str(&format!(
            "    selector:\n      strategy: {}\n      maxFails: 1\n      failTimeout: 30s\n",
            self.routing
        ));
        s.push_str("    nodes:\n");
        if self.nodes.is_empty() {
            // 空池占位节点（P5-005）：GOST 空 chain 会 fallback 直连，
            // 占位不可达节点让请求明确失败（SOCKS5 error 97）。
            s.push_str("    - name: no-upstream\n      addr: \"127.0.0.1:1\"\n      connector:\n        type: socks5\n      dialer:\n        type: tcp\n");
        } else {
            for node in &self.nodes {
                s.push_str(&format!(
                    "    - name: {}\n      addr: \"{}\"\n      connector:\n        type: socks5\n      dialer:\n        type: tcp\n",
                    node.name, node.addr
                ));
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: i64) -> ProxyNode {
        ProxyNode {
            name: format!("warp-{id}"),
            addr: format!("127.0.0.1:{}", 40000 + id),
        }
    }

    #[test]
    fn render_basic_snapshot() {
        let cfg =
            GostConfig::new(true, true, None, &[], None, None, vec![node(0), node(1)]).unwrap();
        let yaml = cfg.render();
        let expected = "\
services:
- name: socks5
  addr: \":11080\"
  handler:
    type: socks5
    chain: chain-0
  listener:
    type: tcp
- name: http
  addr: \":18080\"
  handler:
    type: http
    chain: chain-0
  listener:
    type: tcp
log:
  output: stderr
  level: warn
chains:
- name: chain-0
  hops:
  - name: hop-0
    selector:
      strategy: round
      maxFails: 1
      failTimeout: 30s
    nodes:
    - name: warp-0
      addr: \"127.0.0.1:40000\"
      connector:
        type: socks5
      dialer:
        type: tcp
    - name: warp-1
      addr: \"127.0.0.1:40001\"
      connector:
        type: socks5
      dialer:
        type: tcp
";
        assert_eq!(yaml, expected);
    }

    #[test]
    fn render_with_auth_admission_and_limiters() {
        let cfg = GostConfig::new(
            true,
            false,
            Some(ProxyAuth {
                username: "alice".into(),
                password: "s3cret".into(),
            }),
            &["192.168.0.0/16".to_string(), "127.0.0.1".to_string()],
            Some(1000),
            Some(100),
            vec![node(2)],
        )
        .unwrap();
        let yaml = cfg.render();

        assert!(yaml.contains("handler:\n    type: auto\n    auther: auther-0\n"));
        assert!(yaml.contains(
            "authers:\n- name: auther-0\n  auths:\n  - username: \"alice\"\n    password: \"s3cret\"\n"
        ));
        assert!(yaml.contains("admission: allowlist-0"));
        assert!(yaml.contains("admissions:\n- name: allowlist-0\n  whitelist: true\n  matchers:\n  - 192.168.0.0/16\n  - 127.0.0.1\n"));
        assert!(yaml.contains("climiter: climiter-0"));
        assert!(yaml.contains("climiters:\n- name: climiter-0\n  limits:\n  - '$ 1000'\n"));
        assert!(yaml.contains("rlimiter: rlimiter-0"));
        assert!(yaml.contains("addr: \"127.0.0.1:40002\""));
        // HTTP listener 不出现。
        assert!(!yaml.contains("name: http"));
    }

    #[test]
    fn empty_pool_still_renders_listeners() {
        let cfg = GostConfig::new(true, true, None, &[], None, None, vec![]).unwrap();
        let yaml = cfg.render();
        assert!(yaml.contains("addr: \":11080\""));
        assert!(yaml.contains("addr: \":18080\""));
        // 空池占位节点 no-upstream（P5-005）：保证请求明确失败而非 GOST 直连 fallback。
        assert!(yaml.contains("name: no-upstream"));
        assert!(yaml.contains("addr: \"127.0.0.1:1\""));
        assert!(!yaml.contains("name: warp-"));
    }

    /// P1 审查 R1#8：含 `#` / 前导特殊字符 / 引号 / 反斜杠的凭据必须被双引号
    /// 转义，渲染结果仍是结构合法的单值标量（不被截断、不注入新键）。
    /// （换行凭据在 `new()` 即被拒绝——此处覆盖的是其余危险字符。）
    #[test]
    fn auth_credentials_are_yaml_escaped() {
        let cfg = GostConfig::new(
            true,
            false,
            Some(ProxyAuth {
                username: "user#1".into(),
                password: "\"pass word\" \\ x\ty".into(),
            }),
            &[],
            None,
            None,
            vec![node(1)],
        )
        .unwrap();
        let yaml = cfg.render();

        // `#` 后不再可能成为行内注释；引号/反斜杠转义；制表符折叠为 \t 字面量。
        assert!(yaml.contains(r#"username: "user#1""#));
        assert!(yaml.contains(r#"password: "\"pass word\" \\ x\ty""#));
        // 单行结构不被破坏（authers 段内不得出现第二个 username 键）。
        assert_eq!(yaml.matches("username:").count(), 1);
    }

    #[test]
    fn yaml_double_quoted_handles_control_chars() {
        assert_eq!(yaml_double_quoted("plain"), "\"plain\"");
        assert_eq!(yaml_double_quoted("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(
            yaml_double_quoted("l\nin\ter"),
            "\"l\\nin\\ter\"",
            "换行/制表符折叠为字面转义（纵深防御；上游已拒绝换行凭据）"
        );
    }

    #[test]
    fn both_listeners_disabled_is_rejected() {
        let err = GostConfig::new(false, false, None, &[], None, None, vec![]).unwrap_err();
        assert!(matches!(err, ConfigError::NoListeners));
    }

    #[test]
    fn empty_password_rejected() {
        let err = GostConfig::new(
            true,
            false,
            Some(ProxyAuth {
                username: "a".into(),
                password: "".into(),
            }),
            &[],
            None,
            None,
            vec![],
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::EmptyPassword));
    }

    #[test]
    fn username_with_newline_rejected() {
        // YAML 注入：换行会破坏 authers 映射结构（review 补强）。
        let err = GostConfig::new(
            true,
            false,
            Some(ProxyAuth {
                username: "alice\n  - username: eve".into(),
                password: "s3cret".into(),
            }),
            &[],
            None,
            None,
            vec![],
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::UsernameWithNewline));
    }

    mod cidr {
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
        fn rejects_host_bits_set() {
            let err = parse_cidr("192.168.0.1/16").unwrap_err();
            assert!(matches!(err, ConfigError::InvalidCidr { .. }));
            assert!(err.to_string().contains("host bits"));
        }

        #[test]
        fn rejects_bad_prefix() {
            assert!(matches!(
                parse_cidr("192.168.0.0/33").unwrap_err(),
                ConfigError::InvalidCidr { .. }
            ));
            assert!(matches!(
                parse_cidr("10.0.0.0/abc").unwrap_err(),
                ConfigError::InvalidCidr { .. }
            ));
        }

        #[test]
        fn rejects_garbage() {
            assert!(matches!(
                parse_cidr("not-an-ip").unwrap_err(),
                ConfigError::InvalidCidr { .. }
            ));
        }

        #[test]
        fn allowlist_validation_happens_at_construction() {
            let err = GostConfig::new(
                true,
                true,
                None,
                &["300.1.1.0/24".into()],
                None,
                None,
                vec![],
            )
            .unwrap_err();
            assert!(matches!(err, ConfigError::InvalidCidr { .. }));
        }
    }
}
