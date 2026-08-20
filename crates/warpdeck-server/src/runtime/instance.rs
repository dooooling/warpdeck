//! 类型化的实例标识与内部端口计算（P2-001）。
//!
//! 设计约束（DESIGN §25.8 / §16，AGENTS.md）：
//! - 禁止裸 `i64` 散布端口计算；端口计算必须由唯一函数 `instance_port` 实现并测试。
//! - `40000 + instance_id` 必须检查 `u16` 上限与保留端口冲突。
//! - 全局统一从这里取端口，任何模块不得自行 `40000 +`。

use std::fmt;

use crate::config::{FIRST_WARP_PORT, HTTP_PORT, SOCKS5_PORT, WEB_PORT};

/// 保留端口集合：Web/API、SOCKS5 proxy、HTTP proxy。
/// WARP 内部区从 `FIRST_WARP_PORT` 起步（`40000+`），正常不与保留区重叠；
/// 检查是防御性的——若未来 FIRST_WARP_PORT 被改低，立即被发现。
const RESERVED_PORTS: [u16; 3] = [WEB_PORT, SOCKS5_PORT, HTTP_PORT];

/// 实例标识：非负整数，对应 SQLite `warp_instances.id`（DESIGN §16.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstanceId(i64);

/// WARP 实例的内部 upstream 端口：`FIRST_WARP_PORT + instance_id`，仅容器内回环使用。
/// 只能通过 `instance_port` 构造；不提供直接构造器，杜绝手写端口号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InternalProxyPort(u16);

/// InstanceId / 端口计算错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InstanceIdError {
    #[error("instance id must be >= 0, got {0}")]
    Negative(i64),
    #[error("instance id {0} out of range: internal port would exceed u16::MAX")]
    OutOfRange(i64),
    #[error("instance id {0} collides with reserved port range")]
    Reserved(i64),
}

impl InstanceId {
    /// 从数据库 / 外部输入构造，拒绝负数。
    pub fn from_db(value: i64) -> Result<Self, InstanceIdError> {
        if value < 0 {
            return Err(InstanceIdError::Negative(value));
        }
        Ok(Self(value))
    }

    /// 底层 i64（日志、数据库读写等需要原始值的场合）。
    pub fn as_i64(self) -> i64 {
        self.0
    }
}

/// 端口是否落在保留区（Web/API、SOCKS5、HTTP）。
pub(crate) fn is_reserved_port(port: u16) -> bool {
    RESERVED_PORTS.contains(&port)
}

/// 计算实例内部 upstream 端口（全代码库唯一端口计算点，DESIGN §25.8）。
pub fn instance_port(id: InstanceId) -> Result<InternalProxyPort, InstanceIdError> {
    let base = FIRST_WARP_PORT as i64;
    let candidate = base
        .checked_add(id.as_i64())
        .ok_or(InstanceIdError::OutOfRange(id.as_i64()))?;

    if candidate > u16::MAX as i64 {
        return Err(InstanceIdError::OutOfRange(id.as_i64()));
    }

    let port = candidate as u16;
    if is_reserved_port(port) {
        return Err(InstanceIdError::Reserved(id.as_i64()));
    }

    Ok(InternalProxyPort(port))
}

impl InternalProxyPort {
    /// 底层 u16（监听器绑定、配置下发等）。
    pub fn as_u16(self) -> u16 {
        self.0
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for InternalProxyPort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_db_rejects_negative_ids() {
        assert!(matches!(
            InstanceId::from_db(-1),
            Err(InstanceIdError::Negative(-1))
        ));
        assert!(InstanceId::from_db(0).is_ok());
    }

    #[test]
    fn instance_port_starts_at_first_warp_port() {
        let id = InstanceId::from_db(0).unwrap();
        assert_eq!(instance_port(id).unwrap().as_u16(), FIRST_WARP_PORT);
    }

    #[test]
    fn instance_port_scales_with_id() {
        let id = InstanceId::from_db(7).unwrap();
        assert_eq!(instance_port(id).unwrap().as_u16(), FIRST_WARP_PORT + 7);
    }

    #[test]
    fn instance_port_max_valid_id_is_u16_max() {
        let max_id = (u16::MAX - FIRST_WARP_PORT) as i64;
        let id = InstanceId::from_db(max_id).unwrap();
        assert_eq!(instance_port(id).unwrap().as_u16(), u16::MAX);
    }

    #[test]
    fn instance_port_rejects_overflow() {
        let over = (u16::MAX - FIRST_WARP_PORT + 1) as i64;
        let id = InstanceId::from_db(over).unwrap();
        assert!(matches!(
            instance_port(id),
            Err(InstanceIdError::OutOfRange(v)) if v == over
        ));
    }

    #[test]
    fn warp_port_range_never_overlaps_reserved_ports() {
        let base = FIRST_WARP_PORT as i64;
        for id in 0..=(u16::MAX as i64 - base) {
            let port = instance_port(InstanceId::from_db(id).unwrap())
                .unwrap()
                .as_u16();
            assert!(
                !RESERVED_PORTS.contains(&port),
                "port {port} collides with reserved range"
            );
        }
    }

    #[test]
    fn reserved_port_detection_covers_all_proxy_listeners() {
        // 正常实例 id 永远到不了保留区（40000+ 起步），
        // 因此直接验证检测函数本身（防御性路径）。
        for reserved in RESERVED_PORTS {
            assert!(is_reserved_port(reserved), "{reserved} should be reserved");
        }
        assert!(!is_reserved_port(FIRST_WARP_PORT));
        assert!(!is_reserved_port(u16::MAX));
    }
}
