//! Fake 运行时实现（P2-003 / P2-004）。
//!
//! 供单元与集成测试使用，让 ≥80% 的测试在无真实 WARP / 无子进程环境下运行。
//! Fake 是"语义状态机 + 脚本化错误注入"，不是 mock 框架——断言时读取内部状态。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::clock::Clock;
use super::context::InstanceContext;
use super::control::{WarpCliStatus, WarpControl, WarpControlError};
use super::credentials::{
    CredentialError, CredentialMode, CredentialResolver, InstanceCredentials,
};
use super::instance::{InstanceId, InternalProxyPort};
use super::manager::{ManagerError, WarpRuntime};
use super::probe::TraceResult;
use super::probe::{DataPlaneProber, DataPlaneReport, ProbeError};
use super::process::{ProcessHandle, ProcessSpawner, ProcessStatus, SpawnCommand};
use super::registry::{InstanceRuntime, RuntimeRegistry, RuntimeState};
use super::stop::StopOutcome;

// ---------- FakeWarpRuntime ----------

/// 可编程的 `WarpRuntime`（P6 reconciler 测试接缝）。
///
/// 与真实 `InstanceManager` 相同：动作同步更新共享 `RuntimeRegistry`
/// （`with_registry` 注入，测试与 reconciler 共读），同时维护内部调用记录
/// 供断言。支持注入 start/restart 失败（P6 backoff 测试）。
#[derive(Debug)]
pub struct FakeWarpRuntime {
    inner: Mutex<FakeWarpRuntimeInner>,
    registry: Arc<RuntimeRegistry>,
}

#[derive(Debug, Default)]
struct FakeWarpRuntimeInner {
    started: Vec<i64>,
    stopped: Vec<i64>,
    restarted: Vec<i64>,
    deleted: Vec<(i64, bool)>,
    fail_next_start: bool,
    fail_restart: std::collections::HashSet<i64>,
}

impl Default for FakeWarpRuntime {
    fn default() -> Self {
        Self::with_registry(Arc::new(RuntimeRegistry::new()))
    }
}

impl FakeWarpRuntime {
    /// 注入共享 registry（reconciler 测试需要：actual 读取与动作同源）。
    pub fn with_registry(registry: Arc<RuntimeRegistry>) -> Self {
        Self {
            inner: Mutex::new(FakeWarpRuntimeInner::default()),
            registry,
        }
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// 测试与 reconciler 共读的 registry。
    pub fn registry(&self) -> Arc<RuntimeRegistry> {
        self.registry.clone()
    }

    /// 布置 fake 实际状态（registry 同步，语义同真实 manager 的失败上报）。
    pub fn set_state(&self, id: InstanceId, state: RuntimeState) {
        self.registry.set_state(id, state);
    }

    /// 断言辅助：被 start 过的实例 id（顺序）。
    pub fn started_ids(&self) -> Vec<i64> {
        self.inner.lock().unwrap().started.clone()
    }

    /// 断言辅助：被 stop 过的实例 id（顺序）。
    pub fn stopped_ids(&self) -> Vec<i64> {
        self.inner.lock().unwrap().stopped.clone()
    }

    /// 断言辅助：被 restart 过的实例 id（顺序）。
    pub fn restarted_ids(&self) -> Vec<i64> {
        self.inner.lock().unwrap().restarted.clone()
    }

    pub fn deleted_calls(&self) -> Vec<(i64, bool)> {
        self.inner.lock().unwrap().deleted.clone()
    }

    /// 注入下一（且仅下一次）start 失败：记录调用后返回 StartFailed。
    pub fn fail_next_start(&self) {
        self.inner.lock().unwrap().fail_next_start = true;
    }

    /// 注入：该实例的 restart 恒失败（直到不再调用本方法；用于 backoff 测试）。
    pub fn fail_restart(&self, id: InstanceId) {
        self.inner.lock().unwrap().fail_restart.insert(id.as_i64());
    }

    /// 取消 restart 失败注入。
    pub fn unfail_restart(&self, id: InstanceId) {
        self.inner.lock().unwrap().fail_restart.remove(&id.as_i64());
    }
}

#[async_trait]
impl WarpRuntime for FakeWarpRuntime {
    async fn start(
        &self,
        ctx: &InstanceContext,
        _account_profile_id: Option<i64>,
    ) -> Result<(), ManagerError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_next_start {
            inner.fail_next_start = false;
            // 与真实 manager 一致：失败立即落 registry（Failed + 计数），
            // 否则下一轮 reconcile 会把未标记的实例当 Stopped 重启。
            self.registry
                .record_error(ctx.id, "injected failure".into());
            return Err(ManagerError::StartFailed(ctx.id, "injected failure".into()));
        }
        inner.started.push(ctx.id.as_i64());
        self.registry.on_started(ctx.id, 4242, 4343);
        Ok(())
    }

