//! 用户仓储（P8-001/002）。
//!
//! MVP 单管理员：`users` 表 + 首次 setup 语义（用户表为空时才能创建首个账号）。

use async_trait::async_trait;
use sqlx::SqlitePool;
use thiserror::Error;

/// 用户记录（读侧；password_hash 绝不出现在 API 响应）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
}

/// 用户仓储错误。
#[derive(Debug, Error)]
pub enum UserRepoError {
    #[error("user db error: {0}")]
    Db(String),
}

impl From<sqlx::Error> for UserRepoError {
    fn from(e: sqlx::Error) -> Self {
        UserRepoError::Db(e.to_string())
    }
}

/// 用户仓储接缝。
#[async_trait]
pub trait UserRepository: Send + Sync {
    /// 用户总数（setup 判定用）。
    async fn count(&self) -> Result<i64, UserRepoError>;
    /// 创建用户（username 唯一冲突返回错误）。
    async fn create(&self, username: &str, password_hash: &str) -> Result<User, UserRepoError>;
    /// 按用户名查询。
    async fn find_by_username(&self, username: &str) -> Result<Option<User>, UserRepoError>;
    /// 按 id 查询。
    async fn get(&self, id: i64) -> Result<Option<User>, UserRepoError>;
    /// 首次 setup 原子语义：`BEGIN IMMEDIATE` 事务内「查空 + 插入」，
    /// 并发双写时只有一个成功（P8-001 永久锁定不变量，DESIGN §20.1）。
    async fn create_admin_if_empty(
        &self,
        username: &str,
        password_hash: &str,
    ) -> Result<AdminSetupResult, UserRepoError>;
}

/// `create_admin_if_empty` 的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminSetupResult {
    /// 首个管理员创建成功。
    Created(UserId),
    /// 已有管理员（本次请求未创建）。
    AlreadyInitialized,
}

/// 新管理员 id（避免在结果枚举里暴露内部结构）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserId(pub i64);

/// SQLite 实现（`users` 表，migration 0003）。
pub struct SqliteUserRepository {
    pool: SqlitePool,
}

impl SqliteUserRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl UserRepository for SqliteUserRepository {
    async fn count(&self) -> Result<i64, UserRepoError> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?)
    }

    async fn create(&self, username: &str, password_hash: &str) -> Result<User, UserRepoError> {
        let id = sqlx::query("INSERT INTO users (username, password_hash) VALUES (?, ?)")
            .bind(username)
            .bind(password_hash)
            .execute(&self.pool)
            .await?
            .last_insert_rowid();
        self.get(id)
            .await?
            .ok_or_else(|| UserRepoError::Db("user vanished after insert".into()))
    }

    async fn find_by_username(&self, username: &str) -> Result<Option<User>, UserRepoError> {
        let row = sqlx::query("SELECT id, username, password_hash FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(user_from_row))
    }

    async fn get(&self, id: i64) -> Result<Option<User>, UserRepoError> {
        let row = sqlx::query("SELECT id, username, password_hash FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(user_from_row))
    }

    async fn create_admin_if_empty(
        &self,
        username: &str,
        password_hash: &str,
    ) -> Result<AdminSetupResult, UserRepoError> {
        // BEGIN IMMEDIATE：获取写锁，阻止并发 setup 通过「查空」检查。
        let mut conn = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&mut *conn)
            .await?;
        let result = if count > 0 {
            AdminSetupResult::AlreadyInitialized
        } else {
            let id = sqlx::query("INSERT INTO users (username, password_hash) VALUES (?, ?)")
                .bind(username)
                .bind(password_hash)
                .execute(&mut *conn)
                .await?
                .last_insert_rowid();
            AdminSetupResult::Created(UserId(id))
        };
        sqlx::query("COMMIT").execute(&mut *conn).await?;
        Ok(result)
    }
}

fn user_from_row(row: &sqlx::sqlite::SqliteRow) -> User {
    use sqlx::Row;
    User {
        id: row.get("id"),
        username: row.get("username"),
        password_hash: row.get("password_hash"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{cleanup_temp_db, temp_db_url};

    #[tokio::test]
    async fn create_find_roundtrip_and_count() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = SqliteUserRepository::new(pool.clone());

        assert_eq!(repo.count().await.unwrap(), 0);
        let user = repo.create("admin", "$argon2id$test$hash").await.unwrap();
        assert_eq!(repo.count().await.unwrap(), 1);
        assert_eq!(repo.get(user.id).await.unwrap(), Some(user.clone()));
        let found = repo.find_by_username("admin").await.unwrap().unwrap();
        assert_eq!(found.id, user.id);
        assert_eq!(found.password_hash, "$argon2id$test$hash");
        assert!(repo.find_by_username("nobody").await.unwrap().is_none());
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn duplicate_username_fails() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = SqliteUserRepository::new(pool.clone());
        repo.create("admin", "h1").await.unwrap();
        assert!(repo.create("admin", "h2").await.is_err());
        pool.close().await;
        cleanup_temp_db(&db_path);
    }
}
