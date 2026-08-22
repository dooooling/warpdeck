//! 运行时注册表（P3-001）。
//!
//! 设计（DESIGN §21.2 / §10，AGENTS.md「Runtime Registry 持有实际状态」）：
//! - 与 SQLite **期望状态**分离：本注册表只保存每个实例的*实际*运行状态快照；
//!   HTTP 处理器永不直接改这里，状态由 InstanceManager / Crash Watcher 更新。
//! - `RuntimeState` 九态（DESIGN §10），状态转换图见 DESIGN §10；
//!   不要把 `warp-cli status` 的原始字符串当领域状态，稳定映射由 Adapter 负责。
//! - 内部 `RwLock`：读多写少（status 查询、UI 轮询），短临界区。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::RwLock;

use super::crash::CrashEvent;
use super::instance::InstanceId;

/// 实例运行时状态（DESIGN §10 九态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeState {
    /// enabled=false：不参与任何生命周期操作。
    Disabled,
    /// 未运行（初始 / 停止完成）。
    Stopped,
    /// 启动流程进行中（dbus/warp-svc 拉起、注册、配置、连接、验证）。
    Starting,
    /// 注册阶段（`registration new` + backoff）。
    Registering,
    /// connect 已发出，等待数据面 connected。
    Connecting,
    /// 数据面已验证（warp=on 探测；P4 健康检查升级后仍是目标态）。
    Healthy,
    /// 瞬时健康异常（未达阈值）。
    Degraded,
    /// 优雅停止流程进行中。
    Stopping,
    /// 启动失败或崩溃后的终态；由 stop/delete/restart 收敛离开。
    Failed,
}

impl RuntimeState {
    /// 稳定字符串形式（P7-004 定义：API DTO 的 `runtime_state` 字段）。
    /// 与数据库 `desired_state` 列无关——这是运行时九态。
    pub fn as_str(self) -> &'static str {
        match self {
            RuntimeState::Disabled => "disabled",
            RuntimeState::Stopped => "stopped",
            RuntimeState::Starting => "starting",
            RuntimeState::Registering => "registering",
            RuntimeState::Connecting => "connecting",
            RuntimeState::Healthy => "healthy",
            RuntimeState::Degraded => "degraded",
            RuntimeState::Stopping => "stopping",
            RuntimeState::Failed => "failed",
        }
    }

    /// 是否处于“预期在运行”的中间/稳态（启动中、连接中、健康、退化、停止中）。
    pub fn is_running(self) -> bool {
        !matches!(self, RuntimeState::Stopped | RuntimeState::Disabled)
    }
}

/// 一个实例的运行时快照（DESIGN §21.2 `InstanceRuntime`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRuntime {
    pub state: RuntimeState,
    /// warp-svc 进程 PID（未运行 / 崩溃后为 None）。
    pub warp_pid: Option<u32>,
    /// 实例 D-Bus daemon PID。
    pub dbus_pid: Option<u32>,
    /// v4 出口 IP（P13-001 双地址族探测填充；v4 探测失败时为 None）。
    pub exit_ip_v4: Option<IpAddr>,
    /// v6 出口 IP（P13-001 双地址族探测填充；v6 探测失败时为 None）。
    pub exit_ip_v6: Option<IpAddr>,
    /// 接入的 Cloudflare 数据中心（P4 填充）。
    pub colo: Option<String>,
    /// 往返延迟毫秒（P4 填充）。
    pub latency_ms: Option<u32>,
    /// 启动次数（首次 = 1，restart 递增）。
    pub restart_count: u32,
    /// 连续失败/崩溃次数（健康检查恢复后清零；P4 阈值判定用）。
    pub consecutive_failures: u32,
    /// 连续成功次数（P4 恢复阈值判定用；失败后清零）。
    pub consecutive_successes: u32,
    /// 最近一次错误摘要（流程失败 / 崩溃）。**稳定安全摘要**：不含外部进程
    /// 输出内容（P0 审查 #6），仅结构化信息如退出码；该字段经 DTO 直出 API/SSE。
    pub last_error: Option<String>,
}

impl InstanceRuntime {
    /// 新建停止态快照。
    pub fn stopped() -> Self {
        Self {
            state: RuntimeState::Stopped,
            warp_pid: None,
            dbus_pid: None,
            exit_ip_v4: None,
            exit_ip_v6: None,
            colo: None,
            latency_ms: None,
            restart_count: 0,
            consecutive_failures: 0,
            consecutive_successes: 0,
            last_error: None,
        }
    }
}

