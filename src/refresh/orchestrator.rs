use std::sync::Arc;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::core::{Account, Manager, now_unix_ms};

use super::refresher::{RefreshError, Refresher};
use super::save::SaveQueue;

pub fn filter_need_refresh(accounts: &[Arc<Account>]) -> Vec<Arc<Account>> {
    let now_ms = now_unix_ms();
    let mut result = Vec::new();

    for acc in accounts {
        if acc.is_refreshing() {
            continue;
        }
        if acc.last_refresh_ms() > 0 && (now_ms - acc.last_refresh_ms()) < 60_000 {
            continue;
        }

        let token = acc.token();
        if token.refresh_token.trim().is_empty() {
            continue;
        }

        if !token.expired.is_empty() {
            if let Ok(expire_at) = OffsetDateTime::parse(&token.expired, &Rfc3339) {
                let left = expire_at - OffsetDateTime::now_utc();
                if left > time::Duration::minutes(5) {
                    continue;
                }
            }
        }

        result.push(acc.clone());
    }

    result
}

pub async fn refresh_account(
    manager: &Manager,
    refresher: &Refresher,
    save_queue: &SaveQueue,
    account: Arc<Account>,
    max_retries: usize,
) -> Result<(), RefreshError> {
    refresh_account_with_options(
        manager,
        refresher,
        save_queue,
        account,
        max_retries,
        "refresh_failed",
        60_000,
    )
    .await
}

pub async fn refresh_account_with_remove_reason(
    manager: &Manager,
    refresher: &Refresher,
    save_queue: &SaveQueue,
    account: Arc<Account>,
    max_retries: usize,
    remove_reason: &str,
) -> Result<(), RefreshError> {
    refresh_account_with_options(
        manager,
        refresher,
        save_queue,
        account,
        max_retries,
        remove_reason,
        60_000,
    )
    .await
}

