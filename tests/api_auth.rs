use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use codex_proxy_rs::admin::AdminAuth;
use codex_proxy_rs::api::{self, AppState};
use codex_proxy_rs::core::Manager;
use codex_proxy_rs::quota::QuotaChecker;
use codex_proxy_rs::refresh::{Refresher, SaveQueue};
use codex_proxy_rs::upstream::codex::CodexClient;
use tower::util::ServiceExt;
use url::Url;

fn build_state(api_keys: &[&str]) -> AppState {
    let dir = tempfile::tempdir().expect("tempdir");

    let keys: HashSet<String> = api_keys.iter().map(|s| s.to_string()).collect();

    AppState {
        manager: Arc::new(Manager::new(dir.path())),
        quota_checker: Arc::new(
            QuotaChecker::new(
                "https://chatgpt.com/backend-api/codex",
                "chatgpt.com",
                "",
                1,
            )
            .unwrap(),
        ),
        codex_client: Arc::new(
            CodexClient::new(
                Url::parse("https://chatgpt.com/backend-api/codex").unwrap(),
                "",
            )
            .unwrap(),
        ),
        request_stats: Arc::new(api::RequestStats::default()),
        api_keys: Arc::new(keys),
        max_retry: 0,
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
        runtime_state: Arc::new(codex_proxy_rs::state::RuntimeStateStore::new(dir.path())),
        on_401: None,
        rate_limiter: Arc::new(codex_proxy_rs::limit::RateLimiter::default()),
        persist_store: None,
    }
}

#[tokio::test]
async fn admin_auth_setup_login_and_change_password() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.yaml");
    std::fs::write(
        &config_path,
        r#"# top comment
listen: ":18080"

# admin comment
admin:
  # username comment
  username: "admin"
  password-hash: ""

# keep this
api-keys:
  - "sk-123"
"#,
    )
    .unwrap();
    api::set_admin_auth(Arc::new(AdminAuth::new(
        &config_path,
        "admin".to_string(),
        String::new(),
    )));

    let app = api::router(build_state(&[]));

    let setup_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/setup")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "username": "admin",
                        "password": "password123"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(setup_res.status(), StatusCode::OK);
    let setup_body = axum::body::to_bytes(setup_res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let setup_json: serde_json::Value = serde_json::from_slice(&setup_body).unwrap();
    let token = setup_json["data"]["token"].as_str().unwrap().to_string();

    let config = std::fs::read_to_string(&config_path).unwrap();
    assert!(config.contains("# top comment"));
    assert!(config.contains("# admin comment"));
    assert!(config.contains("# keep this"));
    assert!(config.contains("listen: \":18080\""));
    assert!(config.contains("api-keys:"));
    assert!(config.contains("password-hash"));
    assert!(!config.contains("password123"));

    let stats_without_token = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stats_without_token.status(), StatusCode::UNAUTHORIZED);

    let stats_with_token = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/stats")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stats_with_token.status(), StatusCode::OK);

    let logout_without_token = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/logout")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logout_without_token.status(), StatusCode::UNAUTHORIZED);

    let change_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/change-password")
                .header("Authorization", format!("Bearer {token}"))
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "current_password": "password123",
                        "new_password": "password456"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(change_res.status(), StatusCode::OK);

    let old_token_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/stats")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(old_token_res.status(), StatusCode::UNAUTHORIZED);

    let login_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/login")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "username": "admin",
                        "password": "password456"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(login_res.status(), StatusCode::OK);
    let login_body = axum::body::to_bytes(login_res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let login_json: serde_json::Value = serde_json::from_slice(&login_body).unwrap();
    let new_token = login_json["data"]["token"].as_str().unwrap().to_string();

    let logout_with_token = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/logout")
                .header("Authorization", format!("Bearer {new_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logout_with_token.status(), StatusCode::OK);

    let logged_out_res = app
        .oneshot(
            Request::builder()
                .uri("/stats")
                .header("Authorization", format!("Bearer {new_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logged_out_res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_auth_middleware_allows_when_no_keys_configured() {
    let app = api::router(build_state(&[]));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/check-quota")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn api_auth_middleware_rejects_missing_key() {
    let app = api::router(build_state(&["k1"]));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/check-quota")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes);
    assert!(
        body.contains("invalid_api_key"),
        "expected invalid_api_key, got body: {body}"
    );
}

#[tokio::test]
async fn api_auth_middleware_accepts_authorization_bearer() {
    let app = api::router(build_state(&["k1"]));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/check-quota")
                .header("Authorization", "Bearer k1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn api_auth_middleware_accepts_x_api_key_header() {
    let app = api::router(build_state(&["k1"]));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/check-quota")
                .header("x-api-key", "k1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn api_auth_middleware_protects_v1_routes() {
    let app = api::router(build_state(&["k1"]));

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let res = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("Authorization", "Bearer k1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn removed_conversion_routes_are_not_registered() {
    let app = api::router(build_state(&[]));

    for path in [
        "/v1/chat/completions",
        "/v1/completions",
        "/v1/messages",
        "/v1/messages/count_tokens",
        "/v1/images/generations",
        "/v1/images/edits",
    ] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                res.status(),
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ),
            "{path}: {}",
            res.status()
        );
    }
}

#[tokio::test]
async fn api_auth_middleware_does_not_protect_health() {
    let app = api::router(build_state(&["k1"]));
    let res = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}
