//! 会话模型与仓储（P8-003/004）。
//!
//! 设计（DESIGN §16.2 + P8 补充）：
//! - session id 与 CSRF token 都是随机值（UUID v4 + 随机 hex）；
//! - cookie 只存 session id；CSRF token 通过 `/auth/me` 提供给前端，
//!   mutation 请求以 `X-CSRF-Token` 头带回；
//! - 过期 session 惰性清理（登录时 DELETE expired），`get` 对过期记录视为不存在。

use async_trait::async_trait;
use sqlx::SqlitePool;
use thiserror::Error;

use super::SESSION_TTL;

/// 一个有效会话（服务端唯一事实）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub user_id: i64,
    pub csrf_token: String,
    /// ISO8601 UTC 过期时间。
    pub expires_at: String,
}

/// 会话仓储接缝。
#[async_trait]
pub trait SessionRepository: Send + Sync {
    /// 创建新会话（随机 id + CSRF token），返回完整会话。
    async fn create(&self, user_id: i64) -> Result<Session, SessionError>;
    /// 按 session id 查询；过期视为不存在。
    async fn get(&self, id: &str) -> Result<Option<Session>, SessionError>;
    /// 刷新 `last_seen_at`（惰性滑动）。
    async fn touch(&self, id: &str) -> Result<(), SessionError>;
    /// 删除（logout）。
    async fn delete(&self, id: &str) -> Result<(), SessionError>;
    /// 清理所有过期会话（登录路径调用）。
    async fn delete_expired(&self) -> Result<(), SessionError>;
}

/// 会话仓储错误。
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session db error: {0}")]
    Db(String),
}

impl From<sqlx::Error> for SessionError {
    fn from(e: sqlx::Error) -> Self {
        SessionError::Db(e.to_string())
    }
}

fn new_random_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn new_csrf_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn expires_at_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .checked_add(SESSION_TTL)
        .expect("session ttl within offsetdatetime range")
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// SQLite 实现（`sessions` 表，migration 0003）。
pub struct SqliteSessionRepository {
    pool: SqlitePool,
}

impl SqliteSessionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 当前时间是否已过 `expires_at`（RFC3339 UTC）。
    fn is_expired(expires_at: &str) -> bool {
        time::OffsetDateTime::parse(expires_at, &time::format_description::well_known::Rfc3339)
            .map(|t| t <= time::OffsetDateTime::now_utc())
            .unwrap_or(true)
    }
}

#[async_trait]
impl SessionRepository for SqliteSessionRepository {
    async fn create(&self, user_id: i64) -> Result<Session, SessionError> {
        let id = new_random_id();
        let csrf = new_csrf_token();
        let expires = expires_at_rfc3339();
        sqlx::query(
            "INSERT INTO sessions (id, user_id, csrf_token, expires_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(user_id)
        .bind(&csrf)
        .bind(&expires)
        .execute(&self.pool)
        .await?;
        Ok(Session {
            id,
            user_id,
            csrf_token: csrf,
            expires_at: expires,
        })
    }

    async fn get(&self, id: &str) -> Result<Option<Session>, SessionError> {
        let row =
            sqlx::query("SELECT id, user_id, csrf_token, expires_at FROM sessions WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        use sqlx::Row;
        let session = Session {
            id: row.get("id"),
            user_id: row.get("user_id"),
            csrf_token: row.get("csrf_token"),
            expires_at: row.get("expires_at"),
        };
        if Self::is_expired(&session.expires_at) {
            // 过期 = 不存在（惰性删除）。
            let _ = self.delete(&session.id).await;
            return Ok(None);
        }
        Ok(Some(session))
    }

    async fn touch(&self, id: &str) -> Result<(), SessionError> {
        sqlx::query(
            "UPDATE sessions SET last_seen_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<(), SessionError> {
        sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_expired(&self) -> Result<(), SessionError> {
        sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
            .bind(
                time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
            )
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{cleanup_temp_db, temp_db_url};

    fn now_rfc3339() -> String {
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    /// sessions.user_id 有外键约束：测试先插入一个用户。
    async fn seed_user(pool: &SqlitePool) {
        sqlx::query("INSERT INTO users (username, password_hash) VALUES ('tester', 'hash')")
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_get_touch_delete_roundtrip() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        seed_user(&pool).await;
        let repo = SqliteSessionRepository::new(pool.clone());

        let session = repo.create(1).await.unwrap();
        assert_ne!(session.id, session.csrf_token);
        assert_eq!(repo.get(&session.id).await.unwrap(), Some(session.clone()));

        repo.touch(&session.id).await.unwrap();
        assert_eq!(repo.get(&session.id).await.unwrap(), Some(session.clone()));

        repo.delete(&session.id).await.unwrap();
        assert_eq!(repo.get(&session.id).await.unwrap(), None);
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn unknown_session_is_none() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = SqliteSessionRepository::new(pool.clone());
        assert_eq!(repo.get("does-not-exist").await.unwrap(), None);
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn expired_session_is_treated_as_absent() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        seed_user(&pool).await;
        let repo = SqliteSessionRepository::new(pool.clone());

        // 直接插入一条已过期会话。
        let expired = now_rfc3339();
        let past =
            time::OffsetDateTime::parse(&expired, &time::format_description::well_known::Rfc3339)
                .unwrap()
                .checked_sub(time::SignedDuration::hours(1))
                .unwrap()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap();
        sqlx::query(
            "INSERT INTO sessions (id, user_id, csrf_token, expires_at) VALUES ('expired-1', 1, 'csrf', ?)",
        )
        .bind(&past)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(repo.get("expired-1").await.unwrap(), None);
        // 惰性删除生效：记录已被清理。
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        pool.close().await;
        cleanup_temp_db(&db_path);
    }
}
