//! Observability: structured tracing setup and the request-id middleware.
//!
//! Every HTTP request gets a `request_id` (accepted from a valid
//! `X-Request-Id` header or generated fresh). It is:
//! - returned in the `X-Request-Id` response header,
//! - injected into the JSON error contract body (`error.request_id`),
//! - attached to the per-request tracing span and the completion log event.

pub mod file_logger;
pub mod redactor;

use std::path::Path;
use std::time::Instant;

use axum::body::{to_bytes, Body};
use axum::extract::FromRequestParts;
use axum::http::header::{HeaderName, HeaderValue};
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use tracing::info_span;
use tracing_subscriber::EnvFilter;

pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The request id carried in request extensions and logs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RequestId(pub String);

/// Init the global tracing subscriber from the given env-filter level.
/// `RUST_LOG` wins over the app-configured level when both are present.
/// When `data_dir` is given, tracing events are also appended to
/// `{data_dir}/logs/manager.log` (P10-005).
pub fn init_tracing(
    level: &str,
    data_dir: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    match data_dir {
        Some(dir) => {
            // P10-005：同时写 `manager.log`（持久化）与 stderr（`docker logs` 可见）。
            let layer = file_logger::DualLogLayer::new(dir)?;
            builder.with_writer(layer).with_ansi(false).try_init()
        }
        None => builder.try_init(),
    }
}

