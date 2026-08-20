//! WARP 实例运行时域模型与生命周期（Phase 2-5）。
//!
//! 本模块承载单实例 WARP Runtime 的类型与抽象：
//! InstanceId / InternalProxyPort（P2-001）、InstancePaths（P2-002）、
//! WarpControl / ProcessSpawner traits 与 Fake 实现（P2-003/004），
//! 以及后续的 D-Bus runtime、warp-svc / warp-cli 适配器。

pub mod backoff;
pub mod clock;
pub mod context;
pub mod control;
pub mod crash;
pub mod credentials;
pub mod dbus;
pub mod events;
pub mod fake;
pub mod flow;
pub mod health;
pub mod health_monitor;
pub mod instance;
pub mod log_tail;
pub mod logs;
pub mod manager;
pub mod mdm;
pub mod paths;
pub mod probe;
pub mod process;
pub mod readiness;
pub mod registry;
pub mod service;
pub mod stop;
pub mod warp_cli;

pub use manager::{InstanceManager, WarpRuntime};
