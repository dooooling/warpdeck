//! P7 API 集成测试（§12.4：Axum Router in-memory，不监听真实 TCP；
//! §12.5 gate：API 测试无需真实 WARP）。
//!
//! 每个 mutation 覆盖：happy path / invalid input / not found / conflict /
//! internal application failure。栈 = `app::TestApp`（fake runtime + 临时 sqlite）。

use axum::body::to_bytes;
use axum::http::{Method, StatusCode};
use serde_json::{json, Value};

use crate::app::TestApp;

/// 解析响应 JSON。
async fn body_json(resp: axum::http::Response<axum::body::Body>) -> Value {
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn create_instance(app: &TestApp, name: &str) -> i64 {
    let resp = app
        .request_json(Method::POST, "/api/v1/instances", json!({ "name": name }))
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    body["id"].as_i64().expect("created instance id")
}

// ---------- System ----------

#[tokio::test]
async fn system_status_returns_ok_with_counts() {
    let app = TestApp::new().await;
    let resp = app.request(Method::GET, "/api/v1/system/status").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "ok");
    assert!(body["version"].as_str().is_some());
    assert!(body["uptime_secs"].as_u64().is_some());
    assert_eq!(body["instances"]["total"], 0);
    app.close().await;
}

#[tokio::test]
async fn system_version_returns_version() {
    let app = TestApp::new().await;
    let resp = app.request(Method::GET, "/api/v1/system/version").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    app.close().await;
}

// ---------- Instances: list / get ----------

#[tokio::test]
async fn list_returns_empty_then_created_instances() {
    let app = TestApp::new().await;
    let resp = app.request(Method::GET, "/api/v1/instances").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body.as_array().unwrap().len(), 0);

    create_instance(&app, "alpha").await;
    create_instance(&app, "beta").await;

    let resp = app.request(Method::GET, "/api/v1/instances").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // 合并视图字段齐全（P7-004）。
    let first = &arr[0];
    assert_eq!(first["name"], "alpha");
    assert_eq!(first["desired_state"], "running");
    assert_eq!(first["runtime_state"], "stopped");
    assert_eq!(first["enabled"], true);
    assert_eq!(first["auto_restart"], true);
    assert!(first.get("last_error").is_some());
    app.close().await;
}

#[tokio::test]
async fn get_existing_instance_returns_view() {
    let app = TestApp::new().await;
    let id = create_instance(&app, "gamma").await;

    let resp = app
        .request(Method::GET, &format!("/api/v1/instances/{id}"))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["id"], id);
    assert_eq!(body["name"], "gamma");
    app.close().await;
}

#[tokio::test]
async fn get_missing_instance_returns_404_with_error_contract() {
    let app = TestApp::new().await;
    let resp = app.request(Method::GET, "/api/v1/instances/999").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "NOT_FOUND");
    assert!(body["error"]["message"].is_string());
    assert!(body["error"]["request_id"].as_str().is_some());
    app.close().await;
}

#[tokio::test]
async fn invalid_instance_id_is_rejected() {
    let app = TestApp::new().await;
    let resp = app
        .request(Method::GET, "/api/v1/instances/not-a-number")
        .await;
    // 计划 §12.4：Validation → 400/422 均为合法语义（axum Path extractor 默认 400）。
    assert!(
        resp.status().is_client_error(),
        "expected 4xx for malformed id, got {}",
        resp.status()
    );
    app.close().await;
}

// ---------- Instances: create ----------

#[tokio::test]
async fn create_happy_path_and_notify() {
    let app = TestApp::new().await;
    let resp = app
        .request_json(
            Method::POST,
            "/api/v1/instances",
            json!({ "name": "  spaced name  " }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["name"], "spaced name"); // trim 后落库
    assert!(body["id"].as_i64().is_some());
    app.close().await;
}

#[tokio::test]
async fn create_empty_name_is_422() {
    let app = TestApp::new().await;
    for bad in [json!({ "name": "" }), json!({ "name": "   " })] {
        let resp = app
            .request_json(Method::POST, "/api/v1/instances", bad)
            .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "VALIDATION");
    }
    app.close().await;
}

