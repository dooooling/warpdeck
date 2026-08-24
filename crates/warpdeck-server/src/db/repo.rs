//! Desired-state 仓储（P6-001/002/003）。
//!
//! 设计（DESIGN §5.1 / §16.3，AGENTS.md「SQLite 持有 desired state」）：
//! - `DesiredState` 只表达用户期望（running/stopped），运行时九态见
//!   `runtime::registry::RuntimeState` —— 两者语义分离，不允许互相转换。
//! - `enabled = false` 优先级高于 `desired_state`：关闭即要求停止（DESIGN §12.1）。
//! - 领域代码（reconciler）只依赖 `WarpInstanceRepository` trait，不接触 sqlx；
//!   sqlx 实现 `SqliteWarpInstanceRepository` 在本模块内。
//! - 不持久化 PID（Gate §11.4：DB 不保存短生命周期进程标识）。

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::SqlitePool;
use thiserror::Error;

use crate::runtime::instance::InstanceId;

/// 实例期望状态（P6-003：MVP 两态；对应 `warp_instances.desired_state` 列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    Running,
    Stopped,
}

impl DesiredState {
    pub fn as_str(self) -> &'static str {
        match self {
            DesiredState::Running => "running",
            DesiredState::Stopped => "stopped",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "running" => Ok(DesiredState::Running),
            "stopped" => Ok(DesiredState::Stopped),
            other => Err(format!("invalid desired state `{other}`")),
        }
    }
}

/// 一条实例期望记录（warp_instances 行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarpInstanceSpec {
    pub id: InstanceId,
    pub name: String,
    /// enabled=false 时无论 desired_state 都必须停止（DESIGN §12.1）。
    pub enabled: bool,
    pub desired_state: DesiredState,
    /// 失败后是否允许 reconciler 自动重启（P6-006；阈值 Failed 的唯一自动恢复路径）。
    pub auto_restart: bool,
    /// v0.2 多账号：绑定的账号档案（NULL = 默认 free 档，§16.9）。
    pub account_profile_id: Option<i64>,
    /// v0.2 档案变更标记：=1 表示档案（凭据/模式）已更新，需重启后清零
    /// （migration 0006，§16.9；由 Reconciler 收敛并清零）。
    pub restart_pending: bool,
    /// P1 审查 R2#1（migration 0007）：显式重启命令代数——API 每次 restart +1；
    /// Reconciler 是唯一执行者。> observed = 有待执行命令；停机期间多条排队合并。
    pub restart_command_generation: i64,
    /// Reconciler 已处理到的命令代数（单调追平，不回退）。
    pub observed_restart_generation: i64,
    /// 上次失败时间（ISO8601 UTC；P6-007 backoff）。
    pub last_failure_at: Option<String>,
    /// 下次允许自动重试时间（ISO8601 UTC；None = 立即可重试）。
    pub next_retry_at: Option<String>,
}

impl WarpInstanceSpec {
    /// 是否应运行：enabled 且 desired=running（DESIGN §12.1 的决策入口）。
    pub fn should_run(&self) -> bool {
        self.enabled && self.desired_state == DesiredState::Running
    }
}

