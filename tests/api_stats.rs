use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use codex_proxy_rs::api::{self, AppState};
use codex_proxy_rs::core::{Manager, QuotaInfo};
use codex_proxy_rs::quota::QuotaChecker;
use codex_proxy_rs::refresh::{Refresher, SaveQueue};
use codex_proxy_rs::upstream::codex::CodexClient;
use tower::util::ServiceExt;
use url::Url;

fn build_test_id_token(email: &str, plan_type: &str) -> String {
    let payload = serde_json::json!({
        "email": email,
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": plan_type
        }
    });
    let payload_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
    format!("header.{payload_b64}.sig")
}

async fn write_auth_file(dir: &std::path::Path, name: &str, access_token: &str) {
    let path = dir.join(name);
    std::fs::write(
        &path,
        serde_json::json!({
            "id_token": build_test_id_token("x@example.com", "plus"),
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

    let now = codex_proxy_rs::core::now_unix_ms();
    acc.record_success(now);
    acc.record_usage(10, 20, 30);
    acc.set_quota_cooldown(60_000, now);

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
        on_401: None,
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
    assert_eq!(v["accounts"][0]["plan_type"], "plus");
    assert_eq!(v["accounts"][0]["quota_exhausted"], true);
    assert!(v["accounts"][0]["last_used_at"].is_string());
    assert_eq!(v["accounts"][0]["usage"]["total_completions"], 1);
    assert_eq!(v["accounts"][0]["usage"]["input_tokens"], 10);
    assert_eq!(v["accounts"][0]["usage"]["output_tokens"], 20);
    assert_eq!(v["accounts"][0]["usage"]["total_tokens"], 30);
    assert_eq!(v["accounts"][0]["quota"]["valid"], true);
    assert_eq!(v["accounts"][0]["quota"]["status_code"], 200);
    assert_eq!(v["accounts"][0]["quota"]["checked_at_ms"], 123);
    assert_eq!(
        v["accounts"][0]["quota"]["raw_data"]["rate_limit"]["primary_window"]["used_percent"],
        12.34
    );
}
