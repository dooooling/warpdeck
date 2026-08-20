//! Uniform application error type and the HTTP error contract.
//!
//! Error codes are a registry (DEVELOPMENT_PLAN §18.2): once a code exists it
//! must not be renamed. Internal error context (db details, anyhow chains)
//! is logged, never sent to API clients.
#![allow(dead_code)] // 部分 variant 的构造函数待 P5+ API 全面接入时使用；届时移除

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Central error code registry (stable identifiers, never renamed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// Input failed request validation (400).
    Validation,
    /// Request conflicts with current state (409).
    Conflict,
    /// Requested resource does not exist (404).
    NotFound,
    /// Authentication required (401).
    Unauthorized,
    /// Authenticated but not allowed (403).
    Forbidden,
    /// Database failure (500).
    DbError,
    /// Underlying runtime (WARP/GOST/process) unavailable (503).
    RuntimeUnavailable,
    /// Unclassified internal failure (500).
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::Validation => "VALIDATION",
            ErrorCode::Conflict => "CONFLICT",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::Unauthorized => "UNAUTHORIZED",
            ErrorCode::Forbidden => "FORBIDDEN",
            ErrorCode::DbError => "DB_ERROR",
            ErrorCode::RuntimeUnavailable => "RUNTIME_UNAVAILABLE",
            ErrorCode::Internal => "INTERNAL",
        }
    }
}

/// Application-wide error type. Handlers return `Result<T, AppError>`.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("{message}")]
    Validation {
        field: Option<String>,
        message: String,
    },
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
    #[error("authentication required")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("database error")]
    Database(#[source] sqlx::Error),
    #[error("{0}")]
    RuntimeUnavailable(String),
    #[error("internal error")]
    Internal(#[source] anyhow::Error),
}

impl AppError {
    fn code(&self) -> ErrorCode {
        match self {
            AppError::Validation { .. } => ErrorCode::Validation,
            AppError::Conflict(_) => ErrorCode::Conflict,
            AppError::NotFound(_) => ErrorCode::NotFound,
            AppError::Unauthorized => ErrorCode::Unauthorized,
            AppError::Forbidden => ErrorCode::Forbidden,
            AppError::Database(_) => ErrorCode::DbError,
            AppError::RuntimeUnavailable(_) => ErrorCode::RuntimeUnavailable,
            AppError::Internal(_) => ErrorCode::Internal,
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            AppError::Validation { .. } => StatusCode::BAD_REQUEST,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::Forbidden => StatusCode::FORBIDDEN,
            AppError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::RuntimeUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// User-facing message. Sensitive variants always use a generic message;
    /// the full detail is available via `Display`/`source()` for logs.
    fn public_message(&self) -> String {
        match self {
            AppError::Database(_) => "database error".to_string(),
            AppError::Internal(_) => "internal server error".to_string(),
            AppError::Unauthorized => "authentication required".to_string(),
            AppError::Forbidden => "forbidden".to_string(),
            other => other.to_string(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorBodyInner,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBodyInner {
    pub code: ErrorCode,
    pub message: String,
    /// Filled in by the request-id middleware (see observability::request_id).
    pub request_id: Option<String>,
}

/// NOTE: `request_id` is injected by the request-id middleware; a `null` here
/// means the middleware has not run (e.g. direct unit construction).
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = ErrorBody {
            error: ErrorBodyInner {
                code: self.code(),
                message: self.public_message(),
                request_id: None,
            },
        };
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn decode(resp: Response) -> (StatusCode, ErrorBody) {
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 4096).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn validation_maps_to_400_with_code_and_message() {
        let (status, body) = decode(
            AppError::Validation {
                field: Some("name".into()),
                message: "name is required".into(),
            }
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.error.code, ErrorCode::Validation);
        assert_eq!(body.error.message, "name is required");
    }

    #[tokio::test]
    async fn not_found_maps_to_404() {
        let (status, body) = decode(AppError::NotFound("instance 12".into()).into_response()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body.error.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn conflict_maps_to_409() {
        let (status, _) =
            decode(AppError::Conflict("already running".into()).into_response()).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn unauthorized_maps_to_401_and_forbidden_to_403() {
        let (status, body) = decode(AppError::Unauthorized.into_response()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body.error.code, ErrorCode::Unauthorized);

        let (status, body) = decode(AppError::Forbidden.into_response()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body.error.code, ErrorCode::Forbidden);
    }

    #[tokio::test]
    async fn runtime_unavailable_maps_to_503() {
        let (status, _) =
            decode(AppError::RuntimeUnavailable("no healthy upstream".into()).into_response())
                .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn database_error_does_not_leak_details() {
        let err = sqlx::Error::RowNotFound;
        let (status, body) = decode(AppError::Database(err).into_response()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.error.code, ErrorCode::DbError);
        assert_eq!(body.error.message, "database error");
        assert!(!body.error.message.contains("RowNotFound"));
    }

    #[tokio::test]
    async fn internal_error_does_not_leak_chain() {
        let err = anyhow::anyhow!("secret detail: token=abc123");
        let (status, body) = decode(AppError::Internal(err).into_response()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.error.code, ErrorCode::Internal);
        assert_eq!(body.error.message, "internal server error");
        assert!(!body.error.message.contains("abc123"));
    }

    #[tokio::test]
    async fn request_id_is_null_without_middleware() {
        let (_, body) = decode(AppError::NotFound("x".into()).into_response()).await;
        assert_eq!(body.error.request_id, None);
    }

    #[test]
    fn error_code_strings_match_registry() {
        assert_eq!(ErrorCode::Validation.as_str(), "VALIDATION");
        assert_eq!(ErrorCode::Conflict.as_str(), "CONFLICT");
        assert_eq!(ErrorCode::NotFound.as_str(), "NOT_FOUND");
        assert_eq!(ErrorCode::Unauthorized.as_str(), "UNAUTHORIZED");
        assert_eq!(ErrorCode::Forbidden.as_str(), "FORBIDDEN");
        assert_eq!(ErrorCode::DbError.as_str(), "DB_ERROR");
        assert_eq!(
            ErrorCode::RuntimeUnavailable.as_str(),
            "RUNTIME_UNAVAILABLE"
        );
        assert_eq!(ErrorCode::Internal.as_str(), "INTERNAL");
    }

    #[test]
    fn serialized_code_uses_registry_string() {
        let json = serde_json::to_string(&ErrorCode::NotFound).unwrap();
        assert_eq!(json, "\"NOT_FOUND\"");
    }
}
