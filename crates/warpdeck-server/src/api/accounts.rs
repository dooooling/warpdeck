//! 账号档案端点（v0.2 §17.6；PLAN §27.2 任务 D）。
//!
//! 秘密边界（AGENTS.md / §17.6）：
//! - GET 只返回 `*_configured` mask，**永不回显明文**；
//! - secret 提交即入 `secrets` 密文库（profile_id 维度），API 响应不含；
//! - 校验先于写入（先预测目标状态，失败不得留部分更新）；
//! - 凭据/模式变更 → 绑定实例 mark dirty（restart_pending），
//!   Reconciler 按序重启；失败上浮，不静默成功（§16.9）。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use crate::api::dto::{AccountProfileView, AccountProfileWriteRequest};
use crate::api::error::{invalid_json_body, repo_error, ApiError};
use crate::api::middleware::AuthUser;
use crate::api::{ApiResult, ApiState};
use crate::crypto::secret_store::SecretKind;
use crate::db::account::AccountMode;
use crate::db::profiles::{AccountProfile, AccountProfileError};
use crate::observability::RequestId;

/// `GET /api/v1/accounts`：全部档案（含默认档；mask 凭据状态 + 绑定实例数）。
pub async fn list(
    State(state): State<ApiState>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
) -> ApiResult<Json<Vec<AccountProfileView>>> {
    let profiles = state
        .profiles
        .list()
        .await
        .map_err(profile_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    let mut views = Vec::with_capacity(profiles.len());
    for p in &profiles {
        views.push(build_view(&state, p).await);
    }
    Ok(Json(views))
}

/// `GET /api/v1/accounts/{id}`。
pub async fn get(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
) -> ApiResult<Json<AccountProfileView>> {
    let profile = state
        .profiles
        .get(id)
        .await
        .map_err(profile_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    Ok(Json(build_view(&state, &profile).await))
}

/// `POST /api/v1/accounts`：创建档案（name 唯一；凭据按 mode 必填）。
pub async fn create(
    State(state): State<ApiState>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
    body: Result<Json<AccountProfileWriteRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<(StatusCode, Json<AccountProfileView>)> {
    let Json(req) = body
        .map_err(|_| invalid_json_body())
        .map_err(|e| e.into_response_with(&request_id))?;
    let name = req
        .name
        .as_deref()
        .map(validate_name)
        .transpose()
        .map_err(ApiError::Validation)
        .map_err(|e| e.into_response_with(&request_id))?
        .ok_or_else(|| ApiError::Validation("name is required".to_string()))
        .map_err(|e| e.into_response_with(&request_id))?;
    let mode = req
        .mode
        .as_deref()
        .map(AccountMode::parse)
        .transpose()
        .map_err(ApiError::Validation)
        .map_err(|e| e.into_response_with(&request_id))?
        .ok_or_else(|| ApiError::Validation("mode is required".to_string()))
        .map_err(|e| e.into_response_with(&request_id))?;

    // 先校验凭据（不落盘）。
    let org = clean_org(req.zero_trust_org.as_deref());
    validate_credentials(mode, &req, org.as_deref())
        .map_err(ApiError::Validation)
        .map_err(|e| e.into_response_with(&request_id))?;

    // 落盘：**档案行 + 全部凭据同一事务**（P1 审查 R3#4）——
    // 旧实现先建行再写 secret、失败后补偿删除且补偿本身可能吞错。
    let creds = [
        ("license", SecretKind::WarpPlusLicense, req.license.clone()),
        (
            "client_id",
            SecretKind::ZeroTrustClientId,
            req.client_id.clone(),
        ),
        (
            "client_secret",
            SecretKind::ZeroTrustClientSecret,
            req.client_secret.clone(),
        ),
    ];
    let new_id = state
        .consistency
        .create_profile_with_credentials(&name, mode.as_str(), org.as_deref(), creds)
        .await
        .map_err(consistency_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    let profile = state
        .profiles
        .get(new_id)
        .await
        .map_err(profile_error)
        .map_err(|e| e.into_response_with(&request_id))?;

    let view = build_view(&state, &profile).await;
    Ok((StatusCode::CREATED, Json(view)))
}

/// `PATCH /api/v1/accounts/{id}`：部分更新（mode/org/凭据；name 可改）。
/// 凭据/模式变更自动标记绑定实例重启（无需调用方额外操作）。
pub async fn update(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
    body: Result<Json<AccountProfileWriteRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<AccountProfileView>> {
    let Json(req) = body
        .map_err(|_| invalid_json_body())
        .map_err(|e| e.into_response_with(&request_id))?;

    let before = state
        .profiles
        .get(id)
        .await
        .map_err(profile_error)
        .map_err(|e| e.into_response_with(&request_id))?;

    // §16.9 只读防线（先于任何语义计算，给出确定的消息）：
    // - free 档全局唯一且只读——名称/模式/凭据均不可改，只能留作系统保留资源；
    // - 被实例引用的档案只读——必须先解绑其实例（rebind 到 NULL/其他档）才能编辑。
    if before.mode == AccountMode::Free {
        return Err(ApiError::Conflict(
            "the free profile is read-only: it is the globally-unique built-in default".into(),
        )
        .into_response_with(&request_id));
    }
    let bound = state
        .instances
        .count_bound_to_profile(id)
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    if bound > 0 {
        return Err(ApiError::Conflict(format!(
            "account profile {id} is bound to {bound} instance(s); unbind them before editing"
        ))
        .into_response_with(&request_id));
    }

    // 目标模式：None = 保持现有。
    let mode = match req.mode.as_deref() {
        Some(m) => AccountMode::parse(m)
            .map_err(ApiError::Validation)
            .map_err(|e| e.into_response_with(&request_id))?,
        None => before.mode,
    };
    let name = match &req.name {
        Some(n) => validate_name(n)
            .map_err(ApiError::Validation)
            .map_err(|e| e.into_response_with(&request_id))?,
        None => before.name.clone(),
    };
    let org = match &req.zero_trust_org {
        Some(v) => clean_org(Some(v)),
        None => before.zero_trust_org.clone(),
    };

    // 目标凭据存在性预测（先校验后写入，避免部分更新）。
    let has_license = exists_profile(&state, SecretKind::WarpPlusLicense, id).await;
    let has_zt_id = exists_profile(&state, SecretKind::ZeroTrustClientId, id).await;
    let has_zt_secret = exists_profile(&state, SecretKind::ZeroTrustClientSecret, id).await;
    let license_present = secret_would_present(&req.license, has_license);
    let zt_id = secret_would_present(&req.client_id, has_zt_id);
    let zt_secret = secret_would_present(&req.client_secret, has_zt_secret);

    let effective = AccountProfileWriteRequest {
        name: Some(name.clone()),
        mode: Some(mode.as_str().to_string()),
        zero_trust_org: org.clone(),
        license: if license_present {
            Some("x".into())
        } else {
            Some(String::new())
        },
        client_id: if zt_id {
            Some("x".into())
        } else {
            Some(String::new())
        },
        client_secret: if zt_secret {
            Some("x".into())
        } else {
            Some(String::new())
        },
    };
    validate_credentials(mode, &effective, org.as_deref())
        .map_err(ApiError::Validation)
        .map_err(|e| e.into_response_with(&request_id))?;

    // 校验通过后落盘：**profile 元数据 + 三个 secret + 重启标记同一事务**
    // （P1 审查 R3#4：杜绝部分成功的混合凭据态；标记失败不再可能静默，
    // 因为它就在事务里——失败即整体回滚并返回 500）。
    let creds = [
        ("license", SecretKind::WarpPlusLicense, req.license.clone()),
        (
            "client_id",
            SecretKind::ZeroTrustClientId,
            req.client_id.clone(),
        ),
        (
            "client_secret",
            SecretKind::ZeroTrustClientSecret,
            req.client_secret.clone(),
        ),
    ];
    state
        .consistency
        .update_profile_with_credentials(id, &name, mode.as_str(), org.as_deref(), creds)
        .await
        .map_err(consistency_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    state.notify_change();

    let updated = state
        .profiles
        .get(id)
        .await
        .map_err(profile_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    Ok(Json(build_view(&state, &updated).await))
}

/// `DELETE /api/v1/accounts/{id}`：删除档案。
/// 保护（409）：内置默认档；仍被任一 enabled 实例引用（repo 层转 Conflict）。
pub async fn delete(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
) -> ApiResult<StatusCode> {
    state
        .profiles
        .delete(id)
        .await
        .map_err(profile_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    state.notify_change();
    Ok(StatusCode::NO_CONTENT)
}

// ---------- helpers ----------

fn validate_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("name must not be empty".to_string());
    }
    if n.chars().count() > 64 {
        return Err("name must be at most 64 characters".to_string());
    }
    Ok(n.to_string())
}

fn clean_org(v: Option<&str>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 目标模式所需的凭据校验（DESIGN §16.9）：
/// free 无要求；warp_plus 必须 license；zero_trust 必须 org + client id + secret。
fn validate_credentials(
    mode: AccountMode,
    req: &AccountProfileWriteRequest,
    org: Option<&str>,
) -> Result<(), String> {
    match mode {
        AccountMode::Free => Ok(()),
        AccountMode::WarpPlus if secret_would_present(&req.license, false) => Ok(()),
        AccountMode::WarpPlus => Err("warp_plus mode requires a license".to_string()),
        AccountMode::ZeroTrust
            if secret_would_present(&req.client_id, false)
                && secret_would_present(&req.client_secret, false)
                && org.is_some() =>
        {
            Ok(())
        }
        AccountMode::ZeroTrust => {
            Err("zero_trust mode requires org, client id and client secret".to_string())
        }
    }
}

/// 提交后该 secret 是否存在（预测；None = 不涉及，Other 按请求值定）。
fn secret_would_present(value: &Option<String>, current: bool) -> bool {
    match value {
        None => current,
        Some(v) if v.is_empty() => false,
        Some(_) => true,
    }
}

/// 档案级 secret 写入：None = 保持；Some("") = 清除；Some(v) = 设置/轮换。
async fn exists_profile(state: &ApiState, kind: SecretKind, profile_id: i64) -> bool {
    state
        .secrets
        .exists_for_profile(kind, profile_id)
        .await
        .unwrap_or(false)
}

/// 组装档案视图（mask + 绑定实例数；N 小，逐档查询可接受）。
async fn build_view(state: &ApiState, profile: &AccountProfile) -> AccountProfileView {
    let license = exists_profile(state, SecretKind::WarpPlusLicense, profile.id).await;
    let zt_id = exists_profile(state, SecretKind::ZeroTrustClientId, profile.id).await;
    let zt_secret = exists_profile(state, SecretKind::ZeroTrustClientSecret, profile.id).await;
    let count = state
        .instances
        .list()
        .await
        .map(|specs| {
            specs
                .iter()
                .filter(|s| s.account_profile_id.unwrap_or(1) == profile.id)
                .count()
        })
        .unwrap_or(0);
    AccountProfileView::from_parts(profile, license, zt_id, zt_secret, count)
}

fn profile_error(e: AccountProfileError) -> ApiError {
    match e {
        AccountProfileError::NotFound(id) => {
            ApiError::NotFound(format!("account profile {id} not found"))
        }
        AccountProfileError::Conflict(message) => ApiError::Conflict(message),
        AccountProfileError::Db(message) => ApiError::Internal(message),
    }
}

fn consistency_error(e: crate::db::uow::ConsistencyError) -> ApiError {
    use crate::db::uow::ConsistencyError as E;
    match &e {
        E::FreeProfileConflict => {
            ApiError::Conflict("free profile is unique and reserved".to_string())
        }
        _ => ApiError::Internal(e.to_string()),
    }
}