    async fn stop(&self, id: InstanceId) -> Result<StopOutcome, ManagerError> {
        let mut inner = self.inner.lock().unwrap();
        inner.stopped.push(id.as_i64());
        self.registry.on_stopped(id);
        Ok(StopOutcome {
            kill_required: false,
            exit_status: ProcessStatus { exit_code: Some(0) },
        })
    }

    async fn restart(
        &self,
        id: InstanceId,
        _account_profile_id: Option<i64>,
    ) -> Result<(), ManagerError> {
        let mut inner = self.inner.lock().unwrap();
        inner.restarted.push(id.as_i64());
        if inner.fail_restart.contains(&id.as_i64()) {
            self.registry
                .record_error(id, "injected restart failure".into());
            return Err(ManagerError::StartFailed(
                id,
                "injected restart failure".into(),
            ));
        }
        self.registry.on_started(id, 4242, 4343);
        Ok(())
    }

    async fn status(&self, id: InstanceId) -> Option<InstanceRuntime> {
        self.registry.get(id)
    }

    async fn delete(&self, id: InstanceId, remove_registration: bool) -> Result<(), ManagerError> {
        let mut inner = self.inner.lock().unwrap();
        inner.deleted.push((id.as_i64(), remove_registration));
        self.registry.remove(id);
        Ok(())
    }
}

// ---------- ManualClock ----------

/// 不消耗真实时间的时钟：记录每次 sleep 的时长，立即返回，并推进虚拟时间。
/// 探测/重试/轮询测试的确定性时钟（`now()` 返回虚拟时刻）。
#[derive(Debug)]
pub struct ManualClock {
    slept: Arc<Mutex<Vec<std::time::Duration>>>,
    virtual_now: Arc<Mutex<std::time::Instant>>,
    /// 与 `virtual_now` 同尺度推进的 UTC 时钟（P6 backoff 时间戳比较）。
    virtual_utc: Arc<Mutex<time::OffsetDateTime>>,
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl ManualClock {
    pub fn new() -> Self {
        Self {
            slept: Arc::new(Mutex::new(vec![])),
            virtual_now: Arc::new(Mutex::new(std::time::Instant::now())),
            virtual_utc: Arc::new(Mutex::new(time::macros::datetime!(2026-01-01 0:00 UTC))),
        }
    }

    /// 断言辅助：已发生的全部 sleep 时长（顺序）。
    pub fn slept(&self) -> Vec<std::time::Duration> {
        self.slept.lock().unwrap().clone()
    }

    /// 测试辅助：直接推进虚拟 UTC（等效于 sleep 同时长，但不记录 slept）。
    pub fn advance_utc(&self, duration: std::time::Duration) {
        *self.virtual_utc.lock().unwrap() += duration;
    }
}

#[async_trait]
impl Clock for ManualClock {
    fn now(&self) -> std::time::Instant {
        *self.virtual_now.lock().unwrap()
    }

