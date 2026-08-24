//! warpdeck-server 库 crate。
//!
//! `main.rs` 仅为薄入口；业务模块全部在此，供单元测试与集成测试（`tests/`）复用。

pub mod api;
pub mod app;
pub mod auth;
pub mod config;
pub mod crypto;
pub mod db;
pub mod gateway;
pub mod net;
pub mod observability;
pub mod reconciler;
pub mod runtime;
pub mod shutdown;
pub mod version;
