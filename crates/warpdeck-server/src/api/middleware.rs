//! 认证与 CSRF 中间件（P8-005/006）。
//!
//! 流程（顺序固定）：
//! 1. 从 `warpdeck_session` cookie 取 session id → 查 `SessionRepository`；
//!    无 cookie/无记录/已过期 → 401 统一错误契约。
//! 2. mutation（POST/PUT/PATCH/DELETE）额外校验 `X-CSRF-Token` 头
//!    等于会话绑定的 token → 不一致 → 403（DESIGN §20.4）。
//! 3. 认证上下文（user_id/username/csrf）写入 request extensions，
//!    handler 用 `AuthUser` / `CsrfToken` 提取器读取。
//!
//! 该中间件只挂载在 protected 路由上；public 路由（setup/login/health）
//! 不经过这里。

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

use crate::auth::{CSRF_HEADER, SESSION_COOKIE};
use crate::observability::RequestId;

use super::error::ApiError;
use super::ApiState;

/// 认证后的当前用户（handler 提取器）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthUser {
    pub user_id: i64,
    pub username: String,
}

/// 会话绑定的 CSRF token（handler 需回显给前端时用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCsrf(pub String);

/// 当前请求对应的 session id（auth_guard 注入；注销等场景用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionId(pub String);

impl<S: Send + Sync> FromRequestParts<S> for AuthUser {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthUser>()
            .cloned()
            .ok_or(StatusCode::UNAUTHORIZED)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for SessionCsrf {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<SessionCsrf>()
            .cloned()
            .ok_or(StatusCode::UNAUTHORIZED)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for SessionId {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<SessionId>()
            .cloned()
            .ok_or(StatusCode::UNAUTHORIZED)
    }
}

/// 从 cookie 头解析 session id（`name=value`，仅第一个匹配）。
/// 畸形 segment（不含 `=`）跳过而非中止整个解析。
fn session_id_from_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    for value in headers.get_all(axum::http::header::COOKIE) {
        let Ok(text) = value.to_str() else {
            continue;
        };
        for part in text.split(';') {
            let Some((name, value)) = part.trim().split_once('=') else {
                continue;
            };
            if name == SESSION_COOKIE {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn unauthorized_response(request_id: &str) -> Response {
    ApiError::Unauthorized("authentication required".to_string())
        .into_response_with(&request_id.to_string())
}

fn forbidden_response(request_id: &str) -> Response {
    ApiError::Forbidden("invalid CSRF token".to_string())
        .into_response_with(&request_id.to_string())
}

/// 认证守卫：校验 session cookie → 校验 CSRF（mutation）→ 注入 AuthUser。
pub async fn auth_guard(
    State(state): State<ApiState>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|r| r.0.clone())
        .unwrap_or_default();

    let Some(session_id) = session_id_from_cookie(request.headers()) else {
        return unauthorized_response(&request_id);
    };
    let session = match state.sessions.get(&session_id).await {
        Ok(Some(s)) => s,
        _ => return unauthorized_response(&request_id),
    };
    let Some(user) = state.users.get(session.user_id).await.ok().flatten() else {
        return unauthorized_response(&request_id);
    };

    // CSRF：仅对 mutation 生效（GET 只读安全；SSE 无副作用）。
    if request.method() != axum::http::Method::GET {
        let header_token = request
            .headers()
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        if header_token != session.csrf_token {
            return forbidden_response(&request_id);
        }
    }

    let _ = state.sessions.touch(&session_id).await;
    request.extensions_mut().insert(AuthUser {
        user_id: user.id,
        username: user.username,
    });
    request
        .extensions_mut()
        .insert(SessionCsrf(session.csrf_token));
    request.extensions_mut().insert(SessionId(session_id));
    next.run(request).await
}

/// 供单测解析 cookie 的辅助。
#[cfg(test)]
fn parse_cookie<'a>(headers: &'a axum::http::HeaderMap, name: &'a str) -> Option<&'a str> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .map(str::trim)
        .find_map(|part| part.strip_prefix(&format!("{name}=")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    #[test]
    fn cookie_parsing_finds_named_value() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("other=1; warpdeck_session=abc123; x=y"),
        );
        assert_eq!(parse_cookie(&headers, SESSION_COOKIE), Some("abc123"));
    }

    #[test]
    fn cookie_parsing_handles_missing_and_single() {
        let headers = HeaderMap::new();
        assert_eq!(parse_cookie(&headers, SESSION_COOKIE), None);
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("warpdeck_session=only"),
        );
        assert_eq!(parse_cookie(&headers, SESSION_COOKIE), Some("only"));
    }

    #[test]
    fn cookie_parsing_ignores_prefix_collisions() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            HeaderValue::from_static("warpdeck_session_x=1; warpdeck_session=real"),
        );
        assert_eq!(parse_cookie(&headers, SESSION_COOKIE), Some("real"));
    }
}
