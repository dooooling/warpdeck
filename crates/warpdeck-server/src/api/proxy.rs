//! 代理配置端点（P7-008；P8 接 secret store 密码）。
//!
//! 设计：`GET` 不返回任何 secret（`proxy_username` 明文不出现，只给
//! `auth_configured` 布尔 = secret store 中是否有密码）；`PUT` 部分更新
//! （None = 保持原值；`password: ""` = 清除；非空 = 设置/轮换），写期望
//! 配置后触发 reconciler 走 GOST apply 链路。listener 端口（11080/18080）
//! 不可改（AGENTS.md：host 端口映射归 Compose `.env`）。
//!
//! auth_enabled=true 且无密码时拒绝（P8 起不再允许"悬空"配置）。

use axum::extract::State;
use axum::Json;

use crate::api::dto::{ProxyConfigView, UpdateProxyRequest};
use crate::api::error::{invalid_json_body, repo_error, ApiError};
use crate::api::middleware::AuthUser;
use crate::api::{ApiResult, ApiState};
use crate::crypto::secret_store::SecretKind;
use crate::observability::RequestId;

/// `GET /api/v1/proxy`：当前期望代理配置（无 secret）。
pub async fn get(
    State(state): State<ApiState>,
    RequestId(request_id): RequestId,
    user: AuthUser,
) -> ApiResult<Json<ProxyConfigView>> {
    let cfg = state
        .proxy
        .get()
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    let password_present = state
        .secrets
        .exists(SecretKind::ProxyPassword)
        .await
        .map_err(secret_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    let _ = user;
    // P1 审查 #4：附上 GOST 实际状态（desired ≠ actual 必须可见）。
    let actual = state
        .proxy_applier
        .status()
        .await
        .map(|s| crate::api::dto::ProxyActualView::from_status(&s));
    Ok(Json(
        ProxyConfigView::from_config(&cfg, password_present).with_actual(actual),
    ))
}

/// `PUT /api/v1/proxy`：部分更新期望配置并触发收敛。返回更新后视图。
pub async fn update(
    State(state): State<ApiState>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
    body: Result<Json<UpdateProxyRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<ProxyConfigView>> {
    let Json(req) = body
        .map_err(|_| invalid_json_body())
        .map_err(|e| e.into_response_with(&request_id))?;
    req.validate()
        .map_err(ApiError::Validation)
        .map_err(|e| e.into_response_with(&request_id))?;
    let current = state
        .proxy
        .get()
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(&request_id))?;

    // 先算目标状态与目标密码存在性（只读），校验通过后才落盘，
    // 避免「auth_enabled 无密码 → 422」却已经把原密码删掉的部分更新。
    let updated = req.apply(&current);
    let password_present = match &req.password {
        None => state
            .secrets
            .exists(SecretKind::ProxyPassword)
            .await
            .map_err(secret_error)
            .map_err(|e| e.into_response_with(&request_id))?,
        Some(v) => !v.is_empty(),
    };
    if updated.auth_enabled && !password_present {
        return Err(ApiError::Validation(
            "auth_enabled requires a proxy password (set `password`)".to_string(),
        )
        .into_response_with(&request_id));
    }

    // 校验通过后：**密码与配置行同一事务生效**（P1 审查 R3#4）——
    // 旧实现先写 secret 再更新配置，中途失败会留下「密码已换/配置仍旧」。
    let password_for_tx = req.password.as_deref();
    state
        .consistency
        .update_proxy_with_password(&updated, password_for_tx)
        .await
        .map_err(consistency_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    state.notify_change();
    // P1 审查 #3：PUT 返回的视图同样携带 GOST 实际状态——apply 尚未发生时
    // actual 反映旧状态，UI 据此展示「配置已保存，等待收敛」，不伪装成功。
    let actual = state
        .proxy_applier
        .status()
        .await
        .map(|s| crate::api::dto::ProxyActualView::from_status(&s));
    Ok(Json(
        ProxyConfigView::from_config(&updated, password_present).with_actual(actual),
    ))
}

fn consistency_error(e: crate::db::uow::ConsistencyError) -> ApiError {
    ApiError::Internal(e.to_string())
}

fn secret_error(e: crate::crypto::secret_store::SecretStoreError) -> ApiError {
    ApiError::Internal(e.to_string())
}
