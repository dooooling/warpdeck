//! `/api/v1/health` liveness probe（P1-008）。
//!
//! 设计约束：必须立即返回、不得检查真实 WARP / 网关数据面，不访问数据库。

use axum::Json;
use serde_json::{json, Value};

/// 应用版本（P12-012）：优先 `WARPDECK_VERSION`（镜像注入），其次 Cargo.toml 版本。
pub(crate) fn app_version() -> String {
    crate::version::app_version()
}

pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "version": app_version() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_returns_ok_and_version() {
        let body = health().await;
        assert!(body.0.as_object().is_some());
        assert_eq!(body.0["status"], "ok");
        assert_eq!(body.0["version"], env!("CARGO_PKG_VERSION"));
    }
}
