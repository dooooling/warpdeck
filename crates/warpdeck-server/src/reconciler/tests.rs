//! P6 Reconciler 组件测试（计划 §11.3：SQLite temp DB + FakeRuntime）。
//!
//! 测试拓扑：真实 SQLite（temp file）+ 真实 repository + `FakeWarpRuntime` +
//! `FakeProxyApplier` + `ManualClock`。验证：
//! - 期望 running & 实际未运行 → start，且幂等；
//! - 期望 stopped → 停止运行中的实例；
//! - 禁用覆盖 running → 停止；
//! - Failed + auto_restart → restart（backoff 窗口内不重试、到期后重试）；
//! - Failed + auto_restart=false → 不重启；
//! - 删除 DB 行 → registry 孤儿被停止并移除；
//! - 启动失败 → 记录 backoff，指数翻倍，到期后重试；
//! - proxy_config → ProxyApplier 收到对应 GostSettings；
//! - 单实例失败不阻塞其他实例启动；
//! - §11.3 批量：DB 3 个 running → 一轮全部 start；
//! - §11.3 幂等：连续 10 轮不重复创建进程；
//! - §11.3 manager 重启模拟：fresh runtime + 空 registry → desired 恢复；
//! - §11.3 DB 临时失败：reconcile 不 panic、不产生副作用，恢复后继续。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::SqlitePool;

use crate::crypto::secret_store::{SecretKind, SecretStore, SqliteSecretStore};
use crate::db::repo::{
    DesiredState, ProxyConfig, ProxyConfigRepository, RepoError, SqliteProxyConfigRepository,
    SqliteWarpInstanceRepository, WarpInstanceRepository, WarpInstanceSpec,
};
use crate::db::{cleanup_temp_db, temp_db_url};
use crate::proxy::{GostSettings, ProxyAuth};
use crate::reconciler::{ProxyApplier, Reconciler, DEFAULT_BACKOFF_BASE, DEFAULT_BACKOFF_MAX};
use crate::runtime::events::EventBus;
use crate::runtime::fake::{FakeWarpRuntime, ManualClock};
use crate::runtime::instance::InstanceId;
use crate::runtime::registry::{RuntimeRegistry, RuntimeState};

/// 记录每次 apply 的代理配置 + 可注入失败。
#[derive(Default)]
struct FakeProxyApplier {
    applied: std::sync::Mutex<Vec<GostSettings>>,
    fail_next: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl ProxyApplier for FakeProxyApplier {
    async fn apply_config(&self, settings: &GostSettings) -> Result<(), String> {
        if self
            .fail_next
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err("injected proxy failure".into());
        }
        self.applied.lock().unwrap().push(settings.clone());
        Ok(())
    }
}

/// 可开关失败的真实仓库包装（§11.3 "DB 临时失败" 测试注入）。
/// `unavailable = true` 时所有调用返回 `RepoError::Db`，模拟 DB 抖动。
struct FlakyRepo {
    inner: Arc<dyn WarpInstanceRepository>,
    unavailable: AtomicBool,
}

impl FlakyRepo {
    fn new(inner: Arc<dyn WarpInstanceRepository>) -> Self {
        Self {
            inner,
            unavailable: AtomicBool::new(false),
        }
    }

    fn set_unavailable(&self, value: bool) {
        self.unavailable.store(value, Ordering::SeqCst);
    }

