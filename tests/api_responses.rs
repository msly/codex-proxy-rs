use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::Router;
use codex_proxy_rs::api::{self, AppState};
use codex_proxy_rs::core::Manager;
use codex_proxy_rs::quota::QuotaChecker;
use codex_proxy_rs::upstream::codex::CodexClient;
use tower::util::ServiceExt;
use url::Url;

#[derive(Clone)]
struct UpstreamState {
    calls: Arc<AtomicUsize>,
}

async fn upstream_responses(
    State(state): State<UpstreamState>,
    headers: HeaderMap,
) -> (axum::http::StatusCode, &'static str) {
    state.calls.fetch_add(1, Ordering::Relaxed);
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    match auth {
        "Bearer at1" => (axum::http::StatusCode::UNAUTHORIZED, "unauthorized"),
        "Bearer at2" => {
            if accept.contains("text/event-stream") {
                (axum::http::StatusCode::OK, "data: ok\n\n")
            } else {
                (axum::http::StatusCode::OK, r#"{"id":"r1","object":"response"}"#)
            }
        }
        _ => (axum::http::StatusCode::FORBIDDEN, "forbidden"),
    }
}

async fn start_upstream(calls: Arc<AtomicUsize>) -> Url {
    let app = Router::new()
        .route("/backend-api/codex/responses", post(upstream_responses))
        .with_state(UpstreamState { calls });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    Url::parse(&format!("http://{addr}/backend-api/codex/")).unwrap()
}

async fn write_auth_file(dir: &std::path::Path, name: &str, access_token: &str) {
    let path = dir.join(name);
    std::fs::write(
        &path,
        serde_json::json!({
            "access_token": access_token,
            "refresh_token": "rt",
            "account_id": "",
            "email": "x@example.com",
            "type": "codex",
            "expired": "2099-01-01T00:00:00Z"
        })
        .to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn api_v1_responses_stream_passthrough_uses_internal_retry_gate() {
    let calls = Arc::new(AtomicUsize::new(0));
    let base_url = start_upstream(calls.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at1").await;
    write_auth_file(dir.path(), "b.json", "at2").await;

    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let state = AppState {
        manager: manager.clone(),
        quota_checker: Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap()),
        codex_client: Arc::new(CodexClient::new(base_url, "").unwrap()),
        api_keys: Arc::new(HashSet::new()),
        max_retry: 1,
    };

    let app = api::router(state);
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "model": "gpt-5.4",
                        "stream": true,
                        "input": "hi"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), axum::http::StatusCode::OK);
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
    assert_eq!(body, "data: ok\n\n");
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn api_v1_responses_non_stream_passthrough_returns_json() {
    let calls = Arc::new(AtomicUsize::new(0));
    let base_url = start_upstream(calls.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at1").await;
    write_auth_file(dir.path(), "b.json", "at2").await;

    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let state = AppState {
        manager: manager.clone(),
        quota_checker: Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap()),
        codex_client: Arc::new(CodexClient::new(base_url, "").unwrap()),
        api_keys: Arc::new(HashSet::new()),
        max_retry: 1,
    };

    let app = api::router(state);
    let res = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "input": "hi"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), axum::http::StatusCode::OK);
    assert_eq!(
        res.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "application/json"
    );

    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes);
    assert_eq!(body, r#"{"id":"r1","object":"response"}"#);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}
