//! 应用壳：路由组合（§6.3 `app.rs`）与集成测试辅助（P1-010 Test Harness）。
//!
//! P7 起 `router(state)` 接受 `ApiState`（trait 接缝注入）；生产由 `main`
//! 组装真实栈（sqlite + InstanceManager + GostManager），测试用 `TestApp`
//! 组装 fake 栈（FakeWarpRuntime + 真实临时 sqlite），API 测试无需真实 WARP
//! （DEVELOPMENT_PLAN §12.4/§12.5 gate）。
//!
//! P8 起 TestApp 自动完成 setup + login：`request`/`request_json` 自带
//! session cookie（与 mutation 的 CSRF 头）。未认证/CSRF 负向用例使用
//! `TestApp::new_unauthenticated()`（P8-012 安全测试）。

use std::convert::Infallible;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{Response, StatusCode};
use axum::response::IntoResponse;
use axum::{middleware, routing::get, Json, Router};
use sqlx::sqlite::SqlitePool;
use tokio::sync::Notify;
use tower::Service;

use crate::api::ApiState;
use crate::auth::rate_limit::InMemoryLoginRateLimiter;
use crate::auth::repos::SqliteUserRepository;
use crate::auth::session::SqliteSessionRepository;
use crate::crypto::secret_store::SqliteSecretStore;
use crate::db::account::SqliteAccountRepository;
use crate::db::profiles::SqliteAccountProfileRepository;
use crate::observability::RequestId;
use crate::runtime::events::EventBus;
use crate::runtime::fake::FakeWarpRuntime;
use crate::runtime::logs::LogBus;
use crate::runtime::registry::RuntimeRegistry;
use crate::{api, db, observability};

/// 组装完整应用 Router：API 路由 + 全局中间件 + Web UI 静态资源（SPA）。
pub fn router(state: ApiState, ui_dir: PathBuf) -> Router {
    Router::new()
        .route("/api/v1/health", get(api::health::health))
        .nest("/api/v1", api::router(state.clone()))
        .fallback_service(SpaFallback {
            ui_dir: Arc::new(ui_dir),
        })
        .layer(middleware::from_fn(observability::request_id_layer))
        .with_state(state)
}

/// Web UI 静态资源 + SPA fallback（P11-001 前置，DESIGN §22.x「Rust server -> API + static files」）。
///
/// - 静态文件（`/`、`/assets/*`、`/favicon.svg` …）直接返回；
/// - 其它非 `api` 路径（`/setup`、`/login`、`/instances/1` 等客户端路由）返回 `index.html`；
/// - `/api/*` 未匹配路由保持 JSON 404 错误契约（绝不返回 index.html）。
#[derive(Clone)]
struct SpaFallback {
    ui_dir: Arc<PathBuf>,
}

impl Service<Request<Body>> for SpaFallback {
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let ui_dir = self.ui_dir.clone();
        Box::pin(async move {
            let path = req.uri().path();
            if path.starts_with("/api/") {
                let request_id = req
                    .extensions()
                    .get::<RequestId>()
                    .map(|rid| rid.0.clone())
                    .unwrap_or_default();
                return Ok(api_not_found(req.method().as_str(), path, &request_id).into_response());
            }

            // 只允许 ui_dir 内的相对路径；拒绝 .. / . / 反斜杠段（防目录穿越，
            // hyper/axum 不归一化请求路径，未认证的静态路径必须自行消毒）。
            let Some(target) = resolve_ui_target(&ui_dir, path) else {
                return Ok(Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .header("content-type", "text/plain; charset=utf-8")
                    .body(Body::from("not found"))
                    .expect("404 response header valid"));
            };
            match read_file(&target).await {
                Ok((bytes, content_type)) => Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", content_type)
                    .header("cache-control", cache_header_for(path))
                    .body(Body::from(bytes))
                    .expect("static response header valid")),
                Err(_) => match read_file(&ui_dir.join("index.html")).await {
                    Ok((bytes, content_type)) => Ok(Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", content_type)
                        .header("cache-control", "no-cache")
                        .body(Body::from(bytes))
                        .expect("spa fallback header valid")),
                    // 未构建 UI 时给出明确提示（容器内误挂载/缺 dist 的排查路径）。
                    Err(_) => Ok(Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .header("content-type", "text/plain; charset=utf-8")
                        .body(Body::from(
                            "web ui not found: missing index.html in ui dir (`WARPDECK_UI_DIR`)",
                        ))
                        .expect("404 response header valid")),
                },
            }
        })
    }
}

