//! SQLite 连接池（P1-006）。
//!
//! 设计依据：
//! - DESIGN §16：schema 全部由 migration 管理（见 `migrations` 模块）。
//! - DESIGN §25.10：测试使用临时 DB，禁止共享开发者个人数据库。
//! - DEVELOPMENT_PLAN P1-006：busy timeout、WAL、migration 自动执行。

pub mod account;
pub mod credentials;
pub mod migrations;
pub mod profiles;
pub mod repo;

use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

/// busy timeout：SQLite 默认行为是立即报 `database is locked`，
/// 我们改为等待其他连接释放（WAL 下主要是写写竞争）。
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// 连接池上限：当前仍单进程小规模管理，保守取值即可，避免过度并发写。
const MAX_CONNECTIONS: u32 = 5;

/// 打开（必要时创建）并迁移数据库，返回就绪的连接池。
///
/// `database_url` 形如 `sqlite:<绝对路径>`（见 `config::AppConfig::database_url`）。
/// 迁移是幂等的：重复启动同一数据库不会重复执行已记录的 migration。
pub async fn connect(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    // `create_if_missing` 只建文件不建父目录：路径所在目录不存在时
    // SQLite 报 `unable to open database file`（实测）。先确保父目录存在。
    if let Some(path) = database_url.strip_prefix("sqlite:") {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .busy_timeout(BUSY_TIMEOUT)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .connect_with(options)
        .await?;

    migrations::run(&pool).await?;
    Ok(pool)
}

/// 在系统临时目录构造独立 DB URL（测试辅助，§25.10：临时 DB / `:memory:`）。
/// 返回 `(sqlite: URL, 文件路径)`。
#[doc(hidden)]
pub fn temp_db_url() -> (String, std::path::PathBuf) {
    use uuid::Uuid;

    let name = format!("warpdeck-test-{}.db", Uuid::new_v4());
    let path = std::env::temp_dir().join(name);
    (format!("sqlite:{}", path.display()), path)
}

/// 删除临时 DB 及其 `-wal`/`-shm` 伴生文件（与 `temp_db_url` 配对使用）。
/// 测试辅助；`TestApp::close` 与 db 模块测试共用。
#[doc(hidden)]
pub fn cleanup_temp_db(db_path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(std::path::PathBuf::from(format!(
            "{}{}",
            db_path.display(),
            suffix
        )));
    }
}

#[cfg(test)]
mod tests {
    use sqlx::Row;

    use super::*;
    use crate::db::migrations;

