use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use codex_proxy_rs::api::{self, AppState};
use codex_proxy_rs::core::Manager;
use codex_proxy_rs::quota::QuotaChecker;
use codex_proxy_rs::refresh::{Refresher, SaveQueue};
use codex_proxy_rs::upstream::codex::{CodexClient, On401Hook};
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
                (
                    axum::http::StatusCode::OK,
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7,\"total_tokens\":18}}}\n\n",
                )
            } else {
                (
                    axum::http::StatusCode::OK,
                    r#"{"id":"r1","object":"response","usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18}}"#,
                )
            }
        }
        "Bearer at3" => {
            if accept.contains("text/event-stream") {
                (
                    axum::http::StatusCode::OK,
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r3\",\"usage\":{\"input_tokens\":12,\"input_tokens_details\":{\"cached_tokens\":4},\"output_tokens\":8,\"output_tokens_details\":{\"reasoning_tokens\":2},\"total_tokens\":20}}}\n\n",
                )
            } else {
                (
                    axum::http::StatusCode::OK,
                    r#"{"id":"r3","object":"response","usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":4},"output_tokens":8,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":20}}"#,
                )
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
    let runtime_state = Arc::new(codex_proxy_rs::state::RuntimeStateStore::new(dir.path()));

    let hook_calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let hook_calls2 = hook_calls.clone();
    let seen2 = seen.clone();
    let on_401: On401Hook = Arc::new(move |acc| {
        hook_calls2.fetch_add(1, Ordering::Relaxed);
        seen2
            .lock()
            .expect("mutex poisoned")
            .push(acc.file_path().to_string());
    });

    let state = AppState {
        manager: manager.clone(),
        quota_checker: Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap()),
        codex_client: Arc::new(CodexClient::new(base_url, "").unwrap()),
        request_stats: Arc::new(api::RequestStats::default()),
        api_keys: Arc::new(HashSet::new()),
        max_retry: 1,
        empty_retry_max: 0,
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
        runtime_state: runtime_state.clone(),
        on_401: Some(on_401),
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
    assert_eq!(
        body,
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7,\"total_tokens\":18}}}\n\n"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(hook_calls.load(Ordering::Relaxed), 1);
    assert!(
        seen.lock()
            .expect("mutex poisoned")
            .iter()
            .any(|p| p.ends_with("a.json")),
        "expected hook called with a.json"
    );

    let accounts = manager.accounts_snapshot();
    let a = accounts
        .iter()
        .find(|acc| acc.file_path().ends_with("a.json"))
        .expect("a.json account");
    let b = accounts
        .iter()
        .find(|acc| acc.file_path().ends_with("b.json"))
        .expect("b.json account");
    let a_stats = a.stats_snapshot();
    let b_stats = b.stats_snapshot();
    assert_eq!(a_stats.failed_requests, 0);
    assert_eq!(b_stats.successful_requests, 1);
    assert_eq!(b_stats.usage_total_tokens, 18);
    let trend = runtime_state.hourly_trend();
    assert_eq!(trend.len(), 1);
    assert_eq!(trend[0].requests, 1);
    assert_eq!(trend[0].total_tokens, 18);
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
    let runtime_state = Arc::new(codex_proxy_rs::state::RuntimeStateStore::new(dir.path()));

    let state = AppState {
        manager: manager.clone(),
        quota_checker: Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap()),
        codex_client: Arc::new(CodexClient::new(base_url, "").unwrap()),
        request_stats: Arc::new(api::RequestStats::default()),
        api_keys: Arc::new(HashSet::new()),
        max_retry: 1,
        empty_retry_max: 0,
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
        runtime_state: runtime_state.clone(),
        on_401: None,
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
    assert_eq!(
        body,
        r#"{"id":"r1","object":"response","usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18}}"#
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);

    let accounts = manager.accounts_snapshot();
    let b = accounts
        .iter()
        .find(|acc| acc.file_path().ends_with("b.json"))
        .expect("b.json account");
    let stats = b.stats_snapshot();
    assert_eq!(stats.successful_requests, 1);
    assert_eq!(stats.failed_requests, 0);
    assert_eq!(stats.usage_input_tokens, 11);
    assert_eq!(stats.usage_output_tokens, 7);
    assert_eq!(stats.usage_total_tokens, 18);
    let trend = runtime_state.hourly_trend();
    assert_eq!(trend.len(), 1);
    assert_eq!(trend[0].requests, 1);
    assert_eq!(trend[0].input_tokens, 11);
    assert_eq!(trend[0].output_tokens, 7);
    assert_eq!(trend[0].total_tokens, 18);
}

#[tokio::test]
async fn api_v1_responses_non_retryable_failure_counts_as_failed_request() {
    let calls = Arc::new(AtomicUsize::new(0));
    let base_url = start_upstream(calls.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at403").await;

    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let state = AppState {
        manager: manager.clone(),
        quota_checker: Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap()),
        codex_client: Arc::new(CodexClient::new(base_url, "").unwrap()),
        request_stats: Arc::new(api::RequestStats::default()),
        api_keys: Arc::new(HashSet::new()),
        max_retry: 1,
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

    assert_eq!(res.status(), axum::http::StatusCode::FORBIDDEN);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    let stats = manager.accounts_snapshot()[0].stats_snapshot();
    assert_eq!(stats.total_requests, 1);
    assert_eq!(stats.total_errors, 1);
    assert_eq!(stats.successful_requests, 0);
    assert_eq!(stats.failed_requests, 1);
}

#[tokio::test]
async fn api_v1_responses_stats_expose_cached_and_reasoning_tokens() {
    let calls = Arc::new(AtomicUsize::new(0));
    let base_url = start_upstream(calls.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at3").await;

    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();
    let runtime_state = Arc::new(codex_proxy_rs::state::RuntimeStateStore::new(dir.path()));
    let request_stats = Arc::new(api::RequestStats::default());

    let post_app = api::router(AppState {
        manager: manager.clone(),
        quota_checker: Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap()),
        codex_client: Arc::new(CodexClient::new(base_url.clone(), "").unwrap()),
        request_stats: request_stats.clone(),
        api_keys: Arc::new(HashSet::new()),
        max_retry: 0,
        empty_retry_max: 0,
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
        runtime_state: runtime_state.clone(),
        on_401: None,
    });

    let res = post_app
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

    let stats_app = api::router(AppState {
        manager: manager.clone(),
        quota_checker: Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap()),
        codex_client: Arc::new(CodexClient::new(base_url, "").unwrap()),
        request_stats,
        api_keys: Arc::new(HashSet::new()),
        max_retry: 0,
        empty_retry_max: 0,
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
        runtime_state: runtime_state.clone(),
        on_401: None,
    });

    let stats_res = stats_app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri("/stats")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stats_res.status(), axum::http::StatusCode::OK);

    let body = axum::body::to_bytes(stats_res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["summary"]["total_input_tokens"], 12);
    assert_eq!(json["summary"]["total_output_tokens"], 8);
    assert_eq!(json["summary"]["total_cached_tokens"], 4);
    assert_eq!(json["summary"]["total_reasoning_tokens"], 2);
    assert_eq!(json["accounts"][0]["usage"]["input_tokens"], 12);
    assert_eq!(json["accounts"][0]["usage"]["output_tokens"], 8);
    assert_eq!(json["accounts"][0]["usage"]["cached_tokens"], 4);
    assert_eq!(json["accounts"][0]["usage"]["reasoning_tokens"], 2);
    assert_eq!(json["trend"]["hourly"][0]["input_tokens"], 12);
    assert_eq!(json["trend"]["hourly"][0]["output_tokens"], 8);
    assert_eq!(json["trend"]["hourly"][0]["cached_tokens"], 4);
    assert_eq!(json["trend"]["hourly"][0]["reasoning_tokens"], 2);
}

#[tokio::test]
async fn api_v1_responses_persist_cached_and_reasoning_tokens_to_state_file() {
    let calls = Arc::new(AtomicUsize::new(0));
    let base_url = start_upstream(calls).await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at3").await;

    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();
    let runtime_state = Arc::new(codex_proxy_rs::state::RuntimeStateStore::new(dir.path()));

    let app = api::router(AppState {
        manager: manager.clone(),
        quota_checker: Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap()),
        codex_client: Arc::new(CodexClient::new(base_url, "").unwrap()),
        request_stats: Arc::new(api::RequestStats::default()),
        api_keys: Arc::new(HashSet::new()),
        max_retry: 0,
        empty_retry_max: 0,
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
        runtime_state: runtime_state.clone(),
        on_401: None,
    });

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

    runtime_state.save_now(&manager).unwrap();

    let state_path = dir.path().join(".codex-proxy-state.json");
    let bytes = std::fs::read(state_path).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let account = json["accounts"]
        .as_object()
        .and_then(|accounts| accounts.values().next())
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    assert_eq!(account["usage_input_tokens"], 12);
    assert_eq!(account["usage_output_tokens"], 8);
    assert_eq!(account["usage_cached_tokens"], 4);
    assert_eq!(account["usage_reasoning_tokens"], 2);
    assert_eq!(json["hourly"][0]["input_tokens"], 12);
    assert_eq!(json["hourly"][0]["output_tokens"], 8);
    assert_eq!(json["hourly"][0]["cached_tokens"], 4);
    assert_eq!(json["hourly"][0]["reasoning_tokens"], 2);
}
