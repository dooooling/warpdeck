//! 应用版本解析（P12-012）。
//!
//! 优先级：`WARPDECK_VERSION` 环境变量（release 镜像由 cargo xtask release 注入
//! `0.1.0-<git sha>`）→ 编译期 `CARGO_PKG_VERSION`。
//! 同一来源供 `/api/v1/health`、`/api/v1/system/version` 与启动日志使用。

pub fn app_version() -> String {
    std::env::var("WARPDECK_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}
