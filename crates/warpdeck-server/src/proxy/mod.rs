//! P5 GOST Proxy Gateway（DESIGN §13）。
//!
//! GostManager 编排：Healthy pool → 配置渲染（原子替换）→ GOST 进程重启 →
//! listener 探活 → 状态收敛。任何一步失败都不得假装成功（P5-011）：
//! 状态迁到 `Degraded`/`Failed` 并保留原因，供 UI/API（P7/P8）展示与 E2E 断言。
//!
//! 崩溃感知：`refresh` 时对已有进程做非阻塞退出探测；进程死亡 → `Failed`。
//! 完整自动恢复属于 P6 reconciler（`refresh` + `apply` 幂等，可随意周期调用）。

pub mod config;
pub mod pool;
pub mod supervisor;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use crate::runtime::registry::RuntimeRegistry;

pub use self::config::{GostConfig, ProxyAuth};
use self::pool::HealthyPoolBuilder;
use self::supervisor::{GostProcess, GostSupervisor};

/// 对外固定端口（容器内常量，DESIGN §13.3：不进入动态配置）。
/// 单一来源 = `crate::config`；GOST 语境下以 LISTEN 别名重导出。
pub use crate::config::{HTTP_PORT as HTTP_LISTEN_PORT, SOCKS5_PORT as SOCKS5_LISTEN_PORT};

/// 生成的配置文件路径（DESIGN §13.2 / 计划 P5-002）。
pub const GENERATED_DIR: &str = "generated";
pub const GOST_CONFIG_FILE: &str = "gost.yaml";

/// 代理服务状态（DESIGN §13.5：Running / Degraded / Failed 区分）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyStatus {
    Stopped,
    /// 进程存活 + 全部启用 listener 探活通过 + upstream >= 1。
    Running {
        pid: u32,
        healthy_upstreams: usize,
        applied_at: String,
    },
    /// 进程存活但 listener 部分失效，或 upstream 池为空。
    Degraded {
        reason: String,
        pid: Option<u32>,
    },
    /// GOST 进程已退出（崩溃）。
    Failed {
        reason: String,
        exit_code: Option<i32>,
    },
}

