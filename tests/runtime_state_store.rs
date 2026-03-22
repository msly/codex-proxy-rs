use codex_proxy_rs::core::{AccountStatus, Manager, now_unix_ms};
use codex_proxy_rs::state::RuntimeStateStore;

#[test]
fn runtime_state_store_roundtrip_restores_account_snapshot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file_path = dir.path().join("a.json");
    std::fs::write(
        &file_path,
        r#"{
  "access_token": "at-a",
  "refresh_token": "rt-a",
  "account_id": "acc-a",
  "email": "a@example.com",
  "type": "codex",
  "expired": "2099-01-01T00:00:00Z"
}"#,
    )
    .expect("write a.json");

    let manager = Manager::new(dir.path());
    manager.load_accounts().expect("load accounts");
    let acc = manager.accounts_snapshot()[0].clone();

    let now_ms = now_unix_ms();
    acc.apply_runtime_snapshot(
        &codex_proxy_rs::core::AccountRuntimeSnapshot {
            status: AccountStatus::Cooldown,
            cooldown_until_ms: now_ms + 60_000,
            total_requests: 17,
            total_errors: 4,
            successful_requests: 11,
            failed_requests: 2,
            consecutive_failures: 2,
            last_used_ms: now_ms - 2_000,
            quota_exhausted: true,
            quota_resets_at_ms: now_ms + 60_000,
            usage_total_completions: 7,
            usage_input_tokens: 222,
            usage_output_tokens: 111,
            usage_cached_tokens: 33,
            usage_reasoning_tokens: 22,
            usage_total_tokens: 333,
        },
        now_ms,
    );

    let store = RuntimeStateStore::new(dir.path());
    store.record_hourly_usage(now_ms, 222, 111, 33, 22, 333);
    store.record_hourly_request(now_ms);
    store.save_now(&manager).expect("save state");

    let manager2 = Manager::new(dir.path());
    manager2.load_accounts().expect("load accounts again");
    let store2 = RuntimeStateStore::new(dir.path());
    store2.load_and_apply(&manager2).expect("restore state");

    let restored = manager2.accounts_snapshot()[0].stats_snapshot();
    assert_eq!(restored.status, AccountStatus::Cooldown);
    assert_eq!(restored.total_requests, 17);
    assert_eq!(restored.total_errors, 4);
    assert_eq!(restored.successful_requests, 11);
    assert_eq!(restored.failed_requests, 2);
    assert_eq!(restored.usage_cached_tokens, 33);
    assert_eq!(restored.usage_reasoning_tokens, 22);
    assert_eq!(restored.usage_total_tokens, 333);

    let trend = store2.hourly_trend();
    assert_eq!(trend.len(), 1);
    assert_eq!(trend[0].requests, 1);
    assert_eq!(trend[0].cached_tokens, 33);
    assert_eq!(trend[0].reasoning_tokens, 22);
    assert_eq!(trend[0].total_tokens, 333);
}

#[test]
fn runtime_state_store_trims_hourly_buckets_to_retention_window() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manager = Manager::new(dir.path());
    let store = RuntimeStateStore::new(dir.path());
    let base_ms = now_unix_ms();

    for idx in 0..200 {
        let ts = base_ms - (idx as i64) * 3_600_000;
        store.record_hourly_request(ts);
        store.record_hourly_usage(ts, 1, 1, 1, 1, 2);
    }

    store.save_now(&manager).expect("save state");

    let trend = store.hourly_trend();
    assert_eq!(trend.len(), 168);
    assert!(trend.iter().all(|point| point.cached_tokens >= 1));
    assert!(trend.iter().all(|point| point.reasoning_tokens >= 1));
    assert!(trend.iter().all(|point| point.total_tokens >= 2));
}

#[test]
fn runtime_state_store_save_now_if_dirty_persists_runtime_only_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file_path = dir.path().join("a.json");
    std::fs::write(
        &file_path,
        r#"{
  "access_token": "at-a",
  "refresh_token": "rt-a",
  "account_id": "acc-a",
  "email": "a@example.com",
  "type": "codex",
  "expired": "2099-01-01T00:00:00Z"
}"#,
    )
    .expect("write a.json");

    let manager = Manager::new(dir.path());
    manager.load_accounts().expect("load accounts");
    let acc = manager.accounts_snapshot()[0].clone();
    let now_ms = now_unix_ms();
    acc.set_quota_cooldown(60_000, now_ms);

    let store = RuntimeStateStore::new(dir.path());
    store.mark_dirty();
    assert!(store.save_now_if_dirty(&manager).expect("save if dirty"));

    let manager2 = Manager::new(dir.path());
    manager2.load_accounts().expect("load accounts again");
    let store2 = RuntimeStateStore::new(dir.path());
    store2.load_and_apply(&manager2).expect("restore state");

    let restored = manager2.accounts_snapshot()[0].stats_snapshot();
    assert_eq!(restored.status, AccountStatus::Cooldown);
    assert!(restored.quota_exhausted);
    assert!(restored.cooldown_until_ms > now_ms);
}
