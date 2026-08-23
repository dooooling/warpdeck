//! 系统端点（P7-003）。
//! `account` 端点已由 P8 `api::account` 替代（真实 secret 语义）。

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::api::dto::{
    InstanceCountsView, LastApplyErrorView, SystemComponentsView, SystemStatusView,
};
use crate::api::middleware::AuthUser;
use crate::api::{ApiResult, ApiState};
use crate::crypto::secret_store::SecretKind;
use crate::observability::RequestId;

/// `GET /api/v1/system/status`（P7-003）：进程存活 + 实例统计 + 组件
/// operational 状态（P1 审查 #4：GOST/secret store 的真实健康，而非恒 ok）。
pub async fn status(
    State(state): State<ApiState>,
    RequestId(_rid): RequestId,
    _user: AuthUser,
) -> ApiResult<Json<SystemStatusView>> {
    let snapshots = state.registry.list();
    let counts = InstanceCountsView::from_runtime(&snapshots);

    // GOST actual（P1 审查 #4）。
    let gost_view = match state.proxy_applier.status().await {
        Some(s) => crate::api::dto::ProxyActualView::from_status(&s),
        None => crate::api::dto::ProxyActualView {
            status: "unknown".into(),
            pid: None,
            exit_code: None,
            reason: None,
        },
    };
    // secret store 读探针：能回答「密码是否存在」即视为可用。
    let secret_store = match state.secrets.exists(SecretKind::ProxyPassword).await {
        Ok(_) => "ok",
        Err(_) => "unavailable",
    };

    // P1 审查 R3 次要项：区分进程 liveness 与系统 readiness——存在数据面
    // failed/degraded、密钥不可用、未应用的配置错误时降级为 "degraded"；
    // stopped/unknown 且无错误属正常（如双 listener 全关或测试桩）。
    let has_problem = matches!(gost_view.status.as_str(), "failed" | "degraded")
        || secret_store != "ok"
        || state.apply_error.lock().unwrap().is_some();
    let readiness = if has_problem { "degraded" } else { "ok" };

    Ok(Json(SystemStatusView {
        status: readiness,
        version: state.version.clone(),
        uptime_secs: state.started_at.elapsed().as_secs(),
        instances: counts,
        components: SystemComponentsView {
            gost: gost_view.status,
            gost_reason: gost_view.reason,
            secret_store,
        },
        last_apply_error: LastApplyErrorView::from_slot(&state.apply_error),
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
