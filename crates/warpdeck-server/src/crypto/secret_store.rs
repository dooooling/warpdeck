//! 加密 secret 仓储（P8-008/009）。
//!
//! 设计（DESIGN §15.3/§16.6，AGENTS.md）：
//! - secret 以 ciphertext + nonce + key_version 存 `secrets` 表；
//! - 0005 起 `UNIQUE(kind, profile_id)`：本实现的所有操作限定
//!   `profile_id IS NULL`（全局/系统级 secret，v0.1 语义不变）。
//!   档案级 secret（profile_id 非 NULL）由任务 B（AccountProfile 仓储）接入。
//! - API 永不返回明文：写入口 set/clear，读入口只给存在性（`exists`）；
//! - 解密失败 = master key 丢失/数据损坏：标记凭据不可用（返回错误），
//!   调用方引导用户重新录入，不 panic（DESIGN §28.4）。

use async_trait::async_trait;
use sqlx::SqlitePool;
use thiserror::Error;

use super::{decrypt, encrypt, CryptoError, KEY_LEN, NONCE_LEN};

/// secret 稳定标识（secrets.kind）。加新 secret 时在此登记。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    /// 代理认证密码（proxy_config 用户名配套）。
    ProxyPassword,
    /// WARP+ license key。
    WarpPlusLicense,
    /// Zero Trust client id（service token）。
    ZeroTrustClientId,
    /// Zero Trust client secret（service token）。
    ZeroTrustClientSecret,
}

impl SecretKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SecretKind::ProxyPassword => "proxy_password",
            SecretKind::WarpPlusLicense => "warp_plus_license",
            SecretKind::ZeroTrustClientId => "zt_client_id",
            SecretKind::ZeroTrustClientSecret => "zt_client_secret",
        }
    }
}

/// 全局（无档案归属）secret 在 `secrets.profile_id` 上的取值：NULL。
/// 0005 起 `UNIQUE(kind, profile_id)` 在 profile_id 为 NULL 时不做唯一约束，
/// 因此全局 secret 的 upsert 必须显式先删后插（见 `SqliteSecretStore::set`）。
const GLOBAL_PROFILE_FILTER: &str = "profile_id IS NULL";

/// Secret 仓储错误（不携带明文）。
#[derive(Debug, Error)]
pub enum SecretStoreError {
    #[error("secret store db error: {0}")]
    Db(String),
    #[error("secret unavailable: {0}")]
    Unavailable(String),
}

impl From<sqlx::Error> for SecretStoreError {
    fn from(e: sqlx::Error) -> Self {
        SecretStoreError::Db(e.to_string())
    }
}

impl From<CryptoError> for SecretStoreError {
    fn from(e: CryptoError) -> Self {
        SecretStoreError::Unavailable(e.to_string())
    }
}

/// Secret 仓储接缝（测试可注入内存实现）。
#[async_trait]
pub trait SecretStore: Send + Sync {
    /// 写入/轮换全局 secret（覆盖旧值；profile_id IS NULL 语义见 `SqliteSecretStore`）。
    async fn set(&self, kind: SecretKind, plaintext: &str) -> Result<(), SecretStoreError>;
    /// 是否存在（GET/视图用；永不返回明文）。
    async fn exists(&self, kind: SecretKind) -> Result<bool, SecretStoreError>;
    /// 删除（清空）。
    async fn delete(&self, kind: SecretKind) -> Result<(), SecretStoreError>;
    /// 解密读取（仅内部应用路径用：GOST 渲染/注册流；禁止进入 API 响应）。
    async fn get_plaintext(&self, kind: SecretKind) -> Result<Option<String>, SecretStoreError>;

    /// 档案级 secret（v0.2 §16.9）：`profile_id` 非 NULL，upsert 受
    /// `UNIQUE(kind, profile_id)` 约束；档案删除由 DB 级联清理。
    async fn set_for_profile(
        &self,
        kind: SecretKind,
        profile_id: i64,
        plaintext: &str,
    ) -> Result<(), SecretStoreError>;
    async fn exists_for_profile(
        &self,
        kind: SecretKind,
        profile_id: i64,
    ) -> Result<bool, SecretStoreError>;
    async fn delete_for_profile(
        &self,
        kind: SecretKind,
        profile_id: i64,
    ) -> Result<(), SecretStoreError>;
    async fn get_plaintext_for_profile(
        &self,
        kind: SecretKind,
        profile_id: i64,
    ) -> Result<Option<String>, SecretStoreError>;
}

/// SQLite 实现（`secrets` 表，migration 0003）。
pub struct SqliteSecretStore {
    pool: SqlitePool,
    key: [u8; KEY_LEN],
    key_version: u32,
}

impl SqliteSecretStore {
    pub fn new(pool: SqlitePool, key: [u8; KEY_LEN]) -> Self {
        Self {
            pool,
            key,
            key_version: 1,
        }
    }
}

