//! RoundRobin 健康池：只消费 `RuntimeRegistry` 中 Healthy 实例（DESIGN §35.2）。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
    /// 内部端口基址（生产 = FIRST_WARP_PORT 40000；测试注入以指向 fake 上游）。
    upstream_base: u16,
}

impl Clone for RoundRobinPool {
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            rr: AtomicU64::new(self.rr.load(Ordering::Relaxed)),
            upstream_base: self.upstream_base,
        }
    }
}

impl RoundRobinPool {
    pub fn new(registry: Arc<RuntimeRegistry>) -> Self {
        Self::with_upstream_base(registry, crate::config::FIRST_WARP_PORT)
    }

    /// 测试/多租户变体：指定内部端口基址。
    pub fn with_upstream_base(registry: Arc<RuntimeRegistry>, upstream_base: u16) -> Self {
        Self {
            registry,
            rr: AtomicU64::new(0),
            upstream_base,
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
        // 内部端口 = upstream_base + id（AGENTS.md：禁止裸 40000+ 计算）。
        let port = self.upstream_base.wrapping_add(id as u16);
        Some(Upstream {
            instance_id: id,
            addr: SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::instance::InstanceId;
    use crate::runtime::registry::RuntimeState;

    #[test]
    fn picks_only_healthy_round_robin() {
        let reg = Arc::new(RuntimeRegistry::new());
        for id in [1, 2, 3] {
            let iid = InstanceId::from_db(id).unwrap();
            reg.insert(iid);
            reg.set_state(iid, RuntimeState::Healthy);
        }
        // 4 号 Failed，不参与。
        let f = InstanceId::from_db(4).unwrap();
        reg.insert(f);
        reg.set_state(f, RuntimeState::Failed);

        let pool = RoundRobinPool::new(reg.clone());
        let mut seen_ports: Vec<u16> = Vec::new();
        for _ in 0..3 {
            let u = pool.pick().unwrap();
            assert_eq!(u.addr.ip().to_string(), "127.0.0.1");
            seen_ports.push(u.addr.port());
        }
        seen_ports.sort_unstable();
        assert_eq!(seen_ports, vec![40001, 40002, 40003]);
        // RR 循环：下一轮从头部继续。
        assert_eq!(pool.pick().unwrap().addr.port(), 40001);
    }

    #[test]
    fn no_healthy_returns_none() {
        let reg = Arc::new(RuntimeRegistry::new());
        let iid = InstanceId::from_db(9).unwrap();
        reg.insert(iid);
        reg.set_state(iid, RuntimeState::Degraded);
        let pool = RoundRobinPool::new(reg);
        assert!(pool.pick().is_none());
    }
}
