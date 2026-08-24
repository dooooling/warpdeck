//! Bootstrap configuration: the only source of launch-level settings.
//!
//! Dynamic business settings live in SQLite, never in this config.
//! Container listener ports are fixed constants — the API must not allow
//! changing them at runtime (host publishing is owned by Compose/.env).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use serde::Serialize;
use thiserror::Error;

/// Web UI + REST API + SSE listener port (container-internal).
pub const WEB_PORT: u16 = 9000;
/// SOCKS5 proxy listener port through WARP (container-internal).
pub const SOCKS5_PORT: u16 = 11080;
/// HTTP proxy listener port through WARP (container-internal).
pub const HTTP_PORT: u16 = 18080;
/// First WARP instance internal upstream port.
pub const FIRST_WARP_PORT: u16 = 40000;
/// Hard cap on instance ids so `FIRST_WARP_PORT + id` never overflows `u16`.
pub const MAX_INSTANCES: usize = u16::MAX as usize - FIRST_WARP_PORT as usize;

pub const DEFAULT_DATA_DIR: &str = "/var/lib/warpdeck";
pub const DEFAULT_RUNTIME_DIR: &str = "/run/warpdeck";
const DEFAULT_BIND_IP: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
const DEFAULT_LOG_LEVEL: &str = "info";

/// Launch-level application configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppConfig {
    pub web_bind: SocketAddr,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
    /// Web UI 静态资源目录（SPA：index.html + assets/）。
    pub ui_dir: PathBuf,
    pub database_url: String,
    pub log_level: String,
    /// `WARPDECK_MASTER_KEY`（base64 32 字节）或 None（用 `data_dir/master.key`）。
    pub master_key_env: Option<String>,
    /// P8-004：HTTPS 部署下 cookie 加 `Secure` 标志。
    pub secure_cookie: bool,
    /// P13（DESIGN §35）：代理网关实现选择。默认 gost；builtin 为内置网关
    /// （Phase A 起可用，迁移期共存）。
    pub gateway: GatewayKind,
    /// SOCKS5/HTTP 入站绑定地址（builtin 网关使用；gost 路径沿用渲染常量）。
    pub socks5_bind: SocketAddr,
    pub http_bind: SocketAddr,
}

/// 代理网关实现（DESIGN §35.5）。`WARPDECK_GATEWAY=gost|builtin`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub enum GatewayKind {
    #[default]
    Gost,
    Builtin,
}

impl GatewayKind {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "gost" => Ok(GatewayKind::Gost),
            "builtin" => Ok(GatewayKind::Builtin),
            other => Err(format!(
                "invalid WARPDECK_GATEWAY `{other}` (expected gost|builtin)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            GatewayKind::Gost => "gost",
            GatewayKind::Builtin => "builtin",
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("invalid `{var}` `{value}`: {reason}")]
    Invalid {
        var: &'static str,
        value: String,
        reason: String,
    },
}

impl AppConfig {
    /// Build the config from environment variables with sane defaults.
    ///
    /// Env vars (per DESIGN §15.1):
    /// - `WARPDECK_BIND` — IP to bind (default `0.0.0.0`)
    /// - `WARPDECK_PORT` — web port (default `9000`)
    /// - `WARPDECK_DATA_DIR` — persistent data dir (default `/var/lib/warpdeck`)
    /// - `WARPDECK_RUNTIME_DIR` — runtime (tmpfs) dir (default `/run/warpdeck`)
    /// - `WARPDECK_LOG` — tracing filter (default `info`)
    /// - `DATABASE_URL` — sqlite URL (default `sqlite:<data_dir>/warpdeck.db`)
    /// - `WARPDECK_MASTER_KEY` — secret-store master key (base64 32 bytes)
    /// - `WARPDECK_SECURE_COOKIE` — add `Secure` to session cookie (HTTPS)
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from(env_getter)
    }