/// 将请求路径解析为 `ui_dir` 内的安全目标文件（SPA fallback 语义）。
///
/// - 空路径/`/` → `index.html`；
/// - 拒绝包含 `..`、`.`（独立段）或反斜杠的路径（未认证静态服务，必须显式防穿越）；
/// - 其余按 `/` 分段拼接，不做任何“向上”解析。
fn resolve_ui_target(ui_dir: &Path, path: &str) -> Option<PathBuf> {
    let rel = path.trim_start_matches('/');
    let mut target = PathBuf::new();
    for segment in rel.split('/') {
        match segment {
            "" => continue,
            "." | ".." => return None,
            seg if seg.contains('\\') => return None,
            seg => target.push(seg),
        }
    }
    if target.as_os_str().is_empty() {
        target.push("index.html");
    }
    Some(ui_dir.join(target))
}

/// 静态响应缓存策略：
/// - `/assets/*` 为构建产物的哈希文件名（内容寻址）→ 不可变长缓存（一年）；
/// - 其余（`index.html`、SPA fallback 的页面）→ `no-cache`，要求浏览器每次
///   回源校验，避免无缓存头时浏览器的启发式缓存让「F5 加载陈旧页面」与
///   「手输地址加载新页面」行为不一致（部署新版本后常见困惑）。
fn cache_header_for(path: &str) -> &'static str {
    if path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

async fn read_file(path: &Path) -> Result<(Vec<u8>, &'static str), std::io::Error> {
    let bytes = tokio::fs::read(path).await?;
    let content_type = match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "json" => "application/json",
        _ => "application/octet-stream",
    };
    Ok((bytes, content_type))
}

/// `/api/*` 未匹配时保持与 `api::error` 一致的 JSON 错误契约。
fn api_not_found(method: &str, path: &str, request_id: &str) -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": {
                "code": "NOT_FOUND",
                "message": format!("no such endpoint: {method} {path}"),
                "request_id": request_id,
            }
        })),
    )
        .into_response()
}

/// 测试用 GOST 桩（P1 审查 #4）：可注入 actual 状态、记录 stop/apply。
#[derive(Default)]
pub struct FakeProxyRuntime {
    inner: std::sync::Mutex<FakeProxyRuntimeInner>,
}

#[derive(Default)]
struct FakeProxyRuntimeInner {
    status: Option<crate::proxy::ProxyStatus>,
    stops: usize,
    applies: Vec<crate::proxy::GostSettings>,
}

impl FakeProxyRuntime {
    #[doc(hidden)]
    pub fn set_status(&self, status: Option<crate::proxy::ProxyStatus>) {
        self.inner.lock().unwrap().status = status;
    }

    #[doc(hidden)]
    pub fn stop_count(&self) -> usize {
        self.inner.lock().unwrap().stops
    }
}

#[async_trait::async_trait]
impl crate::reconciler::ProxyApplier for FakeProxyRuntime {
    async fn apply_config(&self, settings: &crate::proxy::GostSettings) -> Result<(), String> {
        self.inner.lock().unwrap().applies.push(settings.clone());
        Ok(())
    }

    async fn status(&self) -> Option<crate::proxy::ProxyStatus> {
        self.inner.lock().unwrap().status.clone()
    }

    async fn stop(&self) -> Result<(), String> {
        self.inner.lock().unwrap().stops += 1;
        let mut inner = self.inner.lock().unwrap();
        inner.status = Some(crate::proxy::ProxyStatus::Stopped);
        drop(inner);
        Ok(())
    }
}

/// 集成测试辅助（P1-010 / §6.4 L2；P7/P8 API 测试复用）：
/// 每个 TestApp 拥有独立临时 SQLite（§25.10），不共享开发者个人数据库。
#[doc(hidden)]
pub struct TestApp {
    pool: SqlitePool,
    db_path: std::path::PathBuf,
    state: ApiState,
    /// fake runtime（动作断言用）。
    runtime: Arc<FakeWarpRuntime>,
    /// fake GOST（actual 状态/stop 断言用，P1 审查 #4）。
    gost: std::sync::Arc<FakeProxyRuntime>,
    /// Web UI 静态目录（临时目录 + 占位 index.html）。
    ui: tempfile::TempDir,
    /// 持久化数据目录（临时目录；日志源枚举/历史测试）。
    data: tempfile::TempDir,
    /// 自动登录获得的会话凭据（默认未认证实例）。
    session_id: Option<String>,
    csrf_token: Option<String>,
}

