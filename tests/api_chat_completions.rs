use std::collections::HashSet;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use codex_proxy_rs::api::{self, AppState};
use codex_proxy_rs::core::Manager;
use codex_proxy_rs::quota::QuotaChecker;
use codex_proxy_rs::refresh::{Refresher, SaveQueue};
use codex_proxy_rs::upstream::codex::CodexClient;
use tower::util::ServiceExt;
use url::Url;

const UPSTREAM_SSE: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"created_at\":111,\"model\":\"gpt-5.4\"}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"created_at\":111,\"model\":\"gpt-5.4\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2},\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}]}}\n\n",
);

const UPSTREAM_REASONING_ONLY_SSE: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r2\",\"created_at\":222,\"model\":\"gpt-5.4\"}}\n\n",
    "data: {\"type\":\"response.reasoning_text.done\",\"item_id\":\"it1\",\"text\":\"chain\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\",\"created_at\":222,\"model\":\"gpt-5.4\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2},\"output\":[{\"type\":\"reasoning_text\",\"text\":\"chain\"}]}}\n\n",
);

const UPSTREAM_EMPTY_SSE: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r3\",\"created_at\":333,\"model\":\"gpt-5.4\"}}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r3\",\"created_at\":333,\"model\":\"gpt-5.4\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0,\"total_tokens\":1},\"output\":[]}}\n\n",
);

#[derive(Clone)]
struct UpstreamState;

async fn upstream_responses(
    State(_state): State<UpstreamState>,
    _headers: HeaderMap,
) -> (axum::http::StatusCode, &'static str) {
    (axum::http::StatusCode::OK, UPSTREAM_SSE)
}

async fn upstream_responses_reasoning_only(
    State(_state): State<UpstreamState>,
    _headers: HeaderMap,
) -> (axum::http::StatusCode, &'static str) {
    (axum::http::StatusCode::OK, UPSTREAM_REASONING_ONLY_SSE)
}

async fn upstream_responses_empty_then_text(
    State(_state): State<UpstreamState>,
    headers: HeaderMap,
) -> (axum::http::StatusCode, &'static str) {
    match headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
    {
        "Bearer at-empty" => (axum::http::StatusCode::OK, UPSTREAM_EMPTY_SSE),
        "Bearer at-good" => (axum::http::StatusCode::OK, UPSTREAM_SSE),
        _ => (axum::http::StatusCode::FORBIDDEN, "forbidden"),
    }
}

async fn start_upstream() -> Url {
    let app = Router::new()
        .route("/backend-api/codex/responses", post(upstream_responses))
        .with_state(UpstreamState);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    Url::parse(&format!("http://{addr}/backend-api/codex/")).unwrap()
}

async fn start_reasoning_only_upstream() -> Url {
    let app = Router::new()
        .route(
            "/backend-api/codex/responses",
            post(upstream_responses_reasoning_only),
        )
        .with_state(UpstreamState);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    Url::parse(&format!("http://{addr}/backend-api/codex/")).unwrap()
}

async fn start_empty_retry_upstream() -> Url {
    let app = Router::new()
        .route(
            "/backend-api/codex/responses",
            post(upstream_responses_empty_then_text),
        )
        .with_state(UpstreamState);
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

fn build_state(base_url: Url, manager: Arc<Manager>) -> AppState {
    build_state_with_empty_retry(base_url, manager, 0)
}

fn build_state_with_empty_retry(
    base_url: Url,
    manager: Arc<Manager>,
    empty_retry_max: usize,
) -> AppState {
    AppState {
        manager,
        quota_checker: Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap()),
        codex_client: Arc::new(CodexClient::new(base_url, "").unwrap()),
        request_stats: Arc::new(api::RequestStats::default()),
        api_keys: Arc::new(HashSet::new()),
        max_retry: 0,
        empty_retry_max,
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
        on_401: None,
    }
}

#[tokio::test]
async fn api_v1_chat_completions_non_stream_converts_response() {
    let base_url = start_upstream().await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at").await;
    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let app = api::router(build_state(base_url, manager));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "messages": [{"role":"user","content":"hi"}]
                    })
                    .to_string(),
                ))
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
        "application/json"
    );

    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["choices"][0]["message"]["role"], "assistant");
    assert_eq!(v["choices"][0]["message"]["content"], "hi");
}

#[tokio::test]
async fn api_v1_chat_completions_stream_converts_sse_and_appends_done() {
    let base_url = start_upstream().await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at").await;
    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let app = api::router(build_state(base_url, manager));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "model": "gpt-5.4",
                        "stream": true,
                        "messages": [{"role":"user","content":"hi"}]
                    })
                    .to_string(),
                ))
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
        body.contains("\"object\":\"chat.completion.chunk\""),
        "body: {body}"
    );
    assert!(body.contains("\"content\":\"hi\""), "body: {body}");
    assert!(body.contains("data: [DONE]"), "body: {body}");
}

#[tokio::test]
async fn api_v1_chat_completions_non_stream_accepts_reasoning_only_output() {
    let base_url = start_reasoning_only_upstream().await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at").await;
    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let app = api::router(build_state(base_url, manager));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "messages": [{"role":"user","content":"hi"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["choices"][0]["message"]["reasoning_content"], "chain");
}

#[tokio::test]
async fn api_v1_chat_completions_non_stream_retries_empty_response_on_another_account() {
    let base_url = start_empty_retry_upstream().await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at-empty").await;
    write_auth_file(dir.path(), "b.json", "at-good").await;
    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let app = api::router(build_state_with_empty_retry(base_url, manager, 1));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "model": "gpt-5.4",
                        "stream": false,
                        "messages": [{"role":"user","content":"hi"}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "hi");
}

#[tokio::test]
async fn api_v1_chat_completions_stream_accepts_reasoning_only_output() {
    let base_url = start_reasoning_only_upstream().await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at").await;
    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let app = api::router(build_state(base_url, manager));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "model": "gpt-5.4",
                        "stream": true,
                        "messages": [{"role":"user","content":"hi"}]
                    })
                    .to_string(),
                ))
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
        body.contains("\"reasoning_content\":\"chain\""),
        "body: {body}"
    );
    assert!(body.contains("data: [DONE]"), "body: {body}");
}
