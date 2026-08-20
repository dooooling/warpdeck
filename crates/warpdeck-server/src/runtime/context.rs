//! 单实例执行上下文（P2-003 起所有 warp-svc / warp-cli 调用的载体）。

use std::path::Path;

use super::instance::{instance_port, InstanceId, InstanceIdError, InternalProxyPort};
use super::paths::InstancePaths;

/// 一个实例的完整执行上下文：标识 + 文件系统归属 + 内部端口。
/// 由 `InstanceContext::new` 一次性构造，模块间传递整对象而非散字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceContext {
    pub id: InstanceId,
    pub paths: InstancePaths,
    pub internal_proxy_port: InternalProxyPort,
}

impl InstanceContext {
    /// `data_dir` / `runtime_base` 来自 `AppConfig`。
    pub fn new(
        data_dir: &Path,
        runtime_base: &Path,
        id: InstanceId,
    ) -> Result<Self, InstanceIdError> {
        Ok(Self {
            id,
            paths: InstancePaths::new(data_dir, runtime_base, id),
            internal_proxy_port: instance_port(id)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn context_carries_consistent_id_port_and_paths() {
        let ctx = InstanceContext::new(
            Path::new("/var/lib/warpdeck"),
            Path::new("/run/warpdeck"),
            InstanceId::from_db(4).unwrap(),
        )
        .unwrap();

        assert_eq!(ctx.id.as_i64(), 4);
        assert_eq!(ctx.internal_proxy_port.as_u16(), 40004);
        assert!(ctx.paths.state_dir.ends_with("instances/4/state"));
    }

    #[test]
    fn oversized_id_propagates_port_error() {
        // 负数构造在 `InstanceId::from_db` 已被拒绝；这里验证超出端口容量
        // 的 id（如未来 schema 放宽）会在上下文构造时被拒绝。
        let big = InstanceId::from_db(30_000).unwrap();
        let err = InstanceContext::new(
            Path::new("/var/lib/warpdeck"),
            Path::new("/run/warpdeck"),
            big,
        );
        assert!(matches!(err, Err(InstanceIdError::OutOfRange(30_000))));
    }
}
