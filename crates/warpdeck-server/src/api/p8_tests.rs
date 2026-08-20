//! P8 安全测试（计划 §13.3 Phase Gate + P8-012 Security Tests）。
//!
//! 覆盖：
//! - setup 首次成功后永久锁定（409）；
//! - login 成功/失败/限流（Argon2id 校验、per-IP 阈值 403）；
//! - CSRF 缺失/错误 → 403；
//! - logout 注销后会话失效（Set-Cookie 过期）；
//! - /auth/me 回显用户与 CSRF token；
//! - P8-012 marker 泄漏：写入 secret 后所有响应/审计日志不含明文。

use axum::http::{Method, StatusCode};
use serde_json::{json, Value};

use crate::app::{TestApp, TEST_ADMIN_PASSWORD, TEST_ADMIN_USER};

/// P8-012 要求的泄漏探测 marker（与响应比对；不得出现于任何 capture）。
const TEST_SECRET_DO_NOT_LEAK_123: &str = "TEST_SECRET_DO_NOT_LEAK_123";

async fn body_json(resp: axum::http::Response<axum::body::Body>) -> Value {
    use axum::body::to_bytes;
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn body_text(resp: axum::http::Response<axum::body::Body>) -> String {
    use axum::body::to_bytes;
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 只做 setup（不登录），用于 login/限流负向用例。
async fn setup_only(app: &mut TestApp) {
    let resp = app
        .request_json(
            Method::POST,
            "/api/v1/setup",
            json!({ "username": TEST_ADMIN_USER, "password": TEST_ADMIN_PASSWORD }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// P8-001：首次 setup 成功后永久锁定（第二次 409 + CONFLICT）。
#[tokio::test]
async fn setup_locks_after_first_admin() {
    let app = TestApp::new_unauthenticated().await;

    let status = app.request(Method::GET, "/api/v1/setup/status").await;
    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(body_json(status).await["initialized"], false);

    let first = app
        .request_json(
            Method::POST,
            "/api/v1/setup",
            json!({ "username": TEST_ADMIN_USER, "password": TEST_ADMIN_PASSWORD }),
        )
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(body_json(first).await["initialized"], true);

    // 第二次（即使用不同账号）必须锁定。
    let second = app
        .request_json(
            Method::POST,
            "/api/v1/setup",
            json!({ "username": "intruder", "password": "whatever-123" }),
        )
        .await;
    assert_eq!(second.status(), StatusCode::CONFLICT);
    let body = body_json(second).await;
    assert_eq!(body["error"]["code"], "CONFLICT");

    // status 反映已初始化。
    let status = app.request(Method::GET, "/api/v1/setup/status").await;
    assert_eq!(body_json(status).await["initialized"], true);
    app.close().await;
}

/// P8-001 并发：两个并发 setup 只有一个成功（BEGIN IMMEDIATE 原子性）。
#[tokio::test]
async fn concurrent_setup_creates_single_admin() {
    let app = TestApp::new_unauthenticated().await;
    let (r1, r2) = tokio::join!(
        app.request_json(
            Method::POST,
            "/api/v1/setup",
            json!({ "username": "alice", "password": "password-123" }),
        ),
        app.request_json(
            Method::POST,
            "/api/v1/setup",
            json!({ "username": "bob", "password": "password-456" }),
        ),
    );
    let ok_count = [r1.status(), r2.status()]
        .iter()
        .filter(|s| **s == StatusCode::OK)
        .count();
    assert_eq!(ok_count, 1, "exactly one concurrent setup must win");
    let user_count = app
        .state_for_test()
        .users
        .count()
        .await
        .expect("count users");
    assert_eq!(user_count, 1, "permanent lock: only one admin may exist");
    app.close().await;
}

/// 用户不存在的登录也走恒时验证路径（dummy hash），返回统一 401。
#[tokio::test]
async fn login_unknown_user_is_uniform_401() {
    let mut app = TestApp::new_unauthenticated().await;
    setup_only(&mut app).await;

    let resp = app
        .request_json(
            Method::POST,
            "/api/v1/auth/login",
            json!({ "username": "ghost", "password": "whatever-123" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(resp).await["error"]["code"], "UNAUTHORIZED");
    app.close().await;
}

/// P8-002：错误密码登录失败，且不发放 session cookie。
#[tokio::test]
async fn login_wrong_password_is_401_without_cookie() {
    let mut app = TestApp::new_unauthenticated().await;
    setup_only(&mut app).await;

    let resp = app
        .request_json(
            Method::POST,
            "/api/v1/auth/login",
            json!({ "username": TEST_ADMIN_USER, "password": "wrong-password-123" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let no_cookie = resp
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .all(|v| !v.contains("warpdeck_session="));
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
    assert!(no_cookie, "failed login must not issue a session cookie");
    app.close().await;
}

/// P8-011：连续失败达到阈值后该 IP 被临时封禁（403）。
#[tokio::test]
async fn login_rate_limit_blocks_after_repeated_failures() {
    let mut app = TestApp::new_unauthenticated().await;
    setup_only(&mut app).await;

    // TestApp 的 limiter 阈值 = 3 次（app.rs）。
    for attempt in 1..=3 {
        let resp = app
            .request_json(
                Method::POST,
                "/api/v1/auth/login",
                json!({ "username": TEST_ADMIN_USER, "password": "bad-password-123" }),
            )
            .await;
        if attempt < 3 {
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        } else {
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "3rd consecutive failure must be rate-limited"
            );
            assert_eq!(body_json(resp).await["error"]["code"], "FORBIDDEN");
        }
    }

    // 封禁期间即使密码正确也拒绝。
    let blocked = app
        .request_json(
            Method::POST,
            "/api/v1/auth/login",
            json!({ "username": TEST_ADMIN_USER, "password": TEST_ADMIN_PASSWORD }),
        )
        .await;
    assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
    app.close().await;
}

/// P8-005 负向：未认证 mutation → 401。
#[tokio::test]
async fn unauth_mutation_is_401() {
    let app = TestApp::new_unauthenticated().await;
    let resp = app
        .raw_request(
            Method::POST,
            "/api/v1/instances",
            Some(json!({"name": "x"})),
            &[],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    app.close().await;
}

/// P8-006：mutation 缺失或错误 CSRF → 403；GET 只读不受影响。
#[tokio::test]
async fn mutation_without_or_bad_csrf_is_403() {
    let app = TestApp::new().await;
    let (session_id, csrf) = app.session().expect("logged in");
    let cookie = format!("warpdeck_session={session_id}");

    // 有 cookie、无 CSRF 头 → 403。
    let no_csrf = app
        .raw_request(
            Method::POST,
            "/api/v1/instances",
            Some(json!({"name": "x"})),
            &[("cookie", &cookie)],
        )
        .await;
    assert_eq!(no_csrf.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(no_csrf).await["error"]["code"], "FORBIDDEN");

    // 错误 CSRF 值 → 403。
    let bad_csrf = app
        .raw_request(
            Method::POST,
            "/api/v1/instances",
            Some(json!({"name": "x"})),
            &[("cookie", &cookie), ("x-csrf-token", "deadbeef")],
        )
        .await;
    assert_eq!(bad_csrf.status(), StatusCode::FORBIDDEN);

    // 正确 CSRF → 放行（对照）。
    let ok = app
        .raw_request(
            Method::POST,
            "/api/v1/instances",
            Some(json!({"name": "x"})),
            &[("cookie", &cookie), ("x-csrf-token", csrf)],
        )
        .await;
    assert_eq!(ok.status(), StatusCode::CREATED);

    // GET 只读不要求 CSRF。
    let get = app
        .raw_request(
            Method::GET,
            "/api/v1/instances",
            None,
            &[("cookie", &cookie)],
        )
        .await;
    assert_eq!(get.status(), StatusCode::OK);
    app.close().await;
}

/// P8 gate「session 可注销」：logout 后原 cookie 立即失效。
#[tokio::test]
async fn logout_invalidates_session() {
    let app = TestApp::new().await;
    let (session_id, _csrf) = app.session().expect("logged in");

    // 注销前可用。
    let before = app.request(Method::GET, "/api/v1/auth/me").await;
    assert_eq!(before.status(), StatusCode::OK);

    let logout = app.request(Method::POST, "/api/v1/auth/logout").await;
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    let cookie = logout
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(";");
    assert!(
        cookie.contains("Max-Age=0"),
        "logout cookie must expire: {cookie}"
    );

    // 原 cookie 再访问 protected → 401。
    let after = app
        .raw_request(
            Method::GET,
            "/api/v1/auth/me",
            None,
            &[("cookie", &format!("warpdeck_session={session_id}"))],
        )
        .await;
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
    app.close().await;
}

/// P8-005 配套：/auth/me 回显用户与 CSRF token（前端刷新后取回）。
#[tokio::test]
async fn me_returns_user_and_csrf_token() {
    let app = TestApp::new().await;
    let (_session_id, csrf) = app.session().expect("logged in");

    let resp = app.request(Method::GET, "/api/v1/auth/me").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["user"]["username"], TEST_ADMIN_USER);
    assert_eq!(body["user"]["id"], 1);
    assert_eq!(body["x-csrf-token"], csrf, "CSRF token must match session");
    app.close().await;
}

/// P8-009 语义：被拒绝的更新不得留下部分副作用（密码保持）。
#[tokio::test]
async fn rejected_proxy_update_keeps_existing_password() {
    let app = TestApp::new().await;

    // 先设置密码 + 启用 auth。
    let ok = app
        .request_json(
            Method::PUT,
            "/api/v1/proxy",
            json!({ "auth_enabled": true, "password": "keep-me-secret" }),
        )
        .await;
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(
        body_json(app.request(Method::GET, "/api/v1/proxy").await).await["auth_configured"],
        true
    );

    // 提交「启用 auth 但密码为空」→ 422；密码不得被删除。
    let rejected = app
        .request_json(
            Method::PUT,
            "/api/v1/proxy",
            json!({ "auth_enabled": true, "password": "" }),
        )
        .await;
    assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body_json(app.request(Method::GET, "/api/v1/proxy").await).await["auth_configured"],
        true,
        "rejected update must not delete the existing password"
    );
    app.close().await;
}

/// P8-009 语义：被拒绝的账号更新不得清掉已配置的 license。
#[tokio::test]
async fn rejected_account_update_keeps_existing_license() {
    let app = TestApp::new().await;

    let ok = app
        .request_json(
            Method::PUT,
            "/api/v1/account",
            json!({ "mode": "warp_plus", "license": "LICENSE-KEEP-123" }),
        )
        .await;
    assert_eq!(ok.status(), StatusCode::OK);

    // 目标 zero_trust 但缺 zt 凭据 → 422；license 必须原样保留。
    let rejected = app
        .request_json(
            Method::PUT,
            "/api/v1/account",
            json!({ "mode": "zero_trust", "license": "" }),
        )
        .await;
    assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(app.request(Method::GET, "/api/v1/account").await).await;
    assert_eq!(body["mode"], "warp_plus");
    assert_eq!(
        body["license_present"], true,
        "license must survive rejection"
    );
    app.close().await;
}

/// P8-012：写入 secret 后，任何响应/审计日志都不含 marker 明文。
#[tokio::test]
async fn secrets_never_leak_in_any_response() {
    let app = TestApp::new().await;

    // 写入两个 secret（proxy password + WARP+ license）。
    let proxy = app
        .request_json(
            Method::PUT,
            "/api/v1/proxy",
            json!({
                "auth_enabled": true,
                "password": TEST_SECRET_DO_NOT_LEAK_123,
            }),
        )
        .await;
    assert_eq!(proxy.status(), StatusCode::OK);
    assert!(
        !body_text(proxy).await.contains(TEST_SECRET_DO_NOT_LEAK_123),
        "PUT /proxy response must not echo the password"
    );

    let account = app
        .request_json(
            Method::PUT,
            "/api/v1/account",
            json!({
                "mode": "warp_plus",
                "license": TEST_SECRET_DO_NOT_LEAK_123,
            }),
        )
        .await;
    assert_eq!(account.status(), StatusCode::OK);
    assert!(
        !body_text(account)
            .await
            .contains(TEST_SECRET_DO_NOT_LEAK_123),
        "PUT /account response must not echo the license"
    );

    // GET 端点：只应出现 mask 布尔。
    assert!(!body_text(app.request(Method::GET, "/api/v1/proxy").await)
        .await
        .contains(TEST_SECRET_DO_NOT_LEAK_123));
    assert!(
        !body_text(app.request(Method::GET, "/api/v1/account").await)
            .await
            .contains(TEST_SECRET_DO_NOT_LEAK_123)
    );

    // 错误契约响应也不得回显请求体。
    let bad = app
        .request_json(
            Method::PUT,
            "/api/v1/proxy",
            json!({
                "auth_enabled": true,
                "password": "",
                "socks5_enabled": TEST_SECRET_DO_NOT_LEAK_123,
            }),
        )
        .await;
    assert!(
        !body_text(bad).await.contains(TEST_SECRET_DO_NOT_LEAK_123),
        "error responses must not reflect request bodies"
    );
    app.close().await;
}
