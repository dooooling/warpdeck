//! 首次初始化（P8-001）。
//!
//! 设计（DESIGN §20.1）：
//! - `GET /api/v1/setup/status`：public，`{initialized: bool}`；
//! - `POST /api/v1/setup`：public，仅当 users 表为空时成功；创建成功后
//!   永久锁定（后续请求 409）。并发安全：事务内先 SELECT 再 INSERT
//!   （BEGIN IMMEDIATE 避免两个请求同时通过检查）。

use axum::extract::State;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::error::{invalid_json_body, ApiError};
use crate::api::{ApiResult, ApiState};
use crate::auth::password::hash_password;
use crate::auth::repos::UserRepoError;
use crate::observability::RequestId;

/// `GET /api/v1/setup/status`（P8-001）：是否已初始化。
pub async fn status(
    State(state): State<ApiState>,
    RequestId(_rid): RequestId,
) -> ApiResult<Json<Value>> {
    let count = state
        .users
        .count()
        .await
        .map_err(user_error)
        .map_err(|e| e.into_response_with(&_rid))?;
    Ok(Json(json!({ "initialized": count > 0 })))
}

/// 创建首个管理员请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct CreateAdminRequest {
    pub username: String,
    pub password: String,
}

impl CreateAdminRequest {
    /// 用户名/密码基本校验（密码强度策略：≥8 字符；用户名 1..=64）。
    pub fn validate(&self) -> Result<(String, String), String> {
        let username = self.username.trim();
        if username.is_empty() || username.chars().count() > 64 {
            return Err("username must be 1..=64 characters".to_string());
        }
        if self.password.chars().count() < 8 {
            return Err("password must be at least 8 characters".to_string());
        }
        if self.password.chars().count() > 1024 {
            return Err("password too long".to_string());
        }
        Ok((username.to_string(), self.password.clone()))
    }
}

/// `POST /api/v1/setup`：创建首个管理员（只能成功一次）。
pub async fn create_admin(
    State(state): State<ApiState>,
    RequestId(request_id): RequestId,
    body: Result<Json<CreateAdminRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<Value>> {
    let Json(req) = body
        .map_err(|_| invalid_json_body())
        .map_err(|e| e.into_response_with(&request_id))?;

    // BEGIN IMMEDIATE 事务内「查空 + 插入」：并发 setup 只有一个成功
    // （P8-001 永久锁定；DESIGN §20.1）。先哈希（Argon2id）再入事务，
    // 缩短持锁时间——校验竞态由数据库锁保证，不在 SQL 层泄露用户名。
    let (username, password) = req
        .validate()
        .map_err(ApiError::Validation)
        .map_err(|e| e.into_response_with(&request_id))?;
    let hash = hash_password(&password).map_err(|e| {
        tracing::error!(error = %e, "password hashing failed");
        ApiError::Internal("internal server error".to_string()).into_response_with(&request_id)
    })?;
    match state
        .users
        .create_admin_if_empty(&username, &hash)
        .await
        .map_err(user_error)
        .map_err(|e| e.into_response_with(&request_id))?
    {
        crate::auth::repos::AdminSetupResult::Created(_) => {}
        crate::auth::repos::AdminSetupResult::AlreadyInitialized => {
            return Err(ApiError::Conflict("setup already completed".to_string())
                .into_response_with(&request_id));
        }
    }

    Ok(Json(json!({ "initialized": true })))
}

fn user_error(e: UserRepoError) -> ApiError {
    ApiError::Internal(e.to_string())
}