    fn now_utc_rfc3339(&self) -> String {
        self.virtual_utc
            .lock()
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default()
    }

    async fn sleep(&self, duration: std::time::Duration) {
        self.slept.lock().unwrap().push(duration);
        *self.virtual_now.lock().unwrap() += duration;
        *self.virtual_utc.lock().unwrap() += duration;
        // 不消耗真实时间，但让出一次调度：无限轮询（如 CrashWatcher）在
        // 单线程 runtime 下需要给其他任务（崩溃注入）执行机会，否则
        // manual clock 会饿死测试。
        tokio::task::yield_now().await;
    }
}

// ---------- FakeCredentialResolver ----------

/// 可编程的 `CredentialResolver`：测试直接注入固定凭据（默认 free）。
#[derive(Debug, Clone)]
pub struct FakeCredentialResolver {
    inner: Arc<Mutex<InstanceCredentials>>,
}

impl Default for FakeCredentialResolver {
    fn default() -> Self {
        Self::new(InstanceCredentials::free())
    }
}

impl FakeCredentialResolver {
    pub fn new(credentials: InstanceCredentials) -> Self {
        Self {
            inner: Arc::new(Mutex::new(credentials)),
        }
    }

    /// 替换解析结果（测试布置用）。
    pub fn set(&self, credentials: InstanceCredentials) {
        *self.inner.lock().unwrap() = credentials;
    }
}

#[async_trait]
impl CredentialResolver for FakeCredentialResolver {
    async fn resolve(
        &self,
        _profile_id: Option<i64>,
    ) -> Result<InstanceCredentials, CredentialError> {
        Ok(self.inner.lock().unwrap().clone())
    }
}

// ---------- FakeWarpControl ----------

/// 可编程的 WARP 控制面 Fake。
///
/// 默认初始状态：未注册、未连接、无代理端口。测试通过 setter 布置状态，
/// 用 `fail_next` 注入一次性错误（模拟 CommandTimeout / RegistrationFailure /
/// ConnectFailure 等场景）。
#[derive(Debug, Default)]
pub struct FakeWarpControl {
    inner: Mutex<FakeWarpControlInner>,
}

#[derive(Debug, Default)]
struct FakeWarpControlInner {
    registered: bool,
    connected: bool,
    proxy_mode: bool,
    proxy_port: Option<InternalProxyPort>,
    scripted_errors: VecDeque<WarpControlError>,
    /// 模拟"connect 已执行但 WARP 未真正连接"（license 无效等）：
    /// Some(v) 时 status 报告 v，忽略 `connected`。
    status_override: Option<bool>,
    /// 前 N 次 status 报告未连接（模拟连接建立中的短暂窗口）。
    status_pending_calls: u32,
    register_calls: u32,
    /// connect 专用错误注入（不受其他命令消费）。
    connect_failure: Option<WarpControlError>,
    /// 模拟 ZeroTrust mdm 异步注册：Some(n) 时前 n 次 connect 返回
    /// MissingRegistration，第 n 次失败时视为注册完成（registered 置 true）。
    connect_missing_registration: Option<u32>,
    /// v0.2：最近一次 apply_account 收到的凭据（断言用）。
    applied_credentials: Option<InstanceCredentials>,
    /// apply_account 被调用次数。
    apply_account_calls: u32,
}

impl FakeWarpControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_registered(&self, value: bool) {
        self.inner.lock().unwrap().registered = value;
    }

    pub fn set_connected(&self, value: bool) {
        self.inner.lock().unwrap().connected = value;
    }

    /// 注入一条一次性错误：下一次对应命令返回该错误（模拟超时 / 失败）。
    pub fn fail_next(&self, err: WarpControlError) {
        self.inner.lock().unwrap().scripted_errors.push_back(err);
    }

    /// 断言辅助：当前记录到的代理端口。
    pub fn proxy_port(&self) -> Option<InternalProxyPort> {
        self.inner.lock().unwrap().proxy_port
    }