#[tokio::test]
async fn create_too_long_name_is_422() {
    let app = TestApp::new().await;
    let long_name = "x".repeat(65);
    let resp = app
        .request_json(
            Method::POST,
            "/api/v1/instances",
            json!({ "name": long_name }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    app.close().await;
}

#[tokio::test]
async fn create_malformed_body_is_422() {
    let app = TestApp::new().await;
    let resp = app
        .request_json(Method::POST, "/api/v1/instances", json!({}))
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    app.close().await;
}

#[tokio::test]
async fn create_non_json_body_follows_error_contract() {
    let app = TestApp::new().await;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    // 非 JSON 文本：rejection 也必须是统一 error 契约（review 补强）。
    // P8：手工构造请求需自带会话 + CSRF（protected 路由）。
    let (session_id, csrf) = app.session().expect("TestApp logged in");
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/instances")
        .header("content-type", "application/json")
        .header("cookie", format!("warpdeck_session={session_id}"))
        .header("x-csrf-token", csrf)
        .body(Body::from("this is not json"))
        .unwrap();
    let router = crate::app::router(app.state_for_test(), app.ui_dir_for_test());
    let resp = router.oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "VALIDATION");
    assert!(body["error"]["request_id"].as_str().is_some());
    app.close().await;
}

// ---------- Instances: start / stop ----------

#[tokio::test]
async fn start_sets_desired_running_and_returns_202() {
    let app = TestApp::new().await;
    let id = create_instance(&app, "workload1").await;

    let resp = app
        .request(Method::POST, &format!("/api/v1/instances/{id}/start"))
        .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let resp = app
        .request(Method::GET, &format!("/api/v1/instances/{id}"))
        .await;
    let body = body_json(resp).await;
    assert_eq!(body["desired_state"], "running");

    // 幂等：再次 start 仍 202。
    let resp = app
        .request(Method::POST, &format!("/api/v1/instances/{id}/start"))
        .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    app.close().await;
}

#[tokio::test]
async fn stop_sets_desired_stopped_and_returns_202() {
    let app = TestApp::new().await;
    let id = create_instance(&app, "workload2").await;

    let resp = app
        .request(Method::POST, &format!("/api/v1/instances/{id}/stop"))
        .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let resp = app
        .request(Method::GET, &format!("/api/v1/instances/{id}"))
        .await;
    let body = body_json(resp).await;
    assert_eq!(body["desired_state"], "stopped");
    app.close().await;
}

#[tokio::test]
async fn start_on_missing_instance_is_404() {
    let app = TestApp::new().await;
    let resp = app
        .request(Method::POST, "/api/v1/instances/4242/start")
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "NOT_FOUND");
    app.close().await;
}

#[tokio::test]
async fn stop_on_missing_instance_is_404() {
    let app = TestApp::new().await;
    let resp = app
        .request(Method::POST, "/api/v1/instances/4242/stop")
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    app.close().await;
}

// ---------- Instances: restart ----------

/// P1 审查 R2#1：restart 只写命令代数（202 受理），**不直接调用运行时**；
/// 实际重启由 Reconciler 消费代数差完成。
#[tokio::test]
async fn restart_running_instance_writes_generation_and_returns_202() {
    let app = TestApp::new().await;
    let id = create_instance(&app, "restartable").await;
    // 布置实际状态：运行中（快速失败 UX 的 registry 读）。
    let fake = app.runtime();
    let id_i = crate::runtime::instance::InstanceId::from_db(id).unwrap();
    fake.registry().insert(id_i);
    fake.set_state(id_i, crate::runtime::registry::RuntimeState::Healthy);

    let resp = app
        .request(Method::POST, &format!("/api/v1/instances/{id}/restart"))
        .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    // 运行时未被触碰（Reconciler 才是唯一写者）。
    assert!(
        fake.restarted_ids().is_empty(),
        "API 不得直接调用 runtime.restart"
    );
    // 命令代数已写入期望侧。
    let gen: i64 =
        sqlx::query_scalar("SELECT restart_command_generation FROM warp_instances WHERE id = ?")
            .bind(id)
            .fetch_one(&app.pool_for_test())
            .await
            .unwrap();
    assert_eq!(gen, 1, "restart 命令必须递增 restart_command_generation");
    app.close().await;
}

