use std::collections::HashSet;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use codex_proxy_rs::api::{self, AppState};
use codex_proxy_rs::core::Manager;
use codex_proxy_rs::quota::QuotaChecker;
use codex_proxy_rs::upstream::codex::CodexClient;
use tower::util::ServiceExt;
use url::Url;

fn build_state() -> AppState {
    let dir = tempfile::tempdir().unwrap();
    AppState {
        manager: Arc::new(Manager::new(dir.path())),
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
    }
}

#[tokio::test]
async fn api_v1_models_contains_expected_variants() {
    let app = api::router(build_state());
    let res = app
        .oneshot(Request::builder().uri("/v1/models").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let ids: std::collections::HashSet<String> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|it| it.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();

    assert!(ids.contains("gpt-5.4"));
    assert!(ids.contains("gpt-5.4-fast"));
    assert!(ids.contains("gpt-5.4-high"));
    assert!(ids.contains("gpt-5.4-high-fast"));
    assert!(ids.contains("gpt-5.1-codex-max"));
    assert!(ids.contains("gpt-5.1-codex-max-fast"));
}

