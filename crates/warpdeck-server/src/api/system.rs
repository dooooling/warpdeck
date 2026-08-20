//! 系统端点（P7-003）。
//! `account` 端点已由 P8 `api::account` 替代（真实 secret 语义）。

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::api::dto::{InstanceCountsView, SystemStatusView};
use crate::api::middleware::AuthUser;
use crate::api::{ApiResult, ApiState};
use crate::observability::RequestId;

/// `GET /api/v1/system/status`（P7-003）：进程存活 + 实例统计。
pub async fn status(
    State(state): State<ApiState>,
    RequestId(_rid): RequestId,
    _user: AuthUser,
) -> ApiResult<Json<SystemStatusView>> {
    let snapshots = state.registry.list();
    let counts = InstanceCountsView::from_runtime(&snapshots);
    Ok(Json(SystemStatusView {
        status: "ok",
        version: state.version.clone(),
        uptime_secs: state.started_at.elapsed().as_secs(),
        instances: counts,
    }))
}

/// `GET /api/v1/system/version`（P7-003）：版本信息。
pub async fn version(
    State(state): State<ApiState>,
    RequestId(_rid): RequestId,
    _user: AuthUser,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "version": state.version })))
}