#[tokio::test]
async fn restart_stopped_instance_is_409() {
    let app = TestApp::new().await;
    let id = create_instance(&app, "not-running").await;
    let resp = app
        .request(Method::POST, &format!("/api/v1/instances/{id}/restart"))
        .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "CONFLICT");
    app.close().await;
}

#[tokio::test]
async fn restart_missing_instance_is_404() {
    let app = TestApp::new().await;
    let resp = app
        .request(Method::POST, "/api/v1/instances/4242/restart")
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    app.close().await;
}

// ---------- Instances: delete ----------

/// P1 审查 R2#1：delete 只删期望行 + 触发收敛（202 受理）；运行中实例由
/// Reconciler 孤儿收敛停止——API 不直接碰运行时。
#[tokio::test]
async fn delete_removes_instance_and_returns_202() {
    let app = TestApp::new().await;
    let id = create_instance(&app, "ephemeral").await;

    let resp = app
        .request(Method::DELETE, &format!("/api/v1/instances/{id}"))
        .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let resp = app
        .request(Method::GET, &format!("/api/v1/instances/{id}"))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    app.close().await;
}

#[tokio::test]
async fn delete_missing_instance_is_404() {
    let app = TestApp::new().await;
    let resp = app.request(Method::DELETE, "/api/v1/instances/4242").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    app.close().await;
}

// ---------- Instances: PATCH rebind profile (v0.2 §17.4) ----------

