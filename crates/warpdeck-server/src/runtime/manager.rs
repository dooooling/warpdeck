//! 多实例生命周期编排（P3 核心，P3-002/003/004/005/006/007/008/009）。
//!
//! 设计（DESIGN §11 / §21.2，计划 §8.2）：
//! - **P3-002 并发安全**：`runs` 全局互斥表 + 所有 start/stop/restart/delete 全程持锁，
//!   天然保证"同一实例串行"（同实例并发 start 只执行一次）。
//! - **P3-009 并发启动节流**：MVP 采用 sequential 方案（计划 §8.2 允许），全局串行
//!   即最多一个实例处于启动流程；stagger 由流程时长（注册/连接/验证）自然形成。
//! - **P3-005 端口冲突探测**：启动前 `PortProber` 探测内部端口空闲。
//! - **Crash Watcher 挂接**：实例启动成功后 spawn 独立 watcher 任务（独占进程句柄），
//!   结束后把 `WarpService` 所有权归还给 stop/restart/delete 使用。
//! - **P3-008 删除语义**：Stop = 停止 + 保留 registration/state；Delete = 停止 +
//!   移除 registry record（是否连带删除 reg.json 由显式参数决定）。
//! - 状态迁移：start 途中 Starting；成功 Healthy（P4 健康检查接入后仍为目标态）；
//!   失败/crash Failed；stop/delete Stopping → Stopped（DESIGN §10）。
//!
//! 本模块只编排**实际状态**（RuntimeRegistry）；期望状态来自 SQLite，由后续
//! Reconciler（P6）驱动（AGENTS.md：HTTP 处理器不得直接调用本模块）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::task::JoinHandle;

use super::backoff::{BackoffPolicy, ExponentialBackoff};
use super::clock::Clock;
use super::context::InstanceContext;
use super::control::WarpControl;
use super::crash::{CrashEvent, CrashWatcher};
use super::credentials::CredentialResolver;
use super::dbus::DbusRuntime;
use super::events::{EventBus, HealthEvent, StateTransition};
use super::flow::RegistrationFlow;
use super::health::{HealthVerdict, LayersReport};
use super::instance::InstanceId;
use super::probe::{DataPlaneProber, DataPlaneReport, ProbeProto};
use super::process::{ProcessSpawner, ProcessStatus};
use super::readiness::ReadinessProbe;
use super::registry::{InstanceRuntime, RuntimeRegistry, RuntimeState};
use super::service::WarpService;
use super::stop::{GracefulStop, StopOutcome};

/// crash watcher 轮询间隔。
const CRASH_POLL: Duration = Duration::from_millis(500);
/// 优雅停止 grace 预算（§11.7 第 4 步）。
const STOP_GRACE: Duration = Duration::from_secs(10);
/// 优雅停止轮询步长。
const STOP_POLL: Duration = Duration::from_millis(100);
/// 就绪探测上限（P2-008；warp-svc 注册到 D-Bus 有启动窗口）。
const READINESS_MAX_ATTEMPTS: u32 = 40;
/// 启动尾部数据面验证（P4 Gate "Healthy ≠ PID alive"）：增长等窗口给数据面
/// 建连时间（P3 实测首连在 connect 后可达 10s+），有界重试，超时 → Degraded
/// （健康循环后续拉回）。
const DATA_PLANE_VERIFY_ATTEMPTS: u32 = 12;
/// 启动尾部验证重试间隔。
const DATA_PLANE_VERIFY_BACKOFF: Duration = Duration::from_secs(5);

/// 管理器错误（P3 全部生命周期操作）。
#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error("instance {0} is already running")]
    AlreadyRunning(InstanceId),
    #[error("instance {0} is not running")]
    NotRunning(InstanceId),
    #[error("instance {0} internal port {1} is already in use")]
    PortInUse(InstanceId, u16),
    #[error("instance {0} id invalid: {1}")]
    InvalidId(InstanceId, super::instance::InstanceIdError),
    #[error("instance {0} start failed: {1}")]
    StartFailed(InstanceId, String),
    #[error("instance {0} stop failed: {1}")]
    StopFailed(InstanceId, String),
    #[error("instance {0} crash watcher task aborted: {1}")]
    WatcherFailed(InstanceId, String),
}

/// 内部端口冲突探测（P3-005）。测试注入恒空闲的 fake。
pub trait PortProber: Send + Sync {
    fn is_free(&self, port: u16) -> bool;
}

/// 真实探测：对回环地址临时 bind，成功即认为空闲。
#[derive(Debug, Default)]
pub struct TcpPortProber;

impl PortProber for TcpPortProber {
    fn is_free(&self, port: u16) -> bool {
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok()
    }
}

/// 对外生命周期接口（DESIGN §11.1）。
///
/// 与设计文档的差异（P3 阶段说明）：
/// - `start` 参数为 `InstanceContext`（DB 层的 `InstanceSpec` 属 P5）；
/// - `connect`/`disconnect` 已由启动流程（RegistrationFlow）与停止流程
///   （GracefulStop）内部覆盖，独立接口留给 Reconciler（P6）需要时补充；
/// - 追加 `delete`（P3-008 删除语义的必要入口）。
#[async_trait]
pub trait WarpRuntime: Send + Sync {
    /// 启动实例到 Healthy（失败返回错误并记录 Failed）。
    /// `account_profile_id`：v0.2 多账号，实例绑定档案（NULL = 默认 free 档）。
    async fn start(
        &self,
        ctx: &InstanceContext,
        account_profile_id: Option<i64>,
    ) -> Result<(), ManagerError>;
    /// 优雅停止（幂等：未运行也收敛为 Stopped）。
    async fn stop(&self, id: InstanceId) -> Result<StopOutcome, ManagerError>;
    /// 停止后立即重启（保留 registration/state）。
    async fn restart(
        &self,
        id: InstanceId,
        account_profile_id: Option<i64>,
    ) -> Result<(), ManagerError>;
    /// 查询一实例实际状态快照。
    async fn status(&self, id: InstanceId) -> Option<InstanceRuntime>;
    /// 删除：停止（如运行中）+ 移除 registry record；`remove_registration` 显式
    /// 决定是否连带删除注册数据（P3-008 危险操作）。
    async fn delete(&self, id: InstanceId, remove_registration: bool) -> Result<(), ManagerError>;
}

/// crash watcher 任务结束产物：崩溃事件（受控取消为 None）+ 归还的进程句柄。
type WatcherOutcome = (Option<CrashEvent>, WarpService);

/// 运行中实例的句柄（P3-001 RuntimeHandle 的运行时形态）。
///
/// 进程句柄（svc）不在此表中：start 完成后移交给 crash watcher 任务独占
/// （watcher 需要调用 `try_wait`），任务结束后随返回值归还，由 stop/restart/
/// delete 回收。`dbus` 留在表中供 GracefulStop 第 5 步使用。
struct RunningInstance {
    ctx: InstanceContext,
    dbus: DbusRuntime,
    /// drop = 取消 watcher（受控停止，避免把受控退出误报为崩溃）。
    watcher_cancel: tokio::sync::watch::Sender<()>,
    watcher_task: JoinHandle<WatcherOutcome>,
}

