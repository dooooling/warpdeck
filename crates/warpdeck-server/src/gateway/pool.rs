//! RoundRobin 健康池：只消费 `RuntimeRegistry` 中 Healthy 实例（DESIGN §35.2）。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::runtime::instance::{instance_port, InstanceId};
use crate::runtime::registry::{RuntimeRegistry, RuntimeState};

/// 上游选择结果：实例内部 SOCKS5 端口（127.0.0.1:40000+id）与实例 id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub instance_id: i64,
    pub addr: SocketAddr,
}

pub struct RoundRobinPool {
    registry: Arc<RuntimeRegistry>,
    rr: AtomicU64,
}

impl Clone for RoundRobinPool {
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            rr: AtomicU64::new(self.rr.load(Ordering::Relaxed)),
        }
    }
}

impl RoundRobinPool {
    pub fn new(registry: Arc<RuntimeRegistry>) -> Self {
        Self {
            registry,
            rr: AtomicU64::new(0),
        }
    }

    /// 轮询挑选一个 Healthy 实例的内部端口。无健康实例 → None。
    pub fn pick(&self) -> Option<Upstream> {
        let mut healthy: Vec<i64> = self
            .registry
            .list()
            .iter()
            .filter(|(_, r)| r.state == RuntimeState::Healthy)
            .map(|(id, _)| id.as_i64())
            .collect();
        healthy.sort_unstable();
        if healthy.is_empty() {
            return None;
        }
        let idx = self.rr.fetch_add(1, Ordering::Relaxed) as usize % healthy.len();
        let id = healthy[idx];
        let inst = InstanceId::from_db(id).ok()?;
        // 内部端口计算唯一入口（AGENTS.md：禁止裸 40000+）。
        let port = instance_port(inst).ok()?;
        Some(Upstream {
            instance_id: id,
            addr: SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                port.as_u16(),
            ),
        })
    }
}
