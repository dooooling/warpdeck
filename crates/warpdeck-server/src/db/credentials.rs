//! 实例凭据解析（v0.2 §16.9）：profile → 启动所需明文凭据。
//!
//! 语义（DESIGN §16.9 / §16.5 视图）：
//! - `profile_id = Some(id)`：只读该档案级 secret；缺失 = 配置错误，
//!   上浮 `MissingCredentials`（禁止伪装成功）。
//! - `profile_id = None`（实例未指定）：按默认档（`DEFAULT_PROFILE_ID`）解析；
//!   凭据先查档案级，回退全局（`profile_id IS NULL`，v0.1 老数据）。
//! - 返回的 `InstanceCredentials` 明文仅存在于启动路径，禁止日志/API 回显。

use std::sync::Arc;

use async_trait::async_trait;

use crate::crypto::secret_store::{SecretKind, SecretStore, SecretStoreError};
use crate::db::account::AccountMode;
use crate::db::profiles::{
    AccountProfile, AccountProfileError, AccountProfileRepository, DEFAULT_PROFILE_ID,
};
use crate::runtime::credentials::{
    CredentialError, CredentialMode, CredentialResolver, InstanceCredentials,
};

/// SQLite 驱动实现：`account_profiles`（非 secret 字段）+ `secrets`（密文）。
pub struct SqliteCredentialResolver {
    profiles: Arc<dyn AccountProfileRepository>,
    secrets: Arc<dyn SecretStore>,
}

impl SqliteCredentialResolver {
    pub fn new(profiles: Arc<dyn AccountProfileRepository>, secrets: Arc<dyn SecretStore>) -> Self {
        Self { profiles, secrets }
    }

    /// 档案级凭据，回退全局（仅默认档解析路径）。
    async fn secret_for(
        &self,
        kind: SecretKind,
        profile: &AccountProfile,
    ) -> Result<Option<String>, CredentialError> {
        let profile_val = self
            .secrets
            .get_plaintext_for_profile(kind, profile.id)
            .await
            .map_err(err_map)?;
        if profile_val.is_some() {
            return Ok(profile_val);
        }
        if profile.id == DEFAULT_PROFILE_ID {
            return self.secrets.get_plaintext(kind).await.map_err(err_map);
        }
        Ok(None)
    }
}

fn err_map(e: SecretStoreError) -> CredentialError {
    CredentialError::Resolution(e.to_string())
}

fn profile_err_map(e: AccountProfileError) -> CredentialError {
    match e {
        AccountProfileError::NotFound(id) => CredentialError::ProfileNotFound(id),
        other => CredentialError::Resolution(other.to_string()),
    }
}