/// 实例管理器（DESIGN §11）：多实例生命周期编排的唯一入口。
pub struct InstanceManager {
    registry: Arc<RuntimeRegistry>,
    spawner: Arc<dyn ProcessSpawner>,
    control: Arc<dyn WarpControl>,
    clock: Arc<dyn Clock>,
    flow: RegistrationFlow,
    /// v0.2 多账号：按实例绑定的档案解析启动凭据（§11.2 注入点）。
    credentials: Arc<dyn CredentialResolver>,
    graceful_stop: GracefulStop,
    prober: Arc<dyn PortProber>,
    /// 数据面探测（P4-004；启动尾部验证 + 健康循环共用）。
    dplane_prober: Arc<dyn DataPlaneProber>,
    /// 事件总线（P4-008；状态迁移事件发布）。
    bus: EventBus,
    /// 数据目录（delete 构造未运行实例上下文用）。
    data_dir: PathBuf,
    /// runtime 目录基址（同上）。
    runtime_base: PathBuf,
    /// 运行中实例表。持有期间禁止其他生命周期操作交错：
    /// 同一把锁 = 同实例串行（P3-002）+ 全局串行启动（P3-009）。
    runs: tokio::sync::Mutex<HashMap<InstanceId, RunningInstance>>,
}

impl InstanceManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<RuntimeRegistry>,
        spawner: Arc<dyn ProcessSpawner>,
        control: Arc<dyn WarpControl>,
        clock: Arc<dyn Clock>,
        backoff: Box<dyn BackoffPolicy>,
        max_register_attempts: u32,
        credentials: Arc<dyn CredentialResolver>,
        data_dir: PathBuf,
        runtime_base: PathBuf,
        prober: Arc<dyn PortProber>,
        dplane_prober: Arc<dyn DataPlaneProber>,
        bus: EventBus,
    ) -> Self {
        let flow = RegistrationFlow::new(
            control.clone(),
            clock.clone(),
            backoff,
            max_register_attempts,
        );
        let graceful_stop =
            GracefulStop::new(control.clone(), clock.clone(), STOP_GRACE, STOP_POLL);
        Self {
            registry,
            spawner,
            control,
            clock,
            flow,
            graceful_stop,
            prober,
            dplane_prober,
            bus,
            credentials,
            data_dir,
            runtime_base,
            runs: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// 启动一个实例（调用方必须持有 `runs` 锁）。
    async fn do_start(
        &self,
        ctx: &InstanceContext,
        account_profile_id: Option<i64>,
    ) -> Result<RunningInstance, ManagerError> {
        self.registry.insert(ctx.id);
        // 迁移起点：restart 场景下可能是 Healthy/Degraded/Failed，不是 Stopped。
        let from = self
            .registry
            .get(ctx.id)
            .map(|e| e.state)
            .unwrap_or(RuntimeState::Stopped);

        // P3-005：启动前端口冲突探测。
        let port = ctx.internal_proxy_port.as_u16();
        if !self.prober.is_free(port) {
            let msg = format!("internal port {port} already in use");
            self.fail_start(ctx.id, from, msg);
            return Err(ManagerError::PortInUse(ctx.id, port));
        }
        self.registry.set_state(ctx.id, RuntimeState::Starting);
        self.publish_transition(ctx.id, from, RuntimeState::Starting, "start");

        // v0.2 多账号：启动凭据必须在任何子进程之前解析（§11.2 注入点）。
        // ZeroTrust 的 mdm.xml 也要在 warp-svc 启动前落位（service-token 自动
        // 注册，warp-svc 启动即读取）；非 ZeroTrust 则清理残留，防止换档后旧
        // 注册污染实例。此阶段失败尚无子进程，直接登记 Failed 返回。
        let credentials = match self.credentials.resolve(account_profile_id).await {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("credential resolution failed: {e}");
                self.fail_start(ctx.id, RuntimeState::Starting, msg.clone());
                return Err(ManagerError::StartFailed(ctx.id, msg));
            }
        };
        if let Err(e) = super::mdm::sync_mdm_xml(
            &ctx.paths.state_dir,
            &credentials,
            ctx.internal_proxy_port.as_u16(),
        )
        .await
        {
            let msg = format!("mdm.xml sync failed: {e}");
            self.fail_start(ctx.id, RuntimeState::Starting, msg.clone());
            return Err(ManagerError::StartFailed(ctx.id, msg));
        }

        // 先 D-Bus 后 warp-svc（warp-svc 依赖 DBUS_SYSTEM_BUS_ADDRESS socket）。
        let dbus = match DbusRuntime::start(self.spawner.as_ref(), ctx).await {
            Ok(dbus) => dbus,
            Err(e) => {
                let msg = format!("dbus-daemon start failed: {e}");
                self.fail_start(ctx.id, RuntimeState::Starting, msg.clone());
                return Err(ManagerError::StartFailed(ctx.id, msg));
            }
        };
        let mut svc = match WarpService::start(self.spawner.as_ref(), ctx).await {
            Ok(svc) => svc,
            Err(e) => {
                // warp-svc 失败：回收已启动的 dbus，避免孤儿进程（P3 Gate 要求）。
                let _ = dbus.shutdown().await;
                let msg = format!("warp-svc start failed: {e}");
                self.fail_start(ctx.id, RuntimeState::Starting, msg.clone());
                return Err(ManagerError::StartFailed(ctx.id, msg));
            }
        };

        // 就绪桥（§11.2 "poll status until ready" / P2-008）：warp-svc 注册到
        // D-Bus 有启动窗口，不能立刻发 warp-cli 配置命令（否则 ENOENT 连不上
        // daemon）；bounded retry + backoff。
        let probe = ReadinessProbe::new(
            self.control.clone(),
            self.clock.clone(),
            Box::new(ExponentialBackoff::recommended()),
            READINESS_MAX_ATTEMPTS,
        );
        let r = probe.probe(ctx).await;
        if !r.ready {
            // 进程已起但控制面未就绪：完整清理再返回（无 orphan）。
            let _ = self.graceful_stop.stop(ctx, &mut svc, dbus).await;
            let msg = format!(
                "control plane not ready after {} attempts: {:?}",
                r.attempts, r.last_error
            );
            self.fail_start(ctx.id, RuntimeState::Starting, msg.clone());
            return Err(ManagerError::StartFailed(ctx.id, msg));
        }

        // 注册（按需）→ 配置 → 连接 → 验证（P2-009）。凭据已在启动前解析
        // （ZeroTrust 的 mdm.xml 同步、WarpPlus/Free 无需额外取件）；流程失败
        // 走完整清理路径并上浮，绝不伪装成功。
        if let Err(e) = self.flow.run(ctx, &credentials).await {
            // 流程失败：warp-svc/dbus 均已运行，必须完整清理再返回（无 orphan）。
            let _ = self.graceful_stop.stop(ctx, &mut svc, dbus).await;
            let msg = format!("{e:?}");
            self.fail_start(ctx.id, RuntimeState::Starting, msg.clone());
            return Err(ManagerError::StartFailed(ctx.id, msg));
        }

        // 启动成功：移交 crash watcher 独占进程句柄。
        let warp_pid = svc.pid();
        let dbus_pid = dbus.pid();
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(());
        let watcher_task = spawn_crash_watcher(
            self.registry.clone(),
            self.clock.clone(),
            self.bus.clone(),
            svc,
            cancel_rx,
        );

        // P4 Gate "Healthy ≠ PID alive"：注册/连接成功只代表控制面就绪，
        // Healthy 必须经真实数据面探测（warp=on）验证（AGENTS.md）。
        self.registry.on_started(ctx.id, warp_pid, dbus_pid);
        self.publish_transition(
            ctx.id,
            RuntimeState::Starting,
            RuntimeState::Healthy,
            "start",
        );
        self.bus
            .publish(HealthEvent::HealthChanged(StateTransition {
                instance_id: ctx.id,
                from: RuntimeState::Starting,
                to: RuntimeState::Healthy,
                reason: "start".to_string(),
            }));
        match self.verify_data_plane(ctx).await {
            Ok(report) => {
                self.registry.update(ctx.id, |e| {
                    record_probe_metrics(e, &report);
                    e.last_error = None;
                });
                tracing::info!(
                    component = "manager",
                    instance_id = %ctx.id,
                    exit_ip_v4 = ?report.exit_ip_v4(),
                    exit_ip_v6 = ?report.exit_ip_v6(),
                    colo = ?report.colo(),
                    latency_ms = report.latency_ms,
                    "data plane verified at startup"
                );
            }
            Err(msg) => {
                // 进程/控制面均正常，只是数据面建连窗口未结束：降级而非启动失败
                // （无 orphan、不误报失败）；健康循环的下一轮探测会拉回 Healthy。
                // 竞态保护：verify 期间若 warp-svc 已崩溃（watcher 已置 Failed），
                // 不降级覆盖 Failed（health.rs 的 current==Healthy 例外同理）。
                let still_healthy = self
                    .registry
                    .get(ctx.id)
                    .map(|r| r.state == RuntimeState::Healthy)
                    .unwrap_or(false);
                if still_healthy {
                    self.registry.update(ctx.id, |e| {
                        e.state = RuntimeState::Degraded;
                        e.consecutive_failures = 1;
                        e.last_error = Some(msg.clone());
                    });
                    let reason = msg.clone();
                    self.publish_transition(
                        ctx.id,
                        RuntimeState::Healthy,
                        RuntimeState::Degraded,
                        reason.clone(),
                    );
                    self.bus
                        .publish(HealthEvent::HealthChanged(StateTransition {
                            instance_id: ctx.id,
                            from: RuntimeState::Healthy,
                            to: RuntimeState::Degraded,
                            reason,
                        }));
                }
                tracing::warn!(
                    component = "manager",
                    instance_id = %ctx.id,
                    error = %msg,
                    "started but data plane not verified yet"
                );
            }
        }
        tracing::info!(
            component = "manager",
            instance_id = %ctx.id,
            warp_pid,
            dbus_pid,
            "instance started"
        );
        Ok(RunningInstance {
            ctx: ctx.clone(),
            dbus,
            watcher_cancel: cancel_tx,
            watcher_task,
        })
    }

    /// 启动失败登记 + 状态/健康双事件发布（P4-008 完整性：启动失败必须对
    /// UI/告警可见，不能只写 registry）。`from` 是失败前的已知阶段状态。
    fn fail_start(&self, id: InstanceId, from: RuntimeState, msg: String) {
        self.registry.record_error(id, msg.clone());
        self.publish_transition(id, from, RuntimeState::Failed, msg.clone());
        self.bus
            .publish(HealthEvent::HealthChanged(StateTransition {
                instance_id: id,
                from,
                to: RuntimeState::Failed,
                reason: msg,
            }));
    }

    /// 启动尾部数据面验证（P4 Gate）：有界重试等待 `warp=on`。
    async fn verify_data_plane(&self, ctx: &InstanceContext) -> Result<DataPlaneReport, String> {
        let port = ctx.internal_proxy_port.as_u16();
        for attempt in 1..=DATA_PLANE_VERIFY_ATTEMPTS {
            match self.dplane_prober.probe(ProbeProto::Socks5, port).await {
                Ok(report) if report.warp_on() => return Ok(report),
                Ok(report) => {
                    tracing::debug!(
                        component = "manager",
                        instance_id = %ctx.id,
                        attempt,
                        warp_v4 = ?report.trace_v4.as_ref().and_then(|t| t.warp.as_deref()),
                        warp_v6 = ?report.trace_v6.as_ref().and_then(|t| t.warp.as_deref()),
                        "data plane not ready yet (warp != on)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        component = "manager",
                        instance_id = %ctx.id,
                        attempt,
                        error = %e,
                        "data plane probe failed during startup verify"
                    );
                }
            }
            if attempt < DATA_PLANE_VERIFY_ATTEMPTS {
                self.clock.sleep(DATA_PLANE_VERIFY_BACKOFF).await;
            }
        }
        Err(format!(
            "data plane not verified (warp=on) after {DATA_PLANE_VERIFY_ATTEMPTS} attempts"
        ))
    }

    /// 停止实例（调用方必须持有 `runs` 锁）。
    ///
    /// 崩溃场景：watcher 已结束（进程已死），await 立即可回收句柄；
    /// GracefulStop 对已死进程安全（TERM 忽略 → try_exited 直接命中 → 清理 dbus/runtime）。
    async fn do_stop(
        &self,
        ctx: &InstanceContext,
        running: RunningInstance,
    ) -> Result<StopOutcome, ManagerError> {
        // 迁移起点：崩溃场景下可能是 Failed，不是 Healthy。
        let from = self
            .registry
            .get(ctx.id)
            .map(|e| e.state)
            .unwrap_or(RuntimeState::Healthy);
        self.registry.set_state(ctx.id, RuntimeState::Stopping);
        self.publish_transition(ctx.id, from, RuntimeState::Stopping, "stop");

        drop(running.watcher_cancel);
        let (event, mut svc) = running
            .watcher_task
            .await
            .map_err(|e| ManagerError::WatcherFailed(ctx.id, e.to_string()))?;
        if event.is_some() {
            tracing::info!(
                component = "manager",
                instance_id = %ctx.id,
                "collecting already-crashed instance"
            );
        }

        let outcome = self
            .graceful_stop
            .stop(ctx, &mut svc, running.dbus)
            .await
            .map_err(|e| ManagerError::StopFailed(ctx.id, e.to_string()))?;

        self.registry.on_stopped(ctx.id);
        self.publish_transition(
            ctx.id,
            RuntimeState::Stopping,
            RuntimeState::Stopped,
            "stop",
        );
        tracing::info!(
            component = "manager",
            instance_id = %ctx.id,
            kill_required = outcome.kill_required,
            "instance stopped"
        );
        Ok(outcome)
    }

    /// 状态迁移事件（P4-008 `instance.state_changed`）。
    fn publish_transition(
        &self,
        id: InstanceId,
        from: RuntimeState,
        to: RuntimeState,
        reason: impl Into<String>,
    ) {
        self.bus.publish(HealthEvent::StateChanged(StateTransition {
            instance_id: id,
            from,
            to,
            reason: reason.into(),
        }));
    }

    /// 健康检查用：全部已知实例 id。
    pub fn all_ids(&self) -> Vec<InstanceId> {
        self.registry.ids()
    }

    /// 健康检查用：收集一实例的三层探测报告（P4-002/003/004）。
    ///
    /// 跳过非健康态实例（Starting/Stopping/Failed/Stopped——进程由 crash
    /// watcher 独立覆盖，健康循环只评估运行中的 Healthy/Degraded）。
    pub async fn collect_health_layers(
        &self,
        id: InstanceId,
    ) -> Option<(InstanceRuntime, LayersReport)> {
        let snapshot = self.registry.get(id)?;
        if !matches!(
            snapshot.state,
            RuntimeState::Healthy | RuntimeState::Degraded
        ) {
            return None;
        }
        let ctx = InstanceContext::new(&self.data_dir, &self.runtime_base, id).ok()?;
        let control_connected = match self.control.status(&ctx).await {
            Ok(status) => status.connected,
            Err(e) => {
                tracing::warn!(
                    component = "health_monitor",
                    instance_id = %id,
                    error = %e,
                    "warp-cli status failed"
                );
                false
            }
        };
        let data_plane = match self
            .dplane_prober
            .probe(ProbeProto::Socks5, ctx.internal_proxy_port.as_u16())
            .await
        {
            Ok(report) => Some(report),
            Err(e) => {
                tracing::warn!(
                    component = "health_monitor",
                    instance_id = %id,
                    error = %e,
                    "data plane probe failed"
                );
                None
            }
        };
        let report = LayersReport {
            process_alive: snapshot.warp_pid.is_some(),
            control_connected,
            data_plane,
        };
        Some((snapshot, report))
    }

    /// 应用健康判定结果（P4-006）：迁移状态、记录指标、发布事件。
    pub async fn apply_health_verdict(
        &self,
        id: InstanceId,
        verdict: HealthVerdict,
        counters: super::health::HealthCounters,
        report: &LayersReport,
    ) -> Option<()> {
        let before = self.registry.get(id)?;
        if !matches!(before.state, RuntimeState::Healthy | RuntimeState::Degraded) {
            return None;
        }
        let target = verdict.as_runtime_state();
        let new_exit_v4 = report.data_plane.as_ref().and_then(|d| d.exit_ip_v4());
        let new_exit_v6 = report.data_plane.as_ref().and_then(|d| d.exit_ip_v6());
        let exit_ip_changed = (new_exit_v4.is_some() && new_exit_v4 != before.exit_ip_v4)
            || (new_exit_v6.is_some() && new_exit_v6 != before.exit_ip_v6);

        // 窄竞态守卫：`before` 读取与 `update` 写之间，watcher/stop 可能已把实例
        // 置 Failed/Stopping——只有 update 闭包内再校验才原子；守卫失败则放弃
        // 本次判定（不迁移状态、不发事件），与 collect_health_layers 的宽窗口
        // 防护互为补充。
        let mut applied = false;
        self.registry.update(id, |e| {
            if !matches!(e.state, RuntimeState::Healthy | RuntimeState::Degraded) {
                return;
            }
            applied = true;
            e.state = target;
            e.consecutive_failures = counters.consecutive_failures;
            e.consecutive_successes = counters.consecutive_successes;
            if let Some(dplane) = report.data_plane.as_ref() {
                e.exit_ip_v4 = dplane.exit_ip_v4();
                e.exit_ip_v6 = dplane.exit_ip_v6();
                e.colo = dplane.colo();
                e.latency_ms = Some(dplane.latency_ms.min(u32::MAX as u64) as u32);
            }
            e.last_error = if !report.control_connected {
                Some("warp-cli status disconnected".to_string())
            } else if report.data_plane.is_none() {
                Some("data-plane probe failed".to_string())
            } else if report.data_plane.as_ref().is_some_and(|d| !d.warp_on()) {
                Some("warp is off".to_string())
            } else {
                None
            };
        });
        if !applied {
            return None;
        }

        let reason = if target == RuntimeState::Healthy {
            "recovered".to_string()
        } else if target == RuntimeState::Failed {
            format!(
                "consecutive failures reached {}",
                counters.consecutive_failures
            )
        } else {
            "health check degraded".to_string()
        };
        if before.state != target {
            self.publish_transition(id, before.state, target, reason.clone());
            self.bus
                .publish(HealthEvent::HealthChanged(StateTransition {
                    instance_id: id,
                    from: before.state,
                    to: target,
                    reason,
                }));
        }
        if exit_ip_changed {
            let dplane = report.data_plane.as_ref().expect("exit ip implies report");
            self.bus.publish(HealthEvent::ExitIpChanged {
                instance_id: id,
                exit_ip_v4: new_exit_v4.map(|ip| ip.to_string()),
                exit_ip_v6: new_exit_v6.map(|ip| ip.to_string()),
                colo: dplane.colo(),
                latency_ms: Some(dplane.latency_ms.min(u32::MAX as u64) as u32),
            });
        }
        Some(())
    }
}