/// 创建 zero_trust 模式账号档案（返回其 id）。
/// 注：free 全局唯一（§16.9），测试辅助一律建 ZT 档避免与默认档冲突。
async fn create_profile(app: &TestApp, name: &str) -> i64 {
    let resp = app
        .request_json(
            Method::POST,
            "/api/v1/accounts",
            json!({
                "name": name,
                "mode": "zero_trust",
                "zero_trust_org": "demo-org",
                "client_id": "cid",
                "client_secret": "csecret",
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "create profile {name}");
    let body = body_json(resp).await;
    body["id"].as_i64().expect("created profile id")
}

#[tokio::test]
async fn create_with_profile_binds_and_view_exposes_account() {
    let app = TestApp::new().await;
    let pid = create_profile(&app, "work").await;
    let resp = app
        .request_json(
            Method::POST,
            "/api/v1/instances",
            json!({ "name": "bound", "account_profile_id": pid }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    // 创建响应就带档案摘要（UI 无需二次请求）。
    assert_eq!(body["account"]["profile_id"], pid);
    assert_eq!(body["account"]["name"], "work");
    assert_eq!(body["account"]["mode"], "zero_trust");

    let resp = app.request(Method::GET, "/api/v1/instances").await;
    let list = body_json(resp).await;
    let view = &list.as_array().unwrap()[0];
    assert_eq!(view["account"]["profile_id"], pid);
    app.close().await;
}

#[tokio::test]
async fn create_with_nonexistent_profile_is_422() {
    let app = TestApp::new().await;
    let resp = app
        .request_json(
            Method::POST,
            "/api/v1/instances",
            json!({ "name": "x", "account_profile_id": 999 }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "VALIDATION");
    app.close().await;
}

#[tokio::test]
async fn patch_rebind_profile_updates_view() {
    let app = TestApp::new().await;
    let pid_a = create_profile(&app, "work").await;
    let pid_b = create_profile(&app, "gaming").await;
    let id = create_instance(&app, "dual").await;

    // 初始：未绑定 → 默认 free 档展开。
    let resp = app
        .request(Method::GET, &format!("/api/v1/instances/{id}"))
        .await;
    let view = body_json(resp).await;
    assert_eq!(view["account"]["profile_id"], 1);
    assert_eq!(view["account"]["mode"], "free");

    // 改绑 A。
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/instances/{id}"),
            json!({ "account_profile_id": pid_a }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let view = body_json(resp).await;
    assert_eq!(view["account"]["profile_id"], pid_a);
    assert_eq!(view["account"]["name"], "work");

    // 改绑 B，然后解绑（null=默认档）。
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/instances/{id}"),
            json!({ "account_profile_id": pid_b }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/instances/{id}"),
            json!({ "account_profile_id": null }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let view = body_json(resp).await;
    assert_eq!(view["account"]["profile_id"], 1);
    app.close().await;
}

#[tokio::test]
async fn only_one_free_profile_is_allowed() {
    let app = TestApp::new().await;
    // 默认档已覆盖 free → 再建 free 档 409。
    let resp = app
        .request_json(
            Method::POST,
            "/api/v1/accounts",
            json!({ "name": "dup-free", "mode": "free" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // free 档只读：改名 / 升级均 409。
    let resp = app
        .request_json(
            Method::PATCH,
            "/api/v1/accounts/1",
            json!({ "name": "renamed" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "free 档不能改名");
    let resp = app
        .request_json(
            Method::PATCH,
            "/api/v1/accounts/1",
            json!({ "mode": "warp_plus", "license": "WPL-MAIN" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "free 档不能升级");

    // 非 free 档改成 free 同样被拒（默认档还没释放名额）。
    let pid = create_profile(&app, "zt-a").await;
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/accounts/{pid}"),
            json!({ "mode": "free" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    app.close().await;
}

#[tokio::test]
async fn bound_profile_is_read_only_until_unbound() {
    let app = TestApp::new().await;
    let pid = create_profile(&app, "team-a").await;
    let id = create_instance(&app, "worker").await;

    // 未绑定时可编辑。
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/accounts/{pid}"),
            json!({ "name": "team-a-2" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // 绑定实例后：改名/改模式均 409。
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/instances/{id}"),
            json!({ "account_profile_id": pid }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/accounts/{pid}"),
            json!({ "name": "team-a-3" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/accounts/{pid}"),
            json!({ "mode": "free" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // 解绑后可重新编辑。
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/instances/{id}"),
            json!({ "account_profile_id": null }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/accounts/{pid}"),
            json!({ "name": "team-a-4" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    app.close().await;
}

#[tokio::test]
async fn warp_plus_profile_is_single_instance() {
    let app = TestApp::new().await;
    // WARP+ 档（带 license）。
    let resp = app
        .request_json(
            Method::POST,
            "/api/v1/accounts",
            json!({ "name": "plus-a", "mode": "warp_plus", "license": "WPL-A" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let pid = body_json(resp).await["id"].as_i64().unwrap();

    // 实例 A 绑定成功。
    let resp = app
        .request_json(
            Method::POST,
            "/api/v1/instances",
            json!({ "name": "a", "account_profile_id": pid }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let id_a = body_json(resp).await["id"].as_i64().unwrap();

    // 实例 B 创建时绑同一档 → 409。
    let resp = app
        .request_json(
            Method::POST,
            "/api/v1/instances",
            json!({ "name": "b", "account_profile_id": pid }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // 已绑实例 A 重绑同一档 → 200（排除自身，幂等）。
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/instances/{id_a}"),
            json!({ "account_profile_id": pid }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // 其他实例改绑同一档 → 409。
    let id_b = create_instance(&app, "b2").await;
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/instances/{id_b}"),
            json!({ "account_profile_id": pid }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // A 解绑回默认后，B 可绑定。
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/instances/{id_a}"),
            json!({ "account_profile_id": null }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/instances/{id_b}"),
            json!({ "account_profile_id": pid }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    app.close().await;
}

#[tokio::test]
async fn patch_rebind_nonexistent_profile_is_422() {
    let app = TestApp::new().await;
    let id = create_instance(&app, "xp").await;
    let resp = app
        .request_json(
            Method::PATCH,
            &format!("/api/v1/instances/{id}"),
            json!({ "account_profile_id": 999 }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "VALIDATION");
    app.close().await;
}

#[tokio::test]
async fn patch_empty_body_is_422() {
    let app = TestApp::new().await;
    let id = create_instance(&app, "xp").await;
    let resp = app
        .request_json(Method::PATCH, &format!("/api/v1/instances/{id}"), json!({}))
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "VALIDATION");
    app.close().await;
}

#[tokio::test]
async fn patch_missing_instance_is_404() {
    let app = TestApp::new().await;
    let resp = app
        .request_json(
            Method::PATCH,
            "/api/v1/instances/4242",
            json!({ "account_profile_id": null }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    app.close().await;
}

// ---------- Proxy ----------

#[tokio::test]
async fn proxy_get_returns_defaults_without_secrets() {
    let app = TestApp::new().await;
    let resp = app.request(Method::GET, "/api/v1/proxy").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["socks5_enabled"], true);
    assert_eq!(body["http_enabled"], true);
    assert_eq!(body["auth_enabled"], false);
    assert_eq!(body["auth_configured"], false);
    // 秘密边界：绝无 username/password 字段（AGENTS.md）。
    assert!(body.get("proxy_username").is_none());
    assert!(body.get("proxy_password").is_none());
    app.close().await;
}

#[tokio::test]
async fn proxy_put_partial_update_applies() {
    let app = TestApp::new().await;
    let resp = app
        .request_json(
            Method::PUT,
            "/api/v1/proxy",
            json!({ "socks5_enabled": false, "max_rps": 5 }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["socks5_enabled"], false);
    assert_eq!(body["http_enabled"], true); // 未提供的字段保持原值
    assert_eq!(body["max_rps"], 5);

    // 持久化有效。
    let resp = app.request(Method::GET, "/api/v1/proxy").await;
    let body = body_json(resp).await;
    assert_eq!(body["socks5_enabled"], false);
    app.close().await;
}

#[tokio::test]
async fn proxy_put_zero_limits_is_422() {
    let app = TestApp::new().await;
    let resp = app
        .request_json(
            Method::PUT,
            "/api/v1/proxy",
            json!({ "max_connections": 0 }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "VALIDATION");
    app.close().await;
}

/// P1 审查 R1#7：显式 null = 清除限额；缺省 = 保持。单层 Option 时代
/// 「设了就清不掉」的回归探针。
#[tokio::test]
async fn proxy_put_null_clears_limit_and_omission_keeps_it() {
    let app = TestApp::new().await;
    // 先设置限额。
    let resp = app
        .request_json(
            Method::PUT,
            "/api/v1/proxy",
            json!({ "max_connections": 64 }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(app.request(Method::GET, "/api/v1/proxy").await).await;
    assert_eq!(body["max_connections"], 64);

    // 缺省（不提供字段）→ 保持。
    let resp = app
        .request_json(
            Method::PUT,
            "/api/v1/proxy",
            json!({ "http_enabled": true }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(app.request(Method::GET, "/api/v1/proxy").await).await;
    assert_eq!(body["max_connections"], 64, "缺省必须保持原值");

    // 显式 null → 清除。
    let resp = app
        .request_json(
            Method::PUT,
            "/api/v1/proxy",
            json!({ "max_connections": null }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(app.request(Method::GET, "/api/v1/proxy").await).await;
    assert!(
        body["max_connections"].is_null(),
        "显式 null 必须清除限额（P1 审查 R1#7）"
    );
    app.close().await;
}

// ---------- Account skeleton (P7-011) ----------

#[tokio::test]
async fn account_returns_masked_defaults() {
    let app = TestApp::new().await;
    let resp = app.request(Method::GET, "/api/v1/account").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["mode"], "free");
    assert_eq!(body["configured"], false);
    assert_eq!(body["license_present"], false);
    assert_eq!(body["zero_trust_configured"], false);
    // 绝不返回凭据明文。
    assert!(body.get("license").is_none());
    assert!(body.get("token").is_none());
    app.close().await;
}

// ---------- SSE (P7-009) ----------

#[tokio::test]
async fn events_endpoint_is_event_stream() {
    let app = TestApp::new().await;
    let resp = app.request(Method::GET, "/api/v1/events").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        content_type.starts_with("text/event-stream"),
        "expected SSE content type, got {content_type}"
    );
    // 流不消费（长连接语义由客户端断开）；此处只验证握手。
    drop(resp);
    app.close().await;
}

// ---------- Error contract ----------

#[tokio::test]
async fn every_error_body_carries_request_id_code_message() {
    let app = TestApp::new().await;
    for (method, uri) in [
        (Method::GET, "/api/v1/instances/9999"),
        (Method::POST, "/api/v1/instances/9999/start"),
        (Method::POST, "/api/v1/instances/9999/restart"),
    ] {
        let resp = app.request(method.clone(), uri).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{method} {uri}");
        let has_rid_header = resp.headers().get("x-request-id").is_some();
        let body = body_json(resp).await;
        let error = &body["error"];
        assert_eq!(error["code"], "NOT_FOUND", "{uri}");
        assert!(error["message"].as_str().is_some(), "{uri}");
        let rid = error["request_id"].as_str().unwrap();
        assert!(!rid.is_empty(), "{uri}");
        // 响应头与 body 一致（契约自检）。
        assert!(has_rid_header, "{uri}");
    }
    app.close().await;
}

// ---------- Logs (P10-006) ----------

#[tokio::test]
async fn log_sources_are_authenticated_and_enum_existing_files() {
    let mut app = TestApp::new_unauthenticated().await;

    // 未认证 → 401（protected 区）。
    let resp = app.request(Method::GET, "/api/v1/logs/sources").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    app.setup_and_login().await;

    // 在 data dir 写入一个实例日志，验证 sources 枚举。
    let logs_dir = app.data_dir_for_test().join("logs");
    std::fs::create_dir_all(&logs_dir).unwrap();
    std::fs::write(logs_dir.join("instance-2.log"), "x").unwrap();

    let resp = app.request(Method::GET, "/api/v1/logs/sources").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let sources = body.as_array().unwrap();
    let ids: Vec<&str> = sources
        .iter()
        .map(|s| s["source"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"manager"));
    assert!(ids.contains(&"gost"));
    assert!(ids.contains(&"instance:2"));
    let instance = sources
        .iter()
        .find(|s| s["source"] == "instance:2")
        .unwrap();
    assert_eq!(instance["kind"], "instance");
    assert_eq!(instance["instance_id"], 2);
    app.close().await;
}

#[tokio::test]
async fn log_history_paginates_and_redacts_process_lines() {
    let app = TestApp::new().await;
    let logs_dir = app.data_dir_for_test().join("logs");
    std::fs::create_dir_all(&logs_dir).unwrap();

    // manager.log：结构化行原样返回。
    let mut manager = String::new();
    for i in 1..=5 {
        manager.push_str(&format!("2026-08-18T00:00:0{i}Z INFO line-{i}\n"));
    }
    std::fs::write(logs_dir.join("manager.log"), manager).unwrap();

    // instance-0.log：进程输出行按模式片段脱敏（敏感值替换、其余保留）。
    std::fs::write(
        logs_dir.join("instance-0.log"),
        "registration token abc\nwarp: connected\n",
    )
    .unwrap();

    let resp = app
        .request(Method::GET, "/api/v1/logs?source=manager&limit=2")
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["source"], "manager");
    assert_eq!(body["offset"], 0);
    assert_eq!(body["next_offset"], 1);
    assert!(body["has_more"].as_bool().unwrap());
    let lines: Vec<&str> = body["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    // 最新一页（旧→新）。
    assert_eq!(
        lines,
        vec![
            "2026-08-18T00:00:04Z INFO line-4",
            "2026-08-18T00:00:05Z INFO line-5"
        ]
    );

    // 向前翻页：line-2/3。
    let resp = app
        .request(Method::GET, "/api/v1/logs?source=manager&limit=2&offset=1")
        .await;
    let body = body_json(resp).await;
    let lines: Vec<&str> = body["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert_eq!(
        lines,
        vec![
            "2026-08-18T00:00:02Z INFO line-2",
            "2026-08-18T00:00:03Z INFO line-3"
        ]
    );

    // 实例日志：敏感值片段脱敏，普通内容原样（P1 审查 R2：模式化脱敏）。
    let resp = app
        .request(Method::GET, "/api/v1/logs?source=instance:0")
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let lines: Vec<&str> = body["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap())
        .collect();
    assert_eq!(
        lines,
        vec!["registration token [REDACTED]", "warp: connected"]
    );

    // 未知源 → 422 VALIDATION。
    let resp = app.request(Method::GET, "/api/v1/logs?source=bogus").await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "VALIDATION");

    // 无文件源 → 空页（200，has_more=false）。
    let resp = app.request(Method::GET, "/api/v1/logs?source=gost").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(!body["has_more"].as_bool().unwrap());
    assert_eq!(body["lines"].as_array().unwrap().len(), 0);
    app.close().await;
}
