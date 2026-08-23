//! 跨表一致性写服务（P1 审查 R3#4：Secret 与业务配置必须同事务）。
//!
//! `proxy.rs` 先写密码 secret 再更新配置行、`accounts.rs` 依次写三个档案
//! secret + profile 行——中途失败会留下「密码已换 / 配置仍旧」或「部分凭据」
//! 的混合态。本服务把这类多表写收进单个 `BEGIN IMMEDIATE` 事务，由 SQLite
//! 保证全有或全无。
//!
//! 实现：直接持有具体类型（非 trait 对象），SQL 与各 repo 语义一致。

use sqlx::SqlitePool;

use crate::crypto::encrypt;
use crate::crypto::secret_store::SecretKind;
use crate::crypto::CryptoError;
use crate::db::repo::ProxyConfig;

/// 单条跨表写失败。
#[derive(Debug, thiserror::Error)]
pub enum ConsistencyError {
    #[error("database error: {0}")]
    Db(String),
    #[error("crypto error: {0}")]
    Crypto(String),
    /// §16.9：free 档案全局唯一且系统保留（API 映射 409）。
    #[error("free profile is unique and reserved")]
    FreeProfileConflict,
}

impl From<sqlx::Error> for ConsistencyError {
    fn from(e: sqlx::Error) -> Self {
        ConsistencyError::Db(e.to_string())
    }
}

impl From<CryptoError> for ConsistencyError {
    fn from(e: CryptoError) -> Self {
        ConsistencyError::Crypto(e.to_string())
    }
}

/// 跨表一致性写入口。clone 廉价（内部仅 SqlitePool + key）。
#[derive(Clone)]
pub struct ConsistencyService {
    pool: SqlitePool,
    key: [u8; crate::crypto::KEY_LEN],
}

impl ConsistencyService {
    pub fn new(pool: SqlitePool, key: [u8; crate::crypto::KEY_LEN]) -> Self {
        Self { pool, key }
    }

