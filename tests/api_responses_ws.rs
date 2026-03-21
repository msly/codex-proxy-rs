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
use codex_proxy_rs::state::RuntimeStateStore;
use codex_proxy_rs::upstream::codex::CodexClient;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::time::{Duration, timeout};
use tokio_tungstenite::tungstenite::Message;
use url::Url;

type TestWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const SSE_BODY: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n",
    "data: [DONE]\n\n",
);

const SSE_BODY_COMPLETED_WITH_OUTPUT: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}]}}\n\n",
    "data: [DONE]\n\n",
);

const SSE_BODY_COMPLETED_ONLY_OUTPUT: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r2\"}}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}]}}\n\n",
    "data: [DONE]\n\n",
);

#[derive(Clone)]
struct UpstreamState {
    calls: Arc<AtomicUsize>,
    body: &'static str,
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
        "Bearer at2" => (axum::http::StatusCode::OK, state.body),
        _ => (axum::http::StatusCode::FORBIDDEN, "forbidden"),
    }
}

async fn start_upstream(calls: Arc<AtomicUsize>, body: &'static str) -> Url {
    let app = Router::new()
        .route("/backend-api/codex/responses", post(upstream_responses))
        .with_state(UpstreamState { calls, body });
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

async fn open_responses_ws(
    sse_body: &'static str,
) -> (
    TestWs,
    Arc<AtomicUsize>,
    Arc<api::RequestStats>,
    Arc<RuntimeStateStore>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let base_url = start_upstream(calls.clone(), sse_body).await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at1").await;
    write_auth_file(dir.path(), "b.json", "at2").await;

    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();
    let request_stats = Arc::new(api::RequestStats::default());
    let runtime_state = Arc::new(RuntimeStateStore::new(dir.path()));

    let state = AppState {
        manager: manager.clone(),
        quota_checker: Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap()),
        codex_client: Arc::new(CodexClient::new(base_url, "").unwrap()),
        request_stats: request_stats.clone(),
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

    (ws, calls, request_stats, runtime_state)
}

async fn recv_text_json(ws: &mut TestWs) -> serde_json::Value {
    let msg = timeout(Duration::from_secs(2), ws.next()).await.unwrap();
    let msg = match msg {
        Some(Ok(m)) => m,
        Some(Err(err)) => panic!("ws error: {err}"),
        None => panic!("websocket closed unexpectedly"),
    };
    match msg {
        Message::Text(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("expected text frame, got {other:?}"),
    }
}

async fn assert_no_extra_text_frames(ws: &mut TestWs) {
    match timeout(Duration::from_millis(300), ws.next()).await {
        Err(_) => {}
        Ok(None) => {}
        Ok(Some(Ok(Message::Close(_)))) => {}
        Ok(Some(Err(_))) => {}
        Ok(Some(Ok(Message::Text(text)))) => {
            panic!("unexpected extra text frame: {text}");
        }
        Ok(Some(Ok(other))) => {
            panic!("unexpected extra websocket frame: {other:?}");
        }
    }
}

#[tokio::test]
async fn api_v1_responses_websocket_fallback_forwards_sse_payloads() {
    let (mut ws, calls, request_stats, runtime_state) = open_responses_ws(SSE_BODY).await;

    assert_eq!(
        vec![
            recv_text_json(&mut ws).await,
            recv_text_json(&mut ws).await,
            recv_text_json(&mut ws).await,
        ],
        vec![
            serde_json::json!({"type":"response.created","response":{"id":"r1"}}),
            serde_json::json!({"type":"response.output_text.delta","delta":"hi"}),
            serde_json::json!({"type":"response.completed","response":{"id":"r1"}}),
        ]
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(request_stats.rpm(), 1);
    assert_eq!(runtime_state.hourly_trend().len(), 1);
    assert_eq!(runtime_state.hourly_trend()[0].requests, 1);
}

#[tokio::test]
async fn api_v1_responses_websocket_fallback_strips_completed_output_after_delta() {
    let (mut ws, calls, request_stats, runtime_state) =
        open_responses_ws(SSE_BODY_COMPLETED_WITH_OUTPUT).await;

    assert_eq!(
        vec![
            recv_text_json(&mut ws).await,
            recv_text_json(&mut ws).await,
            recv_text_json(&mut ws).await,
        ],
        vec![
            serde_json::json!({"type":"response.created","response":{"id":"r1"}}),
            serde_json::json!({"type":"response.output_text.delta","delta":"hi"}),
            serde_json::json!({"type":"response.completed","response":{"id":"r1"}}),
        ]
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(request_stats.rpm(), 1);
    assert_eq!(runtime_state.hourly_trend().len(), 1);
    assert_eq!(runtime_state.hourly_trend()[0].requests, 1);
}

#[tokio::test]
async fn api_v1_responses_websocket_fallback_accepts_completed_only_output() {
    let (mut ws, calls, request_stats, runtime_state) =
        open_responses_ws(SSE_BODY_COMPLETED_ONLY_OUTPUT).await;

    assert_eq!(
        vec![recv_text_json(&mut ws).await, recv_text_json(&mut ws).await,],
        vec![
            serde_json::json!({"type":"response.created","response":{"id":"r2"}}),
            serde_json::json!({
                "type":"response.completed",
                "response":{
                    "id":"r2",
                    "output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}]
                }
            }),
        ]
    );
    assert_no_extra_text_frames(&mut ws).await;
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(request_stats.rpm(), 1);
    assert_eq!(runtime_state.hourly_trend().len(), 1);
    assert_eq!(runtime_state.hourly_trend()[0].requests, 1);
}