    async fn table_columns(pool: &SqlitePool, table: &str) -> Vec<String> {
        sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(pool)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.get::<String, _>(1))
            .collect()
    }

    #[tokio::test]
    async fn connect_creates_db_and_applies_migration() {
        let (url, db_path) = temp_db_url();
        let pool = connect(&url).await.unwrap();

        assert_eq!(migrations::applied_count(&pool).await.unwrap(), 6);
        assert_eq!(
            table_columns(&pool, "settings").await,
            vec!["key", "value", "updated_at"]
        );

        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn connect_is_idempotent_across_restarts() {
        let (url, db_path) = temp_db_url();
        let pool = connect(&url).await.unwrap();
        pool.close().await;

        let pool = connect(&url).await.unwrap();
        assert_eq!(migrations::applied_count(&pool).await.unwrap(), 6);

        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn enables_wal_journal_mode() {
        let (url, db_path) = temp_db_url();
        let pool = connect(&url).await.unwrap();

        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mode, "wal");

        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn creates_missing_parent_dir_for_db_path() {
        // 生产缺陷回归：data_dir 不存在时此前 panic（sqlite 不建父目录）。
        let (url, db_path) = temp_db_url();
        let nested =
            std::env::temp_dir().join(format!("warpdeck-db-parent-{}", uuid::Uuid::new_v4()));
        let nested_url = format!("sqlite:{}", nested.join("data/warpdeck.db").display());
        let pool = connect(&nested_url).await.unwrap();
        pool.close().await;
        assert!(nested.join("data/warpdeck.db").exists());
        let _ = std::fs::remove_dir_all(&nested);

        let pool = connect(&url).await.unwrap();
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn settings_table_is_writable() {
        let (url, db_path) = temp_db_url();
        let pool = connect(&url).await.unwrap();

        sqlx::query(
            r#"
            INSERT INTO settings (key, value, updated_at)
            VALUES ('health.interval_seconds', '30', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        let value: String =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'health.interval_seconds'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(value, "30");

        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn pre_release_schema_upgrades_in_place_preserving_data() {
        // P12-010：从上一支持版本 schema（migration 0001+0002）原地升级到
        // 当前 release schema。用临时目录装载旧 migration 文件（内容与内嵌版本
        // 完全一致 → checksum 相同 → 追加的 0003 是唯一待应用项），装入旧数据
        // 后跑当前 migration 集合并断言数据完好、新表可用。
        let dir = std::env::temp_dir().join(format!("warpdeck-old-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["0001_settings.sql", "0002_warp_instances.sql"] {
            let src = format!("{}/migrations/{}", env!("CARGO_MANIFEST_DIR"), f);
            std::fs::copy(src, dir.join(f)).unwrap();
        }

        let (url, db_path) = temp_db_url();
        let pool = SqlitePoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .connect_with(
                SqliteConnectOptions::from_str(&url)
                    .unwrap()
                    .create_if_missing(true)
                    .busy_timeout(BUSY_TIMEOUT)
                    .journal_mode(SqliteJournalMode::Wal),
            )
            .await
            .unwrap();

        // 旧版本（0001+0002）重建 schema 并写入真实业务数据。
        let old = sqlx::migrate::Migrator::new(dir.as_path()).await.unwrap();
        old.run(&pool).await.unwrap();
        assert_eq!(migrations::applied_count(&pool).await.unwrap(), 2);
        sqlx::query("INSERT INTO settings (key, value) VALUES ('health.interval_seconds', '30')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO warp_instances (id, name, enabled, desired_state, auto_restart) \
             VALUES (1, 'upgrade-e2e', 1, 'running', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO proxy_config (id, socks5_enabled, http_enabled, auth_enabled, \
             proxy_username) VALUES (1, 1, 0, 1, 'proxy-user')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // 升级：当前内嵌 migration 集合应追加 0003/0004/0005。
        migrations::run(&pool).await.unwrap();
        assert_eq!(migrations::applied_count(&pool).await.unwrap(), 6);

        // 旧数据完好。
        let v: String =
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'health.interval_seconds'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(v, "30");
        let (name, desired): (String, String) =
            sqlx::query_as("SELECT name, desired_state FROM warp_instances WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(name, "upgrade-e2e");
        assert_eq!(desired, "running");
        let http_enabled: i64 =
            sqlx::query_scalar("SELECT http_enabled FROM proxy_config WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(http_enabled, 0);

        // 0003 新增表存在且可写。
        sqlx::query("INSERT INTO users (username, password_hash) VALUES ('u', 'h')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO secrets (kind, ciphertext, nonce) VALUES ('proxy.password', x'01', x'02')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let _ = std::fs::remove_dir_all(&dir);
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn upgrade_0003_to_0005_preserves_data_and_enables_profiles() {
        // v0.2 任务 A：从上一支持版本 schema（0001+0002+0003）原地升级到
        // 0005。断言：
        //   - 老数据完好（secrets 密文、instance、account_config）
        //   - 0005 建出 account_profiles 且内置默认档 id=1 'free'
        //   - 老 secrets 保留 profile_id=NULL；新 (kind, profile_id) UNIQUE 生效
        //   - 删除档案级联删其 secrets；被实例引用的档案删除报 FK 违例
        let dir = std::env::temp_dir().join(format!("warpdeck-old-mig-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in [
            "0001_settings.sql",
            "0002_warp_instances.sql",
            "0003_auth_secrets.sql",
        ] {
            let src = format!("{}/migrations/{}", env!("CARGO_MANIFEST_DIR"), f);
            std::fs::copy(src, dir.join(f)).unwrap();
        }

        let (url, db_path) = temp_db_url();
        let pool = SqlitePoolOptions::new()
            .max_connections(MAX_CONNECTIONS)
            .connect_with(
                SqliteConnectOptions::from_str(&url)
                    .unwrap()
                    .create_if_missing(true)
                    .busy_timeout(BUSY_TIMEOUT)
                    .journal_mode(SqliteJournalMode::Wal),
            )
            .await
            .unwrap();

        // 旧版本建立 schema 并写入真实业务数据。
        let old = sqlx::migrate::Migrator::new(dir.as_path()).await.unwrap();
        old.run(&pool).await.unwrap();
        assert_eq!(migrations::applied_count(&pool).await.unwrap(), 3);
        sqlx::query(
            "INSERT INTO warp_instances (id, name, enabled, desired_state, auto_restart) \
             VALUES (1, 'legacy-inst', 1, 'running', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO account_config (id, mode, zero_trust_org) VALUES (1, 'zero_trust', 'team-name')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO secrets (kind, ciphertext, nonce) \
             VALUES ('warp_plus_license', x'deadbeef', x'00ff')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO secrets (kind, ciphertext, nonce) VALUES ('proxy_password', x'11', x'22')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // 升级：0004/0005 追加执行。
        migrations::run(&pool).await.unwrap();
        assert_eq!(migrations::applied_count(&pool).await.unwrap(), 6);

        // 老数据完好：secrets 密文原样、kind 两种、profile_id 为 NULL（全局语义）。
        let (kind, ciphertext, profile_id): (String, Vec<u8>, Option<i64>) = sqlx::query_as(
            "SELECT kind, ciphertext, profile_id FROM secrets WHERE kind = 'warp_plus_license'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kind, "warp_plus_license");
        assert_eq!(ciphertext, vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(profile_id, None);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM secrets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);

        // warp_instances 老行 account_profile_id 保持 NULL（= 默认 free）。
        let bound: Option<i64> =
            sqlx::query_scalar("SELECT account_profile_id FROM warp_instances WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(bound, None);

        // 内置默认档存在。
        let (name, mode): (String, String) =
            sqlx::query_as("SELECT name, mode FROM account_profiles WHERE id = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(name, "free");
        assert_eq!(mode, "free");

        // profile 级 secret：写入、同 (kind, profile_id) 冲突、删档案级联删除。
        sqlx::query(
            "INSERT INTO account_profiles (id, name, mode, zero_trust_org) \
             VALUES (2, 'team-a', 'zero_trust', 'team-name')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO secrets (kind, profile_id, ciphertext, nonce) \
             VALUES ('zt_client_id', 2, x'01', x'02')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let dup = sqlx::query(
            "INSERT INTO secrets (kind, profile_id, ciphertext, nonce) \
             VALUES ('zt_client_id', 2, x'03', x'04')",
        )
        .execute(&pool)
        .await;
        assert!(
            dup.is_err(),
            "duplicate (kind, profile_id) must be rejected"
        );

        // 同 kind 不同 profile 允许（多档案互不影响）。
        sqlx::query(
            "INSERT INTO account_profiles (id, name, mode) VALUES (3, 'warp+ 主力', 'warp_plus')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO secrets (kind, profile_id, ciphertext, nonce) \
             VALUES ('zt_client_id', 3, x'05', x'06')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // 级联删除：删档案 3 连带删其 secret。
        sqlx::query("DELETE FROM account_profiles WHERE id = 3")
            .execute(&pool)
            .await
            .unwrap();
        let orphan: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM secrets WHERE profile_id = 3")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(orphan, 0);

        // 被实例引用的档案不可删（FK 拒绝）。
        sqlx::query("UPDATE warp_instances SET account_profile_id = 2 WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        let blocked = sqlx::query("DELETE FROM account_profiles WHERE id = 2")
            .execute(&pool)
            .await;
        assert!(
            blocked.is_err(),
            "deleting a referenced profile must be rejected"
        );

        let _ = std::fs::remove_dir_all(&dir);
        pool.close().await;
        cleanup_temp_db(&db_path);
    }
}
