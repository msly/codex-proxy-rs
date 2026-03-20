use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Url;
use serde_json::Value;
use uuid::Uuid;

use crate::core::{Account, Manager};

pub const CODEX_CLIENT_VERSION: &str = "0.101.0";
pub const CODEX_USER_AGENT: &str = "codex_cli_rs/0.101.0 (Mac OS 26.0.1; arm64) Apple_Terminal/464";

pub type On401Hook = Arc<dyn Fn(Arc<Account>) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub cooldown_401_ms: i64,
    pub default_cooldown_429_ms: i64,
    pub header_timeout: Option<Duration>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            cooldown_401_ms: 30_000,
            default_cooldown_429_ms: 60_000,
            header_timeout: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CodexClient {
    http: reqwest::Client,
    base_url: Url,
    retry_policy: RetryPolicy,
}

impl CodexClient {
    pub fn new(base_url: Url, proxy_url: &str) -> Result<Self, String> {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(60));

        if !proxy_url.trim().is_empty() {
            match reqwest::Proxy::all(proxy_url) {
                Ok(proxy) => builder = builder.proxy(proxy),
                Err(err) => tracing::warn!("代理地址解析失败: {err}"),
            }
        }

        let http = builder
            .build()
            .map_err(|e| format!("构建 upstream HTTP client 失败: {e}"))?;