/// 运行时注册表：`InstanceId -> InstanceRuntime`（DESIGN §21.2）。
#[derive(Debug, Default)]
pub struct RuntimeRegistry {
    inner: RwLock<HashMap<InstanceId, InstanceRuntime>>,
}

impl RuntimeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 幂等注册：不存在则创建 Stopped 快照；已存在保持原状（不重置失败计数）。
    pub fn insert(&self, id: InstanceId) {
        self.inner
            .write()
            .unwrap()
            .entry(id)
            .or_insert_with(InstanceRuntime::stopped);
    }

    /// 读取指定实例快照。
    pub fn get(&self, id: InstanceId) -> Option<InstanceRuntime> {
        self.inner.read().unwrap().get(&id).cloned()
    }

    /// 全量列表（按 id 升序，保证查询/UI 顺序稳定）。
    pub fn list(&self) -> Vec<(InstanceId, InstanceRuntime)> {
        let entries = self.inner.read().unwrap();
        let mut pairs: Vec<_> = entries.iter().map(|(id, e)| (*id, e.clone())).collect();
        pairs.sort_by_key(|(id, _)| *id);
        pairs
    }

    /// 全部实例 id（按升序）。
    pub fn ids(&self) -> Vec<InstanceId> {
        let mut ids: Vec<InstanceId> = self.inner.read().unwrap().keys().copied().collect();
        ids.sort();
        ids
    }

    /// 删除记录（P3-008 Delete 语义：停止后移除 manager record）。
    pub fn remove(&self, id: InstanceId) -> Option<InstanceRuntime> {
        self.inner.write().unwrap().remove(&id)
    }

    /// 通用原地更新（短临界区；闭包不得再触发现持有的锁）。
    pub fn update<F>(&self, id: InstanceId, f: F)
    where
        F: FnOnce(&mut InstanceRuntime),
    {
        if let Some(entry) = self.inner.write().unwrap().get_mut(&id) {
            f(entry);
        }
    }

    /// 仅更新状态位。
    pub fn set_state(&self, id: InstanceId, state: RuntimeState) {
        self.update(id, |e| e.state = state);
    }

    /// 记录失败摘要（启动失败 / 崩溃），递增连续失败计数。
    pub fn record_error(&self, id: InstanceId, error: String) {
        self.update(id, |e| {
            e.state = RuntimeState::Failed;
            e.consecutive_failures = e.consecutive_failures.saturating_add(1);
            e.last_error = Some(error);
        });
    }

    /// start 完整成功：进入 Healthy、记录进程 PID、递增启动计数、清零计数。
    pub fn on_started(&self, id: InstanceId, warp_pid: u32, dbus_pid: u32) {
        self.update(id, |e| {
            e.state = RuntimeState::Healthy;
            e.warp_pid = Some(warp_pid);
            e.dbus_pid = Some(dbus_pid);
            e.restart_count = e.restart_count.saturating_add(1);
            e.consecutive_failures = 0;
            e.consecutive_successes = 0;
            e.last_error = None;
        });
    }

    /// 受控停止完成：回到 Stopped、清除进程 PID（registration/state 保留）。
    pub fn on_stopped(&self, id: InstanceId) {
        self.update(id, |e| {
            e.state = RuntimeState::Stopped;
            e.warp_pid = None;
            e.dbus_pid = None;
        });
    }

    /// Crash Watcher 上报崩溃：Failed + 连续失败 + 错误摘要（进程已死，PID 清零）。
    ///
    /// P0 审查 #6（修订）：`last_error` 会经 DTO 直出 API/SSE，而外部进程
    /// stderr 内容不可信——可能含 license/token 等敏感串。此处只保留**稳定
    /// 安全摘要**（退出码），完整 stderr 已由 SpawnCommand 重定向进实例日志
    /// 文件，读取路径统一过中心 redactor；需要诊断时查 `instance-{id}.log`。
    pub fn on_crash(&self, event: &CrashEvent) {
        self.update(event.instance_id, |e| {
            e.state = RuntimeState::Failed;
            e.warp_pid = None;
            e.consecutive_failures = e.consecutive_failures.saturating_add(1);
            e.last_error = Some(format!(
                "warp-svc crashed: exit_code={}; see instance log for stderr details",
                event
                    .exit_status
                    .exit_code
                    .map_or("?".to_string(), |c| c.to_string()),
            ));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::process::ProcessStatus;

    fn id(value: i64) -> InstanceId {
        InstanceId::from_db(value).unwrap()
    }

    #[test]
    fn insert_creates_stopped_entries_and_list_is_stable() {
        let reg = RuntimeRegistry::new();
        reg.insert(id(7));
        reg.insert(id(3));
        reg.insert(id(0));

        assert_eq!(reg.ids(), vec![id(0), id(3), id(7)]);
        assert_eq!(
            reg.list(),
            vec![
                (id(0), InstanceRuntime::stopped()),
                (id(3), InstanceRuntime::stopped()),
                (id(7), InstanceRuntime::stopped()),
            ]
        );
        assert_eq!(reg.get(id(3)).unwrap().state, RuntimeState::Stopped);
    }

    #[test]
    fn insert_is_idempotent_and_keeps_existing_state() {
        let reg = RuntimeRegistry::new();
        reg.insert(id(0));
        reg.set_state(id(0), RuntimeState::Failed);
        reg.insert(id(0)); // 不重置
        assert_eq!(reg.get(id(0)).unwrap().state, RuntimeState::Failed);
    }

    #[test]
    fn get_missing_returns_none() {
        let reg = RuntimeRegistry::new();
        assert!(reg.get(id(0)).is_none());
    }

    #[test]
    fn update_transitions_through_lifecycle() {
        let reg = RuntimeRegistry::new();
        reg.insert(id(4));
        reg.update(id(4), |e| {
            e.state = RuntimeState::Starting;
            e.warp_pid = Some(42);
        });
        assert_eq!(reg.get(id(4)).unwrap().warp_pid, Some(42));

        reg.on_started(id(4), 102, 101);
        let e = reg.get(id(4)).unwrap();
        assert_eq!(e.state, RuntimeState::Healthy);
        assert_eq!(e.restart_count, 1);
        assert_eq!(e.consecutive_failures, 0);
        assert!(e.last_error.is_none());

        reg.on_stopped(id(4));
        let e = reg.get(id(4)).unwrap();
        assert_eq!(e.state, RuntimeState::Stopped);
        assert!(e.warp_pid.is_none());
        assert_eq!(e.restart_count, 1, "restart_count 保留");
    }

    #[test]
    fn record_error_sets_failed_and_accumulates() {
        let reg = RuntimeRegistry::new();
        reg.insert(id(0));
        reg.record_error(id(0), "connect timed out".to_string());
        reg.record_error(id(0), "registration failed".to_string());

        let e = reg.get(id(0)).unwrap();
        assert_eq!(e.state, RuntimeState::Failed);
        assert_eq!(e.consecutive_failures, 2);
        assert_eq!(e.last_error.as_deref(), Some("registration failed"));
    }

    #[test]
    fn on_started_resets_failure_accumulator_and_increments_restart() {
        let reg = RuntimeRegistry::new();
        reg.insert(id(1));
        reg.record_error(id(1), "boom".to_string());
        reg.on_started(id(1), 7, 6);

        let e = reg.get(id(1)).unwrap();
        assert_eq!(e.restart_count, 1);
        assert_eq!(e.consecutive_failures, 0);
    }

    /// P0 审查 #6：last_error 是稳定安全摘要——**绝不包含 stderr 内容**
    /// （外部进程输出不可信，可能携带 license/token；DTO/SSE 直出该字段）。
    #[test]
    fn on_crash_marks_failed_with_summary() {
        let reg = RuntimeRegistry::new();
        reg.insert(id(2));
        reg.on_started(id(2), 55, 54);
        reg.on_crash(&CrashEvent {
            instance_id: id(2),
            exit_status: ProcessStatus { exit_code: Some(9) },
            stderr_summary: "license invalid".to_string(),
        });

        let e = reg.get(id(2)).unwrap();
        assert_eq!(e.state, RuntimeState::Failed);
        assert!(e.warp_pid.is_none(), "崩溃后 PID 不应残留");
        assert_eq!(e.consecutive_failures, 1);
        let last = e.last_error.as_deref().unwrap_or("");
        assert!(last.contains("exit_code=9"), "保留退出码: {last}");
        assert!(
            !last.contains("license invalid"),
            "stderr 内容不得进入 last_error（P0 审查 #6）"
        );
    }

    #[test]
    fn remove_drops_record_only_for_that_instance() {
        let reg = RuntimeRegistry::new();
        reg.insert(id(0));
        reg.insert(id(1));

        assert!(reg.remove(id(0)).is_some());
        assert!(reg.get(id(0)).is_none());
        assert!(reg.get(id(1)).is_some());
        assert!(reg.remove(id(0)).is_none());
    }

    #[test]
    fn stopped_snapshot_is_clean() {
        let e = InstanceRuntime::stopped();
        assert_eq!(e.state, RuntimeState::Stopped);
        assert_eq!(e.restart_count, 0);
        assert_eq!(e.consecutive_failures, 0);
        assert!(e.last_error.is_none());
    }
}
