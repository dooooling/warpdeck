//! 登录/登出/当前用户（P8-005/011）。
//!
//! 设计（DESIGN §17.2/§20）：
//! - `POST /auth/login`（public）：per-IP 限流 → Argon2id 校验 →
//!   创建服务端 session → Set-Cookie（HttpOnly/SameSite=Lax/[Secure]）；
//!   响应携带 `csrf_token`（前端内存持有，mutation 放 `X-CSRF-Token` 头）。
//! - `POST /auth/logout`（protected）：删除 session + 清除 cookie。
//! - `GET /auth/me`（protected）：当前用户 + CSRF token（前端刷新后取回）。
//!   cookie 只存随机 session id——永不落日志/响应体明文之外。

use std::net::SocketAddr;
use std::sync::OnceLock;

use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::request::Parts;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::error::{invalid_json_body, ApiError};
use crate::api::middleware::{AuthUser, SessionCsrf, SessionId};
use crate::api::{ApiResult, ApiState};
use crate::auth::password::{hash_password, verify_password};
use crate::auth::rate_limit::RateDecision;
use crate::auth::{CSRF_HEADER, SESSION_COOKIE, SESSION_TTL};
use crate::observability::RequestId;

/// 登录请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// 构造 session cookie 的 `Set-Cookie` 值。
fn session_cookie_value(session_id: &str, secure: bool) -> String {
    let mut value = format!(
        "{SESSION_COOKIE}={session_id}; HttpOnly; Path=/; SameSite=Lax; Max-Age={}",
        SESSION_TTL.whole_seconds()
    );
    if secure {
        value.push_str("; Secure");
    }
    value
}

fn expired_cookie_value() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0")
}

/// 用户不存在时的 dummy 验证（防用户名枚举计时侧信道）。
/// 一次性生成生产参数哈希，之后每次登录都执行一次 Argon2 验证，
/// 使用户不存在与密码错误两条路径耗时接近（DESIGN §20.2）。
static DUMMY_HASH: OnceLock<String> = OnceLock::new();

fn dummy_verify(password: &str) -> bool {
    let hash =
        DUMMY_HASH.get_or_init(|| hash_password("dummy-password-for-timing").unwrap_or_default());
    verify_password(hash, password)
}

/// 客户端 IP（从 ConnectInfo 扩展读取；测试 oneshot 无该扩展时回退
/// 127.0.0.1，保证限流组件可测）。
pub struct ClientIp(pub std::net::IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip())
                .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        ))
    }
}

/// `POST /api/v1/auth/login`：登录成功设置 session cookie。
pub async fn login(
    State(state): State<ApiState>,
    RequestId(request_id): RequestId,
    ClientIp(ip): ClientIp,
    body: Result<Json<LoginRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<axum::response::Response> {
    let decision = state.login_limiter.check(ip).await;
    if decision == RateDecision::Blocked {
        return Err(
            ApiError::Forbidden("too many login attempts, try again later".to_string())
                .into_response_with(&request_id),
        );
    }

    let Json(req) = body
        .map_err(|_| invalid_json_body())
        .map_err(|e| e.into_response_with(&request_id))?;

    // 恒定时间路径：用户不存在时也执行一次 Argon2 验证（dummy hash），
    // 与密码错误路径耗时接近（防用户名枚举计时侧信道）。
    let found = state
        .users
        .find_by_username(&req.username)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "user lookup failed");
            ApiError::Internal("internal server error".to_string()).into_response_with(&request_id)
        })?;
    let user = match &found {
        Some(u) => verify_password(&u.password_hash, &req.password).then(|| u.clone()),
        None => {
            let _ = dummy_verify(&req.password);
            None
        }
    };

    let Some(user) = user else {
        let decision = state.login_limiter.record_failure(ip).await;
        if decision == RateDecision::Blocked {
            return Err(ApiError::Forbidden(
                "too many login attempts, try again later".to_string(),
            )
            .into_response_with(&request_id));
        }
        return Err(
            ApiError::Unauthorized("invalid username or password".to_string())
                .into_response_with(&request_id),
        );
    };

    state.login_limiter.record_success(ip).await;
    // 惰性清理过期会话（登录是天然机会点）。
    let _ = state.sessions.delete_expired().await;
    let session = match state.sessions.create(user.id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "session create failed");
            return Err(ApiError::Internal("internal server error".to_string())
                .into_response_with(&request_id));
        }
    };

    let cookie = session_cookie_value(&session.id, state.secure_cookie);
    let body = json!({
        "user": { "id": user.id, "username": user.username },
        CSRF_HEADER: session.csrf_token,
    });
    Ok(axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(axum::http::header::SET_COOKIE, cookie)
        .body(axum::body::Body::from(body.to_string()))
        .expect("login response is valid"))
}

/// `POST /api/v1/auth/logout`：注销当前会话。
pub async fn logout(
    State(state): State<ApiState>,
    RequestId(_request_id): RequestId,
    _user: AuthUser,
    SessionId(session_id): SessionId,
) -> ApiResult<axum::response::Response> {
    // SessionId 由 auth_guard 注入（同 cookie 解析，避免重复实现）。
    let _ = state.sessions.delete(&session_id).await;
    Ok(axum::response::Response::builder()
        .status(axum::http::StatusCode::NO_CONTENT)
        .header(axum::http::header::SET_COOKIE, expired_cookie_value())
        .body(axum::body::Body::empty())
        .expect("logout response is valid"))
}

/// `GET /api/v1/auth/me`：当前用户信息 + CSRF token。
pub async fn me(
    RequestId(_rid): RequestId,
    user: AuthUser,
    csrf: SessionCsrf,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "user": { "id": user.user_id, "username": user.username },
        CSRF_HEADER: csrf.0,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_cookie_flags() {
        let value = session_cookie_value("abc", false);
        assert!(value.contains("HttpOnly"));
        assert!(value.contains("SameSite=Lax"));
        assert!(value.contains("Path=/"));
        assert!(
            !value.contains("Secure"),
            "insecure mode must not add Secure"
        );
        assert!(value.contains("warpdeck_session=abc"));
        let secure = session_cookie_value("abc", true);
        assert!(secure.contains("; Secure"));
    }
}