    fn fail(&self) -> Result<(), RepoError> {
        if self.unavailable.load(Ordering::SeqCst) {
            Err(RepoError::Db("injected db failure".into()))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl WarpInstanceRepository for FlakyRepo {
    async fn create(
        &self,
        name: &str,
        account_profile_id: Option<i64>,
    ) -> Result<WarpInstanceSpec, RepoError> {
        self.fail()?;
        self.inner.create(name, account_profile_id).await
    }

    async fn get(
        &self,
        id: crate::runtime::instance::InstanceId,
    ) -> Result<Option<WarpInstanceSpec>, RepoError> {
        self.fail()?;
        self.inner.get(id).await
    }

    async fn list(&self) -> Result<Vec<WarpInstanceSpec>, RepoError> {
        self.fail()?;
        self.inner.list().await
    }

    async fn set_desired(
        &self,
        id: crate::runtime::instance::InstanceId,
        enabled: bool,
        desired_state: DesiredState,
    ) -> Result<(), RepoError> {
        self.fail()?;
        self.inner.set_desired(id, enabled, desired_state).await
    }

    async fn delete(&self, id: crate::runtime::instance::InstanceId) -> Result<(), RepoError> {
        self.fail()?;
        self.inner.delete(id).await
    }

    async fn record_backoff(
        &self,
        id: crate::runtime::instance::InstanceId,
        last_failure_at: &str,
        next_retry_at: Option<String>,
    ) -> Result<(), RepoError> {
        self.fail()?;
        self.inner
            .record_backoff(id, last_failure_at, next_retry_at)
            .await
    }

    async fn clear_backoff(
        &self,
        id: crate::runtime::instance::InstanceId,
    ) -> Result<(), RepoError> {
        self.fail()?;
        self.inner.clear_backoff(id).await
    }

    async fn mark_restart_pending_by_profile(&self, profile_id: i64) -> Result<usize, RepoError> {
        self.fail()?;
        self.inner.mark_restart_pending_by_profile(profile_id).await
    }

    async fn clear_restart_pending(
        &self,
        id: crate::runtime::instance::InstanceId,
    ) -> Result<(), RepoError> {
        self.fail()?;
        self.inner.clear_restart_pending(id).await
    }

    async fn rebind_profile(
        &self,
        id: crate::runtime::instance::InstanceId,
        account_profile_id: Option<i64>,
    ) -> Result<(), RepoError> {
        self.fail()?;
        self.inner.rebind_profile(id, account_profile_id).await
    }

    async fn count_bound_to_profile(&self, profile_id: i64) -> Result<usize, RepoError> {
        self.fail()?;
        self.inner.count_bound_to_profile(profile_id).await
    }
}

/// 测试环境装配：真实 repo + Fake runtime + 共享 registry + ManualClock。
struct TestEnv {
    repo: Arc<dyn WarpInstanceRepository>,
    proxy_repo: Arc<SqliteProxyConfigRepository>,
    pool: SqlitePool,
    registry: Arc<RuntimeRegistry>,
    runtime: Arc<FakeWarpRuntime>,
    proxy: Arc<FakeProxyApplier>,
    secrets: Arc<SqliteSecretStore>,
    clock: Arc<ManualClock>,
    reconciler: Reconciler,
    _db_path: PathBuf,
}

impl TestEnv {
    async fn new() -> Self {
        let (url, db_path) = temp_db_url();
        let pool = crate::db::connect(&url).await.unwrap();
        let repo: Arc<dyn WarpInstanceRepository> =
            Arc::new(SqliteWarpInstanceRepository::new(pool.clone()));
        let proxy_repo = Arc::new(SqliteProxyConfigRepository::new(pool.clone()));
        let registry = Arc::new(RuntimeRegistry::new());
        let runtime = Arc::new(FakeWarpRuntime::with_registry(registry.clone()));
        let proxy = Arc::new(FakeProxyApplier::default());
        let secrets = Arc::new(SqliteSecretStore::new(pool.clone(), [9u8; 32]));
        let clock = Arc::new(ManualClock::new());
        let (_, shutdown) = tokio::sync::watch::channel(false);
        let reconciler = Reconciler::new(
            repo.clone(),
            proxy_repo.clone(),
            runtime.clone(),
            registry.clone(),
            proxy.clone(),
            secrets.clone(),
            clock.clone(),
            PathBuf::from("/tmp/warpdeck-test/data"),
            PathBuf::from("/tmp/warpdeck-test/run"),
            DEFAULT_BACKOFF_BASE,
            DEFAULT_BACKOFF_MAX,
            Arc::new(tokio::sync::Notify::new()),
            shutdown,
            EventBus::default(),
        );
        Self {
            repo,
            proxy_repo,
            pool,
            registry,
            runtime,
            proxy,
            secrets,
            clock,
            reconciler,
            _db_path: db_path,
        }
    }

    fn registry_state(&self, instance_id: InstanceId) -> Option<RuntimeState> {
        self.registry.get(instance_id).map(|r| r.state)
    }

    /// 布置 registry 状态（registry 的 `set_state` 对不存在的 id 是 no-op，
    /// 故先确保 entry 存在——等价于真实 manager 的 insert-on-create 语义）。
    fn set_registry_state(&self, instance_id: InstanceId, state: RuntimeState) {
        if self.registry.get(instance_id).is_none() {
            self.registry.insert(instance_id);
        }
        self.registry.set_state(instance_id, state);
    }

    /// 直接更新 auto_restart（写路径 P7 才引入 setter，此处用 SQL）。
    async fn set_auto_restart(&self, instance_id: i64, value: bool) {
        sqlx::query("UPDATE warp_instances SET auto_restart = ? WHERE id = ?")
            .bind(value)
            .bind(instance_id)
            .execute(&self.pool)
            .await
            .unwrap();
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let p = std::mem::replace(&mut self._db_path, PathBuf::new());
        cleanup_temp_db(&p);
    }
}

#[tokio::test]
async fn starts_when_desired_running_and_not_running() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-0", None).await.unwrap();

    env.reconciler.reconcile_once().await;

    assert_eq!(env.runtime.started_ids(), vec![spec.id.as_i64()]);
    assert!(matches!(
        env.registry_state(spec.id),
        Some(RuntimeState::Healthy)
    ));
}

#[tokio::test]
async fn reconcile_is_idempotent_when_healthy() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-1", None).await.unwrap();
    env.set_registry_state(spec.id, RuntimeState::Healthy);

    env.reconciler.reconcile_once().await;
    env.reconciler.reconcile_once().await;

    assert!(
        env.runtime.started_ids().is_empty(),
        "healthy instance must not be restarted"
    );
}

#[tokio::test]
async fn stops_when_desired_stopped() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-2", None).await.unwrap();
    env.set_registry_state(spec.id, RuntimeState::Healthy);
    env.repo
        .set_desired(spec.id, true, DesiredState::Stopped)
        .await
        .unwrap();

