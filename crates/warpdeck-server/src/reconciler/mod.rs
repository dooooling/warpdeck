//! P6 Reconciler：把 SQLite 期望状态收敛为运行时实际状态（DESIGN §12 核心）。
//!
//! 设计约束（DESIGN §12.1-12.5 / DEVELOPMENT_PLAN P6）：
//! - 只读期望状态（`WarpInstanceRepository`），实际状态读 `RuntimeRegistry`；
//! - 动作经 `WarpRuntime`（manager）执行，不直接碰进程/DBus；
//! - 幂等：Healthy/E 进行中的实例不重复 start；保证重入安全；
//! - 单实例失败不阻塞全局（逐实例 try，错误记录后继续）；
//! - Failed 且 auto_restart 时指数退避重试（base 5s，翻倍，封顶 max_backoff）；
//! - 代理（GOST）同步与本循环同频，经 `ProxyApplier` 接缝注入；
//! - 触发：固定 interval + `Notify`（P7 API 变更后调用）+ 事件总线
//!   （health/crash 状态迁移；仅 Failed 类事件触发整轮 reconcile）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{watch, Notify};
use tracing::{debug, error, info, warn};

use crate::db::repo::{ProxyConfigRepository, WarpInstanceRepository, WarpInstanceSpec};
use crate::proxy::GostSettings;
use crate::runtime::clock::Clock;
use crate::runtime::context::InstanceContext;
use crate::runtime::events::{EventBus, HealthEvent};
use crate::runtime::instance::InstanceId;
use crate::runtime::manager::WarpRuntime;
use crate::runtime::registry::{RuntimeRegistry, RuntimeState};

/// 默认轮询间隔（无事件触发时的兜底）。
pub const DEFAULT_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
/// 失败重试基础退避：5s。
pub const DEFAULT_BACKOFF_BASE: Duration = Duration::from_secs(5);
/// 失败重试退避上限：5min。
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(300);

/// 代理配置应用接缝（P6-008 GOST 配置收敛）。
///
/// Reconciler 不直接依赖 `GostManager` 具体类型：测试注入 fake，
/// 生产由 `GostManager` 实现（main 组装时 `Arc<dyn ProxyApplier>`）。
#[async_trait]
pub trait ProxyApplier: Send + Sync {
    /// 应用期望代理配置（GOST 幂等：diff 后跳过或 apply+restart）。
    async fn apply_config(&self, settings: &GostSettings) -> Result<(), String>;

    /// GOST 当前**实际**状态（P1 审查 #4：actual state 必须可观测）。
    /// None = 该实现不追踪（fake 缺省）。
    async fn status(&self) -> Option<crate::proxy::ProxyStatus> {
        None
    }

    /// 显式停止 GOST（P1 审查 #4：期望 = 无任何 listener 时，实际必须无进程；
    /// GostConfig 本身拒绝全关配置，因此「全关」只能走 stop 而非 apply）。
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
}

/// 最近一次代理配置应用失败摘要（API `/system/status` 直出；P1 审查 #3/#4：
/// 应用失败绝不伪装成功）。成功应用后清空。fail-closed 跳过同样记入，
/// 让「密码不可用 → 保持旧配置」在 UI 可见而非只有日志。
pub type ApplyErrorSlot = Arc<std::sync::Mutex<Option<ApplyError>>>;

/// 单条应用失败记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyError {
    pub error: String,
    pub at_rfc3339: String,
}

/// 构造空的失败槽（main / 测试装配用）。
pub fn new_apply_error_slot() -> ApplyErrorSlot {
    Arc::new(std::sync::Mutex::new(None))
}

/// 由 `proxy_config` 仓库行构造 GOST 期望配置（无密码版本；
/// 运行时循环内用 `Reconciler::gost_settings` 补齐密码——见下）。
pub fn proxy_config_to_gost(cfg: &crate::db::repo::ProxyConfig) -> GostSettings {
    GostSettings {
        socks5_enabled: cfg.socks5_enabled,
        http_enabled: cfg.http_enabled,
        auth: None, // 密码由 P8 secret store 注入（sync_proxy）
        allowlist: cfg.allowed_ips.clone(),
        max_connections: cfg.max_connections,
        max_rps: cfg.max_rps,
    }
}

