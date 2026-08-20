//! API 统一错误契约（P7-002；P8 扩展 UNAUTHORIZED/FORBIDDEN）。
//!
//! 设计（DEVELOPMENT_PLAN §12.4 HTTP code 规范，DESIGN §17.x）：
//! - 所有端点错误响应统一 `{"error": {"code", "message", "request_id"}}`；
//! - `request_id` 来自 observability 中间件，与 `X-Request-Id` 响应头一致；
//! - 状态码映射：Validation→422，Unauthorized→401，Forbidden→403，
//!   NotFound→404，Conflict→409，Internal→500；
//! - `Internal` 的 message 不携带内部细节（防泄漏），细节走 tracing。

use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::db::repo::RepoError;

/// API 层错误。核心错误（`RepoError`/`ManagerError` 等）在 handler 中映射到此类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// 输入校验失败（422）。
    Validation(String),
    /// 未认证：无有效会话（401）。
    Unauthorized(String),
    /// 已认证但被拒绝：CSRF 失败等（403）。
    Forbidden(String),
    /// 资源不存在（404）。
    NotFound(String),
    /// 当前状态不允许该操作（409）。
    Conflict(String),
    /// 内部错误（500）。
    Internal(String),
}

/// Handler 统一返回类型：错误分支返回已构建好的错误响应
/// （`ApiError::into_response_with(&request_id)`，request_id 随契约进 body）。
pub type ApiResult<T> = Result<T, Response>;

/// 仓储错误 → API 错误（核心错误映射；FK 违例归因于引用不存在）。
pub(crate) fn repo_error(e: RepoError) -> ApiError {
    if e.to_string().contains("FOREIGN KEY constraint failed") {
        return ApiError::Validation("referenced record does not exist".to_string());
    }
    ApiError::Internal(e.to_string())
}

/// JSON body rejection → Validation 错误。
///
/// 不用 `JsonRejection::body_text()`：serde 反序列化错误消息会回显
/// 请求体中的字段值（P8-012 泄漏探测会抓到这个面），统一固定文案。
pub fn invalid_json_body() -> ApiError {
    ApiError::Validation("invalid JSON request body".to_string())
}

impl ApiError {
    /// 错误码（响应体 `error.code`，机器可读、稳定）。
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::Validation(_) => "VALIDATION",
            ApiError::Unauthorized(_) => "UNAUTHORIZED",
            ApiError::Forbidden(_) => "FORBIDDEN",
            ApiError::NotFound(_) => "NOT_FOUND",
            ApiError::Conflict(_) => "CONFLICT",
            ApiError::Internal(_) => "INTERNAL",
        }
    }

    /// 对外消息（Internal 固定为通用文案，不暴露内部错误字符串）。
    pub fn message(&self) -> String {
        match self {
            ApiError::Validation(m) => m.clone(),
            ApiError::Unauthorized(m) => m.clone(),
            ApiError::Forbidden(m) => m.clone(),
            ApiError::NotFound(m) => m.clone(),
            ApiError::Conflict(m) => m.clone(),
            ApiError::Internal(detail) => {
                tracing::error!(error = %detail, "internal API error");
                "internal server error".to_string()
            }
        }
    }

    /// 构造带 `request_id` 的响应（handler 从 extractor 解构 `RequestId(String)` 后传入）。
    pub fn into_response_with(self, request_id: &String) -> Response {
        let status = match &self {
            ApiError::Validation(_) => axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::Unauthorized(_) => axum::http::StatusCode::UNAUTHORIZED,
            ApiError::Forbidden(_) => axum::http::StatusCode::FORBIDDEN,
            ApiError::NotFound(_) => axum::http::StatusCode::NOT_FOUND,
            ApiError::Conflict(_) => axum::http::StatusCode::CONFLICT,
            ApiError::Internal(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = Json(json!({
            "error": {
                "code": self.code(),
                "message": self.message(),
                "request_id": request_id,
            }
        }));
        (status, body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid() -> String {
        "test-request".to_string()
    }

    #[tokio::test]
    async fn validation_maps_to_422() {
        let resp = ApiError::Validation("bad name".into())
            .into_response_with(&rid())
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn not_found_maps_to_404() {
        let resp = ApiError::NotFound("no such instance".into())
            .into_response_with(&rid())
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn conflict_maps_to_409() {
        let resp = ApiError::Conflict("not running".into())
            .into_response_with(&rid())
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn internal_maps_to_500_and_hides_detail() {
        let resp = ApiError::Internal("secret internals".into())
            .into_response_with(&rid())
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("internal server error"));
        assert!(!text.contains("secret internals"));
    }

    #[tokio::test]
    async fn body_carries_code_and_request_id() {
        let resp = ApiError::NotFound("x".into())
            .into_response_with(&rid())
            .into_response();
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("\"NOT_FOUND\""));
        assert!(text.contains("test-request"));
    }
}
