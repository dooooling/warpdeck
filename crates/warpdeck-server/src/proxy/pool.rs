//! P5-003 Healthy Pool Builder。
//!
//! 只把满足以下条件的实例加入 GOST 节点池（DESIGN §13.1/§13.2）：
//! 1. registry 状态为 `Healthy`（健康循环已保证数据面 warp=on）；
//! 2. 内部端口（`40000+id`）此刻 TCP 可达（pool 生成瞬间的轻量确认）。
//!
//! enabled（desired state 概念）属于 P6 reconciler 输入，P5 不参与过滤。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::config::ProxyNode;
use crate::runtime::instance::instance_port;
#[cfg(test)]
use crate::runtime::registry::InstanceRuntime;
use crate::runtime::registry::{RuntimeRegistry, RuntimeState};
/// 单节点可达性探测（可注入 fake；真实实现 TCP connect + 短超时）。
#[async_trait]
pub trait ReachabilityProbe: Send + Sync {
    async fn is_reachable(&self, addr: SocketAddr) -> bool;
}

/// 真实实现：TCP connect + `connect_timeout`。GOST 侧偶发失败由 GOST 自身的
/// `maxFails` 选择器兜底，这里只做 pool 构建瞬间的尽力过滤。
pub struct TcpReachabilityProbe {
    pub connect_timeout: Duration,
}

#[async_trait]
impl ReachabilityProbe for TcpReachabilityProbe {
    async fn is_reachable(&self, addr: SocketAddr) -> bool {
        tokio::time::timeout(self.connect_timeout, tokio::net::TcpStream::connect(addr))
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
    }
}

/// Healthy 池构建器：registry 快照 → 排序稳定的节点列表。
pub struct HealthyPoolBuilder {
    registry: Arc<RuntimeRegistry>,
    probe: Arc<dyn ReachabilityProbe>,
}

impl HealthyPoolBuilder {
    pub fn new(registry: Arc<RuntimeRegistry>, probe: Arc<dyn ReachabilityProbe>) -> Self {
        Self { registry, probe }
    }

    /// 构建当前 Healthy 节点池（按实例 id 升序，轮询顺序稳定）。
    pub async fn build(&self) -> Vec<ProxyNode> {
        let mut nodes = Vec::new();
        for (id, entry) in self.registry.list() {
            if entry.state != RuntimeState::Healthy {
                continue;
            }
            let port = match instance_port(id) {
                Ok(port) => port.as_u16(),
                Err(e) => {
                    tracing::warn!(component = "proxy_pool", instance_id = %id, error = %e, "skip instance with invalid port");
                    continue;
                }
            };
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
            if !self.probe.is_reachable(addr).await {
                continue;
            }
            nodes.push(ProxyNode {
                name: format!("warp-{}", id.as_i64()),
                addr: addr.to_string(),
            });
        }
        nodes
    }

    /// 只按 registry 状态过滤（不做 TCP 可达性），供测试/诊断。
    pub fn healthy_ids(&self) -> Vec<i64> {
        let mut ids: Vec<i64> = self
            .registry
            .list()
            .into_iter()
            .filter(|(_, e)| e.state == RuntimeState::Healthy)
            .map(|(id, _)| id.as_i64())
            .collect();
        ids.sort();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: i64) -> crate::runtime::instance::InstanceId {
        crate::runtime::instance::InstanceId::from_db(n).unwrap()
    }

    fn healthy(n: i64) -> InstanceRuntime {
        let mut e = InstanceRuntime::stopped();
        e.state = RuntimeState::Healthy;
        e.warp_pid = Some(1000 + n as u32);
        e.restart_count = 1;
        e
    }

    /// 可达性探测器：按端口黑名单失败。
    #[derive(Debug, Clone)]
    struct BlacklistProbe(Vec<u16>);

    #[async_trait]
    impl ReachabilityProbe for BlacklistProbe {
        async fn is_reachable(&self, addr: SocketAddr) -> bool {
            !self.0.contains(&addr.port())
        }
    }

    fn registry_with(
        entries: Vec<(crate::runtime::instance::InstanceId, InstanceRuntime)>,
    ) -> Arc<RuntimeRegistry> {
        let reg = Arc::new(RuntimeRegistry::new());
        for (id, entry) in entries {
            reg.insert(id);
            reg.update(id, |e| {
                e.state = entry.state;
                e.warp_pid = entry.warp_pid;
                e.restart_count = entry.restart_count;
            });
        }
        reg
    }

    #[tokio::test]
    async fn only_healthy_and_reachable_enter_pool() {
        let (i0, i1, i2, i3) = (id(0), id(1), id(2), id(3));
        let mut degraded = healthy(2);
        degraded.state = RuntimeState::Degraded;
        let reg = registry_with(vec![
            (i0, healthy(0)),
            (i1, healthy(1)),
            (i2, degraded),
            (i3, InstanceRuntime::stopped()),
        ]);

        let builder = HealthyPoolBuilder::new(reg, Arc::new(BlacklistProbe(vec![40001])));
        let nodes = builder.build().await;

        assert_eq!(nodes.len(), 1, "仅 Healthy 且可达的 #0 进池");
        assert_eq!(nodes[0].name, "warp-0");
        assert_eq!(nodes[0].addr, "127.0.0.1:40000");
    }

    #[tokio::test]
    async fn order_is_stable_by_instance_id() {
        let reg = registry_with(vec![
            (id(2), healthy(2)),
            (id(0), healthy(0)),
            (id(1), healthy(1)),
        ]);
        let builder = HealthyPoolBuilder::new(reg, Arc::new(BlacklistProbe(vec![])));
        let nodes = builder.build().await;
        let names: Vec<_> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["warp-0", "warp-1", "warp-2"]);
    }

    #[tokio::test]
    async fn empty_pool_when_nothing_healthy() {
        let mut failed = healthy(0);
        failed.state = RuntimeState::Failed;
        let reg = registry_with(vec![(id(0), failed), (id(1), InstanceRuntime::stopped())]);
        let builder = HealthyPoolBuilder::new(reg, Arc::new(BlacklistProbe(vec![])));
        let nodes = builder.build().await;
        assert!(nodes.is_empty());
        assert!(builder.healthy_ids().is_empty());
    }

    #[tokio::test]
    async fn healthy_ids_lists_only_healthy() {
        let mut degraded = healthy(1);
        degraded.state = RuntimeState::Degraded;
        let reg = registry_with(vec![
            (id(0), healthy(0)),
            (id(1), degraded),
            (id(2), healthy(2)),
        ]);
        let builder = HealthyPoolBuilder::new(reg, Arc::new(BlacklistProbe(vec![])));
        assert_eq!(builder.healthy_ids(), vec![0, 2]);
    }
}
