use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
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

const UPSTREAM_IMAGE_SSE: &str = concat!(
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"model\":\"gpt-5.4\"}}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"model\":\"gpt-5.4\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3},\"output\":[{\"type\":\"image_generation_call\",\"output_format\":\"png\",\"result\":\"aGVsbG8=\"}]}}\n\n",
    "data: [DONE]\n\n",
);

#[derive(Clone)]
struct UpstreamState {
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
}

async fn upstream_responses(
    State(state): State<UpstreamState>,
    _headers: HeaderMap,
    body: Bytes,
) -> (axum::http::StatusCode, &'static str) {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) {
        state.bodies.lock().unwrap().push(value);
    }
    (axum::http::StatusCode::OK, UPSTREAM_IMAGE_SSE)
}

async fn start_upstream(bodies: Arc<Mutex<Vec<serde_json::Value>>>) -> Url {
    let app = Router::new()
        .route("/backend-api/codex/responses", post(upstream_responses))
        .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024))
        .with_state(UpstreamState { bodies });
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

fn build_state(base_url: Url, manager: Arc<Manager>, dir: &std::path::Path) -> AppState {
    AppState {
        manager,
        quota_checker: Arc::new(QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap()),
        codex_client: Arc::new(CodexClient::new(base_url, "").unwrap()),
        request_stats: Arc::new(api::RequestStats::default()),
        api_keys: Arc::new(HashSet::new()),
        max_retry: 0,
        empty_retry_max: 0,
        refresher: Refresher::new("").unwrap(),
        save_queue: SaveQueue::start(1),
        refresh_concurrency: 1,
        runtime_state: Arc::new(codex_proxy_rs::state::RuntimeStateStore::new(dir)),
        on_401: None,
    }
}

#[tokio::test]
async fn api_v1_images_generations_returns_openai_images_json() {
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let base_url = start_upstream(bodies.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at").await;
    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let app = api::router(build_state(base_url, manager, dir.path()));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/images/generations")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "model": "gpt-5.4",
                        "prompt": "draw a cat",
                        "response_format": "b64_json"
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

    assert_eq!(v["data"][0]["b64_json"], "aGVsbG8=");

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["tools"][0]["type"], "image_generation");
    assert_eq!(bodies[0]["input"][0]["content"][0]["text"], "draw a cat");
}

#[tokio::test]
async fn api_v1_images_edits_includes_input_image() {
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let base_url = start_upstream(bodies.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at").await;
    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let app = api::router(build_state(base_url, manager, dir.path()));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/images/edits")
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({
                        "model": "gpt-5.4",
                        "prompt": "make it blue",
                        "image": "data:image/png;base64,aGVsbG8="
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies[0]["input"][0]["content"][1]["type"], "input_image");
    assert_eq!(
        bodies[0]["input"][0]["content"][1]["image_url"],
        "data:image/png;base64,aGVsbG8="
    );
}

#[tokio::test]
async fn api_v1_images_edits_accepts_multipart_image() {
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let base_url = start_upstream(bodies.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at").await;
    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let boundary = "codexproxyboundary";
    let body = concat!(
        "--codexproxyboundary\r\n",
        "Content-Disposition: form-data; name=\"model\"\r\n\r\n",
        "gpt-5.4\r\n",
        "--codexproxyboundary\r\n",
        "Content-Disposition: form-data; name=\"prompt\"\r\n\r\n",
        "make it blue\r\n",
        "--codexproxyboundary\r\n",
        "Content-Disposition: form-data; name=\"response_format\"\r\n\r\n",
        "b64_json\r\n",
        "--codexproxyboundary\r\n",
        "Content-Disposition: form-data; name=\"image\"; filename=\"input.png\"\r\n",
        "Content-Type: image/png\r\n\r\n",
        "hello\r\n",
        "--codexproxyboundary--\r\n",
    );

    let app = api::router(build_state(base_url, manager, dir.path()));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/images/edits")
                .header(
                    axum::http::header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["data"][0]["b64_json"], "aGVsbG8=");

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies[0]["input"][0]["content"][0]["text"], "make it blue");
    assert_eq!(bodies[0]["input"][0]["content"][1]["type"], "input_image");
    assert_eq!(
        bodies[0]["input"][0]["content"][1]["image_url"],
        "data:image/png;base64,aGVsbG8="
    );
}

#[tokio::test]
async fn api_v1_images_edits_accepts_large_multipart_image() {
    let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let base_url = start_upstream(bodies.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at").await;
    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let boundary = "codexproxylargeboundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        concat!(
            "--codexproxylargeboundary\r\n",
            "Content-Disposition: form-data; name=\"model\"\r\n\r\n",
            "gpt-5.4\r\n",
            "--codexproxylargeboundary\r\n",
            "Content-Disposition: form-data; name=\"prompt\"\r\n\r\n",
            "make it blue\r\n",
            "--codexproxylargeboundary\r\n",
            "Content-Disposition: form-data; name=\"image\"; filename=\"input.png\"\r\n",
            "Content-Type: image/png\r\n\r\n",
        )
        .as_bytes(),
    );
    body.extend(std::iter::repeat_n(b'a', 2 * 1024 * 1024 + 1));
    body.extend_from_slice(b"\r\n--codexproxylargeboundary--\r\n");

    let app = api::router(build_state(base_url, manager, dir.path()));
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/images/edits")
                .header(
                    axum::http::header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(axum::body::Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);

    let bodies = bodies.lock().unwrap();
    let image_url = bodies[0]["input"][0]["content"][1]["image_url"]
        .as_str()
        .unwrap();
    assert!(image_url.starts_with("data:image/png;base64,"));
    assert!(image_url.len() > 2 * 1024 * 1024);
}
