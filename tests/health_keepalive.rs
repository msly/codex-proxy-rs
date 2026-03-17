use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::routing::head;
use axum::Router;
use codex_proxy_rs::health::{KeepAlive, KeepAliveConfig};
use url::Url;

#[derive(Clone)]
struct AppState {
    calls: Arc<AtomicUsize>,
}

async fn ping(State(state): State<AppState>) -> axum::http::StatusCode {
    state.calls.fetch_add(1, Ordering::Relaxed);
    axum::http::StatusCode::OK
}

async fn start_server(calls: Arc<AtomicUsize>) -> Url {
    let app = Router::new().route("/", head(ping)).with_state(AppState { calls });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Url::parse(&format!("http://{addr}/")).unwrap()
}

#[tokio::test]
async fn health_keepalive_pings_and_can_be_canceled() {
    let calls = Arc::new(AtomicUsize::new(0));
    let ping_url = start_server(calls.clone()).await;

    let ka = KeepAlive::new(
        ping_url,
        "",
        KeepAliveConfig {
            interval: Duration::from_millis(20),
            request_timeout: Duration::from_secs(2),
        },
    )
    .unwrap();

    let (tx, rx) = tokio::sync::watch::channel(false);
    let handle = tokio::spawn(ka.start_loop(rx));

    tokio::time::sleep(Duration::from_millis(60)).await;
    let _ = tx.send(true);

    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("keepalive loop should stop")
        .expect("join should succeed");

    assert!(calls.load(Ordering::Relaxed) >= 1);
}

