use std::future::Future;
use std::time::Duration;

use reqwest::Url;
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::core::{TokenData, parse_id_token_claims};

pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

#[derive(Debug, Clone)]
pub enum RefreshErrorKind {
    Generic,
    MissingRefreshToken,
}

#[derive(Debug, Clone)]
pub struct RefreshError {
    kind: RefreshErrorKind,
    pub status_code: Option<u16>,
    pub msg: String,
}

impl RefreshError {
    pub fn new(status_code: Option<u16>, msg: impl Into<String>) -> Self {
        Self {
            kind: RefreshErrorKind::Generic,
            status_code,
            msg: msg.into(),
        }
    }

    pub fn missing_refresh_token() -> Self {
        Self {
            kind: RefreshErrorKind::MissingRefreshToken,
            status_code: None,
            msg: "缺少 refresh_token".to_string(),
        }
    }

    pub fn is_rate_limited(&self) -> bool {
        self.status_code == Some(429)
    }

    pub fn is_missing_refresh_token(&self) -> bool {
        matches!(self.kind, RefreshErrorKind::MissingRefreshToken)
    }

    pub fn is_non_retryable(&self) -> bool {
        self.msg.to_lowercase().contains("refresh_token_reused")
    }
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(code) = self.status_code {
            write!(f, "刷新失败 [{code}]: {}", self.msg)
        } else {
            write!(f, "{}", self.msg)
        }
    }
}

impl std::error::Error for RefreshError {}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: String,
    expires_in: i64,
}

#[derive(Debug, Clone)]
pub struct Refresher {
    client: reqwest::Client,
    token_url: Url,
}

impl Refresher {
    pub fn new(proxy_url: &str) -> Result<Self, String> {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(30));

        if !proxy_url.trim().is_empty() {
            match reqwest::Proxy::all(proxy_url) {
                Ok(proxy) => builder = builder.proxy(proxy),
                Err(err) => tracing::warn!("代理地址解析失败: {err}"),
            }
        }

