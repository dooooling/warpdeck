//! 多账号档案仓储（v0.2，DESIGN §16.9 / §17.6；PLAN §27.2 任务 B）。
//!
//! `account_profiles` 表由 migration 0005 建立；每个档案是一组独立凭据 +
//! 独立 WARP 出口。凭据仍只存 `secrets` 表密文（profile_id 非 NULL），
//! 本模块只负责非 secret 字段（name/mode/org）与删除保护规则。

use async_trait::async_trait;
use sqlx::error::DatabaseError;
use sqlx::SqlitePool;
use thiserror::Error;

use super::account::AccountMode;
use crate::crypto::secret_store::SecretKind;

/// 内置默认档 id：新实例未指定档案时绑定它；不可删除、可改名（§16.9）。
pub const DEFAULT_PROFILE_ID: i64 = 1;

/// 档案视图（读侧）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountProfile {
    pub id: i64,
    pub name: String,
    pub mode: AccountMode,
    /// Zero Trust org 名（非 secret，明文存储）。
    pub zero_trust_org: Option<String>,
}

/// 档案仓储错误。
#[derive(Debug, Error)]
pub enum AccountProfileError {
    #[error("account profile not found: {0}")]
    NotFound(i64),
    #[error("account profile conflict: {0}")]
    Conflict(String),
    #[error("account profile db error: {0}")]
    Db(String),
}

impl From<sqlx::Error> for AccountProfileError {
    fn from(e: sqlx::Error) -> Self {
        AccountProfileError::Db(e.to_string())
    }
}

/// `mode` 要求具备的凭据（secret kind）列表；free 无要求。
pub fn required_secret_kinds(mode: AccountMode) -> Vec<SecretKind> {
    match mode {
        AccountMode::Free => Vec::new(),
        AccountMode::WarpPlus => vec![SecretKind::WarpPlusLicense],
        AccountMode::ZeroTrust => vec![
            SecretKind::ZeroTrustClientId,
            SecretKind::ZeroTrustClientSecret,
        ],
    }
}

/// 档案仓储接缝。
#[async_trait]
pub trait AccountProfileRepository: Send + Sync {
    /// 创建档案（name 必须唯一；失败原因含唯一冲突）。
    async fn create(
        &self,
        name: &str,
        mode: AccountMode,
        zero_trust_org: Option<&str>,
    ) -> Result<AccountProfile, AccountProfileError>;
    /// 全部档案，含内置默认档。
    async fn list(&self) -> Result<Vec<AccountProfile>, AccountProfileError>;
    async fn get(&self, id: i64) -> Result<AccountProfile, AccountProfileError>;
    /// 更新非 secret 字段（name/mode/org）。
    async fn update(
        &self,
        id: i64,
        name: &str,
        mode: AccountMode,
        zero_trust_org: Option<&str>,
    ) -> Result<AccountProfile, AccountProfileError>;
    /// 删除档案。规则（§16.9）：
    /// - 内置默认档（id = `DEFAULT_PROFILE_ID`）拒绝；
    /// - 先解绑所有 disabled 实例的引用（置 NULL），再删；
    /// - 仍被任一 enabled 实例引用时拒绝（FK 违例转 Conflict）。
    async fn delete(&self, id: i64) -> Result<(), AccountProfileError>;
}

/// SQLite 实现（`account_profiles` 表，migration 0005）。
pub struct SqliteAccountProfileRepository {
    pool: SqlitePool,
}

impl SqliteAccountProfileRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn map_row(
    id: i64,
    name: String,
    mode: String,
    zero_trust_org: Option<String>,
) -> Result<AccountProfile, AccountProfileError> {
    Ok(AccountProfile {
        id,
        name,
        // 表 CHECK 已限制合法值；解析失败按防御性兜底为 free（与 account.rs 一致）。
        mode: AccountMode::parse(&mode).unwrap_or(AccountMode::Free),
        zero_trust_org,
    })
}