/// 记录单次探测指标到 registry 快照（startup verify 复用）。
fn record_probe_metrics(entry: &mut InstanceRuntime, report: &DataPlaneReport) {
    entry.exit_ip_v4 = report.exit_ip_v4();
    entry.exit_ip_v6 = report.exit_ip_v6();
    entry.colo = report.colo();
    entry.latency_ms = Some(report.latency_ms.min(u32::MAX as u64) as u32);
}

/// crash watcher 任务：独占 svc 进程句柄监视崩溃；结束后归还 svc，崩溃时更新
/// registry（Failed）。进程句柄被回收前，manager 侧通过 `watcher_task` 关联。
fn spawn_crash_watcher(
    registry: Arc<RuntimeRegistry>,
    clock: Arc<dyn Clock>,
    bus: EventBus,
    svc: WarpService,
    cancel: tokio::sync::watch::Receiver<()>,
) -> JoinHandle<WatcherOutcome> {
    tokio::spawn(async move {
        let watcher = CrashWatcher::new(clock, CRASH_POLL);
        let (event, source) = watcher.watch(Box::new(svc), cancel).await;
        let svc = source
            .into_warp_service()
            .expect("manager crash watcher always wraps WarpService");
        if let Some(event) = &event {
            let from = registry
                .get(event.instance_id)
                .map(|r| r.state)
                .unwrap_or(RuntimeState::Healthy);
            registry.on_crash(event);
            let reason = format!(
                "warp-svc crashed: exit_code={}, stderr: {}",
                event
                    .exit_status
                    .exit_code
                    .map_or("?".to_string(), |c| c.to_string()),
                event.stderr_summary.trim()
            );
            bus.publish(HealthEvent::StateChanged(StateTransition {
                instance_id: event.instance_id,
                from,
                to: RuntimeState::Failed,
                reason: reason.clone(),
            }));
            bus.publish(HealthEvent::HealthChanged(StateTransition {
                instance_id: event.instance_id,
                from,
                to: RuntimeState::Failed,
                reason,
            }));
        }
        (event, svc)
    })
}