    /// 断言辅助：当前注册状态。
    pub fn is_registered(&self) -> bool {
        self.inner.lock().unwrap().registered
    }

    /// 断言辅助：当前连接状态。
    pub fn is_connected(&self) -> bool {
        self.inner.lock().unwrap().connected
    }

    /// 断言辅助：代理模式是否已设置。
    pub fn is_proxy_mode(&self) -> bool {
        self.inner.lock().unwrap().proxy_mode
    }

    /// 断言辅助：`registration new` 被调用的次数。
    pub fn register_calls(&self) -> u32 {
        self.inner.lock().unwrap().register_calls
    }

    /// 覆盖 status 报告的连接状态（模拟 connect 成功但 WARP 未连接）。
    pub fn set_status_override(&self, value: Option<bool>) {
        self.inner.lock().unwrap().status_override = value;
    }

    /// 模拟 connect 后 WARP 仍在建立连接：前 `n` 次 status 报告未连接，
    /// 之后恢复正常（`connected` / `status_override`）。
    pub fn set_status_pending(&self, n: u32) {
        self.inner.lock().unwrap().status_pending_calls = n;
    }

    /// 注入 connect 专用错误（仅 connect 消费，mode/port 不受影响）。
    pub fn fail_connect(&self, err: WarpControlError) {
        self.inner.lock().unwrap().connect_failure = Some(err);
    }

    /// 模拟 ZeroTrust mdm 异步注册（E2E-08 实测 ~3s）：前 `n` 次 connect 返回
    /// MissingRegistration，第 n 次失败时视为 mdm 注册完成（registered 置 true），
    /// 此后 connect 正常执行。
    pub fn connect_missing_registration(&self, n: u32) {
        self.inner.lock().unwrap().connect_missing_registration = Some(n);
    }

    fn take_scripted(&self) -> Result<(), WarpControlError> {
        match self.inner.lock().unwrap().scripted_errors.pop_front() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// 断言辅助：最近一次 apply_account 收到的凭据。
    pub fn applied_credentials(&self) -> Option<InstanceCredentials> {
        self.inner.lock().unwrap().applied_credentials.clone()
    }

    /// 断言辅助：apply_account 被调用次数。
    pub fn apply_account_calls(&self) -> u32 {
        self.inner.lock().unwrap().apply_account_calls
    }
}

#[async_trait]
impl WarpControl for FakeWarpControl {
    async fn status(&self, _ctx: &InstanceContext) -> Result<WarpCliStatus, WarpControlError> {
        self.take_scripted()?;
        let mut inner = self.inner.lock().unwrap();
        if inner.status_pending_calls > 0 {
            inner.status_pending_calls -= 1;
            return Ok(WarpCliStatus {
                connected: false,
                raw_status: "Disconnected".to_string(),
            });
        }
        let connected = inner.status_override.unwrap_or(inner.connected);
        let raw = if connected {
            "Connected".to_string()
        } else {
            "Disconnected".to_string()
        };
        Ok(WarpCliStatus {
            connected,
            raw_status: raw,
        })
    }

    async fn register(&self, _ctx: &InstanceContext) -> Result<(), WarpControlError> {
        self.take_scripted()?;
        let mut inner = self.inner.lock().unwrap();
        inner.registered = true;
        inner.register_calls += 1;
        Ok(())
    }

    async fn apply_account(
        &self,
        _ctx: &InstanceContext,
        credentials: &InstanceCredentials,
    ) -> Result<(), WarpControlError> {
        self.take_scripted()?;
        // 与 RealWarpControl 对齐：free 档为 no-op（不记录、不计数）。
        if credentials.mode == CredentialMode::Free {
            return Ok(());
        }
        let mut inner = self.inner.lock().unwrap();
        inner.applied_credentials = Some(credentials.clone());
        inner.apply_account_calls += 1;
        Ok(())
    }