    /// P1 审查 R3#4-a：代理配置行 + 密码 secret 同事务生效。
    /// `password`：None = 不动；Some("") = 清除；Some(v) = 设置/轮换。
    pub async fn update_proxy_with_password(
        &self,
        cfg: &ProxyConfig,
        password: Option<&str>,
    ) -> Result<(), ConsistencyError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        match password {
            Some("") => {
                sqlx::query(
                    "DELETE FROM secrets WHERE kind = 'proxy_password' AND profile_id IS NULL",
                )
                .execute(&mut *tx)
                .await?;
            }
            Some(v) => {
                self.set_global_secret(&mut tx, SecretKind::ProxyPassword, v)
                    .await?
            }
            None => {}
        }
        // 配置行 upsert —— SQL 与 SqliteProxyConfigRepository::update 一致。
        sqlx::query(
            r#"
            INSERT INTO proxy_config (id, socks5_enabled, http_enabled, auth_enabled, proxy_username, allowed_ips, max_connections, max_rps, updated_at)
            VALUES (1, ?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            ON CONFLICT(id) DO UPDATE SET
                socks5_enabled = excluded.socks5_enabled,
                http_enabled = excluded.http_enabled,
                auth_enabled = excluded.auth_enabled,
                proxy_username = excluded.proxy_username,
                allowed_ips = excluded.allowed_ips,
                max_connections = excluded.max_connections,
                max_rps = excluded.max_rps,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(if cfg.socks5_enabled { 1 } else { 0 })
        .bind(if cfg.http_enabled { 1 } else { 0 })
        .bind(if cfg.auth_enabled { 1 } else { 0 })
        .bind(&cfg.proxy_username)
        .bind(if cfg.allowed_ips.is_empty() {
            None
        } else {
            Some(cfg.allowed_ips.join(","))
        })
        .bind(cfg.max_connections.map(i64::from).unwrap_or(0))
        .bind(cfg.max_rps.map(i64::from).unwrap_or(0))
        .execute(&mut *tx)
        .await?;
        tx.commit().await.map_err(ConsistencyError::from)
    }

    async fn set_global_secret(
        &self,
        conn: &mut sqlx::SqliteConnection,
        kind: SecretKind,
        plaintext: &str,
    ) -> Result<(), ConsistencyError> {
        let (ciphertext, nonce) = encrypt(&self.key, plaintext.as_bytes())?;
        sqlx::query("DELETE FROM secrets WHERE kind = ? AND profile_id IS NULL")
            .bind(kind.as_str())
            .execute(&mut *conn)
            .await?;
        sqlx::query(
            "INSERT INTO secrets (kind, ciphertext, nonce, key_version, updated_at) VALUES (?, ?, ?, 1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        )
        .bind(kind.as_str())
        .bind(&ciphertext)
        .bind(&nonce[..])
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn set_profile_secret(
        &self,
        conn: &mut sqlx::SqliteConnection,
        kind: SecretKind,
        profile_id: i64,
        plaintext: &str,
    ) -> Result<(), ConsistencyError> {
        let (ciphertext, nonce) = encrypt(&self.key, plaintext.as_bytes())?;
        sqlx::query(
            "INSERT INTO secrets (kind, profile_id, ciphertext, nonce, key_version, updated_at) VALUES (?, ?, ?, ?, 1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) ON CONFLICT(kind, profile_id) DO UPDATE SET ciphertext = excluded.ciphertext, nonce = excluded.nonce, key_version = excluded.key_version, updated_at = excluded.updated_at",
        )
        .bind(kind.as_str())
        .bind(profile_id)
        .bind(&ciphertext)
        .bind(&nonce[..])
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn delete_profile_secret(
        &self,
        conn: &mut sqlx::SqliteConnection,
        kind: SecretKind,
        profile_id: i64,
    ) -> Result<(), ConsistencyError> {
        sqlx::query("DELETE FROM secrets WHERE kind = ? AND profile_id = ?")
            .bind(kind.as_str())
            .bind(profile_id)
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    /// P1 审查 R3#4-b：档案创建 + 全部凭据同事务。
    /// 返回新档案 id。
    pub async fn create_profile_with_credentials(
        &self,
        name: &str,
        mode: &str,
        org: Option<&str>,
        creds: [(&'static str, SecretKind, Option<String>); 3],
    ) -> Result<i64, ConsistencyError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // §16.9：free 档案全局唯一（事务内守卫，防并发双建）。
        if mode == "free" {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM account_profiles WHERE mode = 'free'")
                    .fetch_one(&mut *tx)
                    .await?;
            if count > 0 {
                return Err(ConsistencyError::FreeProfileConflict);
            }
        }
        let id = sqlx::query(
            "INSERT INTO account_profiles (name, mode, zero_trust_org) VALUES (?, ?, ?)",
        )
        .bind(name)
        .bind(mode)
        .bind(org)
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        for (_, kind, value) in creds {
            if let Some(v) = value {
                if !v.is_empty() {
                    self.set_profile_secret(&mut tx, kind, id, &v).await?;
                }
            }
        }
        tx.commit().await.map_err(ConsistencyError::from)?;
        Ok(id)
    }

    /// P1 审查 R3#4-b：档案元数据更新 + 凭据轮换/清除同事务；
    /// 成功后对绑定实例标记 restart_pending（同一事务内，杜绝陈旧凭据静默）。
    #[allow(clippy::type_complexity)]
    pub async fn update_profile_with_credentials(
        &self,
        profile_id: i64,
        name: &str,
        mode: &str,
        org: Option<&str>,
        creds: [(&'static str, SecretKind, Option<String>); 3],
    ) -> Result<(), ConsistencyError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        // §16.9：目标模式为 free 时，除自身外不得已有其他 free 档。
        if mode == "free" {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM account_profiles WHERE mode = 'free' AND id != ?",
            )
            .bind(profile_id)
            .fetch_one(&mut *tx)
            .await?;
            if count > 0 {
                return Err(ConsistencyError::FreeProfileConflict);
            }
        }
        sqlx::query("UPDATE account_profiles SET name = ?, mode = ?, zero_trust_org = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?")
            .bind(name)
            .bind(mode)
            .bind(org)
            .bind(profile_id)
            .execute(&mut *tx)
            .await?;
        for (kind_str_unused, kind, value) in &creds {
            let _ = kind_str_unused;
            match value {
                Some(v) if v.is_empty() => {
                    self.delete_profile_secret(&mut tx, *kind, profile_id)
                        .await?;
                }
                Some(v) => {
                    self.set_profile_secret(&mut tx, *kind, profile_id, v)
                        .await?
                }
                None => {}
            }
        }
        // P1 审查 R4 次要项：被引用档案在 API 层只读（409 拒绝任何修改），
        // 此处「标记绑定实例重启」的 UPDATE 不可能命中行——删除死逻辑，
        // 消除与文档（§16.9 只读 vs 自动重启）的矛盾。
        tx.commit().await.map_err(ConsistencyError::from)
    }
}