/// 测试默认管理员凭据。
pub const TEST_ADMIN_USER: &str = "admin";
pub const TEST_ADMIN_PASSWORD: &str = "test-password-123";

#[doc(hidden)]
impl TestApp {
    /// 创建临时 SQLite + fake 运行时栈 + P8 认证栈，自动完成 setup + login。
    pub async fn new() -> Self {
        let mut app = Self::new_unauthenticated().await;
        app.setup_and_login().await;
        app
    }

    /// 未认证 TestApp（安全负向用例：401/CSRF 校验）。
    pub async fn new_unauthenticated() -> Self {
        let (url, db_path) = db::temp_db_url();
        let pool = db::connect(&url).await.expect("failed to connect temp db");
        let registry = Arc::new(RuntimeRegistry::new());
        let runtime = Arc::new(FakeWarpRuntime::with_registry(registry.clone()));
        let key = [7u8; 32];
        // 日志源枚举/历史测试需要独立 data dir（与 SQLite 临时库同生命周期）。
        let data = tempfile::tempdir().expect("failed to create data temp dir");
        let data_dir_path = data.path().to_path_buf();
        let gost = std::sync::Arc::new(FakeProxyRuntime::default());
        let state = ApiState::new(
            db::repo::instance_repo(pool.clone()),
            Arc::new(db::repo::SqliteProxyConfigRepository::new(pool.clone())),
            registry,
            Arc::new(SqliteUserRepository::new(pool.clone())),
            Arc::new(SqliteSessionRepository::new(pool.clone())),
            Arc::new(SqliteSecretStore::new(pool.clone(), key)),
            Arc::new(SqliteAccountRepository::new(pool.clone())),
            Arc::new(SqliteAccountProfileRepository::new(pool.clone())),
            Arc::new(InMemoryLoginRateLimiter::new(
                3,
                std::time::Duration::from_secs(60),
            )),
            false,
            EventBus::default(),
            LogBus::default(),
            data_dir_path.clone(),
            Arc::new(Notify::new()),
            env!("CARGO_PKG_VERSION").to_string(),
            Arc::new(crate::db::uow::ConsistencyService::new(pool.clone(), key)),
            gost.clone(),
            crate::reconciler::new_apply_error_slot(),
        );
        let ui = tempfile::tempdir().expect("failed to create ui temp dir");
        std::fs::write(
            ui.path().join("index.html"),
            b"<!doctype html><title>warpdeck-test-ui</title>",
        )
        .expect("failed to write test index.html");
        Self {
            pool,
            db_path,
            state,
            runtime,
            gost,
            ui,
            data,
            session_id: None,
            csrf_token: None,
        }
    }

    /// 执行 setup + login，保存会话凭据（幂等：已登录时直接返回）。
    pub async fn setup_and_login(&mut self) {
        if self.session_id.is_some() {
            return;
        }
        let resp = self
            .request_json(
                axum::http::Method::POST,
                "/api/v1/setup",
                serde_json::json!({
                    "username": TEST_ADMIN_USER,
                    "password": TEST_ADMIN_PASSWORD,
                }),
            )
            .await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let login = self
            .request_json(
                axum::http::Method::POST,
                "/api/v1/auth/login",
                serde_json::json!({
                    "username": TEST_ADMIN_USER,
                    "password": TEST_ADMIN_PASSWORD,
                }),
            )
            .await;
        assert_eq!(login.status(), axum::http::StatusCode::OK);
        let cookie = login
            .headers()
            .get_all(axum::http::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| {
                v.split(';')
                    .find_map(|p| p.trim().strip_prefix("warpdeck_session=").map(String::from))
            })
            .expect("login must set session cookie");
        let body = body_json(login).await;
        self.session_id = Some(cookie);
        self.csrf_token = body["x-csrf-token"].as_str().map(String::from);
    }

    /// In-memory 请求 helper：直接驱动 Router，不经过真实 TCP。
    /// 已认证实例自动附加 session cookie（mutation 附 CSRF 头）。
    pub async fn request(
        &self,
        method: axum::http::Method,
        uri: &str,
    ) -> axum::http::Response<axum::body::Body> {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let mut builder = Request::builder().method(method).uri(uri);
        self.attach_auth(&mut builder);
        let request = builder
            .body(Body::empty())
            .expect("failed to build test request");
        self::router(self.state.clone(), self.ui.path().to_path_buf())
            .oneshot(request)
            .await
            .expect("test request failed")
    }