    async fn set_proxy_mode(&self, _ctx: &InstanceContext) -> Result<(), WarpControlError> {
        self.take_scripted()?;
        self.inner.lock().unwrap().proxy_mode = true;
        Ok(())
    }

    async fn set_proxy_port(
        &self,
        _ctx: &InstanceContext,
        port: InternalProxyPort,
    ) -> Result<(), WarpControlError> {
        self.take_scripted()?;
        self.inner.lock().unwrap().proxy_port = Some(port);
        Ok(())
    }

    async fn connect(&self, ctx: &InstanceContext) -> Result<(), WarpControlError> {
        self.take_scripted()?;
        let mut inner = self.inner.lock().unwrap();
        if let Some(err) = inner.connect_failure.take() {
            return Err(err);
        }
        if let Some(remaining) = inner.connect_missing_registration {
            if remaining > 0 {
                inner.connect_missing_registration = Some(remaining - 1);
                if remaining == 1 {
                    inner.registered = true;
                }
                return Err(WarpControlError::ConnectFailure {
                    summary: "Failed to connect err=MissingRegistration".into(),
                });
            }
        }
        if !inner.registered {
            return Err(WarpControlError::RegistrationRequired(ctx.id.as_i64()));
        }
        inner.connected = true;
        Ok(())
    }

    async fn disconnect(&self, _ctx: &InstanceContext) -> Result<(), WarpControlError> {
        self.take_scripted()?;
        self.inner.lock().unwrap().connected = false;
        Ok(())
    }
}

// ---------- FakeDataPlaneProber ----------

/// 可编程数据面探测 Fake（P4-004 测试变体：timeout / warp=off / 缺字段 /
/// 延迟 / 瞬时失败 / 恢复）。
///
/// 无脚本时返回固定成功（默认 trace：warp=on）。脚本按 FIFO 消费。
#[derive(Debug, Default)]
pub struct FakeDataPlaneProber {
    inner: Mutex<FakeDataPlaneProberInner>,
}

#[derive(Debug, Default)]
struct FakeDataPlaneProberInner {
    script: VecDeque<Result<DataPlaneReport, ProbeError>>,
    probed_ports: Vec<u16>,
}

impl FakeDataPlaneProber {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入一次成功探测（v4/v6 双族，P13-001；指定 v4 出口信息与延迟）。
    pub fn push_ok(&self, ip: &str, colo: &str, latency_ms: u64) {
        self.inner
            .lock()
            .unwrap()
            .script
            .push_back(Ok(DataPlaneReport {
                trace_v4: Some(TraceResult {
                    ip: Some(ip.to_string()),
                    colo: Some(colo.to_string()),
                    warp: Some("on".to_string()),
                }),
                trace_v6: Some(TraceResult {
                    ip: Some("2a09:bac5::1".to_string()),
                    colo: Some(colo.to_string()),
                    warp: Some("on".to_string()),
                }),
                latency_ms,
            }));
    }

    /// 注入一次探测成功但 `warp=off`（数据面不健康变体）。
    pub fn push_warp_off(&self, ip: &str, colo: &str, latency_ms: u64) {
        self.inner
            .lock()
            .unwrap()
            .script
            .push_back(Ok(DataPlaneReport {
                trace_v4: Some(TraceResult {
                    ip: Some(ip.to_string()),
                    colo: Some(colo.to_string()),
                    warp: Some("off".to_string()),
                }),
                trace_v6: None,
                latency_ms,
            }));
    }

    /// 注入一次探测失败（超时 / 连接拒绝等）。
    pub fn push_err(&self, err: ProbeError) {
        self.inner.lock().unwrap().script.push_back(Err(err));
    }