    env.reconciler.reconcile_once().await;

    assert_eq!(env.runtime.stopped_ids(), vec![spec.id.as_i64()]);
}

#[tokio::test]
async fn disabling_overrides_running_desired() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-3", None).await.unwrap();
    env.set_registry_state(spec.id, RuntimeState::Healthy);
    env.repo
        .set_desired(spec.id, false, DesiredState::Running)
        .await
        .unwrap();

    env.reconciler.reconcile_once().await;

    assert_eq!(
        env.runtime.stopped_ids(),
        vec![spec.id.as_i64()],
        "enabled=false must stop even when desired=running"
    );
}

#[tokio::test]
async fn failed_with_auto_restart_restarts_with_backoff() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-4", None).await.unwrap();
    env.reconciler.reconcile_once().await;
    assert_eq!(env.runtime.started_ids(), vec![spec.id.as_i64()]);

    // 模拟重启失败 → 记录 backoff → 窗口内不再重试。
    env.runtime.fail_restart(spec.id);
    env.set_registry_state(spec.id, RuntimeState::Failed);
    env.reconciler.reconcile_once().await;
    assert_eq!(env.runtime.restarted_ids().len(), 1);

    // backoff 窗口内：不动。
    env.set_registry_state(spec.id, RuntimeState::Failed);
    env.reconciler.reconcile_once().await;
    assert_eq!(
        env.runtime.restarted_ids().len(),
        1,
        "must not retry inside backoff window"
    );

    // 推进时钟超过窗口（base, 5s→10s 间隔）→ 重试（解除失败注入 → 成功）。
    env.clock.advance_utc(Duration::from_secs(11));
    env.runtime.unfail_restart(spec.id);
    env.set_registry_state(spec.id, RuntimeState::Failed);
    env.reconciler.reconcile_once().await;
    assert_eq!(env.runtime.restarted_ids().len(), 2);
    let spec_final = env.repo.get(spec.id).await.unwrap().unwrap();
    assert!(spec_final.next_retry_at.is_none());
}