        Ok(Self::new_with_http(base_url, http))
    }

    pub fn new_with_http(base_url: Url, http: reqwest::Client) -> Self {
        Self::new_with_http_and_policy(base_url, http, RetryPolicy::default())
    }

    pub fn new_with_http_and_policy(
        base_url: Url,
        http: reqwest::Client,
        retry_policy: RetryPolicy,
    ) -> Self {
        Self {
            http,
            base_url,
            retry_policy,
        }
    }

    pub fn responses_url(&self) -> Result<Url, String> {
        let mut u = self.base_url.clone();
        let base_path = u.path().trim_end_matches('/');
        u.set_path(&format!("{base_path}/responses"));
        u.set_query(None);
        u.set_fragment(None);
        Ok(u)
    }

    pub fn responses_compact_url(&self) -> Result<Url, String> {
        let mut u = self.base_url.clone();
        let base_path = u.path().trim_end_matches('/');
        u.set_path(&format!("{base_path}/responses/compact"));
        u.set_query(None);
        u.set_fragment(None);
        Ok(u)
    }

    pub async fn send_with_retry(
        &self,
        manager: &Manager,
        model: &str,
        url: Url,
        body: Vec<u8>,
        stream: bool,
        max_retry: usize,
        on_401: Option<On401Hook>,
    ) -> Result<(reqwest::Response, Arc<Account>, usize), UpstreamError> {
        self.send_with_retry_excluding(
            manager,
            model,
            url,
            body,
            stream,
            max_retry,
            on_401,
            &HashSet::new(),
        )
        .await
    }

    pub async fn send_with_retry_excluding(
        &self,
        manager: &Manager,
        model: &str,
        url: Url,
        body: Vec<u8>,
        stream: bool,
        max_retry: usize,
        on_401: Option<On401Hook>,
        initial_excluded: &HashSet<String>,
    ) -> Result<(reqwest::Response, Arc<Account>, usize), UpstreamError> {
        let mut excluded = initial_excluded.clone();
        let max_attempts = max_retry.saturating_add(1).max(1);
        let mut last_err: Option<UpstreamError> = None;

        for attempt in 0..max_attempts {
            let account = manager
                .pick_excluding(model, &excluded)
                .map_err(UpstreamError::Pick)?;
            excluded.insert(account.file_path().to_string());

            let token = account.token().access_token.clone();
            let account_id = account.token().account_id.clone();

            let mut req = self
                .http
                .post(url.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
                .header("Version", CODEX_CLIENT_VERSION)
                .header("Session_id", Uuid::new_v4().to_string())
                .header(reqwest::header::USER_AGENT, CODEX_USER_AGENT)
                .header("Origin", "https://chatgpt.com")
                .header(reqwest::header::REFERER, "https://chatgpt.com/")
                .header("Originator", "codex_cli_rs")
                .body(body.clone());

            if stream {
                req = req.header(reqwest::header::ACCEPT, "text/event-stream");
            } else {
                req = req.header(reqwest::header::ACCEPT, "application/json");
            }

            if !account_id.is_empty() {
                req = req.header("Chatgpt-Account-Id", account_id);
            }

            let send_result = match self.retry_policy.header_timeout {
                Some(timeout) => match tokio::time::timeout(timeout, req.send()).await {
                    Ok(result) => result,
                    Err(_) => {
                        let now_ms = crate::core::now_unix_ms();
                        account.record_failure(now_ms);
                        last_err = Some(UpstreamError::Network(format!(
                            "等待上游响应超时: {}s",
                            timeout.as_secs()
                        )));
                        if attempt < max_attempts - 1 {
                            continue;
                        }
                        break;
                    }
                },
                None => req.send().await,
            };

            let resp = match send_result {
                Ok(resp) => resp,
                Err(err) => {
                    let now_ms = crate::core::now_unix_ms();
                    account.record_failure(now_ms);
                    let e = UpstreamError::Network(format!("请求发送失败: {err}"));
                    last_err = Some(e);
                    if attempt < max_attempts - 1 {
                        continue;
                    }
                    break;
                }
            };

            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                return Ok((resp, account, attempt + 1));
            }

            let err_body = resp
                .bytes_stream()
                .take_while(|r| futures_util::future::ready(r.is_ok()))
                .fold(Vec::new(), |mut acc, chunk| async move {
                    if acc.len() >= (1 << 20) {
                        return acc;
                    }
                    if let Ok(bytes) = chunk {
                        let take = (1 << 20) - acc.len();
                        acc.extend_from_slice(&bytes[..bytes.len().min(take)]);
                    }
                    acc
                })
                .await;

            let now_ms = crate::core::now_unix_ms();
            account.record_failure(now_ms);

            self.apply_retry_side_effects(&account, status, &err_body, now_ms, on_401.as_ref());

            if is_retryable_status(status) && attempt < max_attempts - 1 {
                continue;
            }

            let e = UpstreamError::Status {
                code: status,
                body: err_body,
            };
            last_err = Some(e);
            break;
        }

        Err(last_err.unwrap_or_else(|| UpstreamError::Network("请求失败".to_string())))
    }

    fn apply_retry_side_effects(
        &self,
        account: &Arc<Account>,
        status: u16,
        err_body: &[u8],
        now_ms: i64,
        on_401: Option<&On401Hook>,
    ) {
        match status {
            401 => {
                account.set_cooldown(self.retry_policy.cooldown_401_ms.max(0), now_ms);
                if let Some(h) = on_401 {
                    h(account.clone());
                }
            }
            429 => {
                let cooldown_ms = parse_retry_after_ms(
                    err_body,
                    now_ms,
                    self.retry_policy.default_cooldown_429_ms.max(0),
                );
                account.set_quota_cooldown(cooldown_ms, now_ms);
            }
            403 => {
                account.set_cooldown(5 * 60_000, now_ms);
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub enum UpstreamError {
    Pick(String),
    Network(String),
    Status { code: u16, body: Vec<u8> },
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pick(msg) => write!(f, "{msg}"),
            Self::Network(msg) => write!(f, "{msg}"),
            Self::Status { code, body } => write!(
                f,
                "Codex API 错误 [{code}]: {}",
                String::from_utf8_lossy(body)
            ),
        }
    }
}

impl std::error::Error for UpstreamError {}

fn is_retryable_status(code: u16) -> bool {
    if (200..300).contains(&code) {
        return false;
    }
    match code {
        400 | 403 => false,
        _ => true,
    }
}

fn parse_retry_after_ms(body: &[u8], now_ms: i64, default_ms: i64) -> i64 {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return default_ms,
    };

    if let Some(resets_at) = v
        .get("error")
        .and_then(|e| e.get("resets_at"))
        .and_then(Value::as_i64)
    {
        let now_s = (now_ms / 1000).max(0);
        if resets_at > now_s {
            return (resets_at - now_s).saturating_mul(1000);
        }
    }
    if let Some(seconds) = v
        .get("error")
        .and_then(|e| e.get("resets_in_seconds"))
        .and_then(Value::as_i64)
    {
        if seconds > 0 {
            return seconds.saturating_mul(1000);
        }
    }

    default_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::{body::Body, response::Response};
    use std::net::SocketAddr;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    #[derive(Clone)]
    struct UpstreamState {
        calls: Arc<AtomicUsize>,
    }

    async fn upstream_responses(
        State(state): State<UpstreamState>,
        headers: HeaderMap,
    ) -> (axum::http::StatusCode, &'static str) {
        let version = headers
            .get("Version")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if version != CODEX_CLIENT_VERSION {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "missing/bad Version header",
            );
        }

        let session_id = headers
            .get("Session_id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if Uuid::parse_str(session_id).is_err() {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "missing/bad Session_id header",
            );
        }

        let ua = headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if ua != CODEX_USER_AGENT {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "missing/bad User-Agent header",
            );
        }

        let origin = headers
            .get("Origin")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if origin != "https://chatgpt.com" {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "missing/bad Origin header",
            );
        }

        let referer = headers
            .get(axum::http::header::REFERER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if referer != "https://chatgpt.com/" {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "missing/bad Referer header",
            );
        }

        let originator = headers
            .get("Originator")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if originator != "codex_cli_rs" {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "missing/bad Originator header",
            );
        }

        state.calls.fetch_add(1, Ordering::Relaxed);
        let auth = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        match auth {
            "Bearer at1" => (axum::http::StatusCode::UNAUTHORIZED, "unauthorized"),
            "Bearer at429" => (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                r#"{"error":{"resets_in_seconds":7}}"#,
            ),
            "Bearer at2" => (axum::http::StatusCode::OK, "data: ok\n\n"),
            _ => (axum::http::StatusCode::FORBIDDEN, "forbidden"),
        }
    }

    async fn start_upstream(calls: Arc<AtomicUsize>) -> Url {
        let app = Router::new()
            .route("/backend-api/codex/responses", post(upstream_responses))
            .with_state(UpstreamState { calls });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Url::parse(&format!("http://{addr}/backend-api/codex/")).unwrap()
    }

    async fn write_auth_file(dir: &Path, name: &str, access_token: &str) {
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
    async fn upstream_send_with_retry_switches_account() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base_url = start_upstream(calls.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at1").await;
        write_auth_file(dir.path(), "b.json", "at2").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();

        let client = CodexClient::new(base_url, "").unwrap();
        let url = client.responses_url().unwrap();

        let (resp, _acc, attempts) = client
            .send_with_retry(&manager, "gpt-4.1", url, b"{}".to_vec(), true, 1, None)
            .await
            .expect("should succeed on second attempt");

        assert_eq!(attempts, 2);
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert_eq!(body, "data: ok\n\n");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn upstream_send_with_retry_invokes_on_401_hook_and_sets_cooldown() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base_url = start_upstream(calls.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at1").await;
        write_auth_file(dir.path(), "b.json", "at2").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();

        let hook_calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));

        let hook_calls2 = hook_calls.clone();
        let seen2 = seen.clone();
        let on_401: On401Hook = Arc::new(move |acc: Arc<Account>| {
            hook_calls2.fetch_add(1, Ordering::Relaxed);
            seen2
                .lock()
                .expect("mutex poisoned")
                .push(acc.file_path().to_string());
        });

        let client = CodexClient::new(base_url, "").unwrap();
        let url = client.responses_url().unwrap();

        let (resp, _acc, attempts) = client
            .send_with_retry(
                &manager,
                "gpt-4.1",
                url,
                b"{}".to_vec(),
                true,
                1,
                Some(on_401),
            )
            .await
            .unwrap();

        assert_eq!(attempts, 2);
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        assert_eq!(hook_calls.load(Ordering::Relaxed), 1);
        assert!(
            seen.lock()
                .expect("mutex poisoned")
                .iter()
                .any(|p| p.ends_with("a.json")),
            "expected hook called with a.json"
        );

        let snap = manager.accounts_snapshot();
        let a = snap
            .iter()
            .find(|a| a.file_path().ends_with("a.json"))
            .unwrap();
        assert_eq!(a.status(), crate::core::AccountStatus::Cooldown);

        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn upstream_send_with_retry_sets_quota_cooldown_on_429() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base_url = start_upstream(calls.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at429").await;
        write_auth_file(dir.path(), "b.json", "at2").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();

        let client = CodexClient::new(base_url, "").unwrap();
        let url = client.responses_url().unwrap();

        let (resp, _acc, attempts) = client
            .send_with_retry(&manager, "gpt-4.1", url, b"{}".to_vec(), true, 1, None)
            .await
            .expect("should retry on 429 and succeed");

        assert_eq!(attempts, 2);
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        let snap = manager.accounts_snapshot();
        let a = snap
            .iter()
            .find(|a| a.file_path().ends_with("a.json"))
            .unwrap();
        assert_eq!(a.status(), crate::core::AccountStatus::Cooldown);
        assert_eq!(a.used_percent_x100(), 10000);

        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn upstream_send_with_retry_does_not_retry_on_403() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base_url = start_upstream(calls.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at403").await;
        write_auth_file(dir.path(), "b.json", "at2").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();

        let client = CodexClient::new(base_url, "").unwrap();
        let url = client.responses_url().unwrap();

        let err = client
            .send_with_retry(&manager, "gpt-4.1", url, b"{}".to_vec(), true, 1, None)
            .await
            .expect_err("403 should be non-retryable");
        match err {
            UpstreamError::Status { code, .. } => assert_eq!(code, 403),
            _ => panic!("expected status error, got: {err:?}"),
        }

        let snap = manager.accounts_snapshot();
        let a = snap
            .iter()
            .find(|a| a.file_path().ends_with("a.json"))
            .unwrap();
        assert_eq!(a.status(), crate::core::AccountStatus::Cooldown);

        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn upstream_parse_retry_after_ms_defaults_when_body_empty() {
        assert_eq!(parse_retry_after_ms(b"", 1_700_000_000_000, 60_000), 60_000);
    }

    #[test]
    fn upstream_parse_retry_after_ms_prefers_resets_at_over_resets_in_seconds() {
        let now_ms = 1_700_000_000_000i64;
        let now_s = now_ms / 1000;
        let body = serde_json::json!({
            "error": {
                "resets_at": now_s + 10,
                "resets_in_seconds": 999
            }
        });
        assert_eq!(
            parse_retry_after_ms(body.to_string().as_bytes(), now_ms, 60_000),
            10_000
        );
    }

    #[test]
    fn upstream_parse_retry_after_ms_uses_resets_in_seconds_when_resets_at_past() {
        let now_ms = 1_700_000_000_000i64;
        let now_s = now_ms / 1000;
        let body = serde_json::json!({
            "error": {
                "resets_at": now_s - 10,
                "resets_in_seconds": 7
            }
        });
        assert_eq!(
            parse_retry_after_ms(body.to_string().as_bytes(), now_ms, 60_000),
            7_000
        );
    }

    #[derive(Clone)]
    struct ProxyState {
        manager: Arc<Manager>,
        client: CodexClient,
    }

    async fn proxy_stream(State(state): State<ProxyState>) -> Response {
        let url = state.client.responses_url().unwrap();
        let (upstream, _acc, _attempts) = state
            .client
            .send_with_retry(
                &state.manager,
                "gpt-4.1",
                url,
                b"{}".to_vec(),
                true,
                1,
                None,
            )
            .await
            .unwrap();

        let status = upstream.status();
        let stream = upstream
            .bytes_stream()
            .map(|chunk| chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)));
        let mut resp = Response::new(Body::from_stream(stream));
        *resp.status_mut() = status;
        resp.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/event-stream"),
        );
        resp
    }

    #[tokio::test]
    async fn upstream_sse_gate_proxy_returns_only_success_stream() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base_url = start_upstream(calls.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at1").await;
        write_auth_file(dir.path(), "b.json", "at2").await;

        let manager = Arc::new(Manager::new(dir.path()));
        manager.load_accounts().unwrap();

        let client = CodexClient::new(base_url, "").unwrap();
        let app = Router::new()
            .route("/proxy", axum::routing::get(proxy_stream))
            .with_state(ProxyState {
                manager: manager.clone(),
                client,
            });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp = reqwest::get(format!("http://{addr}/proxy")).await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert_eq!(body, "data: ok\n\n");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
