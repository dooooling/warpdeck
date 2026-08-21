//! 实例 CRUD 与动作端点（P7-004/005/006/007）。
//!
//! 语义（DESIGN §12.1 / AGENTS.md「HTTP handlers only mutate desired state
//! and notify」）：
//! - start/stop = 期望状态变更 + 触发 reconciler（幂等，不直接碰进程）；
//! - restart = 运行时意图（WarpRuntime::restart），不改变最终 Desired State
//!   （DEVELOPMENT_PLAN §12.2 P7-006）；
//! - delete = 危险操作：先停止进程（runtime.delete，保留注册数据），再删期望行。
//!   不提供 `preserve_registration` 参数：MVP 取最安全默认（保留注册，P7-007）。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use crate::api::dto::{CreateInstanceRequest, InstanceView, PatchInstanceRequest};
use crate::api::error::{invalid_json_body, repo_error, ApiError, ErrorResponse};
use crate::api::middleware::AuthUser;
use crate::api::{ApiResult, ApiState};
use crate::db::account::AccountMode;
use crate::db::profiles::AccountProfile;
use crate::db::repo::DesiredState;
use crate::observability::RequestId;

/// 将仓储错误映射为 API 错误。`NotFound` 语义由 handler 的 `ensure_exists`
/// 前置检查提供（repo UPDATE/DELETE 保持幂等，不因 0 行报错），实现见 `error::repo_error`。
/// 装载档案摘要所需的最小集合（一次查询，供视图批量关联）。
async fn load_profiles(
    state: &ApiState,
) -> Result<Vec<AccountProfile>, crate::db::profiles::AccountProfileError> {
    state.profiles.list().await
}

fn instance_view(
    state: &ApiState,
    profiles: &[AccountProfile],
    spec: &crate::db::repo::WarpInstanceSpec,
) -> InstanceView {
    InstanceView::from_parts(spec, state.registry.get(spec.id).as_ref())
        .with_account(profiles, spec.account_profile_id)
}

/// `GET /api/v1/instances`（P7-004）：期望记录 ∪ 实际状态，按 id 升序。
pub async fn list(
    State(state): State<ApiState>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
) -> ApiResult<Json<Vec<InstanceView>>> {
    let specs = state
        .instances
        .list()
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    let profiles = load_profiles(&state)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))
        .map_err(|e| e.into_response_with(&request_id))?;
    let views = specs
        .iter()
        .map(|s| instance_view(&state, &profiles, s))
        .collect();
    Ok(Json(views))
}

/// `GET /api/v1/instances/{id}`（P7-004）。
pub async fn get(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
) -> ApiResult<Json<InstanceView>> {
    let id = parse_id(id).map_err(|e| e.into_response_with(&request_id))?;
    let spec = state
        .instances
        .get(id)
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(&request_id))?
        .ok_or_else(|| ApiError::NotFound(format!("instance {} not found", id.as_i64())))
        .map_err(|e| e.into_response_with(&request_id))?;
    let profiles = load_profiles(&state)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))
        .map_err(|e| e.into_response_with(&request_id))?;
    Ok(Json(instance_view(&state, &profiles, &spec)))
}

/// `POST /api/v1/instances`（P7-005）：只写期望状态（enabled + desired=running），
/// reconciler 收敛实际状态。返回 201 + 合并视图。
///
/// body 解析失败（缺字段 / 非法 JSON）也走统一错误契约（422 VALIDATION，
/// review 补强：axum 默认 JsonRejection 响应体不满足 `error` 契约）。
pub async fn create(
    State(state): State<ApiState>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
    body: Result<Json<CreateInstanceRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<(StatusCode, Json<InstanceView>)> {
    let Json(req) = body
        .map_err(|_| invalid_json_body())
        .map_err(|e| e.into_response_with(&request_id))?;
    let name = req
        .validate()
        .map_err(ApiError::Validation)
        .map_err(|e| e.into_response_with(&request_id))?;
    // v0.2：绑定档案须存在（缺省/None = 默认 free 档）；WARP+ 单实例约束（§16.9）。
    if let Some(pid) = req.account_profile_id {
        let profile = state
            .profiles
            .get(pid)
            .await
            .map_err(|_| ApiError::Validation("account_profile_id does not exist".to_string()))
            .map_err(|e| e.into_response_with(&request_id))?;
        reject_warp_plus_rebind(&state, None, &profile, pid, &request_id).await?;
    }
    let spec = state
        .instances
        .create(&name, req.account_profile_id)
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    state.notify_change();
    let profiles = load_profiles(&state)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))
        .map_err(|e| e.into_response_with(&request_id))?;
    Ok((
        StatusCode::CREATED,
        Json(instance_view(&state, &profiles, &spec)),
    ))
}

