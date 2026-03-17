use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use codex_proxy_rs::api::{self, AppState};
use codex_proxy_rs::core::{Manager, QuotaInfo};
use codex_proxy_rs::quota::QuotaChecker;
use codex_proxy_rs::refresh::{Refresher, SaveQueue};
use codex_proxy_rs::upstream::codex::CodexClient;
use tower::util::ServiceExt;
use url::Url;

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
async fn api_stats_returns_cached_quota_raw_json() {
    let dir = tempfile::tempdir().unwrap();
    write_auth_file(dir.path(), "a.json", "at").await;

    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();
    let acc = manager.accounts_snapshot()[0].clone();

    acc.set_quota_info(QuotaInfo {
        valid: true,
        status_code: 200,
        raw_data: serde_json::json!({
            "rate_limit": { "primary_window": { "used_percent": 12.34 } }
        })
        .to_string()
        .into_bytes(),
        checked_at_ms: 123,
    });

    let state = AppState {
        manager: manager.clone(),
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
                .uri("/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(v["summary"]["total"], 1);
    assert_eq!(v["accounts"][0]["email"], "x@example.com");
    assert_eq!(v["accounts"][0]["quota"]["valid"], true);
    assert_eq!(v["accounts"][0]["quota"]["status_code"], 200);
    assert_eq!(v["accounts"][0]["quota"]["checked_at_ms"], 123);
    assert_eq!(
        v["accounts"][0]["quota"]["raw_data"]["rate_limit"]["primary_window"]["used_percent"],
        12.34
    );
}