#[async_trait]
impl WarpRuntime for InstanceManager {
    async fn start(
        &self,
        ctx: &InstanceContext,
        account_profile_id: Option<i64>,
    ) -> Result<(), ManagerError> {
        let mut runs = self.runs.lock().await;
        if runs.contains_key(&ctx.id) {
            return Err(ManagerError::AlreadyRunning(ctx.id));
        }
        let running = self.do_start(ctx, account_profile_id).await?;
        runs.insert(ctx.id, running);
        Ok(())
    }

    async fn stop(&self, id: InstanceId) -> Result<StopOutcome, ManagerError> {
        let mut runs = self.runs.lock().await;
        let Some(running) = runs.remove(&id) else {
            // 幂等：未运行也收敛为 Stopped（多次 stop 安全，可清扫 Failed 残留）。
            tracing::info!(component = "manager", instance_id = %id, "stop on non-running instance (idempotent)");
            let from = self.registry.get(id).map(|e| e.state);
            self.registry.set_state(id, RuntimeState::Stopped);
            if let Some(from) = from.filter(|s| *s != RuntimeState::Stopped) {
                self.publish_transition(id, from, RuntimeState::Stopped, "stop (idempotent)");
            }
            return Ok(StopOutcome {
                kill_required: false,
                exit_status: ProcessStatus { exit_code: None },
            });
        };
        let ctx = running.ctx.clone();
        self.do_stop(&ctx, running).await
    }