    pub fn from(env: impl Fn(&'static str) -> Option<String>) -> Result<Self, ConfigError> {
        let bind_ip = match env("WARPDECK_BIND") {
            Some(v) => v.parse::<IpAddr>().map_err(|_| ConfigError::Invalid {
                var: "WARPDECK_BIND",
                value: v,
                reason: "not a valid IP address".to_string(),
            })?,
            None => DEFAULT_BIND_IP,
        };

        let web_port = match env("WARPDECK_PORT") {
            Some(v) => parse_port(&v).map_err(|reason| ConfigError::Invalid {
                var: "WARPDECK_PORT",
                value: v,
                reason,
            })?,
            None => WEB_PORT,
        };

        let data_dir = match env("WARPDECK_DATA_DIR") {
            Some(v) => parse_absolute_dir(&v).map_err(|reason| ConfigError::Invalid {
                var: "WARPDECK_DATA_DIR",
                value: v,
                reason,
            })?,
            None => PathBuf::from(DEFAULT_DATA_DIR),
        };

        let runtime_dir = match env("WARPDECK_RUNTIME_DIR") {
            Some(v) => parse_absolute_dir(&v).map_err(|reason| ConfigError::Invalid {
                var: "WARPDECK_RUNTIME_DIR",
                value: v,
                reason,
            })?,
            None => PathBuf::from(DEFAULT_RUNTIME_DIR),
        };

        // Web UI 静态目录：默认相对工作目录的 `ui/`（容器内 WORKDIR 布置）。
        let ui_dir = match env("WARPDECK_UI_DIR") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => PathBuf::from("ui"),
        };

        let database_url = match env("DATABASE_URL") {
            Some(v) if !v.is_empty() => v,
            _ => format!("sqlite:{}", data_dir.join("warpdeck.db").display()),
        };

        let log_level = env("WARPDECK_LOG")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_string());

        let master_key_env = env("WARPDECK_MASTER_KEY").filter(|v| !v.is_empty());

        // 默认 false（本地 HTTP 开发）；HTTPS 反代部署必须设 true（DESIGN §20.5）。
        let secure_cookie = env("WARPDECK_SECURE_COOKIE")
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(false);

        // P13（DESIGN §35.5）：代理网关实现选择。
        let gateway = match env("WARPDECK_GATEWAY") {
            Some(v) if !v.is_empty() => {
                GatewayKind::parse(&v).map_err(|reason| ConfigError::Invalid {
                    var: "WARPDECK_GATEWAY",
                    value: v,
                    reason,
                })?
            }
            _ => GatewayKind::default(),
        };

        let socks5_bind = match env("WARPDECK_SOCKS5_BIND") {
            Some(v) => v.parse::<SocketAddr>().map_err(|_| ConfigError::Invalid {
                var: "WARPDECK_SOCKS5_BIND",
                value: v,
                reason: "not a valid SocketAddr".to_string(),
            })?,
            None => SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                SOCKS5_PORT,
            ),
        };

        let http_bind = match env("WARPDECK_HTTP_BIND") {
            Some(v) => v.parse::<SocketAddr>().map_err(|_| ConfigError::Invalid {
                var: "WARPDECK_HTTP_BIND",
                value: v,
                reason: "not a valid SocketAddr".to_string(),
            })?,
            None => SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                HTTP_PORT,
            ),
        };

        Ok(AppConfig {
            web_bind: SocketAddr::new(bind_ip, web_port),
            data_dir,
            runtime_dir,
            ui_dir,
            database_url,
            log_level,
            master_key_env,
            secure_cookie,
            gateway,
            socks5_bind,
            http_bind,
        })
    }
}

fn parse_port(v: &str) -> Result<u16, String> {
    let port = v
        .parse::<u16>()
        .map_err(|_| format!("`{v}` is not a valid u16 port"))?;
    if port == 0 {
        return Err(format!("`{v}` is 0; port must be in 1..=65535"));
    }
    Ok(port)
}

fn parse_absolute_dir(v: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(v);
    if !p.is_absolute() {
        return Err(format!("`{v}` is not an absolute path"));
    }
    Ok(p)
}

