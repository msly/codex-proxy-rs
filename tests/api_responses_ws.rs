use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use codex_proxy_rs::api::{self, AppState};
use codex_proxy_rs::core::Manager;
use codex_proxy_rs::quota::QuotaChecker;
use codex_proxy_rs::refresh::{Refresher, SaveQueue};
use codex_proxy_rs::upstream::codex::CodexClient;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::time::{Duration, timeout};
use tokio_tungstenite::tungstenite::Message;
use url::Url;

const SSE_BODY: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n",
    "data: [DONE]\n\n",
);

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
    match auth {
        "Bearer at1" => (axum::http::StatusCode::UNAUTHORIZED, "unauthorized"),
        "Bearer at2" => (axum::http::StatusCode::OK, SSE_BODY),
        _ => (axum::http::StatusCode::FORBIDDEN, "forbidden"),
    }
}

async fn start_upstream(calls: Arc<AtomicUsize>) -> Url {
    let app = Router::new()
        .route("/backend-api/codex/responses", post(upstream_responses))
        .with_state(UpstreamState { calls });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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
async fn api_v1_responses_websocket_fallback_forwards_sse_payloads() {
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
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
    };

    let app = api::router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/v1/responses"))
        .await
        .unwrap();

    ws.send(Message::Text(
        serde_json::json!({
            "type": "response.create",
            "response": {
                "model": "gpt-5.4",
                "input": "hi"
            }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let mut got = Vec::<String>::new();
    while got.len() < 3 {
        let msg = timeout(Duration::from_secs(2), ws.next()).await.unwrap();
        let msg = match msg {
            Some(Ok(m)) => m,
            Some(Err(err)) => panic!("ws error: {err}"),
            None => break,
        };
        if let Message::Text(text) = msg {
            got.push(text.to_string());
        }
    }

    assert_eq!(
        got,
        vec![
            "{\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}".to_string(),
            "{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}".to_string(),
            "{\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}".to_string(),
        ]
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}
