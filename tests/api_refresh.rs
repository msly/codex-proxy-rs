use std::collections::HashSet;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::{get, post};
use codex_proxy_rs::api::{self, AppState};
use codex_proxy_rs::core::Manager;
use codex_proxy_rs::quota::QuotaChecker;
use codex_proxy_rs::refresh::{Refresher, SaveQueue};
use codex_proxy_rs::upstream::codex::CodexClient;
use tower::util::ServiceExt;
use url::Url;

#[derive(Clone)]
struct UpstreamState;

async fn oauth_token(
    State(_state): State<UpstreamState>,
    _headers: HeaderMap,
    _body: String,
) -> axum::response::Json<serde_json::Value> {
    axum::response::Json(serde_json::json!({
        "access_token": "at-new",
        "refresh_token": "rt-new",
        "id_token": "",
        "expires_in": 3600
    }))
}

async fn wham_usage(
    State(_state): State<UpstreamState>,
) -> axum::response::Json<serde_json::Value> {
    axum::response::Json(serde_json::json!({
        "rate_limit": { "primary_window": { "used_percent": 1.23 } }
    }))
}

async fn start_mock() -> (Url, Url) {
    let app = Router::new()
        .route("/oauth/token", post(oauth_token))
        .route("/backend-api/wham/usage", get(wham_usage))
        .with_state(UpstreamState);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let token_url = Url::parse(&format!("http://{addr}/oauth/token")).unwrap();
    let base_url = Url::parse(&format!("http://{addr}/backend-api/codex/")).unwrap();
    (token_url, base_url)
}

#[tokio::test]
async fn api_refresh_no_accounts_emits_done_event() {
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(Manager::new(dir.path()));

    let state = AppState {
        manager,
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
        api_keys: Arc::new(HashSet::new()),
        max_retry: 0,
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
    };

    let app = api::router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/refresh")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "text/event-stream"
    );

    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes);
    assert!(
        body.contains("\"type\":\"done\""),
        "expected done event, got body: {body}"
    );
    assert!(
        body.contains("无账号"),
        "expected '无账号' message, got body: {body}"
    );
}

#[tokio::test]
async fn api_refresh_with_account_emits_item_and_done_events() {
    let (token_url, base_url) = start_mock().await;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.json"),
        serde_json::json!({
            "access_token": "at-old",
            "refresh_token": "rt-old",
            "account_id": "",
            "email": "a@example.com",
            "type": "codex",
            "expired": "2000-01-01T00:00:00Z"
        })
        .to_string(),
    )
    .unwrap();

    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let state = AppState {
        manager,
        quota_checker: Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap()),
        codex_client: Arc::new(CodexClient::new(base_url, "").unwrap()),
        api_keys: Arc::new(HashSet::new()),
        max_retry: 0,
        refresher: Refresher::new("").unwrap().with_token_url(token_url),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 2,
    };

    let app = api::router(state);
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/refresh")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes);

    assert!(
        body.contains("\"type\":\"item\""),
        "expected item event, got body: {body}"
    );
    assert!(
        body.contains("\"type\":\"done\""),
        "expected done event, got body: {body}"
    );
    assert!(
        body.contains("\"success\":true"),
        "expected success=true, got body: {body}"
    );
}
