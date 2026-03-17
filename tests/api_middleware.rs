use std::collections::HashSet;
use std::io::Read;
use std::sync::Arc;

use axum::http::{Request, StatusCode, header};
use codex_proxy_rs::api::{self, AppState};
use codex_proxy_rs::core::Manager;
use codex_proxy_rs::quota::QuotaChecker;
use codex_proxy_rs::refresh::{Refresher, SaveQueue};
use codex_proxy_rs::upstream::codex::CodexClient;
use flate2::read::GzDecoder;
use tower::util::ServiceExt;
use url::Url;

fn make_state(dir: &tempfile::TempDir, api_key_enabled: bool) -> AppState {
    let manager = Arc::new(Manager::new(dir.path()));
    let base_url = Url::parse("http://example.com/backend-api/codex/").unwrap();
    let quota_checker = Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap());
    let codex_client = Arc::new(CodexClient::new(base_url, "").unwrap());

    let mut keys = HashSet::new();
    if api_key_enabled {
        keys.insert("k1".to_string());
    }

    AppState {
        manager,
        quota_checker,
        codex_client,
        api_keys: Arc::new(keys),
        max_retry: 0,
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
        on_401: None,
    }
}

#[tokio::test]
async fn api_options_preflight_bypasses_auth_and_sets_headers() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_state(&dir, true);

    let app = api::router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/v1/responses")
                .header("Origin", "https://example.com")
                .header("Access-Control-Request-Method", "POST")
                .header(
                    "Access-Control-Request-Headers",
                    "Authorization, Content-Type",
                )
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        res.headers()
            .get("Access-Control-Allow-Origin")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "https://example.com"
    );
    assert_eq!(
        res.headers()
            .get("Access-Control-Allow-Methods")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "POST"
    );
    assert_eq!(
        res.headers()
            .get("Access-Control-Allow-Headers")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "Authorization, Content-Type"
    );
    assert_eq!(
        res.headers()
            .get("Access-Control-Max-Age")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "86400"
    );
}

#[tokio::test]
async fn api_cors_header_present_on_health() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_state(&dir, true);

    let app = api::router(state);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .header("Origin", "https://example.com")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get("Access-Control-Allow-Origin")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "https://example.com"
    );
    assert_eq!(
        res.headers()
            .get(header::VARY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "Origin"
    );
}

#[tokio::test]
async fn api_gzip_applies_to_non_v1_routes() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_state(&dir, false);

    let app = api::router(state);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "gzip"
    );

    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let mut decoder = GzDecoder::new(&bytes[..]);
    let mut out = String::new();
    decoder.read_to_string(&mut out).unwrap();
    assert!(out.contains("数据统计与展示"));
}

#[tokio::test]
async fn api_gzip_does_not_apply_to_v1_routes() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_state(&dir, false);

    let app = api::router(state);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().get(header::CONTENT_ENCODING).is_none());
}

#[tokio::test]
async fn api_sse_is_not_compressed() {
    let dir = tempfile::tempdir().unwrap();
    let state = make_state(&dir, false);

    let app = api::router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/check-quota")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/event-stream"
    );
    assert!(res.headers().get(header::CONTENT_ENCODING).is_none());
}