pub async fn refresh_account_with_options(
    manager: &Manager,
    refresher: &Refresher,
    save_queue: &SaveQueue,
    account: Arc<Account>,
    max_retries: usize,
    remove_reason: &str,
    rate_limit_cooldown_ms: i64,
) -> Result<(), RefreshError> {
    if !account.try_begin_refresh() {
        return Ok(());
    }
    struct Guard(Arc<Account>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.end_refresh();
        }
    }
    let _guard = Guard(account.clone());

    let refresh_token = { account.token().refresh_token.clone() };
    if refresh_token.trim().is_empty() {
        if remove_reason == "auth_401" {
            manager.remove_account(account.file_path(), "auth_401_missing_refresh_token");
        }
        return Err(RefreshError::missing_refresh_token());
    }

    match refresher
        .refresh_token_with_retry(&refresh_token, max_retries)
        .await
    {
        Ok(td) => {
            let now_ms = now_unix_ms();
            account.update_token(td, now_ms);
            save_queue.enqueue(account);
            Ok(())
        }
        Err(err) if err.is_rate_limited() => {
            account.set_cooldown(rate_limit_cooldown_ms.max(0), now_unix_ms());
            Err(err)
        }
        Err(err) => {
            manager.remove_account(account.file_path(), remove_reason);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Form, Router};
    use base64::Engine;
    use reqwest::Url;
    use serde::Deserialize;
    use serde_json::json;
    use std::fs;
    use std::io;
    use std::net::SocketAddr;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tokio::net::TcpListener;

    use crate::core::{AccountStatus, TokenData};

    #[test]
    fn refresh_filter_need_refresh_rules() {
        fn make_account(path: &str, refresh_token: &str, expire: &str) -> Arc<Account> {
            Arc::new(Account::new(
                path.to_string(),
                TokenData {
                    id_token: String::new(),
                    access_token: String::new(),
                    refresh_token: refresh_token.to_string(),
                    account_id: String::new(),
                    email: String::new(),
                    expired: expire.to_string(),
                    plan_type: String::new(),
                },
            ))
        }

        let far = (OffsetDateTime::now_utc() + time::Duration::minutes(10))
            .format(&Rfc3339)
            .unwrap();
        let near = (OffsetDateTime::now_utc() + time::Duration::minutes(4))
            .format(&Rfc3339)
            .unwrap();

        let acc_far = make_account("far.json", "rt", &far);
        let acc_near = make_account("near.json", "rt", &near);
        let acc_invalid = make_account("invalid.json", "rt", "not-a-time");
        let acc_no_rt = make_account("no-rt.json", "", &near);

        let acc_recent = make_account("recent.json", "rt", &near);
        acc_recent.set_last_refresh_ms_for_test(now_unix_ms());

        let acc_refreshing = make_account("refreshing.json", "rt", &near);
        assert!(acc_refreshing.try_begin_refresh());

        let got = filter_need_refresh(&[
            acc_far,
            acc_near.clone(),
            acc_invalid.clone(),
            acc_no_rt,
            acc_recent,
            acc_refreshing,
        ]);

        let mut paths: Vec<String> = got.iter().map(|a| a.file_path().to_string()).collect();
        paths.sort();
        assert_eq!(
            paths,
            vec!["invalid.json".to_string(), "near.json".to_string()]
        );
    }

    #[derive(Clone)]
    struct AppState {
        mode: &'static str,
    }

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl SharedWriter {
        fn snapshot(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap_or_default()
        }
    }

    #[derive(Debug, Deserialize)]
    struct TokenForm {
        refresh_token: String,
    }

    async fn oauth_token(
        State(state): State<AppState>,
        Form(form): Form<TokenForm>,
    ) -> (axum::http::StatusCode, String) {
        match state.mode {
            "429" => (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                "rate limited".to_string(),
            ),
            "500" => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "oops".to_string(),
            ),
            _ => {
                let id_token = build_test_id_token("acc-1", "a@example.com", "plus");
                let body = json!({
                    "access_token": format!("at-{}", form.refresh_token),
                    "refresh_token": format!("rt-{}", form.refresh_token),
                    "id_token": id_token,
                    "expires_in": 3600
                });
                (axum::http::StatusCode::OK, body.to_string())
            }
        }
    }

    fn build_test_id_token(account_id: &str, email: &str, plan_type: &str) -> String {
        let payload = json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_account_id": account_id,
                "chatgpt_plan_type": plan_type
            }
        });
        let payload_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        format!("header.{payload_b64}.sig")
    }

    async fn start_server(mode: &'static str) -> Url {
        let app = Router::new()
            .route("/oauth/token", post(oauth_token))
            .with_state(AppState { mode });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Url::parse(&format!("http://{addr}/oauth/token")).unwrap()
    }

    async fn write_auth_file_with_tokens(
        dir: &Path,
        name: &str,
        access_token: &str,
        refresh_token: &str,
    ) -> String {
        let path = dir.join(name);
        fs::write(
            &path,
            json!({
                "access_token": access_token,
                "refresh_token": refresh_token,
                "account_id": "acc-file",
                "email": "file@example.com",
                "type": "codex",
                "expired": "2099-01-01T00:00:00Z"
            })
            .to_string(),
        )
        .unwrap();
        path.to_string_lossy().to_string()
    }

    async fn write_auth_file(dir: &Path, name: &str, refresh_token: &str) -> String {
        write_auth_file_with_tokens(dir, name, "at0", refresh_token).await
    }

    #[tokio::test]
    async fn refresh_refresh_account_success_updates_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = write_auth_file(dir.path(), "a.json", "rt0").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();
        let acc = manager.accounts_snapshot()[0].clone();

        let token_url = start_server("200").await;
        let refresher = Refresher::new("").unwrap().with_token_url(token_url);
        let save_queue = SaveQueue::start(1);

        refresh_account(&manager, &refresher, &save_queue, acc.clone(), 1)
            .await
            .expect("refresh ok");

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(data) = fs::read_to_string(&file_path) {
                    if data.contains("at-rt0") {
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        let tf: crate::core::TokenFile =
            serde_json::from_str(&fs::read_to_string(&file_path).unwrap()).unwrap();
        assert_eq!(tf.access_token, "at-rt0");
        assert_eq!(tf.refresh_token, "rt-rt0");
        assert_eq!(tf.account_id, "acc-1");
        assert_eq!(tf.email, "a@example.com");
        assert_eq!(manager.account_count(), 1);
    }

    #[tokio::test]
    async fn refresh_refresh_account_rate_limited_sets_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = write_auth_file(dir.path(), "a.json", "rt0").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();
        let acc = manager.accounts_snapshot()[0].clone();

        let token_url = start_server("429").await;
        let refresher = Refresher::new("").unwrap().with_token_url(token_url);
        let save_queue = SaveQueue::start(1);

        let err = refresh_account(&manager, &refresher, &save_queue, acc.clone(), 1)
            .await
            .expect_err("should error");
        assert!(err.is_rate_limited());
        assert_eq!(acc.status(), AccountStatus::Cooldown);
        assert!(Path::new(&file_path).exists());
        assert_eq!(manager.account_count(), 1);
    }

    #[tokio::test]
    async fn refresh_refresh_account_failure_removes_account_and_deletes_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = write_auth_file(dir.path(), "a.json", "rt0").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();
        let acc = manager.accounts_snapshot()[0].clone();

        let token_url = start_server("500").await;
        let refresher = Refresher::new("").unwrap().with_token_url(token_url);
        let save_queue = SaveQueue::start(1);

        let _ = refresh_account(&manager, &refresher, &save_queue, acc, 1).await;

        assert_eq!(manager.account_count(), 0);
        assert!(!Path::new(&file_path).exists());
    }

    #[tokio::test]
    async fn refresh_refresh_account_without_refresh_token_skips_and_keeps_account() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = write_auth_file(dir.path(), "a.json", "rt0").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();
        let acc = manager.accounts_snapshot()[0].clone();
        let token = {
            let token = acc.token();
            TokenData {
                id_token: token.id_token.clone(),
                access_token: token.access_token.clone(),
                refresh_token: String::new(),
                account_id: token.account_id.clone(),
                email: token.email.clone(),
                expired: token.expired.clone(),
                plan_type: token.plan_type.clone(),
            }
        };
        acc.update_token(token, now_unix_ms());

        let token_url = start_server("200").await;
        let refresher = Refresher::new("").unwrap().with_token_url(token_url);
        let save_queue = SaveQueue::start(1);

        let err = refresh_account(&manager, &refresher, &save_queue, acc, 1)
            .await
            .expect_err("should skip refresh");

        assert!(err.is_missing_refresh_token());
        assert_eq!(manager.account_count(), 1);
        assert!(Path::new(&file_path).exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refresh_refresh_account_without_refresh_token_401_path_removes_account() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = write_auth_file(dir.path(), "a.json", "rt0").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();
        let acc = manager.accounts_snapshot()[0].clone();
        let token = {
            let token = acc.token();
            TokenData {
                id_token: token.id_token.clone(),
                access_token: token.access_token.clone(),
                refresh_token: String::new(),
                account_id: token.account_id.clone(),
                email: token.email.clone(),
                expired: token.expired.clone(),
                plan_type: token.plan_type.clone(),
            }
        };
        acc.update_token(token, now_unix_ms());

        let token_url = start_server("200").await;
        let refresher = Refresher::new("").unwrap().with_token_url(token_url);
        let save_queue = SaveQueue::start(1);
        let output = SharedWriter::default();
        let writer = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let err = refresh_account_with_remove_reason(
            &manager,
            &refresher,
            &save_queue,
            acc,
            1,
            "auth_401",
        )
        .await
        .expect_err("should report missing refresh token");

        assert!(err.is_missing_refresh_token());
        assert_eq!(manager.account_count(), 0);
        assert!(!Path::new(&file_path).exists());
        let logs = output.snapshot();
        assert!(
            logs.contains("auth_401_missing_refresh_token"),
            "unexpected logs: {logs}"
        );
    }
}