    async fn restart(
        &self,
        id: InstanceId,
        account_profile_id: Option<i64>,
    ) -> Result<(), ManagerError> {
        let mut runs = self.runs.lock().await;
        let ctx = match runs.remove(&id) {
            Some(running) => {
                let ctx = running.ctx.clone();
                self.do_stop(&ctx, running).await?;
                ctx
            }
            // 已停止（如 Gate 步骤 stop → restart）但保留 record：直接重新启动。
            None => {
                if self.registry.get(id).is_none() {
                    return Err(ManagerError::NotRunning(id));
                }
                InstanceContext::new(&self.data_dir, &self.runtime_base, id)
                    .map_err(|e| ManagerError::InvalidId(id, e))?
            }
        };
        // 停止完成 → 立即重新启动；全程持锁，无并发交错（P3-002）。
        let running = self.do_start(&ctx, account_profile_id).await?;
        runs.insert(id, running);
        Ok(())
    }

    async fn status(&self, id: InstanceId) -> Option<InstanceRuntime> {
        self.registry.get(id)
    }

    async fn delete(&self, id: InstanceId, remove_registration: bool) -> Result<(), ManagerError> {
        let mut runs = self.runs.lock().await;
        let ctx = match runs.remove(&id) {
            Some(running) => {
                let ctx = running.ctx.clone();
                self.do_stop(&ctx, running).await?;
                ctx
            }
            // 从未运行 / 已停止：直接构造上下文处理注册数据。
            None => InstanceContext::new(&self.data_dir, &self.runtime_base, id)
                .map_err(|e| ManagerError::InvalidId(id, e))?,
        };

        // P3-008：删除 manager record。
        self.registry.remove(id);
        tracing::info!(
            component = "manager",
            instance_id = %id,
            remove_registration,
            "instance record deleted"
        );

        // 显式危险的连带删除（reset registration 的前置操作）。
        if remove_registration {
            let reg_file = ctx.paths.state_dir.join(super::flow::REGISTRATION_FILE);
            _ = tokio::fs::remove_file(&reg_file).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::runtime::backoff::ExponentialBackoff;
    use crate::runtime::credentials::InstanceCredentials;
    use crate::runtime::events::EventBus;
    use crate::runtime::fake::{
        FakeCredentialResolver, FakeDataPlaneProber, FakeProcessSpawner, FakeWarpControl,
        ManualClock,
    };

    struct Harness {
        manager: Arc<InstanceManager>,
        registry: Arc<RuntimeRegistry>,
        spawner: Arc<FakeProcessSpawner>,
        control: Arc<FakeWarpControl>,
        credentials: Arc<FakeCredentialResolver>,
        bus: EventBus,
        data_dir: PathBuf,
        runtime_base: PathBuf,
        _keep: Vec<tempfile::TempDir>,
    }

    impl Harness {
        fn new(prober: Arc<dyn PortProber>) -> Self {
            Self::with(
                prober,
                Arc::new(FakeDataPlaneProber::new()),
                EventBus::new(16),
            )
        }

        fn with(
            prober: Arc<dyn PortProber>,
            dplane: Arc<dyn crate::runtime::probe::DataPlaneProber>,
            bus: EventBus,
        ) -> Self {
            let registry = Arc::new(RuntimeRegistry::new());
            let spawner = Arc::new(FakeProcessSpawner::new());
            let control = Arc::new(FakeWarpControl::new());
            control.set_registered(true);
            control.set_connected(true);
            let clock = Arc::new(ManualClock::new());
            let data = tempfile::TempDir::new().unwrap();
            let runtime = tempfile::TempDir::new().unwrap();
            let credentials = Arc::new(FakeCredentialResolver::default());
            let manager = Arc::new(InstanceManager::new(
                registry.clone(),
                spawner.clone(),
                control.clone(),
                clock.clone(),
                Box::new(ExponentialBackoff::new(
                    Duration::from_millis(10),
                    2,
                    Duration::from_millis(100),
                )),
                5,
                credentials.clone(),
                data.path().to_path_buf(),
                runtime.path().to_path_buf(),
                prober,
                dplane,
                bus.clone(),
            ));
            Self {
                manager,
                registry,
                spawner,
                control,
                credentials,
                bus,
                data_dir: data.path().to_path_buf(),
                runtime_base: runtime.path().to_path_buf(),
                _keep: vec![data, runtime],
            }
        }

        fn ctx(&self, id: i64) -> InstanceContext {
            InstanceContext::new(
                &self.data_dir,
                &self.runtime_base,
                InstanceId::from_db(id).unwrap(),
            )
            .unwrap()
        }

        /// 预置 reg.json（模拟已注册实例，跳过 registration new）。
        fn seed_registration(&self, id: InstanceId) {
            let ctx = self.ctx(id.as_i64());
            std::fs::create_dir_all(&ctx.paths.state_dir).unwrap();
            std::fs::write(
                ctx.paths
                    .state_dir
                    .join(crate::runtime::flow::REGISTRATION_FILE),
                b"{}",
            )
            .unwrap();
        }

        async fn wait_until<F: Fn() -> bool>(&self, cond: F) {
            for _ in 0..200 {
                if cond() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("condition not met within timeout");
        }
    }

    /// 恒空闲 prober（manager 功能测试默认）。
    fn always_free() -> Arc<dyn PortProber> {
        #[derive(Debug)]
        struct AlwaysFree;
        impl PortProber for AlwaysFree {
            fn is_free(&self, _port: u16) -> bool {
                true
            }
        }
        Arc::new(AlwaysFree)
    }

    #[tokio::test]
    async fn start_reaches_healthy_with_pids_and_order() {
        let h = Harness::new(always_free());
        let ctx = h.ctx(0);

        h.manager.start(&ctx, None).await.unwrap();

        let e = h.registry.get(ctx.id).unwrap();
        assert_eq!(e.state, RuntimeState::Healthy);
        assert_eq!(e.warp_pid, Some(2), "dbus pid=1, warp pid=2");
        assert_eq!(e.dbus_pid, Some(1));
        assert_eq!(e.restart_count, 1);

        let calls = h.spawner.spawn_calls();
        assert_eq!(
            calls.iter().map(|c| c.program.as_str()).collect::<Vec<_>>(),
            vec!["dbus-daemon", "warp-svc"]
        );
    }

    #[tokio::test]
    async fn start_twice_second_is_rejected_and_no_duplicate_children() {
        let h = Harness::new(always_free());
        let ctx = h.ctx(0);
        h.manager.start(&ctx, None).await.unwrap();

        let err = h.manager.start(&ctx, None).await.unwrap_err();
        assert!(matches!(err, ManagerError::AlreadyRunning(id) if id == ctx.id));
        assert_eq!(h.spawner.spawn_calls().len(), 2, "绝不重复启动 child");
    }

    #[tokio::test]
    async fn concurrent_starts_of_same_instance_serialize() {
        let h = Harness::new(always_free());
        let ctx = h.ctx(0);

        let a = tokio::spawn({
            let h = h.manager.clone();
            let ctx = ctx.clone();
            async move { h.start(&ctx, None).await }
        });
        // 错开排队顺序，确保两个 start 同时在锁上竞争。
        tokio::task::yield_now().await;
        let b = tokio::spawn({
            let h = h.manager.clone();
            let ctx = ctx.clone();
            async move { h.start(&ctx, None).await }
        });

        let (ra, rb) = (a.await.unwrap(), b.await.unwrap());
        let oks = [ra.is_ok(), rb.is_ok()].iter().filter(|v| **v).count();
        assert_eq!(oks, 1, "同实例并发 start 只允许一次成功");
        assert_eq!(h.spawner.spawn_calls().len(), 2);
    }

    #[tokio::test]
    async fn stop_one_does_not_affect_others() {
        let h = Harness::new(always_free());
        for id in 0..3 {
            h.seed_registration(InstanceId::from_db(id).unwrap());
            h.manager.start(&h.ctx(id), None).await.unwrap();
        }
        let warp_0 = h
            .registry
            .get(InstanceId::from_db(0).unwrap())
            .unwrap()
            .warp_pid
            .unwrap();
        let warp_2 = h
            .registry
            .get(InstanceId::from_db(2).unwrap())
            .unwrap()
            .warp_pid
            .unwrap();

        let outcome = h
            .manager
            .stop(InstanceId::from_db(1).unwrap())
            .await
            .unwrap();
        assert!(outcome.kill_required, "未注入优雅退出 → 强杀路径");

        let e1 = h.registry.get(InstanceId::from_db(1).unwrap()).unwrap();
        assert_eq!(e1.state, RuntimeState::Stopped);
        assert!(e1.warp_pid.is_none());

        // #0/#2 不受影响。
        assert_eq!(
            h.registry
                .get(InstanceId::from_db(0).unwrap())
                .unwrap()
                .state,
            RuntimeState::Healthy
        );
        assert_eq!(
            h.registry
                .get(InstanceId::from_db(2).unwrap())
                .unwrap()
                .state,
            RuntimeState::Healthy
        );
        assert!(!h.spawner.was_killed(warp_0));
        assert!(!h.spawner.was_killed(warp_2));

        // #1 的 dbus/warp 都被回收（无 orphan）。
        let dbus_1 = 3; // 顺序: 0:dbus1,warp2 | 1:dbus3,warp4 | 2:dbus5,warp6
        let warp_1 = 4;
        assert!(h.spawner.was_killed(dbus_1));
        assert!(h.spawner.was_killed(warp_1));
    }

    #[tokio::test]
    async fn restart_makes_fresh_processes_and_increments_count() {
        let h = Harness::new(always_free());
        h.seed_registration(InstanceId::from_db(0).unwrap());
        h.manager.start(&h.ctx(0), None).await.unwrap();

        h.manager
            .restart(InstanceId::from_db(0).unwrap(), None)
            .await
            .unwrap();

        let e = h.registry.get(InstanceId::from_db(0).unwrap()).unwrap();
        assert_eq!(e.state, RuntimeState::Healthy);
        assert_eq!(e.restart_count, 2);
        assert_eq!(
            e.warp_pid,
            Some(4),
            "第一次: dbus1/warp2; 第二次: dbus3/warp4"
        );
        assert_eq!(h.spawner.spawn_calls().len(), 4);
    }

    #[tokio::test]
    async fn restart_unknown_instance_errors() {
        let h = Harness::new(always_free());
        let err = h
            .manager
            .restart(InstanceId::from_db(9).unwrap(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, ManagerError::NotRunning(id) if id.as_i64() == 9));
    }

    #[tokio::test]
    async fn restart_after_stop_works_for_gate_sequence() {
        // Gate §8.4 步骤 2→3：stop #1 后 restart #1（实例已不在 runs，恢复 record 重启）。
        let h = Harness::new(always_free());
        h.seed_registration(InstanceId::from_db(1).unwrap());
        h.manager.start(&h.ctx(1), None).await.unwrap();
        h.manager
            .stop(InstanceId::from_db(1).unwrap())
            .await
            .unwrap();
        assert_eq!(
            h.registry
                .get(InstanceId::from_db(1).unwrap())
                .unwrap()
                .state,
            RuntimeState::Stopped
        );

        h.manager
            .restart(InstanceId::from_db(1).unwrap(), None)
            .await
            .unwrap();
        let e = h.registry.get(InstanceId::from_db(1).unwrap()).unwrap();
        assert_eq!(e.state, RuntimeState::Healthy);
        assert_eq!(e.restart_count, 2);

        // 完全未知的实例仍拒绝。
        let err = h
            .manager
            .restart(InstanceId::from_db(42).unwrap(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, ManagerError::NotRunning(_)));
    }

    #[tokio::test]
    async fn crash_marks_failed_and_stop_collects_without_orphan() {
        let h = Harness::new(always_free());
        h.seed_registration(InstanceId::from_db(0).unwrap());
        h.manager.start(&h.ctx(0), None).await.unwrap();
        let warp_pid = h
            .registry
            .get(InstanceId::from_db(0).unwrap())
            .unwrap()
            .warp_pid
            .unwrap();

        h.spawner.crash_process(warp_pid);
        h.wait_until(|| {
            h.registry
                .get(InstanceId::from_db(0).unwrap())
                .is_some_and(|e| e.state == RuntimeState::Failed)
        })
        .await;
        let e = h.registry.get(InstanceId::from_db(0).unwrap()).unwrap();
        assert_eq!(e.consecutive_failures, 1);
        assert!(e.warp_pid.is_none());
        assert!(e.last_error.as_deref().unwrap_or("").contains("crashed"));

        // 崩溃后再 stop：回收已死进程 + 停止 dbus，无 orphan。
        let outcome = h
            .manager
            .stop(InstanceId::from_db(0).unwrap())
            .await
            .unwrap();
        assert!(!outcome.kill_required, "已死进程无需强杀");
        assert_eq!(outcome.exit_status.exit_code, Some(1));
        assert_eq!(
            h.registry
                .get(InstanceId::from_db(0).unwrap())
                .unwrap()
                .state,
            RuntimeState::Stopped
        );
        assert!(h.spawner.was_killed(1), "dbus 必须被回收");
    }

    #[tokio::test]
    async fn delete_keeps_registration_by_default() {
        let h = Harness::new(always_free());
        let id = InstanceId::from_db(2).unwrap();
        h.seed_registration(id);
        h.manager.start(&h.ctx(2), None).await.unwrap();

        h.manager.delete(id, false).await.unwrap();

        assert!(h.registry.get(id).is_none(), "manager record 已删除");
        let ctx = h.ctx(2);
        assert!(
            ctx.paths
                .state_dir
                .join(crate::runtime::flow::REGISTRATION_FILE)
                .exists(),
            "默认保留 registration 数据"
        );
        // 进程全部回收（本测试只有实例 2：dbus pid 1, warp pid 2）。
        assert!(h.spawner.was_killed(1) && h.spawner.was_killed(2));
    }

    #[tokio::test]
    async fn delete_with_remove_registration_is_explicit() {
        let h = Harness::new(always_free());
        let id = InstanceId::from_db(2).unwrap();
        h.seed_registration(id);
        h.manager.start(&h.ctx(2), None).await.unwrap();

        h.manager.delete(id, true).await.unwrap();

        let ctx = h.ctx(2);
        assert!(
            !ctx.paths
                .state_dir
                .join(crate::runtime::flow::REGISTRATION_FILE)
                .exists(),
            "显式参数 enabled 时才删除注册数据"
        );
    }

    #[tokio::test]
    async fn delete_never_started_is_idempotent() {
        let h = Harness::new(always_free());
        let id = InstanceId::from_db(7).unwrap();
        h.manager.delete(id, false).await.unwrap();
        h.manager.delete(id, true).await.unwrap();
    }

    #[tokio::test]
    async fn readiness_bridge_retries_transient_failures() {
        let h = Harness::new(always_free());
        h.seed_registration(InstanceId::from_db(0).unwrap());
        // 首次 status 失败（warp-svc 就绪窗口），probe backoff 重试后成功。
        h.control
            .fail_next(crate::runtime::control::WarpControlError::CommandTimeout);

        h.manager.start(&h.ctx(0), None).await.unwrap();
        assert_eq!(
            h.registry
                .get(InstanceId::from_db(0).unwrap())
                .unwrap()
                .state,
            RuntimeState::Healthy
        );
    }

    #[tokio::test]
    async fn start_failure_cleans_up_all_children_then_retry_works() {
        let h = Harness::new(always_free());
        h.control
            .fail_connect(crate::runtime::control::WarpControlError::CommandTimeout);

        let err = h.manager.start(&h.ctx(0), None).await.unwrap_err();
        assert!(matches!(err, ManagerError::StartFailed(id, _) if id.as_i64() == 0));

        let e = h.registry.get(InstanceId::from_db(0).unwrap()).unwrap();
        assert_eq!(e.state, RuntimeState::Failed);
        assert!(e
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("ConnectFailed"));

        // 无 orphan：dbus 与 warp-svc 全部被回收。
        assert!(h.spawner.was_killed(1));
        assert!(h.spawner.was_killed(2));

        // 失败后可重试成功。
        h.manager.start(&h.ctx(0), None).await.unwrap();
        assert_eq!(
            h.registry
                .get(InstanceId::from_db(0).unwrap())
                .unwrap()
                .state,
            RuntimeState::Healthy
        );
    }

    #[tokio::test]
    async fn port_conflict_blocks_start_without_children() {
        #[derive(Debug)]
        struct Forbid(u16);
        impl PortProber for Forbid {
            fn is_free(&self, port: u16) -> bool {
                self.0 != port
            }
        }
        let prober: Arc<dyn PortProber> = Arc::new(Forbid(40000));
        let h = Harness::new(prober);

        let err = h.manager.start(&h.ctx(0), None).await.unwrap_err();
        assert!(matches!(err, ManagerError::PortInUse(id, 40000) if id.as_i64() == 0));
        assert!(h.spawner.spawn_calls().is_empty(), "冲突时不启动任何进程");
        let e = h.registry.get(InstanceId::from_db(0).unwrap()).unwrap();
        assert_eq!(e.state, RuntimeState::Failed);
        assert!(e.last_error.as_deref().unwrap_or("").contains("40000"));
    }

    #[tokio::test]
    async fn stop_non_running_is_idempotent() {
        let h = Harness::new(always_free());
        let outcome = h
            .manager
            .stop(InstanceId::from_db(0).unwrap())
            .await
            .unwrap();
        assert!(!outcome.kill_required);
        assert!(outcome.exit_status.exit_code.is_none());
    }

    #[tokio::test]
    async fn three_instances_are_fully_isolated_in_dirs_and_ports() {
        let h = Harness::new(always_free());
        for id in 0..3 {
            h.seed_registration(InstanceId::from_db(id).unwrap());
            h.manager.start(&h.ctx(id), None).await.unwrap();
        }

        let calls = h.spawner.spawn_calls();
        // 每个实例: dbus-daemon(带独立 socket 地址) + warp-svc(带独立 env)。
        let warp_envs: Vec<_> = calls
            .iter()
            .filter(|c| c.program == "warp-svc")
            .map(|c| c.envs.clone())
            .collect();
        assert_eq!(warp_envs.len(), 3);
        for (i, envs) in warp_envs.iter().enumerate() {
            let get = |k: &str| {
                envs.iter()
                    .find(|(k2, _)| k2 == k)
                    .map(|(_, v)| v.clone())
                    .unwrap()
            };
            assert_eq!(
                get("STATE_DIRECTORY"),
                h.data_dir
                    .join("instances")
                    .join(i.to_string())
                    .join("state")
                    .display()
                    .to_string()
            );
            assert_eq!(
                get("RUNTIME_DIRECTORY"),
                h.runtime_base
                    .join("instances")
                    .join(i.to_string())
                    .join("warp")
                    .display()
                    .to_string()
            );
        }
        // 三个 STATE_DIRECTORY / dbus socket 互不相同（互不读取、互不串接）。
        let states: std::collections::HashSet<_> = warp_envs
            .iter()
            .map(|e| {
                e.iter()
                    .find(|(k, _)| k == "STATE_DIRECTORY")
                    .unwrap()
                    .1
                    .clone()
            })
            .collect();
        assert_eq!(states.len(), 3, "state 目录必须隔离");
        let sockets: std::collections::HashSet<_> = calls
            .iter()
            .filter(|c| c.program == "dbus-daemon")
            .map(|c| {
                c.args
                    .iter()
                    .find(|a| a.starts_with("--address=unix:path="))
                    .unwrap()
                    .clone()
            })
            .collect();
        assert_eq!(sockets.len(), 3, "每实例 D-Bus socket 独立");

        // 端口：0→40000, 1→40001, 2→40002（pids 顺序 1..6）。
        assert_eq!(
            h.registry
                .get(InstanceId::from_db(0).unwrap())
                .unwrap()
                .warp_pid,
            Some(2)
        );
        assert_eq!(
            h.registry
                .get(InstanceId::from_db(1).unwrap())
                .unwrap()
                .warp_pid,
            Some(4)
        );
        assert_eq!(
            h.registry
                .get(InstanceId::from_db(2).unwrap())
                .unwrap()
                .warp_pid,
            Some(6)
        );
    }

    #[tokio::test]
    async fn status_reflects_live_snapshot() {
        let h = Harness::new(always_free());
        let id = InstanceId::from_db(0).unwrap();
        assert!(h.manager.status(id).await.is_none());

        h.manager.start(&h.ctx(0), None).await.unwrap();
        let e = h.manager.status(id).await.unwrap();
        assert_eq!(e.state, RuntimeState::Healthy);
        assert_eq!(e.restart_count, 1);
    }

    /// P4-008 完整性：启动失败必须发布 StateChanged + HealthChanged → Failed 事件，
    /// 不能只写 registry（UI/告警依赖事件流）。
    #[tokio::test]
    async fn start_failure_publishes_state_and_health_events() {
        let h = Harness::new(always_free());
        let mut rx = h.bus.subscribe();
        h.control
            .fail_connect(crate::runtime::control::WarpControlError::CommandTimeout);

        let err = h.manager.start(&h.ctx(0), None).await.unwrap_err();
        assert!(matches!(err, ManagerError::StartFailed(..)));

        // 事件序列：Stopped→Starting（开始启动）→ Starting→Failed ×2（双通道）。
        let ev = rx.recv().await.expect("starting transition published");
        match ev {
            HealthEvent::StateChanged(t) => {
                assert_eq!(t.instance_id.as_i64(), 0);
                assert_eq!(t.from, RuntimeState::Stopped);
                assert_eq!(t.to, RuntimeState::Starting);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let mut seen_state = false;
        let mut seen_health = false;
        for _ in 0..2 {
            let ev = rx.recv().await.expect("failure events published");
            match ev {
                HealthEvent::StateChanged(t) => {
                    assert_eq!(t.instance_id.as_i64(), 0);
                    assert_eq!(t.from, RuntimeState::Starting);
                    assert_eq!(t.to, RuntimeState::Failed);
                    assert!(t.reason.contains("ConnectFailed"));
                    seen_state = true;
                }
                HealthEvent::HealthChanged(t) => {
                    assert_eq!(t.instance_id.as_i64(), 0);
                    assert_eq!(t.from, RuntimeState::Starting);
                    assert_eq!(t.to, RuntimeState::Failed);
                    assert!(t.reason.contains("ConnectFailed"));
                    seen_health = true;
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(seen_state && seen_health, "both event channels required");
    }

    /// 竞态保护：数据面 verify 期间 warp-svc 崩溃（watcher 已置 Failed）时，
    /// verify 失败不得把 Failed 覆盖为 Degraded。
    #[tokio::test]
    async fn verify_failure_does_not_override_crash_failed() {
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[derive(Debug)]
        struct HangingProber {
            release: Arc<std::sync::atomic::AtomicBool>,
        }
        #[async_trait::async_trait]
        impl crate::runtime::probe::DataPlaneProber for HangingProber {
            async fn probe(
                &self,
                _proto: crate::runtime::probe::ProbeProto,
                _port: u16,
            ) -> Result<crate::runtime::probe::DataPlaneReport, crate::runtime::probe::ProbeError>
            {
                // 轮询等待释放（避免 Notify 丢失唤醒）：放行前挂住 verify。
                loop {
                    if self.release.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                Err(crate::runtime::probe::ProbeError::Timeout(
                    std::time::Duration::from_secs(10),
                ))
            }
        }
        let h = Harness::with(
            always_free(),
            Arc::new(HangingProber {
                release: release.clone(),
            }),
            EventBus::new(32),
        );
        let id = InstanceId::from_db(0).unwrap();
        h.seed_registration(id);
        let ctx0 = h.ctx(0);
        let start = {
            let manager = h.manager.clone();
            tokio::spawn(async move { manager.start(&ctx0, None).await })
        };

        // 等 warp-svc 已 spawn（dbus=pid1, warp-svc=pid2），模拟崩溃。
        h.wait_until(|| h.spawner.spawn_calls().len() >= 2).await;
        h.spawner.crash_process(2);

        // watcher 把实例置 Failed。
        h.wait_until(|| {
            h.registry
                .get(id)
                .map(|e| e.state == RuntimeState::Failed)
                .unwrap_or(false)
        })
        .await;

        // 放行 verify：全部失败，但不得覆盖 Failed 为 Degraded。
        release.store(true, std::sync::atomic::Ordering::Relaxed);
        start.await.unwrap().unwrap();

        let e = h.registry.get(id).unwrap();
        assert_eq!(e.state, RuntimeState::Failed, "crash 状态不被 verify 覆盖");
        assert_eq!(e.warp_pid, None, "崩溃后 pid 已被 watcher 清理");
    }

    /// v0.2 ZeroTrust：mdm.xml 必须在 warp-svc spawn 之前写入实例 state 目录
    /// （service-token 注册只在 warp-svc 启动时读取一次）。
    #[tokio::test]
    async fn zero_trust_start_writes_mdm_xml_before_warp_svc_spawn() {
        use crate::runtime::credentials::CredentialMode;

        let h = Harness::new(always_free());
        h.credentials.set(InstanceCredentials {
            mode: CredentialMode::ZeroTrust,
            zero_trust_org: Some("acme-corp".into()),
            zt_client_id: Some("token-1.access".into()),
            zt_client_secret: Some("secret-1".into()),
            ..InstanceCredentials::free()
        });

        h.manager.start(&h.ctx(0), Some(7)).await.unwrap();

        let mdm = h.ctx(0).paths.state_dir.join(crate::runtime::mdm::MDM_FILE);
        let content = std::fs::read_to_string(&mdm).unwrap();
        assert!(content.contains("<string>acme-corp</string>"));
        assert!(content.contains("<string>token-1.access</string>"));
        assert!(content.contains("<string>secret-1</string>"));
        assert!(
            content.contains("<string>proxy</string>"),
            "mode 必须由 mdm.xml 下发（managed 账号禁 CLI）"
        );
        assert!(
            content.contains("<integer>40000</integer>"),
            "proxy_port 必须由 mdm.xml 下发 = 内部端口"
        );

        let calls = h.spawner.spawn_calls();
        assert_eq!(calls[0].program, "dbus-daemon");
        assert_eq!(calls[1].program, "warp-svc");
    }

    /// 换档保护：实例从 ZeroTrust 切回 free 再启动时，残留 mdm.xml 必须被清除，
    /// 否则旧文件仍会驱动 ZT 注册（实例状态污染）。
    #[tokio::test]
    async fn free_start_removes_stale_mdm_xml() {
        let h = Harness::new(always_free());
        let ctx = h.ctx(0);
        std::fs::create_dir_all(&ctx.paths.state_dir).unwrap();
        std::fs::write(
            ctx.paths.state_dir.join(crate::runtime::mdm::MDM_FILE),
            "<dict/>",
        )
        .unwrap();

        h.manager.start(&ctx, None).await.unwrap();

        assert!(
            !ctx.paths
                .state_dir
                .join(crate::runtime::mdm::MDM_FILE)
                .exists(),
            "非 ZeroTrust 启动必须清除残留 mdm.xml"
        );
    }

    /// 凭据解析失败 → 无任何子进程启动（纯 Failed 路径，无 cleanup 需求）。
    #[tokio::test]
    async fn credential_resolution_failure_starts_no_children() {
        use crate::runtime::credentials::CredentialError;

        let h = Harness::new(always_free());
        struct FailingResolver;
        #[async_trait::async_trait]
        impl super::super::credentials::CredentialResolver for FailingResolver {
            async fn resolve(
                &self,
                _profile_id: Option<i64>,
            ) -> Result<crate::runtime::credentials::InstanceCredentials, CredentialError>
            {
                Err(CredentialError::Resolution("boom".into()))
            }
        }
        let registry = h.registry.clone();
        let spawner = h.spawner.clone();
        let control = h.control.clone();
        let clock = Arc::new(ManualClock::new());
        let manager = Arc::new(InstanceManager::new(
            registry,
            spawner.clone(),
            control,
            clock,
            Box::new(ExponentialBackoff::new(
                Duration::from_millis(10),
                2,
                Duration::from_millis(100),
            )),
            5,
            Arc::new(FailingResolver),
            h.data_dir.clone(),
            h.runtime_base.clone(),
            always_free(),
            Arc::new(FakeDataPlaneProber::new()),
            EventBus::new(16),
        ));

        let err = manager.start(&h.ctx(0), None).await.unwrap_err();
        assert!(matches!(err, ManagerError::StartFailed(id, _) if id.as_i64() == 0));
        assert!(
            spawner.spawn_calls().is_empty(),
            "凭据解析失败时绝不启动子进程"
        );
        let e = h.registry.get(InstanceId::from_db(0).unwrap()).unwrap();
        assert_eq!(e.state, RuntimeState::Failed);
    }
}
