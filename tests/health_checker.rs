use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::Router;
use codex_proxy_rs::core::Manager;
use codex_proxy_rs::health::{HealthChecker, HealthCheckerConfig};
use url::Url;

#[derive(Clone)]
struct UpstreamState {
    calls: Arc<AtomicUsize>,
    status: u16,
}

async fn upstream_responses(
    State(state): State<UpstreamState>,
    headers: HeaderMap,
) -> (axum::http::StatusCode, String) {
    state.calls.fetch_add(1, Ordering::Relaxed);
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let status = if auth.starts_with("Bearer ") {
        state.status
    } else {
        403
    };

    (
        axum::http::StatusCode::from_u16(status).unwrap(),
        serde_json::json!({ "error": { "message": "x" } }).to_string(),
    )
}

async fn start_upstream(status: u16, calls: Arc<AtomicUsize>) -> Url {
    let app = Router::new()
        .route("/backend-api/codex/responses", post(upstream_responses))
        .with_state(UpstreamState { calls, status });
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
async fn health_checker_removes_account_after_consecutive_401() {
    let calls = Arc::new(AtomicUsize::new(0));
    let base_url = start_upstream(401, calls.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at").await;
    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();
    let acc_path = manager.accounts_snapshot()[0].file_path().to_string();

    let cfg = HealthCheckerConfig {
        check_interval: Duration::from_secs(3600),
        max_consecutive_failures: 1,
        concurrency: 1,
        start_delay: Duration::from_secs(0),
        batch_size: 0,
        request_timeout: Duration::from_secs(2),
    };
    let checker = HealthChecker::new(base_url, "", cfg).unwrap();

    checker.check_all_once(manager.clone()).await;

    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(manager.account_count(), 0);
    assert!(!std::path::Path::new(&acc_path).exists());
}

#[tokio::test]
async fn health_checker_loop_can_be_canceled() {
    let calls = Arc::new(AtomicUsize::new(0));
    let base_url = start_upstream(200, calls.clone()).await;

    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at").await;
    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();

    let cfg = HealthCheckerConfig {
        check_interval: Duration::from_millis(20),
        max_consecutive_failures: 3,
        concurrency: 1,
        start_delay: Duration::from_secs(0),
        batch_size: 1,
        request_timeout: Duration::from_secs(2),
    };
    let checker = Arc::new(HealthChecker::new(base_url, "", cfg).unwrap());

    let (tx, rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(checker.start_loop(manager.clone(), rx));

    tokio::time::sleep(Duration::from_millis(60)).await;
    let _ = tx.send(true);

    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("health loop should stop")
        .expect("join should succeed");
    assert!(calls.load(Ordering::Relaxed) >= 1);
}