/// Axum extractor: read the request id set by `request_id_layer`.
impl<S: Send + Sync> FromRequestParts<S> for RequestId {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<RequestId>()
            .cloned()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

/// Reject malformed/abusive incoming ids instead of reflecting them in
/// headers and logs; the middleware falls back to a fresh generated id.
fn sanitize_incoming(value: &str) -> bool {
    let ok_len = (1..=64).contains(&value.len());
    ok_len
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Middleware: validate/assign a request id, log request completion, and
/// stamp the response header + error-contract body with it.
pub async fn request_id_layer(mut req: Request<Body>, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().to_string();
    let path = req.uri().path().to_string();

    let id = match req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        Some(v) if sanitize_incoming(v) => RequestId(v.to_string()),
        _ => RequestId(new_id()),
    };

    req.extensions_mut().insert(id.clone());

    let span = info_span!(
        "http_request",
        request_id = %id.0,
        method = %method,
        path = %path
    );
    let _entered = span.enter();

    let mut resp = next.run(req).await;
    let status = resp.status().as_u16();

    resp.headers_mut().insert(
        HeaderName::from_static(REQUEST_ID_HEADER),
        HeaderValue::from_str(&id.0).expect("request id is always header-safe"),
    );

    if resp.status().is_client_error() || resp.status().is_server_error() {
        let (parts, body) = resp.into_parts();
        // Error responses must stay small (error-contract JSON), so reading
        // without a small cap avoids silently emptying oversized bodies.
        let bytes = to_bytes(body, usize::MAX).await.unwrap_or_default();
        let rewritten = inject_request_id(&bytes, &id.0);
        resp = Response::from_parts(parts, Body::from(rewritten));
        resp.headers_mut().remove("content-length");
    }

    tracing::info!(
        request_id = %id.0,
        method = %method,
        path = %path,
        status = status,
        duration_ms = start.elapsed().as_millis() as u64,
        "http request completed"
    );

    resp
}

/// Insert `request_id` into an error-contract JSON body, if it looks like one.
fn inject_request_id(body: &[u8], id: &str) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.to_vec();
    };
    match value.get_mut("error").and_then(|e| e.as_object_mut()) {
        Some(obj) => {
            obj.insert(
                "request_id".to_string(),
                serde_json::Value::String(id.to_string()),
            );
            serde_json::to_vec(&value).unwrap_or_else(|_| body.to_vec())
        }
        None => body.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::HeaderValue;
    use axum::routing::get;
    use axum::Router;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route("/ok", get(|| async { "hello".to_string() }))
            .route(
                "/fail",
                get(|| async {
                    (
                        StatusCode::NOT_FOUND,
                        axum::Json(json!({
                            "error": {"code": "NOT_FOUND", "message": "nope"}
                        })),
                    )
                }),
            )
            .layer(axum::middleware::from_fn(request_id_layer))
    }

    async fn get_id(
        app: &Router,
        path: &str,
        incoming: Option<&str>,
    ) -> (Response, Option<String>) {
        let mut req = axum::http::Request::builder()
            .uri(path)
            .method("GET")
            .body(Body::empty())
            .unwrap();
        if let Some(v) = incoming {
            // Headers that cannot even be represented are rejected outright.
            if let Ok(value) = HeaderValue::from_str(v) {
                req.headers_mut().insert(REQUEST_ID_HEADER, value);
            }
        }
        let resp = app.clone().oneshot(req).await.unwrap();
        let id = resp
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        (resp, id)
    }

    #[tokio::test]
    async fn generates_request_id_when_header_absent() {
        let app = app();
        let (resp, id) = get_id(&app, "/ok", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let id = id.expect("id header must be present");
        assert!(!id.is_empty());
    }

    #[tokio::test]
    async fn accepts_safe_incoming_request_id() {
        let app = app();
        let (_, id) = get_id(&app, "/ok", Some("req_abc-123.xyz")).await;
        assert_eq!(id.as_deref(), Some("req_abc-123.xyz"));
    }

    #[tokio::test]
    async fn rejects_unsafe_incoming_request_id() {
        let app = app();
        for bad in ["", "sp ace", "a/b", "x\ny", &"x".repeat(100)] {
            let (_, id) = get_id(&app, "/ok", Some(bad)).await;
            assert_ne!(
                id.as_deref(),
                Some(bad),
                "unsafe id `{bad}` must not be reflected"
            );
        }
    }

    #[tokio::test]
    async fn injects_request_id_into_error_body() {
        let app = app();
        let (resp, id) = get_id(&app, "/fail", None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let id = id.unwrap();
        let bytes = to_bytes(resp.into_body(), 8192).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["request_id"], json!(id));
        assert_eq!(value["error"]["code"], json!("NOT_FOUND"));
    }

    #[tokio::test]
    async fn does_not_touch_non_error_bodies() {
        let app = app();
        let (resp, _) = get_id(&app, "/ok", None).await;
        let bytes = to_bytes(resp.into_body(), 8192).await.unwrap();
        assert_eq!(&bytes[..], b"hello");
    }

    #[test]
    fn incoming_id_sanitization_rules() {
        assert!(sanitize_incoming("req_01ABC-._"));
        assert!(!sanitize_incoming(""));
        assert!(!sanitize_incoming(&"a".repeat(65)));
        assert!(!sanitize_incoming("has space"));
        assert!(!sanitize_incoming("has/slash"));
        assert!(!sanitize_incoming("has\nnewline"));
    }

    #[test]
    fn inject_request_id_handles_garbage_body() {
        assert_eq!(inject_request_id(b"not json", "id1"), b"not json".to_vec());
        assert_eq!(inject_request_id(b"[]", "id1"), b"[]".to_vec());
    }

    #[tokio::test]
    async fn extractor_returns_request_id_set_by_middleware() {
        let mut parts = test_parts();
        parts.extensions.insert(RequestId("from-test".into()));
        let extracted = RequestId::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert_eq!(extracted.0, "from-test");
    }

    #[tokio::test]
    async fn extractor_rejects_when_middleware_missing() {
        let mut parts = test_parts();
        let err = RequestId::from_request_parts(&mut parts, &())
            .await
            .unwrap_err();
        assert_eq!(err, StatusCode::INTERNAL_SERVER_ERROR);
    }

    fn test_parts() -> axum::http::request::Parts {
        axum::http::Request::builder()
            .uri("/")
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    struct CaptureLayer {
        events: Arc<Mutex<Vec<(String, String)>>>, // (field name, debug value)
    }

    impl tracing::field::Visit for CaptureLayer {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.events
                .lock()
                .unwrap()
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = Self {
                events: self.events.clone(),
            };
            event.record(&mut visitor);
        }
    }

    #[test]
    fn emitted_log_event_carries_request_id() {
        use tracing_subscriber::prelude::*;

        let events: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let capture = CaptureLayer {
            events: events.clone(),
        };
        let subscriber = tracing_subscriber::registry().with(capture);

        // A GLOBAL default is required, not just a thread-local one: the
        // middleware's callsite interest is cached per-callsite over the first
        // registering thread's dispatch. Without a global subscriber, other
        // tests' threads register the middleware callsite against the
        // `NoSubscriber` default (interest = `never`), silently disabling the
        // event for every thread afterwards.
        tracing::subscriber::set_global_default(subscriber)
            .expect("only this test installs a global subscriber");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let app = app();
        let resp = rt.block_on(async {
            let req = axum::http::Request::builder()
                .uri("/ok")
                .body(Body::empty())
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), app.clone().oneshot(req))
                .await
                .unwrap()
                .unwrap()
        });

        let id = resp
            .headers()
            .get(REQUEST_ID_HEADER)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let locked = events.lock().unwrap();
        let request_ids: Vec<_> = locked
            .iter()
            .filter(|(k, _)| k == "request_id")
            .map(|(_, v)| v.clone())
            .collect();
        assert!(
            !request_ids.is_empty(),
            "no http request event observed: {request_ids:?}"
        );
        assert!(
            request_ids.iter().any(|v| v == &id),
            "event missing id {id}: {request_ids:?}"
        );
    }
}
