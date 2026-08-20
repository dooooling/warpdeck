//! 实例路径/环境变量的唯一生成点（P2-002）。
//!
//! 设计依据：
//! - DESIGN §8.1：`{data_dir}/instances/{id}/state`、`{data_dir}/logs/instance-{id}.log`。
//! - DESIGN §8.1 / §11.3：`{runtime_dir}/instances/{id}/warp`、`{runtime_dir}/instances/{id}/dbus/`。
//! - 任何模块不得手工字符串拼接路径；`DBUS_SYSTEM_BUS_ADDRESS` 也统一从这里取。

use std::path::{Path, PathBuf};

use super::instance::InstanceId;

/// 某实例的全部文件系统归属（state 侧持久 + runtime 侧临时）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstancePaths {
    /// warp-svc 持久状态目录（`STATE_DIRECTORY`）。
    pub state_dir: PathBuf,
    /// 实例日志文件（`{data_dir}/logs/instance-{id}.log`，§8.1）。
    pub log_path: PathBuf,
    /// warp-svc 运行时工作目录（`RUNTIME_DIRECTORY`）。
    pub runtime_dir: PathBuf,
    /// 实例独立 D-Bus daemon 的工作目录。
    pub dbus_dir: PathBuf,
    /// 实例独立 D-Bus system bus socket 路径。
    pub dbus_socket: PathBuf,
}

impl InstancePaths {
    /// `data_dir` = `AppConfig::data_dir`（state 根），
    /// `runtime_base` = `AppConfig::runtime_dir`（临时根，容器内 `/run/warpdeck`）。
    pub fn new(data_dir: &Path, runtime_base: &Path, id: InstanceId) -> Self {
        let state_root = data_dir.join("instances").join(id.to_string());
        let runtime_root = runtime_base.join("instances").join(id.to_string());
        Self {
            state_dir: state_root.join("state"),
            log_path: data_dir.join("logs").join(format!("instance-{id}.log")),
            runtime_dir: runtime_root.join("warp"),
            dbus_dir: runtime_root.join("dbus"),
            dbus_socket: runtime_root.join("dbus").join("system_bus_socket"),
        }
    }

    /// `DBUS_SYSTEM_BUS_ADDRESS` 环境变量值（§11.3）：`unix:path=<socket>`。
    /// 恒输出 POSIX 路径（D-Bus 地址协议固定用 `/`），与宿主平台无关。
    pub fn dbus_system_bus_address(&self) -> String {
        let raw = self.dbus_socket.display().to_string();
        format!("unix:path={}", raw.replace('\\', "/"))
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn paths_for(id: i64) -> InstancePaths {
        InstancePaths::new(
            Path::new("/var/lib/warpdeck"),
            Path::new("/run/warpdeck"),
            InstanceId::from_db(id).unwrap(),
        )
    }

    #[test]
    fn state_and_runtime_paths_match_design_layout() {
        let p = paths_for(0);
        assert_eq!(
            p.state_dir,
            PathBuf::from("/var/lib/warpdeck/instances/0/state")
        );
        assert_eq!(
            p.log_path,
            PathBuf::from("/var/lib/warpdeck/logs/instance-0.log")
        );
        assert_eq!(
            p.runtime_dir,
            PathBuf::from("/run/warpdeck/instances/0/warp")
        );
        assert_eq!(p.dbus_dir, PathBuf::from("/run/warpdeck/instances/0/dbus"));
        assert_eq!(
            p.dbus_socket,
            PathBuf::from("/run/warpdeck/instances/0/dbus/system_bus_socket")
        );
    }

    #[test]
    fn different_ids_are_fully_isolated() {
        let a = paths_for(1);
        let b = paths_for(2);
        for field_a in [
            &a.state_dir,
            &a.log_path,
            &a.runtime_dir,
            &a.dbus_dir,
            &a.dbus_socket,
        ] {
            for field_b in [
                &b.state_dir,
                &b.log_path,
                &b.runtime_dir,
                &b.dbus_dir,
                &b.dbus_socket,
            ] {
                assert_ne!(field_a, field_b, "instances must not share paths");
            }
        }
    }

    #[test]
    fn dbus_address_matches_design_env_format() {
        let p = paths_for(3);
        assert_eq!(
            p.dbus_system_bus_address(),
            "unix:path=/run/warpdeck/instances/3/dbus/system_bus_socket"
        );
    }
}
