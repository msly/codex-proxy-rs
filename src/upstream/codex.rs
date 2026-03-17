use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Url;

use crate::core::{Account, Manager};

pub const CODEX_CLIENT_VERSION: &str = "0.101.0";
pub const CODEX_USER_AGENT: &str = "codex_cli_rs/0.101.0 (Mac OS 26.0.1; arm64) Apple_Terminal/464";

#[derive(Debug, Clone)]
pub struct CodexClient {
    http: reqwest::Client,
    base_url: Url,
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

        Ok(Self { http, base_url })
    }

    pub fn responses_url(&self) -> Result<Url, String> {
        self.base_url
            .join("responses")
            .map_err(|e| format!("base_url 无效: {e}"))
    }

    pub async fn send_with_retry(
        &self,
        manager: &Manager,
        model: &str,
        url: Url,
        body: Vec<u8>,
        stream: bool,
        max_retry: usize,
    ) -> Result<(reqwest::Response, Arc<Account>, usize), UpstreamError> {
        let mut excluded = HashSet::<String>::new();
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

            let resp = match req.send().await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::Router;
    use axum::{body::Body, response::Response};
    use std::net::SocketAddr;
    use std::path::Path;
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
        state.calls.fetch_add(1, Ordering::Relaxed);
        let auth = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        match auth {
            "Bearer at1" => (axum::http::StatusCode::UNAUTHORIZED, "unauthorized"),
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
            .send_with_retry(&manager, "gpt-4.1", url, b"{}".to_vec(), true, 1)
            .await
            .expect("should succeed on second attempt");

        assert_eq!(attempts, 2);
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert_eq!(body, "data: ok\n\n");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
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
            .send_with_retry(&state.manager, "gpt-4.1", url, b"{}".to_vec(), true, 1)
            .await
            .unwrap();

        let status = upstream.status();
        let stream = upstream.bytes_stream().map(|chunk| {
            chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        });
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