#[tokio::test]
async fn start_failure_sets_backoff_and_retries_after_window() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-5", None).await.unwrap();
    env.runtime.fail_next_start();

    env.reconciler.reconcile_once().await;
    assert!(env.runtime.started_ids().is_empty());
    let spec_now = env.repo.get(spec.id).await.unwrap().unwrap();
    assert!(spec_now.last_failure_at.is_some());
    assert!(spec_now.next_retry_at.is_some());

    // 窗口内不重试。
    env.reconciler.reconcile_once().await;
    assert!(env.runtime.started_ids().is_empty());

    // 到期后重试（Failed → restart 路径；无注入 → 成功）→ backoff 清除。
    env.clock.advance_utc(Duration::from_secs(11));
    env.reconciler.reconcile_once().await;
    assert!(matches!(
        env.registry_state(spec.id),
        Some(RuntimeState::Healthy)
    ));
    let spec_final = env.repo.get(spec.id).await.unwrap().unwrap();
    assert!(spec_final.last_failure_at.is_none());
    assert!(spec_final.next_retry_at.is_none());
}

#[tokio::test]
async fn failed_without_auto_restart_stays_failed() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-6", None).await.unwrap();
    env.set_auto_restart(spec.id.as_i64(), false).await;
    env.set_registry_state(spec.id, RuntimeState::Failed);

    env.reconciler.reconcile_once().await;
    env.reconciler.reconcile_once().await;

    assert!(env.runtime.restarted_ids().is_empty());
    assert!(matches!(
        env.registry_state(spec.id),
        Some(RuntimeState::Failed)
    ));
}

#[tokio::test]
async fn deleted_desired_stops_and_removes_runtime() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-7", None).await.unwrap();
    env.set_registry_state(spec.id, RuntimeState::Healthy);
    env.repo.delete(spec.id).await.unwrap();

    env.reconciler.reconcile_once().await;

    assert_eq!(env.runtime.stopped_ids(), vec![spec.id.as_i64()]);
    assert!(env.registry_state(spec.id).is_none());
}

#[tokio::test]
async fn restart_pending_on_healthy_instance_forces_restart_and_clears_flag() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-pending", None).await.unwrap();
    env.set_registry_state(spec.id, RuntimeState::Healthy);
    // 档案（凭据）变更：profile update 会把绑定 running 实例标为 restart_pending。
    sqlx::query("UPDATE warp_instances SET restart_pending = 1 WHERE id = ?")
        .bind(spec.id.as_i64())
        .execute(&env.pool)
        .await
        .unwrap();

    env.reconciler.reconcile_once().await;

    assert_eq!(
        env.runtime.restarted_ids(),
        vec![spec.id.as_i64()],
        "healthy + restart_pending 必须触发 restart"
    );
    let after = env.repo.get(spec.id).await.unwrap().unwrap();
    assert!(!after.restart_pending, "重启成功后标记清零");

    // 第二轮不再重启（标记已清）。
    env.reconciler.reconcile_once().await;
    assert_eq!(env.runtime.restarted_ids().len(), 1, "幂等：不再重复重启");
}

#[tokio::test]
async fn restart_pending_restart_failure_keeps_flag_for_retry() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-pending-flaky", None).await.unwrap();
    env.set_registry_state(spec.id, RuntimeState::Healthy);
    sqlx::query("UPDATE warp_instances SET restart_pending = 1 WHERE id = ?")
        .bind(spec.id.as_i64())
        .execute(&env.pool)
        .await
        .unwrap();
    env.runtime.fail_restart(spec.id);

    env.reconciler.reconcile_once().await;

    let after = env.repo.get(spec.id).await.unwrap().unwrap();
    assert!(
        after.restart_pending,
        "失败必须保留标记（下轮重试，禁止静默成功）"
    );
    assert!(after.last_failure_at.is_some(), "失败记录 backoff");

    // 解除注入 + 越过 backoff 窗口 → 下轮成功 → 标记清零。
    env.runtime.unfail_restart(spec.id);
    env.clock.advance_utc(Duration::from_secs(11));
    env.reconciler.reconcile_once().await;
    let after = env.repo.get(spec.id).await.unwrap().unwrap();
    assert!(!after.restart_pending);
}

#[tokio::test]
async fn restart_pending_on_stopped_instance_starts_it() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-pending-stopped", None).await.unwrap();
    env.set_registry_state(spec.id, RuntimeState::Stopped);
    sqlx::query("UPDATE warp_instances SET restart_pending = 1 WHERE id = ?")
        .bind(spec.id.as_i64())
        .execute(&env.pool)
        .await
        .unwrap();

    env.reconciler.reconcile_once().await;

    assert_eq!(env.runtime.started_ids(), vec![spec.id.as_i64()]);
    let after = env.repo.get(spec.id).await.unwrap().unwrap();
    assert!(!after.restart_pending, "start 成功后同样清零标记");
}