/// 期望状态仓储接缝（P6-002：domain 不直接依赖 sqlx）。
#[async_trait]
pub trait WarpInstanceRepository: Send + Sync {
    /// 新建实例（id 自增；回读写入后的记录）。
    async fn create(
        &self,
        name: &str,
        account_profile_id: Option<i64>,
    ) -> Result<WarpInstanceSpec, RepoError>;
    /// 按 id 查询。
    async fn get(&self, id: InstanceId) -> Result<Option<WarpInstanceSpec>, RepoError>;
    /// 全部记录（按 id 升序）。
    async fn list(&self) -> Result<Vec<WarpInstanceSpec>, RepoError>;
    /// 更新期望状态（enabled + desired_state 合一次调用，事务语义由实现负责）。
    async fn set_desired(
        &self,
        id: InstanceId,
        enabled: bool,
        desired_state: DesiredState,
    ) -> Result<(), RepoError>;
    /// 删除实例（同时是"必须停止"的信号：删除后 runtime record 由调用方收敛）。
    async fn delete(&self, id: InstanceId) -> Result<(), RepoError>;
    /// P6-007：失败后记录 backoff（last_failure_at / next_retry_at）。
    async fn record_backoff(
        &self,
        id: InstanceId,
        last_failure_at: &str,
        next_retry_at: Option<String>,
    ) -> Result<(), RepoError>;
    /// 成功后清除 backoff（可选；保留失败历史也行，语义由调用方定）。
    async fn clear_backoff(&self, id: InstanceId) -> Result<(), RepoError>;
    /// v0.2 §16.9：档案（凭据/模式）更新后，标记所有绑定该档案且
    /// enabled + desired=running 的实例需要重启（返回受影响行数）。
    async fn mark_restart_pending_by_profile(&self, profile_id: i64) -> Result<usize, RepoError>;
    /// 重启成功后清除待重启标记。
    async fn clear_restart_pending(&self, id: InstanceId) -> Result<(), RepoError>;
    /// v0.2 §17.4：改绑账号档案（None = 解绑回默认 free 档）。
    /// 任何绑定变化都置 `restart_pending=1`（改绑在下次重启生效，Desired-state
    /// 语义；reconciler 启动/重启成功后清零）。档案不存在时 FK 违例 → Db。
    async fn rebind_profile(
        &self,
        id: InstanceId,
        account_profile_id: Option<i64>,
    ) -> Result<(), RepoError>;
    /// 绑定到指定档案的实例数（§16.9：WARP+ 单实例约束的支撑查询）。
    async fn count_bound_to_profile(&self, profile_id: i64) -> Result<usize, RepoError>;

    /// P1 审查 R2#1：显式重启命令——命令代数 +1（API 只写期望侧，不碰运行时）。
    /// 返回递增后的代数值。行不存在时 0 行受影响，返回 Ok(0)（handler 已先行
    /// 404 校验；此处幂等兜底）。
    async fn request_restart(&self, id: InstanceId) -> Result<i64, RepoError>;

    /// Reconciler 完成 start/restart 后追平命令代数。MAX 守卫：并发下不回退。
    async fn acknowledge_restart(&self, id: InstanceId, generation: i64) -> Result<(), RepoError>;

    /// v0.2 §17.4 改绑（P1 审查 #5 原子版）：§16.9 WARP+ 单实例检查与写入在
    /// 同一 `BEGIN IMMEDIATE` 事务内完成。目标档案为 warp_plus 且已被其他
    /// 实例绑定时返回 `Err(RepoError::ProfileAlreadyBound)`；改绑到当前已绑
    /// 档案（幂等）不受限。
    async fn rebind_profile_guarded(
        &self,
        id: InstanceId,
        account_profile_id: Option<i64>,
    ) -> Result<(), RepoError>;

    /// 创建实例（P1 审查 #5 原子版）：绑定 WARP+ 档案时与 `rebind_profile_guarded`
    /// 受同一事务守卫约束。
    async fn create_guarded(
        &self,
        name: &str,
        account_profile_id: Option<i64>,
    ) -> Result<WarpInstanceSpec, RepoError>;
}

/// 仓储错误（不携带 secret）。
#[derive(Debug, Error)]
pub enum RepoError {
    #[error("database error: {0}")]
    Db(String),
    #[error("desired state column corrupted: {0}")]
    CorruptDesiredState(String),
    /// §16.9：目标 WARP+ 档案已绑定其他实例（原子守卫拒绝；API 映射 409）。
    #[error("warp_plus profile {0} already bound to another instance")]
    ProfileAlreadyBound(i64),
}

impl From<sqlx::Error> for RepoError {
    fn from(e: sqlx::Error) -> Self {
        RepoError::Db(e.to_string())
    }
}

/// sqlx 实现（P6-002）。
pub struct SqliteWarpInstanceRepository {
    pool: SqlitePool,
}

