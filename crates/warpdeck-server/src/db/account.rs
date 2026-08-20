//! 账号配置仓储（P8-009 配套，DESIGN §16.5）。
//!
//! `account_config` 单行（id=1）保存账号模式与 Zero Trust org（非 secret）；
//! 凭据（license / client id / client secret）经 `crypto::secret_store`
//! 加密存储，通过 kind 关联（不建外键列，避免双写不一致）。

use async_trait::async_trait;
use sqlx::SqlitePool;
use thiserror::Error;

/// 账号模式（互斥：WARP+ 与 Zero Trust 不允许同时激活，DESIGN §16.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountMode {
    Free,
    WarpPlus,
    ZeroTrust,
}

impl AccountMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountMode::Free => "free",
            AccountMode::WarpPlus => "warp_plus",
            AccountMode::ZeroTrust => "zero_trust",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "free" => Ok(AccountMode::Free),
            "warp_plus" => Ok(AccountMode::WarpPlus),
            "zero_trust" => Ok(AccountMode::ZeroTrust),
            other => Err(format!(
                "invalid account mode `{other}` (free|warp_plus|zero_trust)"
            )),
        }
    }
}

/// 账号期望配置（读侧）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountConfig {
    pub mode: AccountMode,
    /// Zero Trust org 名（非 secret，明文存储）。
    pub zero_trust_org: Option<String>,
}

/// 账号仓储错误。
#[derive(Debug, Error)]
pub enum AccountRepoError {
    #[error("account db error: {0}")]
    Db(String),
}

impl From<sqlx::Error> for AccountRepoError {
    fn from(e: sqlx::Error) -> Self {
        AccountRepoError::Db(e.to_string())
    }
}

/// 账号仓储接缝。
#[async_trait]
pub trait AccountRepository: Send + Sync {
    /// 读取模式与 org（表空 = 默认 free）。
    async fn get(&self) -> Result<AccountConfig, AccountRepoError>;
    /// 更新模式与 org（单行 upsert）。
    async fn set_mode(
        &self,
        mode: AccountMode,
        zero_trust_org: Option<String>,
    ) -> Result<(), AccountRepoError>;
}

/// SQLite 实现（`account_config` 表，migration 0003）。
pub struct SqliteAccountRepository {
    pool: SqlitePool,
}

impl SqliteAccountRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AccountRepository for SqliteAccountRepository {
    async fn get(&self) -> Result<AccountConfig, AccountRepoError> {
        let row = sqlx::query("SELECT mode, zero_trust_org FROM account_config WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?;
        match row {
            None => Ok(AccountConfig {
                mode: AccountMode::Free,
                zero_trust_org: None,
            }),
            Some(row) => {
                use sqlx::Row;
                let raw: String = row.get("mode");
                Ok(AccountConfig {
                    mode: AccountMode::parse(&raw).unwrap_or(AccountMode::Free),
                    zero_trust_org: row.get("zero_trust_org"),
                })
            }
        }
    }

    async fn set_mode(
        &self,
        mode: AccountMode,
        zero_trust_org: Option<String>,
    ) -> Result<(), AccountRepoError> {
        sqlx::query(
            r#"
            INSERT INTO account_config (id, mode, zero_trust_org, updated_at)
            VALUES (1, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            ON CONFLICT(id) DO UPDATE SET
                mode = excluded.mode,
                zero_trust_org = excluded.zero_trust_org,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(mode.as_str())
        .bind(zero_trust_org)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{cleanup_temp_db, temp_db_url};

    #[tokio::test]
    async fn defaults_to_free_then_roundtrip_modes() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = SqliteAccountRepository::new(pool.clone());

        assert_eq!(repo.get().await.unwrap().mode, AccountMode::Free);
        repo.set_mode(AccountMode::WarpPlus, None).await.unwrap();
        assert_eq!(repo.get().await.unwrap().mode, AccountMode::WarpPlus);

        repo.set_mode(AccountMode::ZeroTrust, Some("cloudflare-team".to_string()))
            .await
            .unwrap();
        let cfg = repo.get().await.unwrap();
        assert_eq!(cfg.mode, AccountMode::ZeroTrust);
        assert_eq!(cfg.zero_trust_org.as_deref(), Some("cloudflare-team"));
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[test]
    fn mode_parse_roundtrip() {
        for (text, mode) in [
            ("free", AccountMode::Free),
            ("warp_plus", AccountMode::WarpPlus),
            ("zero_trust", AccountMode::ZeroTrust),
        ] {
            assert_eq!(AccountMode::parse(text).unwrap(), mode);
            assert_eq!(mode.as_str(), text);
        }
        assert!(AccountMode::parse("shadowsocks").is_err());
    }
}