#[tokio::test]
async fn restart_pending_uses_bound_profile_by_design() {
    // 回归探针：reconcile 走 start/restart 时携带 spec.account_profile_id
    // （manager 据此 resolve 档案凭据）。这里验证 profile 绑定在 repo 层保持。
    let env = TestEnv::new().await;
    sqlx::query("INSERT INTO account_profiles (id, name, mode) VALUES (2, 'team-a', 'free')")
        .execute(&env.pool)
        .await
        .unwrap();
    let spec = env.repo.create("inst-bound", Some(2)).await.unwrap();
    let fetched = env.repo.get(spec.id).await.unwrap().unwrap();
    assert_eq!(fetched.account_profile_id, Some(2));
}

#[tokio::test]
async fn one_failure_does_not_block_others() {
    let env = TestEnv::new().await;
    let bad = env.repo.create("bad", None).await.unwrap();
    let good = env.repo.create("good", None).await.unwrap();
    env.runtime.fail_next_start(); // 下一个 start（bad）失败

    env.reconciler.reconcile_once().await;

    assert!(!env.runtime.started_ids().contains(&bad.id.as_i64()));
    assert!(env.runtime.started_ids().contains(&good.id.as_i64()));
}

#[tokio::test]
async fn proxy_config_maps_to_gost_settings_and_applies() {
    let env = TestEnv::new().await;
    let default_cfg = env.proxy_repo.get().await.unwrap();

    env.reconciler.reconcile_once().await;

    let applied = env.proxy.applied.lock().unwrap().clone();
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].socks5_enabled, default_cfg.socks5_enabled);
    assert_eq!(applied[0].http_enabled, default_cfg.http_enabled);
    assert!(
        applied[0].auth.is_none(),
        "auth disabled when auth_enabled=false"
    );
}

#[tokio::test]
async fn proxy_config_update_is_reflected() {
    let env = TestEnv::new().await;
    env.secrets
        .set(SecretKind::ProxyPassword, "s3cret-pass")
        .await
        .unwrap();
    let cfg = ProxyConfig {
        socks5_enabled: false,
        http_enabled: true,
        auth_enabled: true,
        proxy_username: Some("alice".into()),
        allowed_ips: vec!["10.0.0.0/8".into()],
        max_connections: Some(42),
        max_rps: None,
    };
    env.proxy_repo.update(&cfg).await.unwrap();

    env.reconciler.reconcile_once().await;

    let applied = env.proxy.applied.lock().unwrap().clone();
    assert_eq!(applied.len(), 1);
    let expected = GostSettings {
        socks5_enabled: false,
        http_enabled: true,
        auth: Some(ProxyAuth {
            username: "alice".into(),
            password: "s3cret-pass".into(),
        }),
        allowlist: vec!["10.0.0.0/8".into()],
        max_connections: Some(42),
        max_rps: None,
    };
    assert_eq!(applied[0], expected);
    assert!(!applied[0].socks5_enabled);
    assert!(applied[0].http_enabled);
    assert_eq!(applied[0].allowlist, vec!["10.0.0.0/8"]);
    assert_eq!(applied[0].max_connections, Some(42));
    assert_eq!(
        applied[0].auth,
        Some(ProxyAuth {
            username: "alice".into(),
            password: "s3cret-pass".into(),
        }),
        "P8: password must be injected from secret store"
    );
}

#[tokio::test]
async fn proxy_auth_enabled_without_password_leaves_auth_disabled() {
    let env = TestEnv::new().await;
    let cfg = ProxyConfig {
        auth_enabled: true,
        proxy_username: Some("alice".into()),
        ..ProxyConfig::default_enabled()
    };
    env.proxy_repo.update(&cfg).await.unwrap();

    env.reconciler.reconcile_once().await;

    let applied = env.proxy.applied.lock().unwrap().clone();
    assert_eq!(applied.len(), 1);
    assert!(
        applied[0].auth.is_none(),
        "fail-open: no password → no auth"
    );
}

#[tokio::test]
async fn proxy_apply_failure_does_not_block_instances() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-8", None).await.unwrap();
    env.proxy
        .fail_next
        .store(true, std::sync::atomic::Ordering::SeqCst);

    env.reconciler.reconcile_once().await;

    assert_eq!(env.runtime.started_ids(), vec![spec.id.as_i64()]);
    assert!(env.proxy.applied.lock().unwrap().is_empty());
}