#[async_trait]
impl AccountProfileRepository for SqliteAccountProfileRepository {
    async fn create(
        &self,
        name: &str,
        mode: AccountMode,
        zero_trust_org: Option<&str>,
    ) -> Result<AccountProfile, AccountProfileError> {
        // §16.9：free 档全局唯一——系统至多一个 free 档（内置默认档覆盖）。
        if mode == AccountMode::Free && self.count_mode(AccountMode::Free).await? > 0 {
            return Err(AccountProfileError::Conflict(
                "only one free profile is allowed; the default profile already covers it".into(),
            ));
        }
        let result = sqlx::query(
            r#"
            INSERT INTO account_profiles (name, mode, zero_trust_org)
            VALUES (?, ?, ?)
            "#,
        )
        .bind(name)
        .bind(mode.as_str())
        .bind(zero_trust_org)
        .execute(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(db) if is_unique_violation(db.as_ref()) => {
                AccountProfileError::Conflict(format!("profile name `{name}` already exists"))
            }
            other => AccountProfileError::from(other),
        })?;
        self.get(result.last_insert_rowid()).await
    }

    async fn list(&self) -> Result<Vec<AccountProfile>, AccountProfileError> {
        let rows =
            sqlx::query("SELECT id, name, mode, zero_trust_org FROM account_profiles ORDER BY id")
                .fetch_all(&self.pool)
                .await?;
        use sqlx::Row;
        rows.into_iter()
            .map(|row| {
                map_row(
                    row.get("id"),
                    row.get("name"),
                    row.get("mode"),
                    row.get("zero_trust_org"),
                )
            })
            .collect()
    }

    async fn get(&self, id: i64) -> Result<AccountProfile, AccountProfileError> {
        let row =
            sqlx::query("SELECT id, name, mode, zero_trust_org FROM account_profiles WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                .ok_or(AccountProfileError::NotFound(id))?;
        use sqlx::Row;
        map_row(
            row.get("id"),
            row.get("name"),
            row.get("mode"),
            row.get("zero_trust_org"),
        )
    }

    async fn update(
        &self,
        id: i64,
        name: &str,
        mode: AccountMode,
        zero_trust_org: Option<&str>,
    ) -> Result<AccountProfile, AccountProfileError> {
        let before = self.get(id).await?;
        // §16.9：free 档只读——内置默认档是系统保留资源，名称/模式/凭据均不可改。
        if before.mode == AccountMode::Free {
            return Err(AccountProfileError::Conflict(
                "the free profile is read-only; the built-in default serves as the system's free profile".into(),
            ));
        }
        // §16.9：free 档全局唯一——改模式为 free 时若已存在其他 free 档则拒绝
        //（上面已排除 before.free，故只有"非 free 档改回 free"会走到这里）。
        if mode == AccountMode::Free && self.count_mode(AccountMode::Free).await? > 0 {
            return Err(AccountProfileError::Conflict(
                "only one free profile is allowed; bind instances to the default profile or upgrade it first"
                    .into(),
            ));
        }
        // §16.9：凭据相关的字段（mode / org）变化会影响运行中实例的出口行为，
        // 必须标记绑定实例重启（仅 enabled + desired=running；由 Reconciler 收敛，
        // 失败上浮，不静默成功）。name 是纯展示字段，不触发。
        let runtime_relevant =
            before.mode != mode || before.zero_trust_org != zero_trust_org.map(str::to_string);
        let result = sqlx::query(
            r#"
            UPDATE account_profiles
            SET name = ?, mode = ?, zero_trust_org = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            WHERE id = ?
            "#,
        )
        .bind(name)
        .bind(mode.as_str())
        .bind(zero_trust_org)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(db) if is_unique_violation(db.as_ref()) => {
                AccountProfileError::Conflict(format!("profile name `{name}` already exists"))
            }
            other => AccountProfileError::from(other),
        })?;
        if result.rows_affected() == 0 {
            return Err(AccountProfileError::NotFound(id));
        }
        if runtime_relevant {
            sqlx::query(
                "UPDATE warp_instances SET restart_pending = 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE account_profile_id = ? AND enabled = 1 AND desired_state = 'running'",
            )
            .bind(id)
            .execute(&self.pool)
            .await?;
        }
        self.get(id).await
    }

    async fn delete(&self, id: i64) -> Result<(), AccountProfileError> {
        if id == DEFAULT_PROFILE_ID {
            return Err(AccountProfileError::Conflict(
                "the built-in default profile cannot be deleted".into(),
            ));
        }
        // 解绑停用实例（enabled=0）的引用；启用的实例仍引用则下句 DELETE 触发 FK 违例。
        sqlx::query("UPDATE warp_instances SET account_profile_id = NULL WHERE account_profile_id = ? AND enabled = 0")
            .bind(id)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM account_profiles WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| match e {
                sqlx::Error::Database(db)
                    if db.as_ref().is_foreign_key_violation() || is_fk_message(db.as_ref()) =>
                {
                    AccountProfileError::Conflict(format!(
                        "profile {id} is still used by an enabled instance"
                    ))
                }
                other => AccountProfileError::from(other),
            })?;
        Ok(())
    }
}

/// 唯一约束冲突检测（name 列 UNIQUE）。
fn is_unique_violation(db: &dyn DatabaseError) -> bool {
    db.code().is_some_and(|c| c == "1555") || db.message().contains("UNIQUE constraint failed")
}