    /// 带 JSON 体的请求 helper。
    pub async fn request_json(
        &self,
        method: axum::http::Method,
        uri: &str,
        body: serde_json::Value,
    ) -> axum::http::Response<axum::body::Body> {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        self.attach_auth(&mut builder);
        let request = builder
            .body(Body::from(body.to_string()))
            .expect("failed to build test request");
        self::router(self.state.clone(), self.ui.path().to_path_buf())
            .oneshot(request)
            .await
            .expect("test request failed")
    }

    /// 未认证请求（安全测试用）：不附加任何会话 header。
    pub async fn raw_request(
        &self,
        method: axum::http::Method,
        uri: &str,
        body: Option<serde_json::Value>,
        headers: &[(&str, &str)],
    ) -> axum::http::Response<axum::body::Body> {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder
            .body(match body {
                Some(v) => Body::from(v.to_string()),
                None => Body::empty(),
            })
            .expect("failed to build test request");
        self::router(self.state.clone(), self.ui.path().to_path_buf())
            .oneshot(request)
            .await
            .expect("test request failed")
    }

    fn attach_auth(&self, builder: &mut axum::http::request::Builder) {
        use axum::http::header::{HeaderName, HeaderValue};
        if let Some(session_id) = &self.session_id {
            builder
                .headers_mut()
                .expect("builder headers are available")
                .insert(
                    HeaderName::from_static("cookie"),
                    HeaderValue::from_str(&format!("warpdeck_session={session_id}"))
                        .expect("cookie header is valid"),
                );
            if let Some(csrf) = &self.csrf_token {
                builder
                    .headers_mut()
                    .expect("builder headers are available")
                    .insert(
                        HeaderName::from_static("x-csrf-token"),
                        HeaderValue::from_str(csrf).expect("csrf token is header-safe"),
                    );
            }
        }
    }
    /// 当前登录会话的 cookie + CSRF（安全测试构造自定义请求用）。
    pub fn session(&self) -> Option<(&str, &str)> {
        match (&self.session_id, &self.csrf_token) {
            (Some(id), Some(csrf)) => Some((id, csrf)),
            _ => None,
        }
    }

    /// fake runtime（启动/停止/重启断言）。
    pub fn runtime(&self) -> Arc<FakeWarpRuntime> {
        self.runtime.clone()
    }

    /// fake GOST 桩（actual 状态注入 / stop 计数断言，P1 审查 #4）。
    pub fn gost(&self) -> std::sync::Arc<FakeProxyRuntime> {
        self.gost.clone()
    }

    /// 直查测试 DB（期望侧字段断言用，如 restart 命令代数）。
    pub fn pool_for_test(&self) -> SqlitePool {
        self.pool.clone()
    }

    /// 导出 state 供测试自建请求（需要自定义 body 的用例）。
    pub fn state_for_test(&self) -> ApiState {
        self.state.clone()
    }

    /// 测试 UI 静态目录路径（手工构造 router 的用例）。
    pub fn ui_dir_for_test(&self) -> PathBuf {
        self.ui.path().to_path_buf()
    }

    /// 测试数据目录路径（日志源枚举/历史用例写日志文件）。
    pub fn data_dir_for_test(&self) -> PathBuf {
        self.data.path().to_path_buf()
    }

    /// 已迁移的数据库连接池（供测试断言使用）。
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// 关闭连接池并删除临时 DB 文件（`-wal`/`-shm` 一并清理）。
    pub async fn close(&self) {
        self.pool.close().await;
        db::cleanup_temp_db(&self.db_path);
    }
}