fn env_getter(key: &'static str) -> Option<String> {
    std::env::var(key).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn from_map(map: &HashMap<&str, &str>) -> Result<AppConfig, ConfigError> {
        AppConfig::from(|k| map.get(k).map(|v| v.to_string()))
    }

    fn url_in(dir: &str, file: &str) -> String {
        format!("sqlite:{}", PathBuf::from(dir).join(file).display())
    }

    /// An absolute directory for tests that works on every platform
    /// (on Windows `/x` alone is root-relative, not absolute).
    fn abs_test_dir() -> String {
        std::env::temp_dir()
            .join("warpdeck-config-test")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn defaults_are_sane() {
        let cfg = from_map(&HashMap::new()).unwrap();
        assert_eq!(cfg.web_bind, SocketAddr::from(([0, 0, 0, 0], WEB_PORT)));
        assert_eq!(cfg.data_dir, PathBuf::from("/var/lib/warpdeck"));
        assert_eq!(cfg.runtime_dir, PathBuf::from("/run/warpdeck"));
        assert_eq!(cfg.ui_dir, PathBuf::from("ui"));
        assert_eq!(cfg.database_url, url_in("/var/lib/warpdeck", "warpdeck.db"));
        assert_eq!(cfg.log_level, "info");
    }

    #[test]
    fn accepts_all_override_env_vars() {
        let mut map = HashMap::new();
        map.insert("WARPDECK_BIND", "127.0.0.1");
        map.insert("WARPDECK_PORT", "8081");
        map.insert("WARPDECK_UI_DIR", "/opt/ui");
        let data_dir = abs_test_dir();
        map.insert("WARPDECK_DATA_DIR", data_dir.as_str());
        let runtime_dir = format!("{data_dir}-runtime");
        map.insert("WARPDECK_RUNTIME_DIR", runtime_dir.as_str());
        map.insert("WARPDECK_LOG", "debug");
        map.insert("DATABASE_URL", "sqlite:/data/warpdeck/custom.db");

        let cfg = from_map(&map).unwrap();
        assert_eq!(cfg.web_bind, SocketAddr::from(([127, 0, 0, 1], 8081)));
        assert_eq!(cfg.ui_dir, PathBuf::from("/opt/ui"));
        assert_eq!(cfg.data_dir, PathBuf::from(&data_dir));
        assert_eq!(cfg.runtime_dir, PathBuf::from(&runtime_dir));
        assert_eq!(cfg.database_url, "sqlite:/data/warpdeck/custom.db");
        assert_eq!(cfg.log_level, "debug");
    }

    #[test]
    fn database_url_falls_back_to_data_dir() {
        let data_dir = abs_test_dir();
        let mut map = HashMap::new();
        map.insert("WARPDECK_DATA_DIR", data_dir.as_str());
        let cfg = from_map(&map).unwrap();
        assert_eq!(cfg.database_url, url_in(&data_dir, "warpdeck.db"));
    }

    #[test]
    fn empty_database_url_is_treated_as_unset() {
        let mut map = HashMap::new();
        map.insert("DATABASE_URL", "");
        let cfg = from_map(&map).unwrap();
        assert_eq!(cfg.database_url, url_in("/var/lib/warpdeck", "warpdeck.db"));
    }

    #[test]
    fn rejects_invalid_bind_ip() {
        let mut map = HashMap::new();
        map.insert("WARPDECK_BIND", "not-an-ip");
        let err = from_map(&map).unwrap_err();
        assert_eq!(
            err,
            ConfigError::Invalid {
                var: "WARPDECK_BIND",
                value: "not-an-ip".into(),
                reason: "not a valid IP address".into()
            }
        );
    }

    #[test]
    fn rejects_invalid_port() {
        for bad in ["0", "abc", "70000"] {
            let mut map = HashMap::new();
            map.insert("WARPDECK_PORT", bad);
            let err = from_map(&map).unwrap_err();
            assert!(
                matches!(
                    err,
                    ConfigError::Invalid {
                        var: "WARPDECK_PORT",
                        ..
                    }
                ),
                "port `{bad}` should fail"
            );
        }
    }

    #[test]
    fn accepts_valid_port_bounds() {
        for port in ["1", "65535"] {
            let mut map = HashMap::new();
            map.insert("WARPDECK_PORT", port);
            let cfg = from_map(&map).unwrap();
            assert_eq!(cfg.web_bind.port(), port.parse::<u16>().unwrap());
        }
    }

    #[test]
    fn rejects_non_absolute_dirs() {
        for var in ["WARPDECK_DATA_DIR", "WARPDECK_RUNTIME_DIR"] {
            let mut map = HashMap::new();
            map.insert(var, "relative/path");
            let err = from_map(&map).unwrap_err();
            assert_eq!(
                err,
                ConfigError::Invalid {
                    var,
                    value: "relative/path".into(),
                    reason: "`relative/path` is not an absolute path".into()
                }
            );
        }
    }

    #[test]
    fn fixed_port_constants_are_in_expected_range() {
        assert_eq!(WEB_PORT, 9000);
        assert_eq!(SOCKS5_PORT, 11080);
        assert_eq!(HTTP_PORT, 18080);
        assert_eq!(FIRST_WARP_PORT, 40000);
        assert!(FIRST_WARP_PORT as usize + MAX_INSTANCES <= u16::MAX as usize);
    }

    #[test]
    fn constants_never_collide() {
        let ports = [WEB_PORT, SOCKS5_PORT, HTTP_PORT];
        for (i, a) in ports.iter().enumerate() {
            for b in &ports[i + 1..] {
                assert_ne!(a, b, "fixed ports must be distinct");
            }
        }
        for p in ports {
            assert!(
                p < FIRST_WARP_PORT,
                "proxy/API ports must not collide with WARP upstream range"
            );
        }
    }
}
