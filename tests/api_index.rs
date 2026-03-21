use std::collections::HashSet;
use std::sync::Arc;

use axum::http::{Request, StatusCode, header};
use codex_proxy_rs::api::{self, AppState};
use codex_proxy_rs::core::Manager;
use codex_proxy_rs::quota::QuotaChecker;
use codex_proxy_rs::refresh::{Refresher, SaveQueue};
use codex_proxy_rs::upstream::codex::CodexClient;
use tower::util::ServiceExt;
use url::Url;

#[tokio::test]
async fn api_index_is_public_and_returns_html() {
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(Manager::new(dir.path()));

    let base_url = Url::parse("http://example.com/backend-api/codex/").unwrap();
    let quota_checker = Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap());
    let codex_client = Arc::new(CodexClient::new(base_url, "").unwrap());

    let mut keys = HashSet::new();
    keys.insert("k1".to_string());

    let state = AppState {
        manager,
        quota_checker,
        codex_client,
        request_stats: Arc::new(api::RequestStats::default()),
        api_keys: Arc::new(keys),
        max_retry: 0,
        empty_retry_max: 0,
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
        runtime_state: Arc::new(codex_proxy_rs::state::RuntimeStateStore::new(dir.path())),
        on_401: None,
    };

    let app = api::router(state);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let ct = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.to_lowercase().starts_with("text/html"),
        "unexpected content-type: {ct}"
    );
}

#[tokio::test]
async fn api_index_exposes_quota_controls_and_sorting() {
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(Manager::new(dir.path()));

    let base_url = Url::parse("http://example.com/backend-api/codex/").unwrap();
    let quota_checker = Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap());
    let codex_client = Arc::new(CodexClient::new(base_url, "").unwrap());

    let state = AppState {
        manager,
        quota_checker,
        codex_client,
        request_stats: Arc::new(api::RequestStats::default()),
        api_keys: Arc::new(HashSet::new()),
        max_retry: 0,
        empty_retry_max: 0,
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
        runtime_state: Arc::new(codex_proxy_rs::state::RuntimeStateStore::new(dir.path())),
        on_401: None,
    };

    let app = api::router(state);
    let res = app
        .oneshot(
            Request::builder()
                .uri("/")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&body);

    assert!(
        html.contains("id=\"checkQuotaBtn\""),
        "expected check quota button in index html"
    );
    assert!(
        html.contains("data-sort-key=\"quota_remaining_sort\""),
        "expected sortable quota column in index html"
    );
    assert!(
        html.contains("return 0.001"),
        "expected quota sorting to distinguish unknown from exhausted"
    );
}