/// 读取响应体 JSON（测试辅助）。
#[doc(hidden)]
pub async fn body_json(response: axum::http::Response<axum::body::Body>) -> serde_json::Value {
    use axum::body::to_bytes;
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap_or_default();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};

    use super::*;

    #[tokio::test]
    async fn health_via_in_memory_request_returns_200() {
        let app = TestApp::new().await;
        let response = app.request(Method::GET, "/api/v1/health").await;
        assert_eq!(response.status(), StatusCode::OK);
        app.close().await;
    }

    #[tokio::test]
    async fn response_always_carries_request_id() {
        let app = TestApp::new().await;
        let response = app.request(Method::GET, "/api/v1/health").await;
        let request_id = response
            .headers()
            .get("x-request-id")
            .expect("missing x-request-id header");
        assert!(!request_id.is_empty());
        app.close().await;
    }

    #[tokio::test]
    async fn unauth_request_is_rejected_with_401_contract() {
        let app = TestApp::new_unauthenticated().await;
        let response = app.request(Method::GET, "/api/v1/instances").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(response).await;
        assert_eq!(body["error"]["code"], "UNAUTHORIZED");
        app.close().await;
    }

    #[tokio::test]
    async fn root_serves_index_html_from_ui_dir() {
        let app = TestApp::new().await;
        let response = app.request(Method::GET, "/").await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("warpdeck-test-ui"));
        app.close().await;
    }

    #[tokio::test]
    async fn spa_fallback_serves_index_html_for_client_routes() {
        let app = TestApp::new().await;
        for route in ["/setup", "/login", "/instances/1", "/nonexistent-page"] {
            let response = app.request(Method::GET, route).await;
            assert_eq!(response.status(), StatusCode::OK, "route {route}");
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap();
            assert!(
                String::from_utf8_lossy(&body).contains("warpdeck-test-ui"),
                "route {route} must fall back to index.html"
            );
        }
        app.close().await;
    }

    #[tokio::test]
    async fn static_assets_are_served_with_content_type() {
        let app = TestApp::new().await;
        std::fs::write(app.ui.path().join("app.js"), b"console.log(1)").expect("write test asset");
        let response = app.request(Method::GET, "/app.js").await;
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .expect("content-type header");
        assert_eq!(content_type, "text/javascript");
        app.close().await;
    }

    #[tokio::test]
    async fn index_html_and_spa_fallback_are_no_cache() {
        let app = TestApp::new().await;
        for route in ["/", "/login", "/dashboard"] {
            let response = app.request(Method::GET, route).await;
            assert_eq!(response.status(), StatusCode::OK, "route {route}");
            assert_eq!(
                response
                    .headers()
                    .get("cache-control")
                    .expect("cache-control"),
                "no-cache",
                "route {route} must be no-cache"
            );
        }
        app.close().await;
    }

    #[tokio::test]
    async fn hashed_assets_are_served_with_immutable_cache() {
        let app = TestApp::new().await;
        std::fs::create_dir_all(app.ui.path().join("assets")).expect("create assets dir");
        std::fs::write(
            app.ui.path().join("assets/app-1A2b3C.js"),
            b"console.log(1)",
        )
        .expect("write test asset");
        let response = app.request(Method::GET, "/assets/app-1A2b3C.js").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("cache-control")
                .expect("cache-control"),
            "public, max-age=31536000, immutable"
        );
        app.close().await;
    }

    #[tokio::test]
    async fn unknown_api_route_keeps_json_404_contract() {
        let app = TestApp::new().await;
        let response = app.request(Method::GET, "/api/v1/does-not-exist").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_json(response).await;
        assert_eq!(body["error"]["code"], "NOT_FOUND");
        assert!(body["error"]["request_id"].as_str().is_some());
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does-not-exist"));
        app.close().await;
    }

    #[tokio::test]
    async fn spa_fallback_rejects_path_traversal() {
        // 未认证静态路径（无 Cookie）不得穿越 ui_dir（P10 review 发现）。
        let app = TestApp::new_unauthenticated().await;
        // 在 ui_dir 的兄弟目录放置秘密文件，尝试经 `..` 读取。
        let sibling = app
            .ui
            .path()
            .parent()
            .expect("tempdir has parent")
            .join("ui-sibling-secret.txt");
        std::fs::write(&sibling, b"top-secret").expect("write sibling secret");
        for route in [
            "/../ui-sibling-secret.txt",
            "/a/../../ui-sibling-secret.txt",
        ] {
            let response = app.raw_request(Method::GET, route, None, &[]).await;
            assert_ne!(
                response.status(),
                StatusCode::OK,
                "route {route} must not serve sibling"
            );
            let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap();
            assert!(
                !String::from_utf8_lossy(&body).contains("top-secret"),
                "route {route} must not leak sibling content"
            );
        }
        let _ = std::fs::remove_file(&sibling);
        app.close().await;
    }
}