#[async_trait]
impl CredentialResolver for SqliteCredentialResolver {
    async fn resolve(
        &self,
        profile_id: Option<i64>,
    ) -> Result<InstanceCredentials, CredentialError> {
        // None = 未绑定，按默认档解析（§16.9 "不选则使用免费档"，但默认档可改名/改模式，
        // 故读取 id=1 的实际行，而不是硬编码 Free）。
        let pid = profile_id.unwrap_or(DEFAULT_PROFILE_ID);
        let profile = self.profiles.get(pid).await.map_err(profile_err_map)?;

        match profile.mode {
            AccountMode::Free => Ok(InstanceCredentials::free()),
            AccountMode::WarpPlus => {
                let license = self
                    .secret_for(SecretKind::WarpPlusLicense, &profile)
                    .await?;
                match license {
                    Some(license) => Ok(InstanceCredentials {
                        mode: CredentialMode::WarpPlus,
                        license: Some(license),
                        ..InstanceCredentials::free()
                    }),
                    None => Err(CredentialError::MissingCredentials(
                        profile.id,
                        "warp_plus (license missing)",
                    )),
                }
            }
            AccountMode::ZeroTrust => {
                let org =
                    profile
                        .zero_trust_org
                        .clone()
                        .ok_or(CredentialError::MissingCredentials(
                            profile.id,
                            "zero_trust (org missing)",
                        ))?;
                let client_id = self
                    .secret_for(SecretKind::ZeroTrustClientId, &profile)
                    .await?;
                let client_secret = self
                    .secret_for(SecretKind::ZeroTrustClientSecret, &profile)
                    .await?;
                match (client_id, client_secret) {
                    (Some(client_id), Some(client_secret)) => Ok(InstanceCredentials {
                        mode: CredentialMode::ZeroTrust,
                        license: None,
                        zero_trust_org: Some(org),
                        zt_client_id: Some(client_id),
                        zt_client_secret: Some(client_secret),
                    }),
                    _ => Err(CredentialError::MissingCredentials(
                        profile.id,
                        "zero_trust (client id/secret missing)",
                    )),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::secret_store::SqliteSecretStore;
    use crate::db::profiles::SqliteAccountProfileRepository;
    use crate::db::{cleanup_temp_db, temp_db_url};

    fn master_key() -> [u8; 32] {
        [7u8; 32]
    }

    #[tokio::test]
    async fn none_resolves_default_profile_fallback_to_global() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let profiles = Arc::new(SqliteAccountProfileRepository::new(pool.clone()));
        let secrets = Arc::new(SqliteSecretStore::new(pool.clone(), master_key()));

        // 默认档升级为 warp_plus（可改名/改模式，§16.9；自由 API 层禁止，此处直改 SQL
        // 模拟历史数据），但凭据仍在全局（v0.1 老数据 profile_id IS NULL）：
        // resolve(None) 应回退读取全局 license。
        sqlx::query("UPDATE account_profiles SET mode = 'warp_plus' WHERE id = ?")
            .bind(DEFAULT_PROFILE_ID)
            .execute(&pool)
            .await
            .unwrap();
        secrets
            .set(SecretKind::WarpPlusLicense, "WPL-0001")
            .await
            .unwrap();

        let resolver = SqliteCredentialResolver::new(profiles, secrets);

        let creds = resolver.resolve(None).await.unwrap();
        assert_eq!(creds.mode, CredentialMode::WarpPlus);
        assert_eq!(creds.license.as_deref(), Some("WPL-0001"));

        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn explicit_profile_uses_profile_secret_not_global() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let profiles = Arc::new(SqliteAccountProfileRepository::new(pool.clone()));
        let secrets = Arc::new(SqliteSecretStore::new(pool, master_key()));

        // 全局有一把 license，但显式档案用自己的一把。
        secrets
            .set(SecretKind::WarpPlusLicense, "WPL-GLOBAL")
            .await
            .unwrap();
        let p = profiles
            .create("team-a", AccountMode::WarpPlus, None)
            .await
            .unwrap();
        secrets
            .set_for_profile(SecretKind::WarpPlusLicense, p.id, "WPL-PROFILE")
            .await
            .unwrap();

        let resolver = SqliteCredentialResolver::new(profiles, secrets);

        let creds = resolver.resolve(Some(p.id)).await.unwrap();
        assert_eq!(creds.mode, CredentialMode::WarpPlus);
        assert_eq!(creds.license.as_deref(), Some("WPL-PROFILE"));

        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn zero_trust_profile_requires_org_and_secrets() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let profiles = Arc::new(SqliteAccountProfileRepository::new(pool.clone()));
        let secrets = Arc::new(SqliteSecretStore::new(pool, master_key()));

        // 缺 org 与 secret：MissingCredentials。
        let p = profiles
            .create("team-b", AccountMode::ZeroTrust, Some("my-org")) // org 齐全
            .await
            .unwrap();
        let resolver = SqliteCredentialResolver::new(profiles.clone(), secrets.clone());
        let err = resolver.resolve(Some(p.id)).await.unwrap_err();
        assert!(matches!(err, CredentialError::MissingCredentials(_, _)));

        // 补全后成功。
        secrets
            .set_for_profile(SecretKind::ZeroTrustClientId, p.id, "cid")
            .await
            .unwrap();
        secrets
            .set_for_profile(SecretKind::ZeroTrustClientSecret, p.id, "csec")
            .await
            .unwrap();
        let creds = resolver.resolve(Some(p.id)).await.unwrap();
        assert_eq!(creds.mode, CredentialMode::ZeroTrust);
        assert_eq!(creds.zero_trust_org.as_deref(), Some("my-org"));

        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn missing_profile_is_error() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let profiles = Arc::new(SqliteAccountProfileRepository::new(pool.clone()));
        let secrets = Arc::new(SqliteSecretStore::new(pool, master_key()));
        let resolver = SqliteCredentialResolver::new(profiles, secrets);

        let err = resolver.resolve(Some(999)).await.unwrap_err();
        assert!(matches!(err, CredentialError::ProfileNotFound(999)));

        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn default_free_profile_resolves_free() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let profiles = Arc::new(SqliteAccountProfileRepository::new(pool.clone()));
        let secrets = Arc::new(SqliteSecretStore::new(pool, master_key()));

        let resolver = SqliteCredentialResolver::new(profiles, secrets);
        let creds = resolver.resolve(None).await.unwrap();
        assert_eq!(creds.mode, CredentialMode::Free);

        cleanup_temp_db(&db_path);
    }
}
