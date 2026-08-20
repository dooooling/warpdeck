//! 认证与会话基础设施（P8-001..006/011）。
//!
//! 设计（DESIGN §16.1/16.2/§20，AGENTS.md）：
//! - 密码：Argon2id（PHC 字符串存 users.password_hash），禁止 MD5/SHA1/SHA256；
//! - 会话：服务端 session（随机 UUID）+ HttpOnly cookie（只存 session id）；
//!   每个 session 绑定独立 CSRF token（sessions.csrf_token）；
//! - 登录限流：per-IP 失败计数 + 时间窗口（内存实现，进程内共享）。

pub mod password;
pub mod rate_limit;
pub mod repos;
pub mod session;

/// 会话 cookie 名。
pub const SESSION_COOKIE: &str = "warpdeck_session";
/// CSRF token 请求头名（mutation 必须携带）。
pub const CSRF_HEADER: &str = "x-csrf-token";
/// 会话 TTL：30 天（管理台刷新页面依赖长会话；logout/过期可撤销）。
pub const SESSION_TTL: time::Duration = time::Duration::days(30);