impl SqliteAccountProfileRepository {
    async fn count_mode(&self, mode: AccountMode) -> Result<i64, AccountProfileError> {
        use sqlx::Row;
        let row = sqlx::query("SELECT COUNT(*) FROM account_profiles WHERE mode = ?")
            .bind(mode.as_str())
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get(0))
    }
}

/// 外键违例检测：`is_foreign_key_violation` 仅在 error code 为
/// `SQLITE_CONSTRAINT_FOREIGNKEY` 时为真；消息匹配兜底。
fn is_fk_message(db: &dyn DatabaseError) -> bool {
    db.message().contains("FOREIGN KEY constraint failed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{cleanup_temp_db, temp_db_url};

    async fn test_repo() -> (
        SqlitePool,
        SqliteAccountProfileRepository,
        std::path::PathBuf,
    ) {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = SqliteAccountProfileRepository::new(pool.clone());
        (pool, repo, db_path)
    }

    #[tokio::test]
    async fn default_profile_exists_and_is_listed() {
        let (pool, repo, db_path) = test_repo().await;
        let all = repo.list().await.unwrap();
        assert_eq!(all.len(), 1);
        let default = &all[0];
        assert_eq!(default.id, DEFAULT_PROFILE_ID);
        assert_eq!(default.name, "free");
        assert_eq!(default.mode, AccountMode::Free);

        // 默认档（free）只读：改名被拒（§16.9）。
        let err = repo
            .update(DEFAULT_PROFILE_ID, "默认", AccountMode::Free, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AccountProfileError::Conflict(_)), "{err}");

        // 直接改名（绕过 API/库只读，模拟历史数据）后仍不可删除（按 id 判定，非名字）。
        sqlx::query("UPDATE account_profiles SET name = '默认' WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        let err = repo.delete(DEFAULT_PROFILE_ID).await.unwrap_err();
        assert!(matches!(err, AccountProfileError::Conflict(_)), "{err}");

        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn create_list_get_update_roundtrip() {
        let (pool, repo, db_path) = test_repo().await;
        let created = repo
            .create("team-a", AccountMode::ZeroTrust, Some("cf-team"))
            .await
            .unwrap();
        let got = repo.get(created.id).await.unwrap();
        assert_eq!(got.name, "team-a");
        assert_eq!(got.mode, AccountMode::ZeroTrust);
        assert_eq!(got.zero_trust_org.as_deref(), Some("cf-team"));

        let updated = repo
            .update(created.id, "team-a-2", AccountMode::WarpPlus, None)
            .await
            .unwrap();
        assert_eq!(updated.name, "team-a-2");
        assert_eq!(updated.mode, AccountMode::WarpPlus);
        assert_eq!(updated.zero_trust_org, None);

        assert_eq!(repo.list().await.unwrap().len(), 2);

        let missing = repo.get(9999).await.unwrap_err();
        assert!(matches!(missing, AccountProfileError::NotFound(_)));
        let missing = repo
            .update(9999, "x", AccountMode::Free, None)
            .await
            .unwrap_err();
        assert!(matches!(missing, AccountProfileError::NotFound(_)));

        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn duplicate_name_is_conflict() {
        let (pool, repo, db_path) = test_repo().await;
        repo.create("same", AccountMode::ZeroTrust, Some("cf-team"))
            .await
            .unwrap();
        let err = repo
            .create("same", AccountMode::ZeroTrust, Some("cf-team"))
            .await
            .unwrap_err();
        assert!(matches!(err, AccountProfileError::Conflict(_)), "{err}");

        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn free_profile_is_globally_unique() {
        let (pool, repo, db_path) = test_repo().await;
        // 默认档已是 free → 再建 free 被拒。
        let err = repo
            .create("free-2", AccountMode::Free, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AccountProfileError::Conflict(_)), "{err}");

        // 默认档（free）只读：改名、升级均被拒。
        let err = repo
            .update(DEFAULT_PROFILE_ID, "renamed", AccountMode::Free, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AccountProfileError::Conflict(_)), "{err}");
        let err = repo
            .update(DEFAULT_PROFILE_ID, "renamed", AccountMode::WarpPlus, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AccountProfileError::Conflict(_)), "{err}");

        // 把 ZT 档改成 free 同样被拒（已存在 free 档）。
        let zt = repo
            .create("zt", AccountMode::ZeroTrust, Some("cf-team"))
            .await
            .unwrap();
        let err = repo
            .update(zt.id, "zt", AccountMode::Free, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AccountProfileError::Conflict(_)), "{err}");

        // 非 free 档的常规更新不受影响。
        repo.update(zt.id, "zt-renamed", AccountMode::ZeroTrust, Some("cf-team"))
            .await
            .unwrap();

        // 边界：若历史数据里默认档已被改为非 free（绕过 API/库逻辑，SQL 直改），
        // 系统暂时没有 free 档，此时允许创建 free——仍守"全系统至多一个"。
        sqlx::query("UPDATE account_profiles SET mode = 'warp_plus' WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        let created = repo
            .create("free-2", AccountMode::Free, None)
            .await
            .unwrap();
        assert_eq!(created.mode, AccountMode::Free);
        // 已有 free 档时再次创建依旧被拒。
        let err = repo
            .create("free-3", AccountMode::Free, None)
            .await
            .unwrap_err();
        assert!(matches!(err, AccountProfileError::Conflict(_)), "{err}");

        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn delete_unbinds_disabled_instances_but_rejects_enabled_references() {
        let (pool, repo, db_path) = test_repo().await;
        let profile = repo
            .create("p1", AccountMode::WarpPlus, None)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO warp_instances (id, name, enabled, desired_state, auto_restart, account_profile_id) \
             VALUES (1, 'a', 0, 'running', 1, ?)",
        )
        .bind(profile.id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO warp_instances (id, name, enabled, desired_state, auto_restart, account_profile_id) \
             VALUES (2, 'b', 1, 'running', 1, ?)",
        )
        .bind(profile.id)
        .execute(&pool)
        .await
        .unwrap();

        // 启用的实例仍引用：拒绝。
        let err = repo.delete(profile.id).await.unwrap_err();
        assert!(matches!(err, AccountProfileError::Conflict(_)), "{err}");

        // 停用实例（id=1）先被解绑，因 id=2 仍引用所以还是拒绝。
        sqlx::query("UPDATE warp_instances SET enabled = 0 WHERE id = 2")
            .execute(&pool)
            .await
            .unwrap();
        repo.delete(profile.id).await.unwrap();
        let rebound: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM warp_instances WHERE account_profile_id = ?")
                .bind(profile.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rebound, 0);
        assert!(repo.get(profile.id).await.is_err());

        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[test]
    fn required_kinds_per_mode() {
        assert!(required_secret_kinds(AccountMode::Free).is_empty());
        assert_eq!(
            required_secret_kinds(AccountMode::WarpPlus),
            vec![SecretKind::WarpPlusLicense]
        );
        assert_eq!(
            required_secret_kinds(AccountMode::ZeroTrust),
            vec![
                SecretKind::ZeroTrustClientId,
                SecretKind::ZeroTrustClientSecret
            ]
        );
    }

    #[tokio::test]
    async fn runtime_relevant_update_marks_bound_instances_restart_pending() {
        let (pool, repo, db_path) = test_repo().await;
        let profile = repo
            .create("p2", AccountMode::WarpPlus, None)
            .await
            .unwrap();
        // 绑定两实例：[1] enabled+running（应标记）、[2] enabled+stopped（不动）。
        sqlx::query(
            "INSERT INTO warp_instances (id, name, enabled, desired_state, auto_restart, account_profile_id) \
             VALUES (1, 'a', 1, 'running', 1, ?)",
        )
        .bind(profile.id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO warp_instances (id, name, enabled, desired_state, auto_restart, account_profile_id) \
             VALUES (2, 'b', 1, 'stopped', 1, ?)",
        )
        .bind(profile.id)
        .execute(&pool)
        .await
        .unwrap();

        // name 变更不触发重启（纯展示字段）。
        repo.update(profile.id, "p2-renamed", AccountMode::WarpPlus, None)
            .await
            .unwrap();
        let pending: i64 =
            sqlx::query_scalar("SELECT restart_pending FROM warp_instances WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pending, 0, "改名不应标记重启");

        // mode 变更触发重启（WarpPlus → ZeroTrust；org 一并变更）。
        repo.update(
            profile.id,
            "p2-renamed",
            AccountMode::ZeroTrust,
            Some("cf-org"),
        )
        .await
        .unwrap();
        let pending: i64 =
            sqlx::query_scalar("SELECT restart_pending FROM warp_instances WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pending, 1, "mode 变更必须标记绑定 running 实例");
        let stopped_pending: i64 =
            sqlx::query_scalar("SELECT restart_pending FROM warp_instances WHERE id = 2")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stopped_pending, 0, "stopped 实例不标记");

        // org 变更（zero_trust）同样触发重启。
        repo.update(
            profile.id,
            "p2-renamed",
            AccountMode::ZeroTrust,
            Some("cf-org2"),
        )
        .await
        .unwrap();
        let pending: i64 =
            sqlx::query_scalar("SELECT restart_pending FROM warp_instances WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(pending, 1, "org 变更应保留标记（幂等置 1）");

        pool.close().await;
        cleanup_temp_db(&db_path);
    }
}
