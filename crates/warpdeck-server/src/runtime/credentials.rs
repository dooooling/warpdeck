//! 实例账号凭据（v0.2 多账号，DESIGN §16.9 / §11.2 注入点）。
//!
//! runtime 层不依赖 DB：`CredentialResolver` 是注入边界，生产实现
//! `SqliteCredentialResolver`（`db::credentials`）从 `account_profiles` +
//! `secrets` 组装，测试用 `FakeCredentialResolver`（free 或固定凭据）。

use async_trait::async_trait;
use thiserror::Error;

/// 凭据的目标账号模式（业务层不需要知道 `AccountMode` 的存储表示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialMode {
    Free,
    WarpPlus,
    ZeroTrust,
}

/// 启动一个实例所需的账号凭据（明文仅存在于启动路径，
/// 禁止进入日志/API 响应；配合 redactor 使用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceCredentials {
    pub mode: CredentialMode,
    /// WarpPlus：license key。
    pub license: Option<String>,
    /// ZeroTrust：组织名（非 secret）。
    pub zero_trust_org: Option<String>,
    /// ZeroTrust：service token client id / client secret——经 mdm.rs 写入实例
    /// state 目录的 `mdm.xml`，由 warp-svc 启动时自动以 service token 注册
    /// （替代不可 headless 化的 `teams-enroll` 交互式 OAuth）。
    pub zt_client_id: Option<String>,
    pub zt_client_secret: Option<String>,
}

impl InstanceCredentials {
    /// free 账号（默认档/未配置时的解析结果）。
    pub fn free() -> Self {
        Self {
            mode: CredentialMode::Free,
            license: None,
            zero_trust_org: None,
            zt_client_id: None,
            zt_client_secret: None,
        }
    }
}

/// 凭据解析错误（不携带明文）。
#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("credential resolution failed: {0}")]
    Resolution(String),
    #[error("profile {0} not found: instance references a deleted account profile")]
    ProfileNotFound(i64),
    /// 档案模式要求凭据但缺失（模式校验失败，fail 上浮不得伪装成功）。
    #[error("profile {0} mode requires credentials that are missing (mode={1})")]
    MissingCredentials(i64, &'static str),
}

/// 凭据解析接缝：由 (可选 profile_id) → 启动凭据。
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    /// `profile_id == None` 为全局账号（v0.1 语义：`account_config` + 全局 secret）。
    async fn resolve(
        &self,
        profile_id: Option<i64>,
    ) -> Result<InstanceCredentials, CredentialError>;
}
