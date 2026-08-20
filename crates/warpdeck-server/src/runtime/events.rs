//! 内部事件总线（P4-008 / P10-001/003）。
//!
//! DESIGN §25.9 事件清单：`instance.state_changed` / `instance.health_changed` /
//! `instance.exit_ip_changed`（另含 GOST/代理与系统事件，P5/P10 扩展）。
//! 实现为 `tokio::sync::broadcast`：多订阅者、慢订阅者 lagged、无背压
//! 状态事件快照可丢弃（P10-008：慢客户端不拖垮 manager）。
//!
//! SSE 帧对外契约（P10-003）由 `api::events` 负责包裹：
//! `{type, version, timestamp, resource_id, data}`。

use tokio::sync::broadcast;

use super::instance::InstanceId;
use super::registry::RuntimeState;

/// 实例 id 与状态迁移组合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateTransition {
    pub instance_id: InstanceId,
    pub from: RuntimeState,
    pub to: RuntimeState,
    /// 迁移原因（启动 / 停止 / 健康迁移 / 崩溃），不含 secret。
    pub reason: String,
}

/// 健康事件（P4-008 最小集合）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthEvent {
    /// 任何生命周期状态迁移（记录 / 停止 / 启动中 / 失败）。
    StateChanged(StateTransition),
    /// 健康维度变化（Healthy/Degraded/Failed）。
    HealthChanged(StateTransition),
    /// 出口 IP 变化（双地址族；colo/延迟供 UI 展示，P13-001）。
    ExitIpChanged {
        instance_id: InstanceId,
        exit_ip_v4: Option<String>,
        exit_ip_v6: Option<String>,
        colo: Option<String>,
        latency_ms: Option<u32>,
    },
}

/// 事件总线（状态/健康帧）：`publish` 由 manager / health monitor，
/// `subscribe` 由 SSE 端点消费。
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<HealthEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(128)
    }
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// 发布一条事件。Ok / Lagged / NoSubscribers 都是可接受状态（快照类事件）。
    pub fn publish(&self, event: HealthEvent) {
        if let Err(err) = self.tx.send(event) {
            tracing::debug!(component = "event_bus", error = %err, "event dropped");
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HealthEvent> {
        self.tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::instance::InstanceId;

    fn id(v: i64) -> InstanceId {
        InstanceId::from_db(v).unwrap()
    }

    #[test]
    fn publish_reaches_subscribers() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        bus.publish(HealthEvent::StateChanged(StateTransition {
            instance_id: id(1),
            from: RuntimeState::Stopped,
            to: RuntimeState::Starting,
            reason: "start".into(),
        }));
        let event = rx.try_recv().expect("event delivered");
        assert!(matches!(event, HealthEvent::StateChanged(_)));
    }
}