    /// 断言辅助：被探测过的端口列表（顺序）。
    pub fn probed_ports(&self) -> Vec<u16> {
        self.inner.lock().unwrap().probed_ports.clone()
    }
}

#[async_trait]
impl DataPlaneProber for FakeDataPlaneProber {
    async fn probe(
        &self,
        _proto: crate::runtime::probe::ProbeProto,
        port: u16,
    ) -> Result<DataPlaneReport, ProbeError> {
        let mut inner = self.inner.lock().unwrap();
        inner.probed_ports.push(port);
        match inner.script.pop_front() {
            Some(result) => result,
            // 无脚本：固定成功（与 FakeWarpControl 默认 connected 语义一致）。
            None => Ok(DataPlaneReport {
                trace_v4: Some(TraceResult {
                    ip: Some("104.28.1.2".to_string()),
                    colo: Some("LAX".to_string()),
                    warp: Some("on".to_string()),
                }),
                trace_v6: Some(TraceResult {
                    ip: Some("2a09:bac5::1".to_string()),
                    colo: Some("LAX".to_string()),
                    warp: Some("on".to_string()),
                }),
                latency_ms: 25,
            }),
        }
    }
}

// ---------- FakeProcessSpawner ----------

/// 记录调用并返回可编程进程句柄的 Fake 启动器。
///
/// 支持：
/// - 记录每次 spawn 的 program / args / env（断言启动参数与环境变量）。
/// - `kill()` 模拟 SIGKILL 退出（exit_code 137）。
/// - `crash_process(pid)` 模拟子进程崩溃（exit_code 1）。
/// - 每个进程句柄独立，多进程互不干扰。
#[derive(Debug, Default)]
pub struct FakeProcessSpawner {
    inner: Mutex<FakeProcessSpawnerInner>,
}

#[derive(Debug)]
struct FakeProcessSpawnerInner {
    spawn_calls: Vec<SpawnCommand>,
    next_pid: u32,
    processes: HashMap<u32, FakeProcessState>,
    /// dbus-daemon spawn 时自动创建其 `--address=unix:path=...` socket 文件
    /// （模拟真实 daemon 就绪语义）；需要测试超时路径时关闭。
    auto_socket: bool,
    /// `set_exit_on_spawn`：新 spawn 的进程立即以该退出码结束。
    spawn_exit: Option<i32>,
}

impl Default for FakeProcessSpawnerInner {
    fn default() -> Self {
        Self {
            spawn_calls: vec![],
            next_pid: 0,
            processes: HashMap::new(),
            auto_socket: true,
            spawn_exit: None,
        }
    }
}

#[derive(Debug, Clone)]
struct FakeProcessState {
    pid: u32,
    killed: Arc<AtomicBool>,
    terminated: Arc<AtomicBool>,
    exit_status: Arc<Mutex<Option<ProcessStatus>>>,
    /// `terminate()` 的注入退出：Some(status) 时优雅退出；None 表示收到信号仍存活。
    terminate_exit: Arc<Mutex<Option<ProcessStatus>>>,
}

impl FakeProcessSpawner {
    pub fn new() -> Self {
        Self::default()
    }

    /// 断言辅助：全部启动调用（program / args / envs）。
    pub fn spawn_calls(&self) -> Vec<SpawnCommand> {
        self.inner.lock().unwrap().spawn_calls.clone()
    }

