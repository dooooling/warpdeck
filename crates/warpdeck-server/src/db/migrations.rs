//! 编译期内嵌的 migration 集合与应用（DESIGN §25.10）。
//!
//! 规则：migration 一经发布不得修改历史内容，只能追加新 migration；
//! migration 必须支持从上一支持版本升级；每个 migration PR 携带迁移测试。

use sqlx::migrate::MigrateError;
use sqlx::sqlite::SqlitePool;

/// sqlx `migrate!` 宏：编译期读取 `migrations/` 目录并内嵌，离线可用。
const MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// 应用尚未执行的 migration（幂等，已记录的会自动跳过）。
pub async fn run(pool: &SqlitePool) -> Result<(), MigrateError> {
    MIGRATIONS.run(pool).await
}

/// 已应用的 migration 数量（测试与诊断用）。
pub async fn applied_count(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await
}
