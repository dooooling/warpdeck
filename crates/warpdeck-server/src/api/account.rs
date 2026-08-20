//! 账号配置端点（P8-009/010；替代 P7 骨架）。
//!
//! 秘密边界（AGENTS.md / DESIGN §15.3/§20.6）：
//! - GET 只返回 mask 状态（configured/license_present/zero_trust_configured），
//!   永不返回明文；Zero Trust org 非 secret 可回显；
//! - PUT 语义：字段缺失 = 保持现有；空串 = 清除；非空 = 设置/轮换。
//! - 校验：warp_plus 必须有 license；zero_trust 必须 org + client id + secret；
//!   mode 互斥（单一字段本身保证）。

use axum::extract::State;
use axum::Json;
use serde::Deserialize;

use crate::api::dto::AccountView;
use crate::api::error::{invalid_json_body, ApiError};
use crate::api::middleware::AuthUser;
use crate::api::{ApiResult, ApiState};
use crate::crypto::secret_store::{SecretKind, SecretStore, SecretStoreError};
use crate::db::account::{AccountMode, AccountRepoError};
use crate::observability::RequestId;

/// `GET /api/v1/account`：mask 后的账号状态（无明文）。
pub async fn get(
    State(state): State<ApiState>,
    RequestId(request_id): RequestId,
    user: AuthUser,
) -> ApiResult<Json<AccountView>> {
    let cfg = state
        .account
        .get()
        .await
        .map_err(account_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    let license = state
        .secrets
        .exists(SecretKind::WarpPlusLicense)
        .await
        .map_err(secret_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    let zt_id = state
        .secrets
        .exists(SecretKind::ZeroTrustClientId)
        .await
        .map_err(secret_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    let zt_secret = state
        .secrets
        .exists(SecretKind::ZeroTrustClientSecret)
        .await
        .map_err(secret_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    let _ = user;
    Ok(Json(AccountView {
        mode: cfg.mode.as_str().to_string(),
        configured: license || zt_id || zt_secret,
        license_present: license,
        zero_trust_configured: zt_id && zt_secret,
        zero_trust_org: cfg.zero_trust_org,
    }))
}

/// 账号更新请求（Option 语义：None = 保持，"" = 清除，非空 = 设置）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateAccountRequest {
    pub mode: Option<String>,
    pub license: Option<String>,
    pub zero_trust_org: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
}

/// 执行一次秘密写入；`None` = 保持，`Some("")` = 清除，`Some(v)` = 设置。
async fn apply_secret(
    store: &dyn SecretStore,
    kind: SecretKind,
    value: &Option<String>,
) -> Result<(), SecretStoreError> {
    match value {
        None => Ok(()),
        Some(v) if v.is_empty() => store.delete(kind).await,
        Some(v) => store.set(kind, v).await,
    }
}

/// `PUT /api/v1/account`：更新模式与凭据。
pub async fn update(
    State(state): State<ApiState>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
    body: Result<Json<UpdateAccountRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<AccountView>> {
    let Json(req) = body
        .map_err(|_| invalid_json_body())
        .map_err(|e| e.into_response_with(&request_id))?;

    // 先求目标模式（None = 保持现有）。
    let current = state
        .account
        .get()
        .await
        .map_err(account_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    let mode = match &req.mode {
        Some(m) => AccountMode::parse(m)
            .map_err(ApiError::Validation)
            .map_err(|e| e.into_response_with(&request_id))?,
        None => current.mode,
    };

    // 目标凭据存在性（只读预测，不落盘）：用于先校验后写入，
    // 避免「校验失败但 secret 已被删除/写入」的部分更新。
    let has_license = state
        .secrets
        .exists(SecretKind::WarpPlusLicense)
        .await
        .map_err(secret_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    let has_zt_id = state
        .secrets
        .exists(SecretKind::ZeroTrustClientId)
        .await
        .map_err(secret_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    let has_zt_secret = state
        .secrets
        .exists(SecretKind::ZeroTrustClientSecret)
        .await
        .map_err(secret_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    let license_present = secret_would_present(&req.license, has_license);
    let zt_id = secret_would_present(&req.client_id, has_zt_id);
    let zt_secret = secret_would_present(&req.client_secret, has_zt_secret);
    let org = req
        .zero_trust_org
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or(current.zero_trust_org.clone());
    match mode {
        AccountMode::Free => {}
        AccountMode::WarpPlus if license_present => {}
        AccountMode::WarpPlus => {
            return Err(
                ApiError::Validation("warp_plus mode requires a license".to_string())
                    .into_response_with(&request_id),
            );
        }
        AccountMode::ZeroTrust if zt_id && zt_secret && org.is_some() => {}
        AccountMode::ZeroTrust => {
            return Err(ApiError::Validation(
                "zero_trust mode requires org, client id and client secret".to_string(),
            )
            .into_response_with(&request_id));
        }
    }

    // 校验通过后才落盘（失败不得留下部分更新）。
    apply_secret(&*state.secrets, SecretKind::WarpPlusLicense, &req.license)
        .await
        .map_err(secret_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    apply_secret(
        &*state.secrets,
        SecretKind::ZeroTrustClientId,
        &req.client_id,
    )
    .await
    .map_err(secret_error)
    .map_err(|e| e.into_response_with(&request_id))?;
    apply_secret(
        &*state.secrets,
        SecretKind::ZeroTrustClientSecret,
        &req.client_secret,
    )
    .await
    .map_err(secret_error)
    .map_err(|e| e.into_response_with(&request_id))?;
    state
        .account
        .set_mode(mode, org.clone())
        .await
        .map_err(account_error)
        .map_err(|e| e.into_response_with(&request_id))?;

    let configured = license_present || zt_id || zt_secret;
    Ok(Json(AccountView {
        mode: mode.as_str().to_string(),
        configured,
        license_present,
        zero_trust_configured: zt_id && zt_secret,
        zero_trust_org: org,
    }))
}

fn account_error(e: AccountRepoError) -> ApiError {
    ApiError::Internal(e.to_string())
}

/// `apply_secret` 的只读预测：字段提交后该 secret 是否存在（用于先校验后写入）。
fn secret_would_present(req: &Option<String>, current: bool) -> bool {
    match req {
        None => current,
        Some(v) if v.is_empty() => false,
        Some(_) => true,
    }
}

fn secret_error(e: SecretStoreError) -> ApiError {
    ApiError::Internal(e.to_string())
}
