//! WARP 控制面抽象（P2-003）。
//!
//! 设计约束（计划 §7.2 / AGENTS.md）：业务层不得直接调用 `warp-cli`，
//! 一律经由 `WarpControl` trait；生产实现 `RealWarpControl`（P2-007），
//! 测试使用 `FakeWarpControl`，保证 ≥80% 测试不依赖真实 WARP。

use async_trait::async_trait;

use super::context::InstanceContext;
use super::credentials::InstanceCredentials;
use super::instance::InternalProxyPort;

/// `warp-cli status` 解析结果（DESIGN §14.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarpCliStatus {
    pub connected: bool,
    pub raw_status: String,
}

/// WARP 控制面错误。
///
/// `CommandFailed` 携带 stderr summary（P2-007 的真实实现必须 capture stderr），
/// 用于日志与 UI，但绝不含明文 secret。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WarpControlError {
    #[error("warp-cli command timed out")]
    CommandTimeout,
    #[error("warp-cli failed: {summary}")]
    CommandFailed { summary: String },
    #[error("instance {0} is not registered")]
    NotRegistered(i64),
    #[error("instance {0} must be registered before this action")]
    RegistrationRequired(i64),
    #[error("connect failed: {summary}")]
    ConnectFailure { summary: String },
}

/// 控制一个 WARP 实例（warp-svc / warp-cli）的领域抽象。
#[async_trait]
pub trait WarpControl: Send + Sync {
    /// 查询实例连接状态（§14.2）。
    async fn status(&self, ctx: &InstanceContext) -> Result<WarpCliStatus, WarpControlError>;

    /// 注册实例到 WARP（`warp-cli registration new`，§11.5）。
    async fn register(&self, ctx: &InstanceContext) -> Result<(), WarpControlError>;

    /// 应用账号凭据（v0.2 §11.2 注入点；注册后、配置前幂等执行）：
    /// - Free：无操作；
    /// - WarpPlus：`warp-cli registration license <KEY>`；
    /// - ZeroTrust：无操作（注册由 mdm.xml 服务令牌在 warp-svc 启动时自动完成）。
    /// 失败必须上浮，禁止伪装成功（AGENTS.md §11.2）。
    async fn apply_account(
        &self,
        ctx: &InstanceContext,
        credentials: &InstanceCredentials,
    ) -> Result<(), WarpControlError>;

    /// 设置代理模式（`warp-cli mode proxy`）。
    async fn set_proxy_mode(&self, ctx: &InstanceContext) -> Result<(), WarpControlError>;

    /// 设置实例 SOCKS5 内部监听端口（`warp-cli proxy port`，官方 2026.6.880.0+ 实测）。
    async fn set_proxy_port(
        &self,
        ctx: &InstanceContext,
        port: InternalProxyPort,
    ) -> Result<(), WarpControlError>;

    /// 建立 WARP 连接。
    async fn connect(&self, ctx: &InstanceContext) -> Result<(), WarpControlError>;

    /// 断开 WARP 连接（幂等）。
    async fn disconnect(&self, ctx: &InstanceContext) -> Result<(), WarpControlError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_variants_carry_actionable_context() {
        let err = WarpControlError::CommandFailed {
            summary: "exit 1".into(),
        };
        assert!(err.to_string().contains("warp-cli failed: exit 1"));
        assert_eq!(err, err.clone());
    }
}