impl SqliteWarpInstanceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn row_to_spec(row: &sqlx::sqlite::SqliteRow) -> Result<WarpInstanceSpec, RepoError> {
    use sqlx::Row;
    let id = row.get::<i64, _>("id");
    let desired_raw: String = row.get("desired_state");
    let desired_state = DesiredState::parse(&desired_raw)
        .map_err(|e| RepoError::CorruptDesiredState(format!("id={id}: {e}")))?;
    Ok(WarpInstanceSpec {
        id: InstanceId::from_db(id).map_err(|e| RepoError::Db(e.to_string()))?,
        name: row.get("name"),
        enabled: row.get::<i64, _>("enabled") != 0,
        desired_state,
        auto_restart: row.get::<i64, _>("auto_restart") != 0,
        account_profile_id: row.get("account_profile_id"),
        restart_pending: row.get::<i64, _>("restart_pending") != 0,
        restart_command_generation: row.get("restart_command_generation"),
        observed_restart_generation: row.get("observed_restart_generation"),
        last_failure_at: row.get("last_failure_at"),
        next_retry_at: row.get("next_retry_at"),
    })
}

const SPEC_COLUMNS: &str =
    "id, name, enabled, desired_state, auto_restart, account_profile_id, restart_pending, restart_command_generation, observed_restart_generation, last_failure_at, next_retry_at";

#[async_trait]
impl WarpInstanceRepository for SqliteWarpInstanceRepository {
    async fn create(
        &self,
        name: &str,
        account_profile_id: Option<i64>,
    ) -> Result<WarpInstanceSpec, RepoError> {
        let id = sqlx::query("INSERT INTO warp_instances (name, account_profile_id) VALUES (?, ?)")
            .bind(name)
            .bind(account_profile_id)
            .execute(&self.pool)
            .await?
            .last_insert_rowid();
        self.get(InstanceId::from_db(id).map_err(|e| RepoError::Db(e.to_string()))?)
            .await?
            .ok_or_else(|| RepoError::Db("row vanished after insert".into()))
    }