    /// 断言辅助：某 pid 是否被 kill。
    pub fn was_killed(&self, pid: u32) -> bool {
        self.inner
            .lock()
            .unwrap()
            .processes
            .get(&pid)
            .map(|s| s.killed.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// 断言辅助：某 pid 是否收到过 terminate（SIGTERM）。
    pub fn was_terminated(&self, pid: u32) -> bool {
        self.inner
            .lock()
            .unwrap()
            .processes
            .get(&pid)
            .map(|s| s.terminated.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// 注入优雅退出：该 pid 收到 `terminate()` 后以 `exit_code` 退出。
    pub fn exit_on_terminate(&self, pid: u32, exit_code: i32) {
        if let Some(state) = self.inner.lock().unwrap().processes.get(&pid) {
            *state.terminate_exit.lock().unwrap() = Some(ProcessStatus {
                exit_code: Some(exit_code),
            });
        }
    }

    /// 模拟进程崩溃（非 kill 途径的非零退出）。
    pub fn crash_process(&self, pid: u32) {
        if let Some(state) = self.inner.lock().unwrap().processes.get(&pid) {
            *state.exit_status.lock().unwrap() = Some(ProcessStatus { exit_code: Some(1) });
        }
    }

    /// 注入"启动即退出"：此后 spawn 的进程立即以 `exit_code` 退出
    /// （模拟 GOST 配置非法启动即崩溃）。`None` 恢复默认。
    pub fn set_exit_on_spawn(&self, exit_code: Option<i32>) {
        self.inner.lock().unwrap().spawn_exit = exit_code;
    }

    /// 关闭/开启 dbus socket 自动就绪（默认开启；关闭用于测试超时路径）。
    pub fn set_auto_socket(&self, enabled: bool) {
        self.inner.lock().unwrap().auto_socket = enabled;
    }

    fn record(&self, cmd: &SpawnCommand) -> FakeProcessState {
        let mut inner = self.inner.lock().unwrap();
        inner.spawn_calls.push(cmd.clone());
        // dbus-daemon 就绪语义：自动创建 --address 指向的 socket 文件。
        if inner.auto_socket && cmd.program == "dbus-daemon" {
            if let Some(path) = cmd
                .args
                .iter()
                .find_map(|a| a.strip_prefix("--address=unix:path="))
            {
                if let Some(parent) = std::path::Path::new(path).parent() {
                    let _ = std::fs::create_dir_all(parent);
                    let _ = std::fs::write(path, b"");
                }
            }
        }
        let pid = inner.next_pid + 1;
        inner.next_pid = pid;
        // spawn_exit 注入：进程在"启动后"立即以给定退出码结束。
        let spawned_exit = inner.spawn_exit.map(|code| ProcessStatus {
            exit_code: Some(code),
        });
        let state = FakeProcessState {
            pid,
            killed: Arc::new(AtomicBool::new(false)),
            terminated: Arc::new(AtomicBool::new(false)),
            exit_status: Arc::new(Mutex::new(spawned_exit)),
            terminate_exit: Arc::new(Mutex::new(None)),
        };
        inner.processes.insert(pid, state.clone());
        state
    }
}

#[async_trait]
impl ProcessSpawner for FakeProcessSpawner {
    fn spawn(&self, cmd: &SpawnCommand) -> std::io::Result<Box<dyn ProcessHandle>> {
        Ok(Box::new(self.record(cmd)))
    }
}

#[async_trait]
impl ProcessHandle for FakeProcessState {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn terminate(&mut self) -> std::io::Result<()> {
        self.terminated.store(true, Ordering::SeqCst);
        if let Some(status) = self.terminate_exit.lock().unwrap().take() {
            *self.exit_status.lock().unwrap() = Some(status);
        }
        Ok(())
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.killed.store(true, Ordering::SeqCst);
        *self.exit_status.lock().unwrap() = Some(ProcessStatus {
            exit_code: Some(137),
        });
        Ok(())
    }

    async fn wait(&mut self) -> ProcessStatus {
        // Fake 进程在真实退出前永远不结束（除 kill / crash 注入）。
        loop {
            if let Some(status) = self.exit_status.lock().unwrap().take() {
                return status;
            }
            tokio::task::yield_now().await;
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ProcessStatus>> {
        Ok(self.exit_status.lock().unwrap().as_ref().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::context::InstanceContext;
    use crate::runtime::instance::{instance_port, InstanceId};
    use std::path::Path;

    fn ctx(id: i64) -> InstanceContext {
        InstanceContext::new(
            Path::new("/var/lib/warpdeck"),
            Path::new("/run/warpdeck"),
            InstanceId::from_db(id).unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn fake_warp_control_full_lifecycle() {
        let warp = FakeWarpControl::new();
        let c = ctx(0);

        assert_eq!(
            warp.status(&c).await.unwrap(),
            WarpCliStatus {
                connected: false,
                raw_status: "Disconnected".into()
            }
        );

        warp.register(&c).await.unwrap();
        assert!(warp.is_registered());

        let port = instance_port(c.id).unwrap();
        warp.set_proxy_port(&c, port).await.unwrap();
        assert_eq!(warp.proxy_port(), Some(port));

        warp.connect(&c).await.unwrap();
        assert!(warp.is_connected());
        assert!(warp.status(&c).await.unwrap().connected);

        warp.disconnect(&c).await.unwrap();
        assert!(!warp.is_connected());
    }

    #[tokio::test]
    async fn fake_warp_control_enforces_registration_before_connect() {
        let warp = FakeWarpControl::new();
        let c = ctx(1);
        assert!(matches!(
            warp.connect(&c).await,
            Err(WarpControlError::RegistrationRequired(1))
        ));
    }

    #[tokio::test]
    async fn fake_warp_control_scripted_failures() {
        let warp = FakeWarpControl::new();
        let c = ctx(2);

        warp.fail_next(WarpControlError::CommandTimeout);
        assert!(matches!(
            warp.status(&c).await,
            Err(WarpControlError::CommandTimeout)
        ));

        // 一次性注入：下一次成功。
        assert!(!warp.status(&c).await.unwrap().connected);

        warp.fail_next(WarpControlError::CommandFailed {
            summary: "exit 7".into(),
        });
        assert!(matches!(
            warp.register(&c).await,
            Err(WarpControlError::CommandFailed { .. })
        ));
        // 失败后状态未被修改。
        assert!(!warp.is_registered());
    }

    #[tokio::test]
    async fn fake_spawner_records_command_and_env() {
        let spawner = FakeProcessSpawner::new();
        let cmd = SpawnCommand::simple("/usr/bin/warp-svc")
            .with_args(vec!["--accept-tos".into()])
            .with_env("STATE_DIRECTORY", "/var/lib/warpdeck/instances/0/state")
            .with_env("RUNTIME_DIRECTORY", "/run/warpdeck/instances/0/warp");

        let mut handle = spawner.spawn(&cmd).unwrap();

        assert_eq!(handle.pid(), 1);
        let calls = spawner.spawn_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], cmd);
        assert!(!spawner.was_killed(handle.pid()));
        assert!(handle.try_wait().unwrap().is_none());

        handle.kill().unwrap();
        assert!(spawner.was_killed(handle.pid()));
        let status = handle.wait().await;
        assert_eq!(
            status,
            ProcessStatus {
                exit_code: Some(137)
            }
        );
    }

    #[tokio::test]
    async fn fake_spawner_crash_event_is_observable() {
        let spawner = FakeProcessSpawner::new();
        let cmd = SpawnCommand::simple("/usr/bin/dbus-daemon");

        let mut handle = spawner.spawn(&cmd).unwrap();
        spawner.crash_process(handle.pid());

        assert_eq!(
            handle.try_wait().unwrap(),
            Some(ProcessStatus { exit_code: Some(1) })
        );
        assert_eq!(handle.wait().await, ProcessStatus { exit_code: Some(1) });
    }

    #[tokio::test]
    async fn fake_spawner_isolates_multiple_processes() {
        let spawner = FakeProcessSpawner::new();
        let mk = |program: &str| SpawnCommand::simple(program);

        let mut a = spawner.spawn(&mk("a")).unwrap();
        let mut b = spawner.spawn(&mk("b")).unwrap();
        assert_ne!(a.pid(), b.pid());

        spawner.crash_process(a.pid());
        assert_eq!(a.wait().await, ProcessStatus { exit_code: Some(1) });
        assert!(b.try_wait().unwrap().is_none(), "b must not be affected");
    }
}
