use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Url;
use reqwest::header::HeaderMap;
use serde_json::Value;
use uuid::Uuid;

use crate::core::{Account, Manager};
use crate::limit::{AccountLimitGuard, RateLimiter};

pub const CODEX_CLIENT_VERSION: &str = "0.137.0";
pub const CODEX_USER_AGENT: &str = "codex_cli_rs/0.137.0 (Mac OS 26.0.1; arm64) Apple_Terminal/464";
const CODEX_ORIGINATOR: &str = "codex_cli_rs";

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
    client_version: String,
    user_agent: String,
}

pub struct UpstreamResponse {
    pub response: reqwest::Response,
    pub account: Arc<Account>,
    pub attempts: usize,
    pub account_limit_guard: AccountLimitGuard,
}

pub struct UpstreamRequest<'a> {
    pub manager: &'a Manager,
    pub model: &'a str,
    pub url: Url,
    pub body: Vec<u8>,
    pub stream: bool,
    pub max_retry: usize,
    pub passthrough_headers: Option<&'a HeaderMap>,
    pub on_401: Option<On401Hook>,
    pub initial_excluded: &'a HashSet<String>,
    pub rate_limiter: &'a RateLimiter,
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
            client_version: CODEX_CLIENT_VERSION.to_string(),
            user_agent: CODEX_USER_AGENT.to_string(),
        }
    }

    pub fn with_client_identity(mut self, client_version: String, user_agent: String) -> Self {
        if !client_version.is_empty() {
            self.client_version = client_version;
        }
        if !user_agent.is_empty() {
            self.user_agent = user_agent;
        }
        self
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

    pub async fn execute(
        &self,
        request: UpstreamRequest<'_>,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let UpstreamRequest {
            manager,
            model,
            url,
            body,
            stream,
            max_retry,
            passthrough_headers,
            on_401,
            initial_excluded,
            rate_limiter,
        } = request;
        let mut excluded = initial_excluded.clone();
        let max_attempts = max_retry.saturating_add(1).max(1);
        let mut last_err: Option<UpstreamError> = None;

        for attempt in 0..max_attempts {
            let account = match manager.pick_excluding(model, &excluded) {
                Ok(account) => account,
                Err(err) => {
                    return Err(last_err.unwrap_or_else(|| UpstreamError::Pick(err)));
                }
            };
            excluded.insert(account.file_path().to_string());

            let account_limit_guard = match rate_limiter.check_account(account.file_path()) {
                Ok(guard) => guard,
                Err(err) => {
                    let will_retry = attempt < max_attempts - 1;
                    tracing::warn!(
                        model = %model,
                        stream,
                        attempt = attempt + 1,
                        total = max_attempts,
                        account = account.file_path(),
                        scope = err.scope,
                        will_retry,
                        "account rate limit exceeded"
                    );
                    last_err = Some(UpstreamError::Status {
                        code: 429,
                        body: err.message.into_bytes(),
                    });
                    if will_retry {
                        continue;
                    }
                    break;
                }
            };

            let token = account.token().access_token.clone();
            let account_id = account.token().account_id.clone();

            let mut req = self
                .http
                .post(url.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
                .header("Origin", "https://chatgpt.com")
                .header(reqwest::header::REFERER, "https://chatgpt.com/")
                .body(body.clone());
            req = match header_clone(passthrough_headers, "User-Agent")
                .or_else(|| header_clone(passthrough_headers, "user-agent"))
            {
                Some(ua) => req.header(reqwest::header::USER_AGENT, ua),
                None => req.header(reqwest::header::USER_AGENT, &self.user_agent),
            };

            req = self.apply_identity_headers(req, passthrough_headers);

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
                        let will_retry = attempt < max_attempts - 1;
                        tracing::warn!(
                            model = %model,
                            stream,
                            attempt = attempt + 1,
                            total = max_attempts,
                            account = account.file_path(),
                            timeout = ?timeout,
                            will_retry,
                            "upstream request timed out before headers"
                        );
                        last_err = Some(UpstreamError::Network(format!(
                            "等待上游响应超时: {}s",
                            timeout.as_secs()
                        )));
                        if attempt < max_attempts - 1 {
                            continue;
                        }
                        account.record_client_failure();
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
                    let will_retry = attempt < max_attempts - 1;
                    tracing::warn!(
                        model = %model,
                        stream,
                        attempt = attempt + 1,
                        total = max_attempts,
                        account = account.file_path(),
                        error = %err,
                        will_retry,
                        "upstream request network error"
                    );
                    let e = UpstreamError::Network(format!("请求发送失败: {err}"));
                    last_err = Some(e);
                    if attempt < max_attempts - 1 {
                        continue;
                    }
                    account.record_client_failure();
                    break;
                }
            };

            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                let now_ms = crate::core::now_unix_ms();
                account.apply_codex_rate_limit_headers(resp.headers(), now_ms);
                if attempt > 0 {
                    tracing::info!(
                        model = %model,
                        stream,
                        attempt = attempt + 1,
                        total = max_attempts,
                        account = account.file_path(),
                        status,
                        "upstream request succeeded after retry"
                    );
                }
                return Ok(UpstreamResponse {
                    response: resp,
                    account,
                    attempts: attempt + 1,
                    account_limit_guard,
                });
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

            let retry_as_capacity = should_treat_as_capacity_retry(status, &err_body);
            self.apply_retry_side_effects(
                &account,
                status,
                &err_body,
                now_ms,
                on_401.as_ref(),
                retry_as_capacity,
            );

            let effective_status = if retry_as_capacity { 429 } else { status };
            let will_retry =
                (is_retryable_status(status) || retry_as_capacity) && attempt < max_attempts - 1;
            tracing::warn!(
                model = %model,
                stream,
                attempt = attempt + 1,
                total = max_attempts,
                account = account.file_path(),
                status,
                effective_status,
                retry_as_capacity,
                will_retry,
                body = %preview_body_for_log(&err_body, 240),
                "upstream request failed"
            );
            let e = UpstreamError::Status {
                code: effective_status,
                body: err_body.clone(),
            };
            last_err = Some(e.clone());
            if will_retry {
                continue;
            }

            account.record_client_failure();
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
        retry_as_capacity: bool,
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
            _ if retry_as_capacity => {
                let cooldown_ms = parse_retry_after_ms(
                    err_body,
                    now_ms,
                    self.retry_policy.default_cooldown_429_ms.max(0),
                );
                account.set_quota_cooldown(cooldown_ms, now_ms);
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

fn truncate_for_log(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

fn preview_body_for_log(body: &[u8], max_chars: usize) -> String {
    truncate_for_log(&String::from_utf8_lossy(body), max_chars)
}

fn is_retryable_status(code: u16) -> bool {
    if (200..300).contains(&code) {
        return false;
    }
    match code {
        400 | 403 => false,
        _ => true,
    }
}

impl CodexClient {
    fn apply_identity_headers(
        &self,
        mut req: reqwest::RequestBuilder,
        passthrough_headers: Option<&HeaderMap>,
    ) -> reqwest::RequestBuilder {
        req = match header_clone(passthrough_headers, "Version") {
            Some(value) => req.header("Version", value),
            None => req.header("Version", &self.client_version),
        };
        req = match header_clone(passthrough_headers, "Session_id") {
            Some(value) => req.header("Session_id", value),
            None => req.header("Session_id", Uuid::new_v4().to_string()),
        };
        req = match header_clone(passthrough_headers, "Originator") {
            Some(value) => req.header("Originator", value),
            None => req.header("Originator", CODEX_ORIGINATOR),
        };
        for header_name in ["X-Codex-Turn-Metadata", "X-Client-Request-Id"] {
            if let Some(value) = header_clone(passthrough_headers, header_name) {
                req = req.header(header_name, value);
            }
        }
        req
    }
}

fn header_clone(
    headers: Option<&HeaderMap>,
    name: &'static str,
) -> Option<reqwest::header::HeaderValue> {
    headers.and_then(|headers| headers.get(name)).cloned()
}

fn should_treat_as_capacity_retry(status: u16, body: &[u8]) -> bool {
    !matches!(status, 400 | 403 | 429) && is_capacity_error(body)
}

fn is_capacity_error(body: &[u8]) -> bool {
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| extract_capacity_message(&value).map(str::to_owned))
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned());
    message
        .to_ascii_lowercase()
        .contains("selected model is at capacity")
}

fn extract_capacity_message<'a>(value: &'a Value) -> Option<&'a str> {
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .or_else(|| value.get("error").and_then(Value::as_str))
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
            "Bearer atcap" => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error":{"message":"selected model is at capacity","resets_in_seconds":9}}"#,
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

    #[derive(Clone)]
    struct CapturedHeadersState {
        headers: Arc<Mutex<Vec<HeaderMap>>>,
    }

    async fn capture_headers(
        State(state): State<CapturedHeadersState>,
        headers: HeaderMap,
    ) -> (axum::http::StatusCode, &'static str) {
        state.headers.lock().unwrap().push(headers);
        (axum::http::StatusCode::OK, "ok")
    }

    async fn start_capture_headers_upstream(headers: Arc<Mutex<Vec<HeaderMap>>>) -> Url {
        let app = Router::new()
            .route("/backend-api/codex/responses", post(capture_headers))
            .with_state(CapturedHeadersState { headers });
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

    async fn execute_tuple_for_test(
        client: &CodexClient,
        manager: &Manager,
        url: Url,
        stream: bool,
        max_retry: usize,
        passthrough_headers: Option<&HeaderMap>,
        on_401: Option<On401Hook>,
    ) -> Result<(reqwest::Response, Arc<Account>, usize), UpstreamError> {
        let empty = HashSet::new();
        let rate_limiter = RateLimiter::default();
        let result = client
            .execute(UpstreamRequest {
                manager,
                model: "gpt-4.1",
                url,
                body: b"{}".to_vec(),
                stream,
                max_retry,
                passthrough_headers,
                on_401,
                initial_excluded: &empty,
                rate_limiter: &rate_limiter,
            })
            .await?;
        Ok((result.response, result.account, result.attempts))
    }

    #[tokio::test]
    async fn upstream_execute_switches_account() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base_url = start_upstream(calls.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at1").await;
        write_auth_file(dir.path(), "b.json", "at2").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();

        let client = CodexClient::new(base_url, "").unwrap();
        let url = client.responses_url().unwrap();

        let (resp, _acc, attempts) =
            execute_tuple_for_test(&client, &manager, url, true, 1, None, None)
                .await
                .expect("should succeed on second attempt");

        assert_eq!(attempts, 2);
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert_eq!(body, "data: ok\n\n");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn upstream_execute_invokes_on_401_hook_and_sets_cooldown() {
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

        let (resp, _acc, attempts) =
            execute_tuple_for_test(&client, &manager, url, true, 1, None, Some(on_401))
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
    async fn upstream_execute_sets_quota_cooldown_on_429() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base_url = start_upstream(calls.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at429").await;
        write_auth_file(dir.path(), "b.json", "at2").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();

        let client = CodexClient::new(base_url, "").unwrap();
        let url = client.responses_url().unwrap();

        let (resp, _acc, attempts) =
            execute_tuple_for_test(&client, &manager, url, true, 1, None, None)
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
    async fn upstream_execute_updates_used_percent_from_success_headers() {
        async fn upstream_with_headers(headers: HeaderMap) -> Response {
            let mut resp = Response::new(Body::from("ok"));
            *resp.status_mut() = axum::http::StatusCode::OK;
            resp.headers_mut().insert(
                "x-codex-primary-used-percent",
                axum::http::HeaderValue::from_static("12"),
            );
            resp.headers_mut().insert(
                "x-codex-primary-window-minutes",
                axum::http::HeaderValue::from_static("10080"),
            );
            resp.headers_mut().insert(
                "x-codex-secondary-used-percent",
                axum::http::HeaderValue::from_static("34"),
            );
            resp.headers_mut().insert(
                "x-codex-secondary-window-minutes",
                axum::http::HeaderValue::from_static("300"),
            );
            let _ = headers;
            resp
        }

        let app = Router::new().route("/backend-api/codex/responses", post(upstream_with_headers));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base_url = Url::parse(&format!("http://{addr}/backend-api/codex/")).unwrap();

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at2").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();

        let client = CodexClient::new(base_url, "").unwrap();
        let url = client.responses_url().unwrap();

        let (resp, _acc, attempts) =
            execute_tuple_for_test(&client, &manager, url, false, 0, None, None)
                .await
                .expect("should succeed");

        assert_eq!(attempts, 1);
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        let snap = manager.accounts_snapshot();
        let a = snap
            .iter()
            .find(|a| a.file_path().ends_with("a.json"))
            .unwrap();
        assert!(
            (a.used_percent() - 34.0).abs() < 0.01,
            "used={}",
            a.used_percent()
        );
        assert!(!a.quota_exhausted());
    }

    #[tokio::test]
    async fn upstream_execute_sets_quota_state_from_success_headers() {
        async fn upstream_with_exhausted_headers(headers: HeaderMap) -> Response {
            let mut resp = Response::new(Body::from("ok"));
            *resp.status_mut() = axum::http::StatusCode::OK;
            resp.headers_mut().insert(
                "x-codex-primary-used-percent",
                axum::http::HeaderValue::from_static("100"),
            );
            resp.headers_mut().insert(
                "x-codex-primary-window-minutes",
                axum::http::HeaderValue::from_static("10080"),
            );
            resp.headers_mut().insert(
                "x-codex-primary-reset-after-seconds",
                axum::http::HeaderValue::from_static("7200"),
            );
            resp.headers_mut().insert(
                "x-codex-secondary-used-percent",
                axum::http::HeaderValue::from_static("3"),
            );
            resp.headers_mut().insert(
                "x-codex-secondary-window-minutes",
                axum::http::HeaderValue::from_static("300"),
            );
            let _ = headers;
            resp
        }

        let app = Router::new().route(
            "/backend-api/codex/responses",
            post(upstream_with_exhausted_headers),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base_url = Url::parse(&format!("http://{addr}/backend-api/codex/")).unwrap();

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at2").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();

        let client = CodexClient::new(base_url, "").unwrap();
        let url = client.responses_url().unwrap();

        let (resp, _acc, attempts) =
            execute_tuple_for_test(&client, &manager, url, false, 0, None, None)
                .await
                .expect("should succeed");

        assert_eq!(attempts, 1);
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        let snap = manager.accounts_snapshot();
        let a = snap
            .iter()
            .find(|a| a.file_path().ends_with("a.json"))
            .unwrap();
        assert_eq!(a.status(), crate::core::AccountStatus::Cooldown);
        assert!(a.quota_exhausted());
        assert_eq!(a.used_percent_x100(), 10000);
        assert!(a.quota_resets_at_ms() > crate::core::now_unix_ms());
    }

    #[tokio::test]
    async fn upstream_execute_does_not_retry_on_403() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base_url = start_upstream(calls.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at403").await;
        write_auth_file(dir.path(), "b.json", "at2").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();

        let client = CodexClient::new(base_url, "").unwrap();
        let url = client.responses_url().unwrap();

        let err = execute_tuple_for_test(&client, &manager, url, true, 1, None, None)
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

    #[test]
    fn upstream_preview_body_for_log_truncates_long_payloads() {
        let preview = preview_body_for_log("你好abcdef".as_bytes(), 4);
        assert_eq!(preview, "你好ab...");
    }

    #[tokio::test]
    async fn upstream_execute_uses_default_identity_headers_when_passthrough_missing() {
        let captured = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
        let base_url = start_capture_headers_upstream(captured.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at2").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();

        let client = CodexClient::new(base_url, "").unwrap();
        let url = client.responses_url().unwrap();

        let (resp, _acc, attempts) =
            execute_tuple_for_test(&client, &manager, url, false, 0, None, None)
                .await
                .expect("should succeed");

        assert_eq!(attempts, 1);
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        let headers = captured.lock().unwrap();
        let headers = headers.first().expect("captured headers");
        assert_eq!(
            headers.get("Version").and_then(|v| v.to_str().ok()),
            Some(CODEX_CLIENT_VERSION)
        );
        assert_eq!(
            headers
                .get(axum::http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok()),
            Some(CODEX_USER_AGENT)
        );
        assert_eq!(
            headers.get("Originator").and_then(|v| v.to_str().ok()),
            Some(CODEX_ORIGINATOR)
        );
        let session_id = headers
            .get("Session_id")
            .and_then(|v| v.to_str().ok())
            .expect("Session_id header");
        assert!(Uuid::parse_str(session_id).is_ok());
        assert!(headers.get("X-Codex-Turn-Metadata").is_none());
        assert!(headers.get("X-Client-Request-Id").is_none());
    }

    #[tokio::test]
    async fn upstream_execute_preserves_whitelisted_passthrough_headers() {
        let captured = Arc::new(Mutex::new(Vec::<HeaderMap>::new()));
        let base_url = start_capture_headers_upstream(captured.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at2").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();

        let client = CodexClient::new(base_url, "").unwrap();
        let url = client.responses_url().unwrap();

        let session_id = Uuid::new_v4().to_string();
        let mut passthrough = HeaderMap::new();
        passthrough.insert(
            "Version",
            axum::http::HeaderValue::from_static("0.120.0-test"),
        );
        passthrough.insert(
            "Session_id",
            axum::http::HeaderValue::from_str(&session_id).unwrap(),
        );
        passthrough.insert(
            "Originator",
            axum::http::HeaderValue::from_static("codex-proxy-test"),
        );
        passthrough.insert(
            "X-Codex-Turn-Metadata",
            axum::http::HeaderValue::from_static("{\"turn\":\"t1\"}"),
        );
        passthrough.insert(
            "X-Client-Request-Id",
            axum::http::HeaderValue::from_static("req-123"),
        );

        let (resp, _acc, attempts) =
            execute_tuple_for_test(&client, &manager, url, false, 0, Some(&passthrough), None)
                .await
                .expect("should succeed");

        assert_eq!(attempts, 1);
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        let headers = captured.lock().unwrap();
        let headers = headers.first().expect("captured headers");
        assert_eq!(
            headers.get("Version").and_then(|v| v.to_str().ok()),
            Some("0.120.0-test")
        );
        assert_eq!(
            headers.get("Session_id").and_then(|v| v.to_str().ok()),
            Some(session_id.as_str())
        );
        assert_eq!(
            headers.get("Originator").and_then(|v| v.to_str().ok()),
            Some("codex-proxy-test")
        );
        assert_eq!(
            headers
                .get("X-Codex-Turn-Metadata")
                .and_then(|v| v.to_str().ok()),
            Some("{\"turn\":\"t1\"}")
        );
        assert_eq!(
            headers
                .get("X-Client-Request-Id")
                .and_then(|v| v.to_str().ok()),
            Some("req-123")
        );
        assert_eq!(
            headers
                .get(axum::http::header::USER_AGENT)
                .and_then(|v| v.to_str().ok()),
            Some(CODEX_USER_AGENT)
        );
    }

    #[tokio::test]
    async fn upstream_execute_treats_capacity_error_as_retryable() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base_url = start_upstream(calls.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "atcap").await;
        write_auth_file(dir.path(), "b.json", "at2").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();

        let client = CodexClient::new(base_url, "").unwrap();
        let url = client.responses_url().unwrap();

        let (resp, _acc, attempts) =
            execute_tuple_for_test(&client, &manager, url, true, 1, None, None)
                .await
                .expect("should retry capacity error and succeed");

        assert_eq!(attempts, 2);
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        let snap = manager.accounts_snapshot();
        let a = snap
            .iter()
            .find(|a| a.file_path().ends_with("a.json"))
            .unwrap();
        assert_eq!(a.status(), crate::core::AccountStatus::Cooldown);
        assert_eq!(a.used_percent_x100(), 10000);
    }

    #[tokio::test]
    async fn upstream_execute_returns_capacity_error_when_no_other_account_available() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base_url = start_upstream(calls.clone()).await;

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "atcap").await;

        let manager = Manager::new(dir.path());
        manager.load_accounts().unwrap();

        let client = CodexClient::new(base_url, "").unwrap();
        let url = client.responses_url().unwrap();

        let err = execute_tuple_for_test(&client, &manager, url, true, 1, None, None)
            .await
            .expect_err("capacity error should be returned cleanly");

        match err {
            UpstreamError::Status { code, body } => {
                assert_eq!(code, 429);
                assert!(String::from_utf8_lossy(&body).contains("selected model is at capacity"));
            }
            other => panic!("expected upstream status error, got: {other:?}"),
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[derive(Clone)]
    struct ProxyState {
        manager: Arc<Manager>,
        client: CodexClient,
    }

    async fn proxy_stream(State(state): State<ProxyState>) -> Response {
        let url = state.client.responses_url().unwrap();
        let empty = HashSet::new();
        let rate_limiter = RateLimiter::default();
        let upstream_result = state
            .client
            .execute(UpstreamRequest {
                manager: state.manager.as_ref(),
                model: "gpt-4.1",
                url,
                body: b"{}".to_vec(),
                stream: true,
                max_retry: 1,
                passthrough_headers: None,
                on_401: None,
                initial_excluded: &empty,
                rate_limiter: &rate_limiter,
            })
            .await
            .unwrap();
        let upstream = upstream_result.response;

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