    async fn get(&self, id: InstanceId) -> Result<Option<WarpInstanceSpec>, RepoError> {
        let row = sqlx::query(&format!(
            "SELECT {SPEC_COLUMNS} FROM warp_instances WHERE id = ?"
        ))
        .bind(id.as_i64())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_spec(&r)).transpose()
    }

    async fn list(&self) -> Result<Vec<WarpInstanceSpec>, RepoError> {
        let rows = sqlx::query(&format!(
            "SELECT {SPEC_COLUMNS} FROM warp_instances ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_spec).collect()
    }

    async fn set_desired(
        &self,
        id: InstanceId,
        enabled: bool,
        desired_state: DesiredState,
    ) -> Result<(), RepoError> {
        sqlx::query(
            "UPDATE warp_instances SET enabled = ?, desired_state = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
        )
        .bind(if enabled { 1 } else { 0 })
        .bind(desired_state.as_str())
        .bind(id.as_i64())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete(&self, id: InstanceId) -> Result<(), RepoError> {
        sqlx::query("DELETE FROM warp_instances WHERE id = ?")
            .bind(id.as_i64())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn record_backoff(
        &self,
        id: InstanceId,
        last_failure_at: &str,
        next_retry_at: Option<String>,
    ) -> Result<(), RepoError> {
        sqlx::query(
            "UPDATE warp_instances SET last_failure_at = ?, next_retry_at = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
        )
        .bind(last_failure_at)
        .bind(next_retry_at)
        .bind(id.as_i64())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn clear_backoff(&self, id: InstanceId) -> Result<(), RepoError> {
        sqlx::query(
            "UPDATE warp_instances SET last_failure_at = NULL, next_retry_at = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
        )
        .bind(id.as_i64())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mark_restart_pending_by_profile(&self, profile_id: i64) -> Result<usize, RepoError> {
        let result = sqlx::query(
            "UPDATE warp_instances SET restart_pending = 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE account_profile_id = ? AND enabled = 1 AND desired_state = 'running'",
        )
        .bind(profile_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() as usize)
    }

    async fn clear_restart_pending(&self, id: InstanceId) -> Result<(), RepoError> {
        sqlx::query(
            "UPDATE warp_instances SET restart_pending = 0, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
        )
        .bind(id.as_i64())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn rebind_profile(
        &self,
        id: InstanceId,
        account_profile_id: Option<i64>,
    ) -> Result<(), RepoError> {
        sqlx::query(
            "UPDATE warp_instances SET account_profile_id = ?, restart_pending = 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
        )
        .bind(account_profile_id)
        .bind(id.as_i64())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn count_bound_to_profile(&self, profile_id: i64) -> Result<usize, RepoError> {
        use sqlx::Row;
        let row = sqlx::query("SELECT COUNT(*) FROM warp_instances WHERE account_profile_id = ?")
            .bind(profile_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get::<i64, _>(0) as usize)
    }

    async fn request_restart(&self, id: InstanceId) -> Result<i64, RepoError> {
        use sqlx::Row;
        let row = sqlx::query(
            "UPDATE warp_instances SET restart_command_generation = restart_command_generation + 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ? RETURNING restart_command_generation",
        )
        .bind(id.as_i64())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.get::<i64, _>(0)).unwrap_or(0))
    }

    async fn acknowledge_restart(&self, id: InstanceId, generation: i64) -> Result<(), RepoError> {
        sqlx::query(
            "UPDATE warp_instances SET observed_restart_generation = MAX(observed_restart_generation, ?), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
        )
        .bind(generation)
        .bind(id.as_i64())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn rebind_profile_guarded(
        &self,
        id: InstanceId,
        account_profile_id: Option<i64>,
    ) -> Result<(), RepoError> {
        // BEGIN IMMEDIATE：写事务在检查前即持有保留锁，串行化并发绑定竞争。
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_warp_plus_bindable(&mut tx, account_profile_id, Some(id.as_i64())).await?;
        sqlx::query(
            "UPDATE warp_instances SET account_profile_id = ?, restart_pending = 1, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
        )
        .bind(account_profile_id)
        .bind(id.as_i64())
        .execute(&mut *tx)
        .await?;
        tx.commit().await.map_err(RepoError::from)
    }

    async fn create_guarded(
        &self,
        name: &str,
        account_profile_id: Option<i64>,
    ) -> Result<WarpInstanceSpec, RepoError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_warp_plus_bindable(&mut tx, account_profile_id, None).await?;
        let id = sqlx::query("INSERT INTO warp_instances (name, account_profile_id) VALUES (?, ?)")
            .bind(name)
            .bind(account_profile_id)
            .execute(&mut *tx)
            .await?
            .last_insert_rowid();
        let spec = sqlx::query(&format!(
            "SELECT {SPEC_COLUMNS} FROM warp_instances WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| RepoError::Db("row vanished after insert".into()));
        let spec = spec.and_then(|row| row_to_spec(&row));
        tx.commit().await?;
        spec
    }
}
/// §16.9 守卫（调用方必须已处于 `BEGIN IMMEDIATE` 事务内）：目标档案存在且为
/// warp_plus 模式时，要求除 `exclude_instance_id`（改绑自身幂等）外无其他实例
/// 绑定；否则返回 `ProfileAlreadyBound`。非 warp_plus / None 恒通过。
async fn ensure_warp_plus_bindable(
    conn: &mut sqlx::SqliteConnection,
    profile_id: Option<i64>,
    exclude_instance_id: Option<i64>,
) -> Result<(), RepoError> {
    use sqlx::Row;
    let Some(pid) = profile_id else {
        return Ok(());
    };
    let mode: Option<String> = sqlx::query("SELECT mode FROM account_profiles WHERE id = ?")
        .bind(pid)
        .fetch_optional(&mut *conn)
        .await?
        .map(|r| r.get(0));
    // 档案不存在 → 放行由 FK 在写入时报错（API 层已先行校验存在性）。
    if mode.as_deref() != Some("warp_plus") {
        return Ok(());
    }
    let bound: i64 = match exclude_instance_id {
        Some(self_id) => {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM warp_instances WHERE account_profile_id = ? AND id != ?",
            )
            .bind(pid)
            .bind(self_id)
            .fetch_one(&mut *conn)
            .await?
        }
        None => {
            sqlx::query_scalar("SELECT COUNT(*) FROM warp_instances WHERE account_profile_id = ?")
                .bind(pid)
                .fetch_one(&mut *conn)
                .await?
        }
    };
    if bound > 0 {
        return Err(RepoError::ProfileAlreadyBound(pid));
    }
    Ok(())
}

/// 代理期望配置（proxy_config 单行 → ProxySettings 的中间形态；P6 只读）。
///
/// auth 的密码在 P8 前不落库（`proxy_password_secret_id` 恒 NULL），
/// `auth_enabled = true` 且无 secret id 时按"未配置"处理：渲染时不启用 auth。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProxyConfig {
    pub socks5_enabled: bool,
    pub http_enabled: bool,
    pub auth_enabled: bool,
    pub proxy_username: Option<String>,
    pub allowed_ips: Vec<String>,
    pub max_connections: Option<u32>,
    pub max_rps: Option<u32>,
}

impl ProxyConfig {
    /// 默认 MOTS：双 listener 开启、无 auth、无 allowlist。
    pub fn default_enabled() -> Self {
        Self {
            socks5_enabled: true,
            http_enabled: true,
            ..Self::default()
        }
    }
}

/// 代理期望配置仓储（单行表 id=1，DESIGN §16.4）。
#[async_trait]
pub trait ProxyConfigRepository: Send + Sync {
    async fn get(&self) -> Result<ProxyConfig, RepoError>;
    /// 未来 P7 写路径（暂仅测试/迁移使用）。
    async fn update(&self, cfg: &ProxyConfig) -> Result<(), RepoError>;
}

pub struct SqliteProxyConfigRepository {
    pool: SqlitePool,
}

impl SqliteProxyConfigRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn row_to_proxy_config(row: &sqlx::sqlite::SqliteRow) -> ProxyConfig {
    use sqlx::Row;
    let allowed_ips: Option<String> = row.get("allowed_ips");
    ProxyConfig {
        socks5_enabled: row.get::<i64, _>("socks5_enabled") != 0,
        http_enabled: row.get::<i64, _>("http_enabled") != 0,
        auth_enabled: row.get::<i64, _>("auth_enabled") != 0,
        proxy_username: row.get("proxy_username"),
        allowed_ips: allowed_ips
            .as_deref()
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        max_connections: Some(row.get::<i64, _>("max_connections") as u32).filter(|&v| v != 0),
        max_rps: Some(row.get::<i64, _>("max_rps") as u32).filter(|&v| v != 0),
    }
}

#[async_trait]
impl ProxyConfigRepository for SqliteProxyConfigRepository {
    async fn get(&self) -> Result<ProxyConfig, RepoError> {
        let row = sqlx::query(
            "SELECT id, socks5_enabled, http_enabled, auth_enabled, proxy_username, allowed_ips, max_connections, max_rps FROM proxy_config WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(row_to_proxy_config(&r)),
            // 表中无行 = 全默认（DESIGN §16.4 默认值）。
            None => Ok(ProxyConfig::default_enabled()),
        }
    }

    async fn update(&self, cfg: &ProxyConfig) -> Result<(), RepoError> {
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// 仓储测试辅助：指定 Pool 直接构造 `Arc<dyn>` 包装。
pub fn instance_repo(pool: SqlitePool) -> Arc<dyn WarpInstanceRepository> {
    Arc::new(SqliteWarpInstanceRepository::new(pool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{cleanup_temp_db, temp_db_url};

    fn id(value: i64) -> InstanceId {
        InstanceId::from_db(value).unwrap()
    }

    #[tokio::test]
    async fn repository_roundtrip_create_get_list() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = SqliteWarpInstanceRepository::new(pool.clone());

        let created = repo.create("instance-zero", None).await.unwrap();
        assert_eq!(created.name, "instance-zero");
        assert!(created.enabled);
        assert_eq!(created.desired_state, DesiredState::Running);
        assert!(created.auto_restart);
        assert_eq!(created.last_failure_at, None);

        let got = repo.get(created.id).await.unwrap().unwrap();
        assert_eq!(got, created);

        let second = repo.create("second", None).await.unwrap();
        assert_eq!(repo.list().await.unwrap().len(), 2);
        assert_ne!(second.id, created.id);

        assert!(repo.get(id(99)).await.unwrap().is_none());
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn set_desired_and_delete() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = SqliteWarpInstanceRepository::new(pool.clone());

        let spec = repo.create("a", None).await.unwrap();
        repo.set_desired(spec.id, false, DesiredState::Stopped)
            .await
            .unwrap();
        let updated = repo.get(spec.id).await.unwrap().unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.desired_state, DesiredState::Stopped);

        repo.delete(spec.id).await.unwrap();
        assert!(repo.get(spec.id).await.unwrap().is_none());
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn backoff_fields_persist_and_clear() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = SqliteWarpInstanceRepository::new(pool.clone());

        let spec = repo.create("b", None).await.unwrap();
        repo.record_backoff(
            spec.id,
            "2026-08-18T00:00:00.000Z",
            Some("2026-08-18T01:00:00.000Z".into()),
        )
        .await
        .unwrap();
        let updated = repo.get(spec.id).await.unwrap().unwrap();
        assert_eq!(
            updated.last_failure_at.as_deref(),
            Some("2026-08-18T00:00:00.000Z")
        );
        assert_eq!(
            updated.next_retry_at.as_deref(),
            Some("2026-08-18T01:00:00.000Z")
        );

        repo.clear_backoff(spec.id).await.unwrap();
        let cleared = repo.get(spec.id).await.unwrap().unwrap();
        assert!(cleared.last_failure_at.is_none());
        assert!(cleared.next_retry_at.is_none());
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn restart_pending_marked_by_profile_only_for_running_instances() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = SqliteWarpInstanceRepository::new(pool.clone());

        // 先建档案（FK 约束），再建绑定实例：running / disabled / stopped + 其他档案。
        sqlx::query("INSERT INTO account_profiles (id, name, mode) VALUES (2, 'team-a', 'free')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO account_profiles (id, name, mode) VALUES (3, 'team-b', 'free')")
            .execute(&pool)
            .await
            .unwrap();

        let running = repo.create("r", Some(2)).await.unwrap();
        let disabled = repo.create("d", Some(2)).await.unwrap();
        let stopped = repo.create("s", Some(2)).await.unwrap();
        let other = repo.create("o", Some(3)).await.unwrap();
        repo.set_desired(disabled.id, false, DesiredState::Running)
            .await
            .unwrap();
        repo.set_desired(stopped.id, true, DesiredState::Stopped)
            .await
            .unwrap();

        let marked = repo.mark_restart_pending_by_profile(2).await.unwrap();
        assert_eq!(marked, 1, "仅 enabled+running 的实例被标记");

        let r = repo.get(running.id).await.unwrap().unwrap();
        assert!(r.restart_pending, "running 实例应被标记重启");
        assert!(
            !repo
                .get(disabled.id)
                .await
                .unwrap()
                .unwrap()
                .restart_pending
        );
        assert!(!repo.get(stopped.id).await.unwrap().unwrap().restart_pending);
        assert!(
            !repo.get(other.id).await.unwrap().unwrap().restart_pending,
            "其他档案不受影响"
        );

        repo.clear_restart_pending(running.id).await.unwrap();
        assert!(!repo.get(running.id).await.unwrap().unwrap().restart_pending);

        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn proxy_config_defaults_when_table_empty() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = SqliteProxyConfigRepository::new(pool.clone());

        let cfg = repo.get().await.unwrap();
        assert!(cfg.socks5_enabled);
        assert!(cfg.http_enabled);
        assert!(!cfg.auth_enabled);
        assert!(cfg.allowed_ips.is_empty());
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn proxy_config_update_roundtrip() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = SqliteProxyConfigRepository::new(pool.clone());

        repo.update(&ProxyConfig {
            socks5_enabled: false,
            http_enabled: true,
            auth_enabled: false,
            proxy_username: None,
            allowed_ips: vec!["10.0.0.0/8".into(), "192.168.1.1".into()],
            max_connections: Some(64),
            max_rps: None,
        })
        .await
        .unwrap();

        let cfg = repo.get().await.unwrap();
        assert!(!cfg.socks5_enabled);
        assert!(cfg.http_enabled);
        assert_eq!(cfg.allowed_ips.len(), 2);
        assert_eq!(cfg.max_connections, Some(64));
        assert_eq!(cfg.max_rps, None);
        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[test]
    fn desired_state_parse_roundtrip() {
        assert_eq!(
            DesiredState::parse("running").unwrap(),
            DesiredState::Running
        );
        assert_eq!(
            DesiredState::parse("stopped").unwrap(),
            DesiredState::Stopped
        );
        assert!(DesiredState::parse("nope").is_err());
        assert_eq!(DesiredState::Running.as_str(), "running");
    }

    #[test]
    fn should_run_semantics() {
        let base = WarpInstanceSpec {
            id: id(0),
            name: "x".into(),
            enabled: true,
            desired_state: DesiredState::Running,
            auto_restart: true,
            account_profile_id: None,
            restart_pending: false,
            restart_command_generation: 0,
            observed_restart_generation: 0,
            last_failure_at: None,
            next_retry_at: None,
        };
        assert!(base.should_run());
        let mut disabled = base.clone();
        disabled.enabled = false;
        assert!(!disabled.should_run());
        let mut stopped = base.clone();
        stopped.desired_state = DesiredState::Stopped;
        assert!(!stopped.should_run());
    }

    // ---------- §16.9 WARP+ 单实例原子守卫（P1 审查 #5） ----------

    async fn seed_profile(pool: &SqlitePool, pid: i64, mode: &str) {
        sqlx::query("INSERT INTO account_profiles (id, name, mode) VALUES (?, ?, ?)")
            .bind(pid)
            .bind(format!("profile-{pid}"))
            .bind(mode)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_guarded_rejects_second_warp_plus_binding() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = SqliteWarpInstanceRepository::new(pool.clone());
        seed_profile(&pool, 10, "warp_plus").await;

        repo.create_guarded("first", Some(10)).await.unwrap();
        let second = repo.create_guarded("second", Some(10)).await.unwrap_err();
        assert!(
            matches!(second, RepoError::ProfileAlreadyBound(10)),
            "第二个 WARP+ 绑定必须被原子守卫拒绝: {second:?}"
        );

        // 非 warp_plus 档案不受限。
        seed_profile(&pool, 11, "free").await;
        repo.create_guarded("third", Some(11)).await.unwrap();
        repo.create_guarded("fourth", Some(11)).await.unwrap();

        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    #[tokio::test]
    async fn rebind_guarded_is_atomic_and_self_rebind_idempotent() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = SqliteWarpInstanceRepository::new(pool.clone());
        seed_profile(&pool, 20, "warp_plus").await;
        seed_profile(&pool, 21, "warp_plus").await;

        let a = repo.create_guarded("a", Some(20)).await.unwrap();
        let b = repo.create("b", None).await.unwrap();

        // b 抢占 21 → OK；随后 b 改绑同一档案（幂等）→ 不受自身排除保护。
        repo.rebind_profile_guarded(b.id, Some(21)).await.unwrap();
        repo.rebind_profile_guarded(b.id, Some(21)).await.unwrap();

        // a 想要已被 b 占用的 21 → 冲突。
        let err = repo
            .rebind_profile_guarded(a.id, Some(21))
            .await
            .unwrap_err();
        assert!(matches!(err, RepoError::ProfileAlreadyBound(21)));

        // b 让出（解绑回 free）后，a 即可绑定 21。
        repo.rebind_profile_guarded(b.id, None).await.unwrap();
        repo.rebind_profile_guarded(a.id, Some(21)).await.unwrap();
        assert_eq!(
            repo.get(b.id).await.unwrap().unwrap().account_profile_id,
            None
        );

        pool.close().await;
        cleanup_temp_db(&db_path);
    }

    /// 竞态回归：并发 create_guarded 同一 WARP+ 档案，恰好一个成功。
    #[tokio::test]
    async fn concurrent_warp_plus_creates_admit_exactly_one() {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo = instance_repo(pool.clone());
        seed_profile(&pool, 30, "warp_plus").await;

        let mut tasks = tokio::task::JoinSet::new();
        for n in 0..6 {
            let r = repo.clone();
            tasks.spawn(async move { r.create_guarded(&format!("race-{n}"), Some(30)).await });
        }
        let mut ok = 0;
        let mut conflicts = 0;
        while let Some(res) = tasks.join_next().await {
            match res.unwrap() {
                Ok(_) => ok += 1,
                Err(RepoError::ProfileAlreadyBound(_)) => conflicts += 1,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert_eq!(ok, 1, "恰好一个赢家");
        assert_eq!(conflicts, 5, "其余全部被守卫拒绝");

        pool.close().await;
        cleanup_temp_db(&db_path);
    }
}