        let client = builder
            .build()
            .map_err(|e| format!("构建刷新 HTTP client 失败: {e}"))?;
        Ok(Self::new_with_http(client))
    }

    pub fn new_with_http(client: reqwest::Client) -> Self {
        let token_url = Url::parse(TOKEN_URL).expect("TOKEN_URL is valid");
        Self { client, token_url }
    }

    pub fn with_token_url(mut self, token_url: Url) -> Self {
        self.token_url = token_url;
        self
    }

    pub async fn refresh_token(&self, refresh_token: &str) -> Result<TokenData, RefreshError> {
        if refresh_token.trim().is_empty() {
            return Err(RefreshError::new(None, "refresh_token 不能为空"));
        }

        let params = [
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", "openid profile email"),
        ];

        let resp = self
            .client
            .post(self.token_url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&params)
            .send()
            .await
            .map_err(|e| RefreshError::new(None, format!("刷新请求发送失败: {e}")))?;

        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|_| RefreshError::new(Some(status.as_u16()), "读取刷新响应失败"))?;

        if status != reqwest::StatusCode::OK {
            let mut msg = String::from_utf8_lossy(&body).to_string();
            if msg.len() > 150 {
                msg.truncate(150);
                msg.push_str("...");
            }
            return Err(RefreshError::new(Some(status.as_u16()), msg));
        }

        let token_resp: TokenResponse = serde_json::from_slice(&body).map_err(|e| {
            RefreshError::new(Some(status.as_u16()), format!("解析刷新响应失败: {e}"))
        })?;

        let (account_id, email, plan_type) = parse_id_token_claims(&token_resp.id_token);
        let expire = OffsetDateTime::now_utc() + time::Duration::seconds(token_resp.expires_in);
        let expire_str = expire.format(&Rfc3339).unwrap_or_else(|_| String::new());

        Ok(TokenData {
            id_token: token_resp.id_token,
            access_token: token_resp.access_token,
            refresh_token: token_resp.refresh_token,
            account_id,
            email,
            expired: expire_str,
            plan_type,
        })
    }

    pub async fn refresh_token_with_retry(
        &self,
        refresh_token: &str,
        max_retries: usize,
    ) -> Result<TokenData, RefreshError> {
        self.refresh_token_with_retry_impl(refresh_token, max_retries, tokio::time::sleep)
            .await
    }

    async fn refresh_token_with_retry_impl<F, Fut>(
        &self,
        refresh_token: &str,
        max_retries: usize,
        sleep_fn: F,
    ) -> Result<TokenData, RefreshError>
    where
        F: Fn(Duration) -> Fut,
        Fut: Future<Output = ()>,
    {
        let mut last_err: Option<RefreshError> = None;
        let retries = max_retries.max(1);

        for attempt in 0..retries {
            if attempt > 0 {
                sleep_fn(Duration::from_secs(attempt as u64)).await;
            }

            match self.refresh_token(refresh_token).await {
                Ok(td) => return Ok(td),
                Err(err) => {
                    if err.is_non_retryable() || err.is_rate_limited() {
                        return Err(err);
                    }
                    last_err = Some(err);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| RefreshError::new(None, "刷新失败")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Form, Router};
    use base64::Engine;
    use serde_json::json;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    #[derive(Clone, Default)]
    struct AppState {
        mode: &'static str,
        calls: Arc<AtomicUsize>,
    }

    #[derive(Debug, Deserialize)]
    struct TokenForm {
        client_id: String,
        grant_type: String,
        refresh_token: String,
        scope: String,
    }

    async fn oauth_token(
        State(state): State<AppState>,
        Form(form): Form<TokenForm>,
    ) -> (axum::http::StatusCode, String) {
        let call_index = state.calls.fetch_add(1, Ordering::Relaxed);
        if state.mode == "429" {
            return (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                "rate limited".to_string(),
            );
        }
        if state.mode == "reused" {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                "refresh_token_reused".to_string(),
            );
        }
        if state.mode == "flaky" && call_index == 0 {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "oops".to_string(),
            );
        }

        if form.client_id != CLIENT_ID
            || form.grant_type != "refresh_token"
            || form.scope != "openid profile email"
        {
            return (axum::http::StatusCode::BAD_REQUEST, "bad form".to_string());
        }

        let id_token = build_test_id_token("acc-1", "a@example.com", "plus");
        let body = json!({
            "access_token": format!("at-{}", form.refresh_token),
            "refresh_token": format!("rt-{}", form.refresh_token),
            "id_token": id_token,
            "token_type": "Bearer",
            "expires_in": 3600
        });

        (axum::http::StatusCode::OK, body.to_string())
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

    async fn start_server(state: AppState) -> Url {
        let app = Router::new()
            .route("/oauth/token", post(oauth_token))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Url::parse(&format!("http://{addr}/oauth/token")).unwrap()
    }

    #[tokio::test]
    async fn refresh_refresher_success() {
        let token_url = start_server(AppState {
            mode: "200",
            calls: Arc::new(AtomicUsize::new(0)),
        })
        .await;
        let refresher = Refresher::new("").unwrap().with_token_url(token_url);
        let td = refresher.refresh_token("rt0").await.expect("refresh ok");

        assert_eq!(td.access_token, "at-rt0");
        assert_eq!(td.refresh_token, "rt-rt0");
        assert_eq!(td.account_id, "acc-1");
        assert_eq!(td.email, "a@example.com");
        assert_eq!(td.plan_type, "plus");
        assert!(!td.expired.is_empty());
    }

    #[tokio::test]
    async fn refresh_refresher_rate_limited() {
        let calls = Arc::new(AtomicUsize::new(0));
        let token_url = start_server(AppState {
            mode: "429",
            calls: calls.clone(),
        })
        .await;
        let refresher = Refresher::new("").unwrap().with_token_url(token_url);
        let err = refresher
            .refresh_token("rt0")
            .await
            .expect_err("should fail");
        assert_eq!(err.status_code, Some(429));
        assert!(err.is_rate_limited());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn refresh_refresher_retries_transient_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let token_url = start_server(AppState {
            mode: "flaky",
            calls: calls.clone(),
        })
        .await;
        let refresher = Refresher::new("").unwrap().with_token_url(token_url);

        let td = refresher
            .refresh_token_with_retry_impl("rt0", 3, |_| async {})
            .await
            .expect("should succeed after retry");
        assert_eq!(td.access_token, "at-rt0");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn refresh_refresher_does_not_retry_on_non_retryable() {
        let calls = Arc::new(AtomicUsize::new(0));
        let token_url = start_server(AppState {
            mode: "reused",
            calls: calls.clone(),
        })
        .await;
        let refresher = Refresher::new("").unwrap().with_token_url(token_url);

        let err = refresher
            .refresh_token_with_retry_impl("rt0", 3, |_| async {})
            .await
            .expect_err("should fail");
        assert!(err.is_non_retryable());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
