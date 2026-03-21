use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
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
        empty_retry_max: 0,
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
        runtime_state: Arc::new(codex_proxy_rs::state::RuntimeStateStore::new(dir.path())),
        on_401: None,
    }
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