#[tokio::test]
async fn should_run_semantics() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-9", None).await.unwrap();
    let fetched = env.repo.get(spec.id).await.unwrap().unwrap();
    assert!(fetched.should_run());

    let mut disabled = fetched.clone();
    disabled.enabled = false;
    assert!(!disabled.should_run());

    let mut stopped = fetched.clone();
    stopped.desired_state = DesiredState::Stopped;
    assert!(!stopped.should_run());
}

#[tokio::test]
async fn backoff_doubles_per_failure_round() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-10", None).await.unwrap();

    // Failed → restart 失败 → 第 1 次 backoff = base（5s）。
    env.set_registry_state(spec.id, RuntimeState::Failed);
    env.runtime.fail_restart(spec.id);
    env.reconciler.reconcile_once().await;
    let first = env.repo.get(spec.id).await.unwrap().unwrap();
    let gap1 = backoff_window_secs(&first);

    // 跨过第一窗口后再失败：间隔翻倍（5s → 10s）。
    env.clock.advance_utc(Duration::from_secs(31));
    env.set_registry_state(spec.id, RuntimeState::Failed);
    env.reconciler.reconcile_once().await;
    let second = env.repo.get(spec.id).await.unwrap().unwrap();
    let gap2 = backoff_window_secs(&second);

    assert!(
        gap2 >= gap1 * 2,
        "backoff must double per round: {gap2} vs {gap1}"
    );
    assert!(gap1 >= DEFAULT_BACKOFF_BASE.as_secs() as usize);
}

/// 从 last_failure_at / next_retry_at 推导退避窗口（秒）。
fn backoff_window_secs(spec: &WarpInstanceSpec) -> usize {
    let parse = |s: &str| {
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
    };
    match (
        spec.last_failure_at.as_deref().and_then(parse),
        spec.next_retry_at.as_deref().and_then(parse),
    ) {
        (Some(l), Some(n)) if n > l => (n - l).whole_seconds().max(0) as usize,
        _ => 0,
    }
}

#[tokio::test]
async fn backoff_caps_at_max() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-11", None).await.unwrap();
    // 连续多轮 restart 失败（每轮跨窗口），退避应封顶于 max（300s）。
    let mut last_gap = 0;
    for _ in 0..12 {
        env.runtime.fail_restart(spec.id);
        env.clock.advance_utc(Duration::from_secs(400));
        env.set_registry_state(spec.id, RuntimeState::Failed);
        env.reconciler.reconcile_once().await;
        last_gap = env
            .repo
            .get(spec.id)
            .await
            .unwrap()
            .map(|s| backoff_window_secs(&s))
            .unwrap_or(0);
    }
    assert!(last_gap <= DEFAULT_BACKOFF_MAX.as_secs() as usize + 1);
    assert!(last_gap > DEFAULT_BACKOFF_BASE.as_secs() as usize);
}

// ---------- §11.3 补缺测试 ----------

/// §11.3-1：DB 3 个 running → 一轮 reconcile 全部 start（含 registry 对齐）。
#[tokio::test]
async fn three_running_instances_start_in_one_round() {
    let env = TestEnv::new().await;
    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(
            env.repo
                .create(&format!("inst-batch-{i}"), None)
                .await
                .unwrap()
                .id,
        );
    }

    env.reconciler.reconcile_once().await;

    let mut started = env.runtime.started_ids();
    started.sort_unstable();
    let mut expected: Vec<i64> = ids.iter().map(|id| id.as_i64()).collect();
    expected.sort_unstable();
    assert_eq!(
        started, expected,
        "all 3 desired-running instances must start in one round"
    );
    for id in &ids {
        assert!(matches!(
            env.registry_state(*id),
            Some(RuntimeState::Healthy)
        ));
    }
}