#[async_trait]
impl SecretStore for SqliteSecretStore {
    async fn set(&self, kind: SecretKind, plaintext: &str) -> Result<(), SecretStoreError> {
        let (ciphertext, nonce) = encrypt(&self.key, plaintext.as_bytes())?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM secrets WHERE kind = ? AND profile_id IS NULL")
            .bind(kind.as_str())
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            r#"
            INSERT INTO secrets (kind, ciphertext, nonce, key_version, updated_at)
            VALUES (?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            "#,
        )
        .bind(kind.as_str())
        .bind(&ciphertext)
        .bind(&nonce[..])
        .bind(i64::from(self.key_version))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn exists(&self, kind: SecretKind) -> Result<bool, SecretStoreError> {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM secrets WHERE kind = ? AND {GLOBAL_PROFILE_FILTER}"
        ))
        .bind(kind.as_str())
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    async fn delete(&self, kind: SecretKind) -> Result<(), SecretStoreError> {
        sqlx::query(&format!(
            "DELETE FROM secrets WHERE kind = ? AND {GLOBAL_PROFILE_FILTER}"
        ))
        .bind(kind.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_plaintext(&self, kind: SecretKind) -> Result<Option<String>, SecretStoreError> {
        let row = sqlx::query(&format!(
            "SELECT ciphertext, nonce FROM secrets WHERE kind = ? AND {GLOBAL_PROFILE_FILTER}"
        ))
        .bind(kind.as_str())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        use sqlx::Row;
        let ciphertext: Vec<u8> = row.get("ciphertext");
        let nonce_raw: Vec<u8> = row.get("nonce");
        let nonce: [u8; NONCE_LEN] = nonce_raw
            .try_into()
            .map_err(|_| SecretStoreError::Unavailable("stored nonce has wrong length".into()))?;
        let plaintext = decrypt(&self.key, &ciphertext, &nonce)?;
        Ok(Some(String::from_utf8_lossy(&plaintext).into_owned()))
    }

    async fn set_for_profile(
        &self,
        kind: SecretKind,
        profile_id: i64,
        plaintext: &str,
    ) -> Result<(), SecretStoreError> {
        let (ciphertext, nonce) = encrypt(&self.key, plaintext.as_bytes())?;
        sqlx::query(
            r#"
            INSERT INTO secrets (kind, profile_id, ciphertext, nonce, key_version, updated_at)
            VALUES (?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            ON CONFLICT(kind, profile_id) DO UPDATE SET
                ciphertext = excluded.ciphertext,
                nonce = excluded.nonce,
                key_version = excluded.key_version,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(kind.as_str())
        .bind(profile_id)
        .bind(&ciphertext)
        .bind(&nonce[..])
        .bind(i64::from(self.key_version))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn exists_for_profile(
        &self,
        kind: SecretKind,
        profile_id: i64,
    ) -> Result<bool, SecretStoreError> {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM secrets WHERE kind = ? AND profile_id = ?")
                .bind(kind.as_str())
                .bind(profile_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(count > 0)
    }

    async fn delete_for_profile(
        &self,
        kind: SecretKind,
        profile_id: i64,
    ) -> Result<(), SecretStoreError> {
        sqlx::query("DELETE FROM secrets WHERE kind = ? AND profile_id = ?")
            .bind(kind.as_str())
            .bind(profile_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get_plaintext_for_profile(
        &self,
        kind: SecretKind,
        profile_id: i64,
    ) -> Result<Option<String>, SecretStoreError> {
        let row =
            sqlx::query("SELECT ciphertext, nonce FROM secrets WHERE kind = ? AND profile_id = ?")
                .bind(kind.as_str())
                .bind(profile_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        use sqlx::Row;
        let ciphertext: Vec<u8> = row.get("ciphertext");
        let nonce_raw: Vec<u8> = row.get("nonce");
        let nonce: [u8; NONCE_LEN] = nonce_raw
            .try_into()
            .map_err(|_| SecretStoreError::Unavailable("stored nonce has wrong length".into()))?;
        let plaintext = decrypt(&self.key, &ciphertext, &nonce)?;
        Ok(Some(String::from_utf8_lossy(&plaintext).into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{cleanup_temp_db, temp_db_url};

    fn test_key() -> [u8; KEY_LEN] {
        [9u8; KEY_LEN]
    }

    #[tokio::test]
    async fn set_exists_get_delete_roundtrip() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let store = SqliteSecretStore::new(pool.clone(), test_key());

        assert!(!store.exists(SecretKind::ProxyPassword).await.unwrap());
        assert!(store
            .get_plaintext(SecretKind::ProxyPassword)
            .await
            .unwrap()
            .is_none());

        store
            .set(SecretKind::ProxyPassword, "s3cret-pass")
            .await
            .unwrap();
        assert!(store.exists(SecretKind::ProxyPassword).await.unwrap());
        assert_eq!(
            store
                .get_plaintext(SecretKind::ProxyPassword)
                .await
                .unwrap()
                .as_deref(),
            Some("s3cret-pass")
        );

        // 覆盖 = rotate。
        store
            .set(SecretKind::ProxyPassword, "new-pass")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_plaintext(SecretKind::ProxyPassword)
                .await
                .unwrap()
                .as_deref(),
            Some("new-pass")
        );

        store.delete(SecretKind::ProxyPassword).await.unwrap();
        assert!(!store.exists(SecretKind::ProxyPassword).await.unwrap());
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn db_holds_ciphertext_never_plaintext() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let marker = "TEST_SECRET_DO_NOT_LEAK_123";
        let store = SqliteSecretStore::new(pool.clone(), test_key());
        store
            .set(SecretKind::WarpPlusLicense, marker)
            .await
            .unwrap();

        let row = sqlx::query("SELECT ciphertext FROM secrets WHERE kind = 'warp_plus_license'")
            .fetch_one(&pool)
            .await
            .unwrap();
        use sqlx::Row;
        let ciphertext: Vec<u8> = row.get("ciphertext");
        let blob = String::from_utf8_lossy(&ciphertext);
        assert!(
            !blob.contains(marker),
            "plaintext must never appear in the db blob"
        );
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn wrong_key_marks_secret_unavailable() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let store = SqliteSecretStore::new(pool.clone(), test_key());
        store
            .set(SecretKind::ZeroTrustClientSecret, "zt-secret")
            .await
            .unwrap();

        // 换 key 打开同一 DB：解密失败且返回错误（不 panic、不崩溃）。
        let wrong = SqliteSecretStore::new(pool.clone(), [0u8; KEY_LEN]);
        let result = wrong.get_plaintext(SecretKind::ZeroTrustClientSecret).await;
        assert!(result.is_err());
        // 存在性不受解密影响（视图仍可显示"已配置"）。
        assert!(wrong
            .exists(SecretKind::ZeroTrustClientSecret)
            .await
            .unwrap());
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn profile_secrets_upsert_and_are_isolated_from_global() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let store = SqliteSecretStore::new(pool.clone(), test_key());

        // 0005 建表时只有默认档 id=1；测试档案 2 需先存在（FK 约束）。
        sqlx::query("INSERT INTO account_profiles (id, name, mode) VALUES (2, 'p2', 'free')")
            .execute(&pool)
            .await
            .unwrap();

        // 未写入前不存在。
        assert!(!store
            .exists_for_profile(SecretKind::WarpPlusLicense, 1)
            .await
            .unwrap());

        store
            .set_for_profile(SecretKind::WarpPlusLicense, 1, "license-for-profile-1")
            .await
            .unwrap();
        store
            .set_for_profile(SecretKind::WarpPlusLicense, 2, "license-for-profile-2")
            .await
            .unwrap();

        // 同档案覆盖 = 轮换。
        store
            .set_for_profile(SecretKind::WarpPlusLicense, 1, "license-for-profile-1-v2")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_plaintext_for_profile(SecretKind::WarpPlusLicense, 1)
                .await
                .unwrap()
                .as_deref(),
            Some("license-for-profile-1-v2")
        );
        assert_eq!(
            store
                .get_plaintext_for_profile(SecretKind::WarpPlusLicense, 2)
                .await
                .unwrap()
                .as_deref(),
            Some("license-for-profile-2")
        );

        // 全局读写互不干扰（不同 profile_id 维度）。
        assert!(store
            .get_plaintext(SecretKind::WarpPlusLicense)
            .await
            .unwrap()
            .is_none());
        store
            .set(SecretKind::WarpPlusLicense, "global-license")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_plaintext_for_profile(SecretKind::WarpPlusLicense, 1)
                .await
                .unwrap()
                .as_deref(),
            Some("license-for-profile-1-v2")
        );

        // 删除只作用于该档案。
        store
            .delete_for_profile(SecretKind::WarpPlusLicense, 1)
            .await
            .unwrap();
        assert!(!store
            .exists_for_profile(SecretKind::WarpPlusLicense, 1)
            .await
            .unwrap());
        assert!(store
            .exists_for_profile(SecretKind::WarpPlusLicense, 2)
            .await
            .unwrap());
        assert!(store.exists(SecretKind::WarpPlusLicense).await.unwrap());

        // 档案级密文同样不能含明文（防回归）。
        store
            .set_for_profile(SecretKind::ZeroTrustClientSecret, 1, "PLAIN_ZT_MARKER_XYZ")
            .await
            .unwrap();
        let row = sqlx::query(
            "SELECT ciphertext FROM secrets WHERE kind = 'zt_client_secret' AND profile_id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        use sqlx::Row;
        let bytes: Vec<u8> = row.get("ciphertext");
        let blob = String::from_utf8_lossy(&bytes);
        assert!(!blob.contains("PLAIN_ZT_MARKER_XYZ"));

        pool.close().await;
        cleanup_temp_db(&db_path);
    }
}
