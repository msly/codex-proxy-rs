use std::sync::Arc;
use std::time::Duration;

use codex_proxy_rs::core::Manager;
use codex_proxy_rs::refresh::{AuthScanLoop, AuthScanLoopConfig};
use codex_proxy_rs::state::RuntimeStateStore;
use tokio::sync::watch;

#[tokio::test]
async fn auth_scan_loop_prunes_deleted_auth_file_without_refresh_loop() {
    let dir = tempfile::tempdir().unwrap();
    let a_path = dir.path().join("a.json");
    let b_path = dir.path().join("b.json");

    std::fs::write(
        &a_path,
        r#"{"access_token":"at-a","refresh_token":"rt-a","account_id":"","email":"a@example.com","type":"codex","expired":"2099-01-01T00:00:00Z"}"#,
    )
    .unwrap();
    std::fs::write(
        &b_path,
        r#"{"access_token":"at-b","refresh_token":"rt-b","account_id":"","email":"b@example.com","type":"codex","expired":"2099-01-01T00:00:00Z"}"#,
    )
    .unwrap();

    let manager = Arc::new(Manager::new(dir.path()));
    manager.load_accounts().unwrap();
    let runtime_state = Arc::new(RuntimeStateStore::new(dir.path()));
    runtime_state.save_now(manager.as_ref()).unwrap();

    std::fs::remove_file(&b_path).unwrap();

    let loop_ = AuthScanLoop::new(
        manager.clone(),
        AuthScanLoopConfig {
            scan_interval: Duration::from_millis(20),
        },
    )
    .unwrap()
    .with_runtime_state(runtime_state.clone());

    let (tx, rx) = watch::channel(false);
    let handle = tokio::spawn(async move {
        loop_.start_loop(rx).await;
    });

    for _ in 0..20 {
        if manager.account_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("loop should stop")
        .unwrap();

    assert_eq!(manager.account_count(), 1);
    assert!(runtime_state.save_now_if_dirty(manager.as_ref()).unwrap());

    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join(".codex-proxy-state.json")).unwrap(),
    )
    .unwrap();
    let accounts = state["accounts"].as_object().unwrap();
    assert_eq!(accounts.len(), 1);
    assert!(accounts.contains_key(&a_path.to_string_lossy().to_string()));
    assert!(!accounts.contains_key(&b_path.to_string_lossy().to_string()));
}