/// §11.3-5：连续 10 轮 reconcile 不重复创建进程（幂等）。
#[tokio::test]
async fn ten_consecutive_rounds_do_not_duplicate_processes() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-idempotent", None).await.unwrap();

    for _ in 0..10 {
        env.reconciler.reconcile_once().await;
    }

    assert_eq!(
        env.runtime.started_ids(),
        vec![spec.id.as_i64()],
        "start must happen exactly once across 10 rounds"
    );
    assert!(env.runtime.restarted_ids().is_empty());
    assert!(env.runtime.stopped_ids().is_empty());
    assert!(matches!(
        env.registry_state(spec.id),
        Some(RuntimeState::Healthy)
    ));
}

/// §11.3-6：manager 重启模拟——fresh runtime + 空 registry，desired 仍 running
/// → 新 reconciler 一轮内全部恢复（P6-008/009 语义：desired 是唯一权威）。
#[tokio::test]
async fn manager_restart_recovers_desired_state() {
    let env = TestEnv::new().await;
    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(
            env.repo
                .create(&format!("inst-recover-{i}"), None)
                .await
                .unwrap()
                .id,
        );
    }
    env.reconciler.reconcile_once().await;
    assert_eq!(
        env.runtime.started_ids().len(),
        3,
        "warm-up round starts all"
    );

    // 模拟 manager 重启：全新 runtime + 全新空 registry（desired 未变）。
    let fresh_registry = Arc::new(RuntimeRegistry::new());
    let fresh_runtime = Arc::new(FakeWarpRuntime::with_registry(fresh_registry.clone()));
    let (_tx, shutdown) = tokio::sync::watch::channel(false);
    let reconciler2 = Reconciler::new(
        env.repo.clone(),
        env.proxy_repo.clone(),
        fresh_runtime.clone(),
        fresh_registry.clone(),
        env.proxy.clone(),
        env.secrets.clone(),
        env.clock.clone(),
        PathBuf::from("/tmp/warpdeck-test/data"),
        PathBuf::from("/tmp/warpdeck-test/run"),
        DEFAULT_BACKOFF_BASE,
        DEFAULT_BACKOFF_MAX,
        Arc::new(tokio::sync::Notify::new()),
        shutdown,
        EventBus::default(),
    );

    reconciler2.reconcile_once().await;

    let mut recovered = fresh_runtime.started_ids();
    recovered.sort_unstable();
    let mut expected: Vec<i64> = ids.iter().map(|id| id.as_i64()).collect();
    expected.sort_unstable();
    assert_eq!(
        recovered, expected,
        "desired running instances must be restarted after manager restart"
    );
    for id in &ids {
        assert!(matches!(
            fresh_registry.get(*id).map(|r| r.state),
            Some(RuntimeState::Healthy)
        ));
    }
}

/// §11.3-Failure：DB 临时失败——reconcile 不 panic、不产生副作用；
/// DB 恢复后下一轮继续收敛。
#[tokio::test]
async fn db_transient_failure_is_contained_and_recovers() {
    let env = TestEnv::new().await;
    let spec = env.repo.create("inst-flaky", None).await.unwrap();
    let flaky = Arc::new(FlakyRepo::new(env.repo.clone()));
    let (_, shutdown) = tokio::sync::watch::channel(false);
    let flaky_reconciler = Reconciler::new(
        flaky.clone(),
        env.proxy_repo.clone(),
        env.runtime.clone(),
        env.registry.clone(),
        env.proxy.clone(),
        env.secrets.clone(),
        env.clock.clone(),
        PathBuf::from("/tmp/warpdeck-test/data"),
        PathBuf::from("/tmp/warpdeck-test/run"),
        DEFAULT_BACKOFF_BASE,
        DEFAULT_BACKOFF_MAX,
        Arc::new(tokio::sync::Notify::new()),
        shutdown,
        EventBus::default(),
    );

    // DB 不可用：不 start、不 apply proxy。
    flaky.set_unavailable(true);
    flaky_reconciler.reconcile_once().await;
    assert!(
        env.runtime.started_ids().is_empty(),
        "no start while DB unavailable"
    );
    assert!(
        env.proxy.applied.lock().unwrap().is_empty(),
        "no proxy apply while DB unavailable"
    );

    // DB 恢复：同一轮收敛逻辑继续工作。
    flaky.set_unavailable(false);
    flaky_reconciler.reconcile_once().await;
    assert_eq!(env.runtime.started_ids(), vec![spec.id.as_i64()]);
    assert!(matches!(
        env.registry_state(spec.id),
        Some(RuntimeState::Healthy)
    ));
}