/// 静态代理参数（P6 后由 SQLite 提供；P5 由应用层直接构造）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GostSettings {
    pub socks5_enabled: bool,
    pub http_enabled: bool,
    pub auth: Option<ProxyAuth>,
    pub allowlist: Vec<String>,
    pub max_connections: Option<u32>,
    pub max_rps: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GostManagerError {
    #[error("config error: {0}")]
    Config(#[from] config::ConfigError),
    #[error("write config failed: {0}")]
    WriteFailed(String),
    #[error("gost supervisor error: {0}")]
    Supervisor(#[from] supervisor::GostSupervisorError),
}

pub struct GostManager {
    /// 期望代理参数（P6 起由 reconciler 从 SQLite 周期同步；短临界区 clone）。
    settings: std::sync::Mutex<GostSettings>,
    builder: HealthyPoolBuilder,
    supervisor: GostSupervisor,
    /// listener 探活（真实 = TCP connect 11080/18080）。
    prober: Arc<dyn pool::ReachabilityProbe>,
    /// 数据面验证（DESIGN §13.4 第 6 步：测试请求验证至少一条 WARP 路径）。
    data_plane: Arc<dyn crate::runtime::probe::DataPlaneProber>,
    generated_dir: PathBuf,
    config_path: PathBuf,
    state: tokio::sync::Mutex<GostRuntime>,
}

/// 运行期持有对象 + 对外状态。
struct GostRuntime {
    process: Option<GostProcess>,
    status: ProxyStatus,
}

impl GostManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<RuntimeRegistry>,
        pool_probe: Arc<dyn pool::ReachabilityProbe>,
        listener_probe: Arc<dyn pool::ReachabilityProbe>,
        data_plane: Arc<dyn crate::runtime::probe::DataPlaneProber>,
        spawner: Arc<dyn crate::runtime::process::ProcessSpawner>,
        clock: Arc<dyn crate::runtime::clock::Clock>,
        gost_binary: String,
        data_dir: PathBuf,
        settings: GostSettings,
        stop_grace: Duration,
        stop_poll: Duration,
    ) -> Self {
        let generated_dir = data_dir.join(GENERATED_DIR);
        let config_path = generated_dir.join(GOST_CONFIG_FILE);
        let supervisor = GostSupervisor::new(
            spawner,
            clock,
            gost_binary,
            config_path.clone(),
            data_dir.join("logs").join("gost.stderr.log"),
            stop_grace,
            stop_poll,
        );
        let builder = HealthyPoolBuilder::new(registry, pool_probe);
        Self {
            settings: std::sync::Mutex::new(settings),
            builder,
            supervisor,
            prober: listener_probe,
            data_plane,
            generated_dir,
            config_path,
            state: tokio::sync::Mutex::new(GostRuntime {
                process: None,
                status: ProxyStatus::Stopped,
            }),
        }
    }

    /// 更新期望代理参数（P6 reconciler 周期调用；`apply` 下一次运行时生效）。
    pub fn update_settings(&self, settings: GostSettings) {
        *self.settings.lock().unwrap() = settings;
    }

    /// 当前期望参数（测试与断言辅助）。
    pub fn settings_snapshot(&self) -> GostSettings {
        self.settings.lock().unwrap().clone()
    }

    /// 当前对外状态（先做一次轻量崩溃感知刷新）。
    pub async fn status(&self) -> ProxyStatus {
        let mut rt = self.state.lock().await;
        self.refresh_locked(&mut rt).await;
        rt.status.clone()
    }

    /// 幂等应用一次完整事务（P5-011）：
    /// pool → render temp →（内建校验）→ atomic rename →（配置未变则跳过）→
    /// GOST restart → listener 探活 → 数据面验证 → 状态收敛。
    /// 失败不假装成功：Degraded/Failed + 原因。
    ///
    /// review 补强（P5）：配置渲染结果与现配置文件一致且进程存活时跳过
    /// 重启（P6 reconciler 周期调用不掐断活跃连接）；listener 轮询期间发现
    /// 进程提前退出直接判 Failed（不再空转到超时）。
    pub async fn apply(&self) -> Result<(), GostManagerError> {
        let mut rt = self.state.lock().await;
        self.refresh_locked(&mut rt).await;
        let settings = self.settings.lock().unwrap().clone();

        let nodes = self.builder.build().await;
        let healthy_count = nodes.len();
        let cfg = GostConfig::new(
            settings.socks5_enabled,
            settings.http_enabled,
            settings.auth.clone(),
            &settings.allowlist,
            settings.max_connections,
            settings.max_rps,
            nodes,
        )?;
        let rendered = cfg.render();

        // 配置未变 + 进程存活 → 跳过 stop/start（幂等 apply 的更优语义）。
        let process_alive = rt
            .process
            .as_mut()
            .map(|p| p.try_exited().is_none())
            .unwrap_or(false);
        let config_unchanged = tokio::fs::read_to_string(&self.config_path)
            .await
            .is_ok_and(|existing| existing == rendered);
        let skip_restart = process_alive && config_unchanged;

        self.write_config_atomically(&cfg).await?;

        let process_missing = !skip_restart;
        if process_missing {
            // 旧进程停止失败只记录（仍在 try_exited 覆盖）。
            if let Some(mut old) = rt.process.take() {
                if let Err(e) = self.supervisor.stop(&mut old).await {
                    tracing::warn!(component = "gost_manager", error = %e, "gost stop before restart failed");
                }
            }
            let proc = match self.supervisor.start().await {
                Ok(proc) => proc,
                Err(e) => {
                    rt.status = ProxyStatus::Failed {
                        reason: format!("gost failed to start: {e}"),
                        exit_code: None,
                    };
                    return Err(GostManagerError::Supervisor(e));
                }
            };
            rt.process = Some(proc);
        }

        // listener 探活（P5-011 第 5 步）：GOST 绑定端口有启动窗口，
        // 有界重试轮询（0.25s 间隔 × 40 = 10s）；进程提前退出 → 直接 Failed。
        let mut failed: Vec<(String, u16)> = Vec::new();
        let mut checks = Vec::new();
        if settings.socks5_enabled {
            checks.push(("socks5".to_string(), SOCKS5_LISTEN_PORT));
        }
        if settings.http_enabled {
            checks.push(("http".to_string(), HTTP_LISTEN_PORT));
        }
        {
            let proc = rt.process.as_mut().expect("process is running");
            for (name, port) in &checks {
                match self.wait_listener(proc, *port).await {
                    ListenerProbe::Listening => {}
                    ListenerProbe::Exited(status) => {
                        rt.process = None;
                        rt.status = ProxyStatus::Failed {
                            reason: "gost exited during startup".to_string(),
                            exit_code: status.exit_code,
                        };
                        return Ok(());
                    }
                    ListenerProbe::TimedOut => failed.push((name.clone(), *port)),
                }
            }
        }

        if !failed.is_empty() {
            let reason = failed
                .iter()
                .map(|(name, port)| format!("{name} :{port} not listening"))
                .collect::<Vec<_>>()
                .join(", ");
            rt.status = ProxyStatus::Degraded {
                reason,
                pid: rt.process.as_ref().map(|p| p.pid()),
            };
            return Ok(());
        }

        // 空池：listener 保留 + 明确 Degraded（DESIGN §13.5）。计数用实际
        // build 结果（含 TCP 探活），与渲染到 chain 的节点一致。
        if healthy_count == 0 {
            rt.status = ProxyStatus::Degraded {
                reason: "no healthy upstreams".to_string(),
                pid: rt.process.as_ref().map(|p| p.pid()),
            };
            return Ok(());
        }

        // 数据面验证（DESIGN §13.4 第 6 步）：经启用 listener 做一次真实
        // trace 请求，必须 warp=on 才算 Running（E2E 实测 forwarder 误用
        // 导致直连 warp=off，此检查可拦截同类问题）。协议跟随启用中的
        // listener：socks5 优先，HTTP-only 模式走 HTTP CONNECT 隧道
        // （review 补强：此前 HTTP-only 永远无法到 Running）。
        let (probe_proto, probe_port) = if settings.socks5_enabled {
            (
                crate::runtime::probe::ProbeProto::Socks5,
                SOCKS5_LISTEN_PORT,
            )
        } else {
            (crate::runtime::probe::ProbeProto::Http, HTTP_LISTEN_PORT)
        };
        match self.data_plane.probe(probe_proto, probe_port).await {
            Ok(report) if report.warp_on() => {
                rt.status = ProxyStatus::Running {
                    pid: rt.process.as_ref().expect("process exists").pid(),
                    healthy_upstreams: healthy_count,
                    applied_at: time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default(),
                };
            }
            Ok(report) => {
                rt.status = ProxyStatus::Degraded {
                    reason: format!(
                        "data plane check: warp={:?} (not on; proxy may be bypassing upstream)",
                        report.trace_v4.as_ref().and_then(|t| t.warp.as_deref())
                    ),
                    pid: rt.process.as_ref().map(|p| p.pid()),
                };
            }
            Err(e) => {
                rt.status = ProxyStatus::Degraded {
                    reason: format!("data plane check failed: {e}"),
                    pid: rt.process.as_ref().map(|p| p.pid()),
                };
            }
        }
        Ok(())
    }

    /// 受控停止 GOST；状态回 Stopped。
    pub async fn stop(&self) -> Result<(), GostManagerError> {
        let mut rt = self.state.lock().await;
        if let Some(mut proc) = rt.process.take() {
            let _ = self.supervisor.stop(&mut proc).await;
        }
        rt.status = ProxyStatus::Stopped;
        Ok(())
    }

    /// 崩溃感知：进程已退出 → Failed（退出码 + stderr 尾部摘要）。
    async fn refresh_locked(&self, rt: &mut GostRuntime) {
        let Some(proc) = rt.process.as_mut() else {
            return;
        };
        if let Some(status) = proc.try_exited() {
            // 先拷贝日志路径再 await：避免 `&GostProcess` 跨 await（非 Sync）。
            let log_path = proc.log_path();
            let summary = self::supervisor::read_log_tail(&log_path).await;
            let reason = format!(
                "gost exited: exit_code={}, stderr: {}",
                status.exit_code.map_or("?".to_string(), |c| c.to_string()),
                summary.trim()
            );
            rt.process = None;
            rt.status = ProxyStatus::Failed {
                reason,
                exit_code: status.exit_code,
            };
        }
    }

    /// render → 写临时文件 → 原子 rename（P5-002 流程；写失败不改正式配置）。
    /// 文件含代理凭据（auth 开启时）——Unix 上强制 mode 0600（P12-004）。
    async fn write_config_atomically(&self, cfg: &GostConfig) -> Result<(), GostManagerError> {
        tokio::fs::create_dir_all(&self.generated_dir)
            .await
            .map_err(|e| GostManagerError::WriteFailed(e.to_string()))?;
        let tmp = self.generated_dir.join("gost.yaml.tmp");
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut f = opts
            .open(&tmp)
            .await
            .map_err(|e| GostManagerError::WriteFailed(e.to_string()))?;
        f.write_all(cfg.render().as_bytes())
            .await
            .map_err(|e| GostManagerError::WriteFailed(e.to_string()))?;
        f.sync_all()
            .await
            .map_err(|e| GostManagerError::WriteFailed(e.to_string()))?;
        drop(f);
        tokio::fs::rename(&tmp, &self.config_path)
            .await
            .map_err(|e| GostManagerError::WriteFailed(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&self.config_path, std::fs::Permissions::from_mode(0o600))
                .await
                .map_err(|e| GostManagerError::WriteFailed(e.to_string()))?;
        }
        Ok(())
    }

    /// 已生成配置文件的路径（测试/诊断）。
    pub fn config_path(&self) -> &PathBuf {
        &self.config_path
    }

    /// Bounded listener probe: GOST needs a startup window to bind ports.
    /// Probe-failure risk is real E2E; retry up to 40x with 250ms interval.
    /// 每轮先探活再查进程存活（review 补强：进程启动即崩溃 → 立即判
    /// Exited，不空转满 40 轮才误报 Degraded）。
    async fn wait_listener(&self, proc: &mut GostProcess, port: u16) -> ListenerProbe {
        for _ in 0..40 {
            if self.prober.is_reachable(socket(port)).await {
                return ListenerProbe::Listening;
            }
            if let Some(status) = proc.try_exited() {
                return ListenerProbe::Exited(status);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        ListenerProbe::TimedOut
    }
}

/// `wait_listener` 的单次判定结果。
enum ListenerProbe {
    Listening,
    /// 探活期间进程退出（崩溃/启动即退出）。
    Exited(crate::runtime::process::ProcessStatus),
    /// 40 轮探活耗尽仍未绑定端口。
    TimedOut,
}

fn socket(port: u16) -> std::net::SocketAddr {
    std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port)
}

/// P6 reconciler 接缝实现：期望配置更新 + 幂等 apply。
#[async_trait::async_trait]
impl crate::reconciler::ProxyApplier for GostManager {
    async fn apply_config(&self, settings: &GostSettings) -> Result<(), String> {
        self.update_settings(settings.clone());
        self.apply().await.map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::{FakeProcessSpawner, ManualClock};
    use crate::runtime::instance::InstanceId;
    use crate::runtime::registry::RuntimeState;
    use std::net::SocketAddr;

    fn id(n: i64) -> InstanceId {
        InstanceId::from_db(n).unwrap()
    }

    fn registry_with_healthy(count: i64) -> Arc<RuntimeRegistry> {
        let reg = Arc::new(RuntimeRegistry::new());
        for n in 0..count {
            reg.insert(id(n));
            reg.update(id(n), |e| {
                e.state = RuntimeState::Healthy;
                e.warp_pid = Some(1000 + n as u32);
                e.restart_count = 1;
            });
        }
        reg
    }

    /// 可编程探活：按端口失败。
    #[derive(Debug, Clone)]
    struct ProbeFail {
        fail: u16,
    }

    #[async_trait::async_trait]
    impl pool::ReachabilityProbe for ProbeFail {
        async fn is_reachable(&self, addr: SocketAddr) -> bool {
            addr.port() != self.fail
        }
    }

    /// 启动窗口模拟：前 `delay_calls` 次失败，之后成功（验证 wait_listener 重试）。
    #[derive(Debug, Clone)]
    struct ProbeStartupDelay {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        delay_calls: usize,
    }

    #[async_trait::async_trait]
    impl pool::ReachabilityProbe for ProbeStartupDelay {
        async fn is_reachable(&self, _addr: SocketAddr) -> bool {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            n >= self.delay_calls
        }
    }

    /// Fake 数据面探针：默认 warp=on；可注入失败（验证数据面检查进 Degraded）。
    #[derive(Debug, Clone)]
    struct FakeDataPlane {
        warp: Option<String>,
        fail: bool,
    }

    impl Default for FakeDataPlane {
        fn default() -> Self {
            Self {
                warp: Some("on".into()),
                fail: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::runtime::probe::DataPlaneProber for FakeDataPlane {
        async fn probe(
            &self,
            _proto: crate::runtime::probe::ProbeProto,
            _port: u16,
        ) -> Result<crate::runtime::probe::DataPlaneReport, crate::runtime::probe::ProbeError>
        {
            if self.fail {
                return Err(crate::runtime::probe::ProbeError::ProxyConnect(
                    "refused".into(),
                ));
            }
            Ok(crate::runtime::probe::DataPlaneReport {
                trace_v4: Some(crate::runtime::probe::TraceResult {
                    ip: None,
                    colo: None,
                    warp: self.warp.clone(),
                }),
                trace_v6: None,
                latency_ms: 0,
            })
        }
    }

    struct Harness {
        manager: GostManager,
        spawner: Arc<FakeProcessSpawner>,
        _keep: tempfile::TempDir,
    }

    impl Harness {
        fn new(healthy_instances: i64, fail_port: Option<u16>) -> Self {
            let spawner = Arc::new(FakeProcessSpawner::new());
            let data = tempfile::TempDir::new().unwrap();
            let manager = GostManager::new(
                registry_with_healthy(healthy_instances),
                Arc::new(ProbeFail { fail: 0 }),
                Arc::new(ProbeFail {
                    fail: fail_port.unwrap_or(0),
                }),
                Arc::new(FakeDataPlane::default()),
                spawner.clone(),
                Arc::new(ManualClock::new()),
                "gost".to_string(),
                data.path().to_path_buf(),
                GostSettings {
                    socks5_enabled: true,
                    http_enabled: true,
                    auth: None,
                    allowlist: vec![],
                    max_connections: None,
                    max_rps: None,
                },
                Duration::from_secs(5),
                Duration::from_millis(50),
            );
            Self {
                manager,
                spawner,
                _keep: data,
            }
        }
    }

    #[tokio::test]
    async fn startup_window_is_polled_not_misreported() {
        // GOST 绑定有启动窗口（实测秒级）：前几次 probe 失败不应误报 Degraded。
        let spawner = Arc::new(FakeProcessSpawner::new());
        let data = tempfile::TempDir::new().unwrap();
        let delay = ProbeStartupDelay {
            calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            delay_calls: 5,
        };
        let manager = GostManager::new(
            registry_with_healthy(1),
            Arc::new(ProbeFail { fail: 0 }),
            Arc::new(delay.clone()),
            Arc::new(FakeDataPlane::default()),
            spawner.clone(),
            Arc::new(ManualClock::new()),
            "gost".to_string(),
            data.path().to_path_buf(),
            GostSettings {
                socks5_enabled: true,
                http_enabled: true,
                auth: None,
                allowlist: vec![],
                max_connections: None,
                max_rps: None,
            },
            Duration::from_secs(5),
            Duration::from_millis(50),
        );
        manager.apply().await.unwrap();
        assert!(
            matches!(manager.status().await, ProxyStatus::Running { .. }),
            "startup window must be absorbed by wait_listener retry"
        );
        let calls = delay.calls.load(std::sync::atomic::Ordering::SeqCst);
        assert!(calls >= 5, "expected retry calls, got {calls}");
    }

    #[tokio::test]
    async fn apply_reaches_running_with_rendered_config() {
        let h = Harness::new(2, None);
        h.manager.apply().await.unwrap();

        match h.manager.status().await {
            ProxyStatus::Running {
                pid,
                healthy_upstreams,
                ..
            } => {
                assert!(pid >= 1);
                assert_eq!(healthy_upstreams, 2);
            }
            other => panic!("expected Running, got {other:?}"),
        }
        let config = std::fs::read_to_string(h.manager.config_path()).unwrap();
        assert!(config.contains("addr: \":11080\""));
        assert!(config.contains("addr: \"127.0.0.1:40000\""));
        assert!(config.contains("strategy: round"));
        // GOST 以 -C 启动。
        let calls = h.spawner.spawn_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "gost");
    }

    #[tokio::test]
    async fn data_plane_warp_off_is_degraded_not_running() {
        // GOST 直连（warp=off）是最危险的静默失败（E2E 实测 forwarder 误用）：
        // 数据面检查必须把它拦成 Degraded。
        let spawner = Arc::new(FakeProcessSpawner::new());
        let data = tempfile::TempDir::new().unwrap();
        let manager = GostManager::new(
            registry_with_healthy(1),
            Arc::new(ProbeFail { fail: 0 }),
            Arc::new(ProbeFail { fail: 0 }),
            Arc::new(FakeDataPlane {
                warp: Some("off".into()),
                fail: false,
            }),
            spawner.clone(),
            Arc::new(ManualClock::new()),
            "gost".to_string(),
            data.path().to_path_buf(),
            GostSettings {
                socks5_enabled: true,
                http_enabled: true,
                auth: None,
                allowlist: vec![],
                max_connections: None,
                max_rps: None,
            },
            Duration::from_secs(5),
            Duration::from_millis(50),
        );
        manager.apply().await.unwrap();
        match manager.status().await {
            ProxyStatus::Degraded { reason, pid } => {
                assert!(reason.contains("data plane check"), "got: {reason}");
                assert!(pid.is_some(), "进程仍在运行，状态必须可恢复");
            }
            other => panic!("expected Degraded (warp=off), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn data_plane_probe_error_is_degraded_with_reason() {
        let spawner = Arc::new(FakeProcessSpawner::new());
        let data = tempfile::TempDir::new().unwrap();
        let manager = GostManager::new(
            registry_with_healthy(1),
            Arc::new(ProbeFail { fail: 0 }),
            Arc::new(ProbeFail { fail: 0 }),
            Arc::new(FakeDataPlane {
                warp: None,
                fail: true,
            }),
            spawner.clone(),
            Arc::new(ManualClock::new()),
            "gost".to_string(),
            data.path().to_path_buf(),
            GostSettings {
                socks5_enabled: true,
                http_enabled: true,
                auth: None,
                allowlist: vec![],
                max_connections: None,
                max_rps: None,
            },
            Duration::from_secs(5),
            Duration::from_millis(50),
        );
        manager.apply().await.unwrap();
        match manager.status().await {
            ProxyStatus::Degraded { reason, .. } => {
                assert!(reason.contains("data plane check failed"), "got: {reason}");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_skips_restart_when_config_unchanged() {
        // review 补强：配置渲染结果未变 + 进程存活 → 不重启（P6 reconciler
        // 周期调用不会掐断活跃连接）。
        let h = Harness::new(1, None);
        h.manager.apply().await.unwrap();
        let first_pid = match h.manager.status().await {
            ProxyStatus::Running { pid, .. } => pid,
            other => panic!("expected Running, got {other:?}"),
        };

        h.manager.apply().await.unwrap();
        match h.manager.status().await {
            ProxyStatus::Running { pid, .. } => {
                assert_eq!(pid, first_pid, "配置未变不得重启进程");
            }
            other => panic!("expected Running, got {other:?}"),
        }
        assert_eq!(h.spawner.spawn_calls().len(), 1, "只 spawn 过一次");
        assert!(!h.spawner.was_terminated(first_pid));
    }

    #[tokio::test]
    async fn apply_restarts_when_config_file_changed() {
        let h = Harness::new(1, None);
        h.manager.apply().await.unwrap();
        let first_pid = match h.manager.status().await {
            ProxyStatus::Running { pid, .. } => pid,
            other => panic!("expected Running, got {other:?}"),
        };
        h.spawner.exit_on_terminate(first_pid, 0);

        // 外部改配置（模拟期望状态变化）：内容不同 → 必须重启。
        std::fs::write(h.manager.config_path(), b"# externally changed\n").unwrap();
        h.manager.apply().await.unwrap();
        let second_pid = match h.manager.status().await {
            ProxyStatus::Running { pid, .. } => pid,
            other => panic!("expected Running, got {other:?}"),
        };
        assert_ne!(first_pid, second_pid, "配置变化应重启进程");
        assert!(h.spawner.was_terminated(first_pid));
        assert_eq!(h.spawner.spawn_calls().len(), 2);
    }

    #[tokio::test]
    async fn http_only_mode_reaches_running_via_http_probe() {
        // review 补强：HTTP-only 配置此前数据面验证对它发 SOCKS5 握手，
        // 永远 Degraded；现在必须经 HTTP CONNECT 隧道探测并到 Running。
        let spawner = Arc::new(FakeProcessSpawner::new());
        let data = tempfile::TempDir::new().unwrap();
        let manager = GostManager::new(
            registry_with_healthy(1),
            Arc::new(ProbeFail { fail: 0 }),
            Arc::new(ProbeFail { fail: 0 }),
            Arc::new(FakeDataPlane::default()),
            spawner.clone(),
            Arc::new(ManualClock::new()),
            "gost".to_string(),
            data.path().to_path_buf(),
            GostSettings {
                socks5_enabled: false,
                http_enabled: true,
                auth: None,
                allowlist: vec![],
                max_connections: None,
                max_rps: None,
            },
            Duration::from_secs(5),
            Duration::from_millis(50),
        );
        manager.apply().await.unwrap();
        match manager.status().await {
            ProxyStatus::Running {
                healthy_upstreams: 1,
                ..
            } => {}
            other => panic!("expected Running with 1 upstream, got {other:?}"),
        }
        // 渲染必须只含 HTTP listener。
        let config = std::fs::read_to_string(manager.config_path()).unwrap();
        assert!(!config.contains("11080"));
        assert!(config.contains(":18080"));
    }

    #[tokio::test]
    async fn startup_immediate_exit_is_failed_not_degraded() {
        // review 补强：GOST 启动即崩溃（如配置非法）→ wait_listener 必须
        // 立即判 Failed，而不是空转 40 轮后误报 Degraded。
        let spawner = Arc::new(FakeProcessSpawner::new());
        spawner.set_exit_on_spawn(Some(2));
        let data = tempfile::TempDir::new().unwrap();
        let manager = GostManager::new(
            registry_with_healthy(1),
            Arc::new(ProbeFail { fail: 0 }),
            // listener 探活必须失败才会走到进程退出检查（GOST 已崩，端口无人监听）。
            Arc::new(ProbeFail {
                fail: SOCKS5_LISTEN_PORT,
            }),
            Arc::new(FakeDataPlane::default()),
            spawner.clone(),
            Arc::new(ManualClock::new()),
            "gost".to_string(),
            data.path().to_path_buf(),
            GostSettings {
                socks5_enabled: true,
                http_enabled: true,
                auth: None,
                allowlist: vec![],
                max_connections: None,
                max_rps: None,
            },
            Duration::from_secs(5),
            Duration::from_millis(50),
        );
        manager.apply().await.unwrap();
        match manager.status().await {
            ProxyStatus::Failed {
                reason,
                exit_code: Some(2),
            } => assert!(
                reason.contains("gost exited during startup"),
                "got: {reason}"
            ),
            other => panic!("expected Failed with exit_code 2, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_pool_is_degraded_with_listeners_kept() {
        let h = Harness::new(0, None);
        h.manager.apply().await.unwrap();
        match h.manager.status().await {
            ProxyStatus::Degraded { reason, pid } => {
                assert!(reason.contains("no healthy upstreams"));
                assert!(pid.is_some(), "进程仍在运行");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
        // listener 配置仍在。
        let config = std::fs::read_to_string(h.manager.config_path()).unwrap();
        assert!(config.contains("addr: \":11080\""));
    }

    #[tokio::test]
    async fn listener_probe_failure_is_degraded_with_reason() {
        let h = Harness::new(1, Some(18080));
        h.manager.apply().await.unwrap();
        match h.manager.status().await {
            ProxyStatus::Degraded { reason, .. } => {
                assert!(
                    reason.contains("http :18080 not listening"),
                    "got: {reason}"
                );
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn crash_is_detected_on_status_refresh() {
        let h = Harness::new(1, None);
        h.manager.apply().await.unwrap();
        let pid = match h.manager.status().await {
            ProxyStatus::Running { pid, .. } => pid,
            other => panic!("expected Running, got {other:?}"),
        };
        // 模拟 GOST 崩溃（非受控退出）。
        h.spawner.crash_process(pid);
        match h.manager.status().await {
            ProxyStatus::Failed { exit_code, .. } => {
                assert_eq!(exit_code, Some(1));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // 崩溃后可以重新 apply 恢复。
        h.manager.apply().await.unwrap();
        assert!(matches!(
            h.manager.status().await,
            ProxyStatus::Running { .. }
        ));
    }

    #[tokio::test]
    async fn stop_returns_to_stopped() {
        let h = Harness::new(1, None);
        h.manager.apply().await.unwrap();
        let pid = match h.manager.status().await {
            ProxyStatus::Running { pid, .. } => pid,
            other => panic!("expected Running, got {other:?}"),
        };
        h.spawner.exit_on_terminate(pid, 0);
        h.manager.stop().await.unwrap();
        assert_eq!(h.manager.status().await, ProxyStatus::Stopped);
        assert!(h.spawner.was_terminated(pid));
    }

    #[tokio::test]
    async fn config_rejection_does_not_touch_process() {
        // ˫ listener �ر� �� ConfigError�����̲��ñ���������ʧ�ܲ��� apply����
        let h = Harness::new(1, None);
        let rt = h.manager.state.lock().await;
        let cfg = GostConfig::new(false, false, None, &[], None, None, vec![]);
        assert!(cfg.is_err());
        drop(rt);
        assert!(h.manager.status().await == ProxyStatus::Stopped);
    }

    /// P12-004：生成的 gost.yaml 含代理凭据，Unix 上必须 0600。
    #[cfg(unix)]
    #[tokio::test]
    async fn generated_config_is_mode_0600() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let h = Harness::new(1, None);
        let cfg = GostConfig::new(
            true,
            true,
            Some(config::ProxyAuth {
                username: "user".into(),
                password: "secret-proxy-pass".into(),
            }),
            &[],
            None,
            None,
            vec![],
        )
        .unwrap();
        h.manager.write_config_atomically(&cfg).await.unwrap();
        let meta = std::fs::metadata(h.manager.config_path()).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(h.manager.config_path()).unwrap();
        assert!(text.contains("secret-proxy-pass"));
    }
}