/// `PATCH /api/v1/instances/{id}`（v0.2 §17.4）：改绑账号档案。
/// 语义：改绑在下次重启生效 — 后端置 `restart_pending`（Desired-state
/// 层表达），UI 提示将触发重启；此处只写期望绑定 + 触发收敛。
pub async fn update(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
    body: Result<Json<PatchInstanceRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<InstanceView>> {
    let id = parse_id(id).map_err(|e| e.into_response_with(&request_id))?;
    let current = state
        .instances
        .get(id)
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(&request_id))?
        .ok_or_else(|| {
            ApiError::NotFound(format!("instance {} not found", id.as_i64()))
                .into_response_with(&request_id)
        })?;
    let Json(req) = body
        .map_err(|_| invalid_json_body())
        .map_err(|e| e.into_response_with(&request_id))?;
    req.validate()
        .map_err(ApiError::Validation)
        .map_err(|e| e.into_response_with(&request_id))?;
    let new_profile = req.account_profile_id.expect("validated non-empty");
    if let Some(pid) = new_profile {
        let profile = state
            .profiles
            .get(pid)
            .await
            .map_err(|_| ApiError::Validation("account_profile_id does not exist".to_string()))
            .map_err(|e| e.into_response_with(&request_id))?;
        reject_warp_plus_rebind(
            &state,
            current.account_profile_id,
            &profile,
            pid,
            &request_id,
        )
        .await?;
    }
    state
        .instances
        .rebind_profile(id, new_profile)
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    state.notify_change();
    let updated = state
        .instances
        .get(id)
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(&request_id))?
        .expect("just updated");
    let profiles = load_profiles(&state)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))
        .map_err(|e| e.into_response_with(&request_id))?;
    Ok(Json(instance_view(&state, &profiles, &updated)))
}

/// §16.9 约束：一个 WARP+ license（档案）同一时刻只能绑定一个实例。
/// `current_binding` 为实例改绑前的绑定（排除自身，允许"重绑同一档"幂等）。
/// 命中返回 409 Conflict；free / zero_trust 档案不受限。
async fn reject_warp_plus_rebind(
    state: &ApiState,
    current_binding: Option<i64>,
    profile: &AccountProfile,
    pid: i64,
    request_id: &String,
) -> Result<(), ErrorResponse> {
    if profile.mode != AccountMode::WarpPlus {
        return Ok(());
    }
    let used = state
        .instances
        .count_bound_to_profile(pid)
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(request_id))?;
    let exclude_self = usize::from(current_binding == Some(pid));
    if used > exclude_self {
        return Err(ApiError::Conflict(format!(
            "WARP+ profile {pid} is already bound to another instance; one WARP+ license = one instance"
        ))
        .into_response_with(request_id));
    }
    Ok(())
}

/// `POST /api/v1/instances/{id}/start`（P7-006）：期望 = 运行，触发收敛。
/// 幂等：已处于期望态也返回 200（收敛循环负责去重）。
pub async fn start(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
) -> ApiResult<StatusCode> {
    let id = parse_id(id).map_err(|e| e.into_response_with(&request_id))?;
    ensure_exists(&state, id)
        .await
        .map_err(|e| e.into_response_with(&request_id))?;
    state
        .instances
        .set_desired(id, true, DesiredState::Running)
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    state.notify_change();
    Ok(StatusCode::ACCEPTED)
}