/// Reconciler 主结构（特征化制造：核心依赖全部 trait 化，可注入 fake）。
pub struct Reconciler {
    repo: Arc<dyn WarpInstanceRepository>,
    proxy_repo: Arc<dyn ProxyConfigRepository>,
    runtime: Arc<dyn WarpRuntime>,
    registry: Arc<RuntimeRegistry>,
    proxy: Arc<dyn ProxyApplier>,
    /// P8：代理密码 / 账号凭据的解密来源（渲染 GOST 时读取，绝不落日志）。
    secrets: Arc<dyn crate::crypto::secret_store::SecretStore>,
    clock: Arc<dyn Clock>,
    data_dir: PathBuf,
    runtime_base: PathBuf,
    backoff_base: Duration,
    backoff_max: Duration,
    /// 外部触发（P7 API 变更后调用 `trigger()`）。
    trigger: Arc<Notify>,
    /// 关停信号（main 收到 SIGTERM 后置位）。
    shutdown: watch::Receiver<bool>,
    bus: EventBus,
    /// 最近一次代理应用失败（API /system/status 消费；成功后清空）。
    apply_error: ApplyErrorSlot,
}

impl Reconciler {
    /// 组装（参数在 main/`bootstrap` 汇聚）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Arc<dyn WarpInstanceRepository>,
        proxy_repo: Arc<dyn ProxyConfigRepository>,
        runtime: Arc<dyn WarpRuntime>,
        registry: Arc<RuntimeRegistry>,
        proxy: Arc<dyn ProxyApplier>,
        secrets: Arc<dyn crate::crypto::secret_store::SecretStore>,
        clock: Arc<dyn Clock>,
        data_dir: PathBuf,
        runtime_base: PathBuf,
        backoff_base: Duration,
        backoff_max: Duration,
        trigger: Arc<Notify>,
        shutdown: watch::Receiver<bool>,
        bus: EventBus,
        apply_error: ApplyErrorSlot,
    ) -> Self {
        Self {
            repo,
            proxy_repo,
            runtime,
            registry,
            proxy,
            secrets,
            clock,
            data_dir,
            runtime_base,
            backoff_base,
            backoff_max,
            trigger,
            shutdown,
            bus,
            apply_error,
        }
    }

    /// 写路径（P7 API）在期望状态变更后调用：立即触发一轮收敛。
    /// 使用 `notify_one`：多写者并发时 coalesce，避免风暴（幂等动作允许丢轮）。
    pub fn trigger(&self) {
        self.trigger.notify_one();
    }

    /// 主循环。三路触发：定时 tick / 外部 notify / 事件总线。
    /// `&mut self`：`watch::Receiver::changed` 需要可变借用（字段级借用即可）。
    pub async fn run(&mut self) {
        info!(component = "reconciler", "reconcile loop started");
        let mut interval = tokio::time::interval(DEFAULT_RECONCILE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut events = self.bus.subscribe();

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.reconcile_once().await;
                }
                _ = self.trigger.notified() => {
                    debug!(component = "reconciler", "triggered by notify");
                    self.reconcile_once().await;
                }
                event = events.recv() => {
                    match event {
                        // 仅在迁移到 Failed 时立即收敛（可能触发重启）；
                        // Healthy/Degraded 由健康循环自行处理，reconcile 无需介入。
                        Ok(HealthEvent::StateChanged(tx)) | Ok(HealthEvent::HealthChanged(tx))
                            if tx.to == RuntimeState::Failed =>
                        {
                            info!(component = "reconciler", instance = %tx.instance_id.as_i64(), "state -> Failed, reconciling");
                            self.reconcile_once().await;
                        }
                        _ => {}
                    }
                }
                _ = self.shutdown.changed() => {
                    if *self.shutdown.borrow() {
                        info!(component = "reconciler", "shutting down");
                        break;
                    }
                }
            }
        }
    }

    /// 单轮完整收敛（幂等，可任意重复调用）：
    /// 1. 期望快照 ↔ registry 对齐（新增实例入 registry；孤儿记录清理）；
    /// 2. 逐实例决策并执行动作；
    /// 3. GOPST 配置同步一次。
    pub async fn reconcile_once(&self) {
        let desired = match self.repo.list().await {
            Ok(list) => list,
            Err(e) => {
                error!(component = "reconciler", error = %e, "failed to load desired state");
                return;
            }
        };

        // registry 对齐：DB 行必须存在于 registry；已删除的实例收敛掉。
        let desired_ids: std::collections::HashSet<i64> =
            desired.iter().map(|s| s.id.as_i64()).collect();
        for spec in &desired {
            if self.registry.get(spec.id).is_none() {
                self.registry.insert(spec.id);
            }
        }
        for (id, _entry) in self.registry.list() {
            if !desired_ids.contains(&id.as_i64()) {
                warn!(
                    component = "reconciler",
                    instance = %id.as_i64(),
                    "desired record removed; stopping runtime",
                );
                if let Err(e) = self.runtime.stop(id).await {
                    // P1 审查 R3#2：stop 失败**不得**移除 registry 条目——
                    // 条目是下一轮重试的唯一凭据；移除后该进程永久失控。
                    error!(
                        component = "reconciler",
                        instance = %id.as_i64(),
                        error = %e,
                        "orphan stop failed; keeping registry entry for retry"
                    );
                    continue;
                }
                self.registry.remove(id);
            }
        }

        // 逐实例收敛：单个失败不阻塞其余。
        for spec in &desired {
            if let Err(e) = self.reconcile_instance(spec).await {
                warn!(
                    component = "reconciler",
                    instance = %spec.id.as_i64(),
                    error = %e,
                    "instance reconcile failed (continuing)",
                );
            }
        }

        // GOST 配置同步（幂等；失败不致命，下一轮重试）。
        self.sync_proxy().await;
    }

    /// 单实例决策 + 执行（DESIGN §12.4 决策表）。
    async fn reconcile_instance(&self, spec: &WarpInstanceSpec) -> Result<(), String> {
        let actual = self.registry.get(spec.id).map(|r| r.state);

        // 期望不运行（禁用，或 desired=stopped）：确保实际保持停止。
        if !spec.should_run() {
            if let Some(state) = actual {
                if !matches!(
                    state,
                    RuntimeState::Stopped | RuntimeState::Stopping | RuntimeState::Disabled
                ) {
                    debug!(component = "reconciler", instance = %spec.id.as_i64(), from = ?state, "ensure stopped");
                    self.runtime
                        .stop(spec.id)
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
            return Ok(());
        }

        match actual {
            None | Some(RuntimeState::Stopped) => self.start_instance(spec).await,
            Some(RuntimeState::Failed) => {
                // P1 审查 R3#1：显式重启命令**优先于**自动重启策略——
                // auto_restart=false 时手动恢复不得永久悬空；
                // auto_restart=true 时手动恢复不被退避窗口阻塞。
                if spec.restart_command_generation > spec.observed_restart_generation {
                    info!(
                        component = "reconciler",
                        instance = %spec.id.as_i64(),
                        "explicit restart command overrides failed-state policy (auto_restart/backoff)"
                    );
                    return self.restart_instance(spec).await;
                }
                if spec.auto_restart {
                    if self.backoff_ready(spec).await {
                        self.restart_instance(spec).await
                    } else {
                        debug!(component = "reconciler", instance = %spec.id.as_i64(), "backoff window active; skipping restart");
                        Ok(())
                    }
                } else {
                    // auto_restart=false：保持 Failed，仅记录观察。
                    Ok(())
                }
            }
            Some(
                RuntimeState::Starting
                | RuntimeState::Registering
                | RuntimeState::Connecting
                | RuntimeState::Healthy
                | RuntimeState::Degraded,
            ) => {
                // v0.2 §16.9：档案（凭据/模式）变更 → 强制重启，不做退避
                // （新凭据即刻生效；应用失败上浮并保留标记，下轮重试）。
                if spec.restart_pending {
                    return self.restart_instance(spec).await;
                }
                // P1 审查 R2#1：显式重启命令（API 只写代数）——Reconciler 是
                // 唯一执行者；停机期间排队的多条命令合并为最新一条。
                if spec.restart_command_generation > spec.observed_restart_generation {
                    return self.restart_instance(spec).await;
                }
                Ok(()) // 流程中/健康：不动（幂等）
            }
            Some(RuntimeState::Stopping) => Ok(()), // 停止中：等待完成
            Some(RuntimeState::Disabled) => {
                // enabled=false 置入（期望已由 should_run=false 分支覆盖，除非竞态）
                debug!(component = "reconciler", instance = %spec.id.as_i64(), "instance disabled; ignoring");
                Ok(())
            }
        }
    }

    async fn start_instance(&self, spec: &WarpInstanceSpec) -> Result<(), String> {
        let ctx = InstanceContext::new(&self.data_dir, &self.runtime_base, spec.id)
            .map_err(|e| e.to_string())?;
        debug!(component = "reconciler", instance = %spec.id.as_i64(), "starting");
        match self.runtime.start(&ctx, spec.account_profile_id).await {
            Ok(()) => {
                let _ = self.repo.clear_backoff(spec.id).await;
                let _ = self.repo.clear_restart_pending(spec.id).await;
                // P1 审查 R2#1：启动成功即视为已处理到当前命令代数
                // （排队中的重启命令被「全新启动」满足）。
                let _ = self
                    .repo
                    .acknowledge_restart(spec.id, spec.restart_command_generation)
                    .await;
                info!(component = "reconciler", instance = %spec.id.as_i64(), "started and healthy");
                Ok(())
            }
            Err(e) => {
                self.record_failure(spec.id).await;
                Err(format!("start failed: {e}"))
            }
        }
    }

    async fn restart_instance(&self, spec: &WarpInstanceSpec) -> Result<(), String> {
        debug!(component = "reconciler", instance = %spec.id.as_i64(), "restarting after failure or explicit command");
        match self.runtime.restart(spec.id, spec.account_profile_id).await {
            Ok(()) => {
                let _ = self.repo.clear_backoff(spec.id).await;
                let _ = self.repo.clear_restart_pending(spec.id).await;
                // P1 审查 R2#1：追平命令代数（MAX 守卫防并发回退）。
                let _ = self
                    .repo
                    .acknowledge_restart(spec.id, spec.restart_command_generation)
                    .await;
                info!(component = "reconciler", instance = %spec.id.as_i64(), "restarted after failure or explicit command");
                Ok(())
            }
            Err(e) => {
                self.record_failure(spec.id).await;
                Err(format!("restart failed: {e}"))
            }
        }
    }

    /// backoff 是否到期（next_retry_at 不存在或已过则就绪）。
    async fn backoff_ready(&self, spec: &WarpInstanceSpec) -> bool {
        let Some(next) = spec.next_retry_at.as_deref() else {
            return true;
        };
        let Ok(next_t) = parse_rfc3339(next) else {
            return true; // 数据损坏按到期处理（不阻塞恢复）
        };
        let Ok(now_t) = parse_rfc3339(&self.clock.now_utc_rfc3339()) else {
            return true;
        };
        now_t >= next_t
    }

    /// 记录失败并指数退避：间隔 = 上次间隔 × 2（首次 = base），封顶 max。
    async fn record_failure(&self, id: InstanceId) {
        let now = self.clock.now();
        let now_iso = self.clock.now_utc_rfc3339();

        // 由存的 last/next 间距推导当前间隔（档位递推，无需额外计数器）。
        // P1 审查 R4：首次失败 = base（与文档一致）；之后 ×2 递推。
        let (had_prior, gap) = match self.repo.get(id).await {
            Ok(Some(spec)) => {
                let last = spec.last_failure_at.as_deref().and_then(parse_rfc3339_opt);
                let next = spec.next_retry_at.as_deref().and_then(parse_rfc3339_opt);
                match (last, next) {
                    (Some(l), Some(n)) if n > l => {
                        (true, (n - l).unsigned_abs().max(self.backoff_base))
                    }
                    _ => (false, self.backoff_base),
                }
            }
            _ => (false, self.backoff_base),
        };
        let next_gap = if had_prior {
            gap.saturating_mul(2).min(self.backoff_max)
        } else {
            self.backoff_base
        };
        let due = parse_rfc3339(&now_iso)
            .map(|t| t + next_gap)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        let next_iso = due
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        if let Err(e) = self.repo.record_backoff(id, &now_iso, Some(next_iso)).await {
            warn!(
                component = "reconciler",
                instance = %id.as_i64(),
                error = %e,
                "failed to persist backoff; retrying next cycle without backoff window",
            );
        }
        let _ = now;
    }

    /// 代理配置同步（幂等；GostManager 内部 diff-skip）。
    ///
    /// P8：auth_enabled 时从 secret store 取密码渲染 GOST。
    ///
    /// **fail-closed（P0 审查 #2 修订）**：`auth_enabled=true` 是用户声明的安全
    /// 姿态。密码缺失（Ok(None)，状态不一致）或读取/解密失败（Err，如 master
    /// key 损坏）时**绝不**降级为 `auth: None` 匿名代理——那等于密钥一坏、
    /// 公网端口裸奔且 UI 毫无感知。正确动作：跳过本次 apply，保留 GOST 当前
    /// 已验证配置（含认证）；首次应用前 GOST 处于 Stopped（无 listener），
    /// 天然不暴露。失败以 error 级日志呈现；实际状态上浮 API/UI 由 #3 承接。
    async fn sync_proxy(&self) {
        let cfg = match self.proxy_repo.get().await {
            Ok(cfg) => cfg,
            Err(e) => {
                error!(component = "reconciler", error = %e, "failed to load proxy config");
                return;
            }
        };
        let mut settings = proxy_config_to_gost(&cfg);

        // P1 审查 #4：期望 = 两个 listener 全关时，实际必须**显式停 GOST**
        // （GostConfig 拒绝全关渲染，旧进程会继续监听 → UI 显示已关但端口仍开）。
        if !settings.socks5_enabled && !settings.http_enabled {
            match self.proxy.stop().await {
                Ok(()) => {
                    debug!(
                        component = "reconciler",
                        "gost stopped (no listeners enabled)"
                    );
                    self.clear_apply_error();
                }
                Err(e) => {
                    let msg = format!("gost stop failed: {e}");
                    error!(component = "reconciler", error = %e, "gost stop failed");
                    self.record_apply_error(msg);
                }
            }
            return;
        }

        if cfg.auth_enabled {
            match self
                .secrets
                .get_plaintext(crate::crypto::secret_store::SecretKind::ProxyPassword)
                .await
            {
                Ok(Some(password)) => {
                    settings.auth = Some(crate::proxy::ProxyAuth {
                        username: cfg.proxy_username.unwrap_or_default(),
                        password,
                    });
                }
                Ok(None) => {
                    // fail-closed（P0 审查 #2）+ 失败上浮（P1 审查 #3/#4）：
                    // 保持上一份已验证配置，但必须在 API 可见，而非只有日志。
                    self.record_apply_error(
                        "proxy auth enabled but password missing; keeping last applied config (fail-closed)"
                            .to_string(),
                    );
                    error!(
                        component = "reconciler",
                        "proxy auth enabled but password missing; keeping last applied config (fail-closed, no anonymous downgrade)"
                    );
                    return;
                }
                Err(e) => {
                    self.record_apply_error(format!(
                        "failed to read proxy password; keeping last applied config (fail-closed): {e}"
                    ));
                    error!(
                        component = "reconciler",
                        error = %e,
                        "failed to read proxy password; keeping last applied config (fail-closed, no anonymous downgrade)"
                    );
                    return;
                }
            }
        }
        match self.proxy.apply_config(&settings).await {
            Ok(()) => {
                debug!(component = "reconciler", "proxy config applied");
                self.clear_apply_error();
            }
            Err(e) => {
                warn!(component = "reconciler", error = %e, "proxy config apply failed");
                self.record_apply_error(format!("gost apply failed: {e}"));
            }
        }
    }

    fn record_apply_error(&self, error: String) {
        let mut slot = self.apply_error.lock().unwrap();
        *slot = Some(ApplyError {
            error,
            at_rfc3339: self.clock.now_utc_rfc3339(),
        });
    }

    fn clear_apply_error(&self) {
        *self.apply_error.lock().unwrap() = None;
    }
}

/// RFC3339 解析（ISO8601；DB 存的是 `strftime('%Y-%m-%dT%H:%M:%fZ')`）。
fn parse_rfc3339(s: &str) -> Result<time::OffsetDateTime, time::error::Parse> {
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
}

fn parse_rfc3339_opt(s: &str) -> Option<time::OffsetDateTime> {
    parse_rfc3339(s).ok()
}

#[cfg(test)]
mod tests;