/// `POST /api/v1/instances/{id}/stop`（P7-006）：期望 = 停止，触发收敛。
/// 幂等同 start。
pub async fn stop(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
) -> ApiResult<StatusCode> {
    let id = parse_id(id).map_err(|e| e.into_response_with(&request_id))?;
    ensure_exists(&state, id)
        .await
        .map_err(|e| e.into_response_with(&request_id))?;
    state
        .instances
        .set_desired(id, true, DesiredState::Stopped)
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    state.notify_change();
    Ok(StatusCode::ACCEPTED)
}

/// `POST /api/v1/instances/{id}/restart`（P7-006）：运行时意图，不改变
/// Desired State。仅对“实际在运行”（非 stopped/disabled）的实例合法。
pub async fn restart(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
) -> ApiResult<StatusCode> {
    let id = parse_id(id).map_err(|e| e.into_response_with(&request_id))?;
    let spec = state
        .instances
        .get(id)
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(&request_id))?
        .ok_or_else(|| {
            ApiError::NotFound(format!("instance {} not found", id.as_i64()))
                .into_response_with(&request_id)
        })?;
    match state.registry.get(id) {
        Some(r) if r.state.is_running() => {}
        _ => {
            return Err(
                ApiError::Conflict(format!("instance {} is not running", id.as_i64()))
                    .into_response_with(&request_id),
            );
        }
    }
    state
        .runtime
        .restart(id, spec.account_profile_id)
        .await
        .map_err(manager_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    Ok(StatusCode::ACCEPTED)
}

/// `DELETE /api/v1/instances/{id}`（P7-007）：危险操作。
/// 顺序：若实际存在运行记录 → 先 runtime.delete（停止进程、保留注册），
/// 成功后删除期望行；任何一步失败都不删行（客户端可重试）。
pub async fn delete(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    RequestId(request_id): RequestId,
    _user: AuthUser,
) -> ApiResult<StatusCode> {
    let id = parse_id(id).map_err(|e| e.into_response_with(&request_id))?;
    ensure_exists(&state, id)
        .await
        .map_err(|e| e.into_response_with(&request_id))?;
    if state.registry.get(id).is_some() {
        state
            .runtime
            .delete(id, false)
            .await
            .map_err(manager_error)
            .map_err(|e| e.into_response_with(&request_id))?;
    }
    state
        .instances
        .delete(id)
        .await
        .map_err(repo_error)
        .map_err(|e| e.into_response_with(&request_id))?;
    state.notify_change();
    Ok(StatusCode::NO_CONTENT)
}

fn parse_id(id: i64) -> Result<crate::runtime::instance::InstanceId, ApiError> {
    crate::runtime::instance::InstanceId::from_db(id)
        .map_err(|e| ApiError::Validation(e.to_string()))
}

async fn ensure_exists(
    state: &ApiState,
    id: crate::runtime::instance::InstanceId,
) -> Result<(), ApiError> {
    if state.instances.get(id).await.map_err(repo_error)?.is_none() {
        return Err(ApiError::NotFound(format!(
            "instance {} not found",
            id.as_i64()
        )));
    }
    Ok(())
}

fn manager_error(e: crate::runtime::manager::ManagerError) -> ApiError {
    use crate::runtime::manager::ManagerError;
    match e {
        ManagerError::NotRunning(id) => {
            ApiError::Conflict(format!("instance {} is not running", id.as_i64()))
        }
        ManagerError::AlreadyRunning(id) => {
            ApiError::Conflict(format!("instance {} is already running", id.as_i64()))
        }
        ManagerError::PortInUse(id, port) => ApiError::Conflict(format!(
            "instance {} internal port {} is already in use",
            id.as_i64(),
            port
        )),
        other => ApiError::Internal(other.to_string()),
    }
}
