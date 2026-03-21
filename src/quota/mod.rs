use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt;
use reqwest::Url;
use serde::Serialize;

use crate::core::{Account, Manager, QuotaInfo, now_unix_ms};
use crate::upstream::codex::CODEX_USER_AGENT;

#[derive(Debug, Clone)]
pub struct QuotaChecker {
    http: reqwest::Client,
    concurrency: usize,
    usage_url: Url,
}

impl QuotaChecker {
    pub fn new(
        base_url: &str,
        backend_domain: &str,
        proxy_url: &str,
        concurrency: usize,
    ) -> Result<Self, String> {
        let usage_url = derive_usage_url(base_url, backend_domain)?;

        let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20));
        if !proxy_url.trim().is_empty() {
            match reqwest::Proxy::all(proxy_url) {
                Ok(proxy) => builder = builder.proxy(proxy),
                Err(err) => tracing::warn!("代理地址解析失败: {err}"),
            }
        }

        let http = builder
            .build()
            .map_err(|e| format!("构建 quota HTTP client 失败: {e}"))?;

        Ok(Self::new_with_http(usage_url, concurrency, http))
    }

    pub fn new_with_http(usage_url: Url, concurrency: usize, http: reqwest::Client) -> Self {
        Self {
            http,
            concurrency: concurrency.max(1),
            usage_url,
        }
    }

    pub fn new_with_config(
        cfg: &crate::config::Config,
        concurrency: usize,
        http: reqwest::Client,
    ) -> Result<Self, String> {
        let usage_url = derive_usage_url(&cfg.base_url, &cfg.backend_domain)?;
        Ok(Self::new_with_http(usage_url, concurrency, http))
    }

    pub fn usage_url(&self) -> &Url {
        &self.usage_url
    }

    pub async fn check_one(&self, acc: Arc<Account>) -> bool {
        matches!(self.check_account(acc).await, CheckOutcome::Valid { .. })
    }

    async fn check_account(&self, acc: Arc<Account>) -> CheckOutcome {
        let (access_token, account_id, email) = {
            let token = acc.token();
            (
                token.access_token.clone(),
                token.account_id.clone(),
                token.email.clone(),
            )
        };

        if access_token.trim().is_empty() {
            return CheckOutcome::Invalid { email };
        }

        let mut req = self
            .http
            .get(self.usage_url.clone())
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {access_token}"),
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, CODEX_USER_AGENT)
            .header("Origin", "https://chatgpt.com")
            .header(reqwest::header::REFERER, "https://chatgpt.com/");

        if !account_id.is_empty() {
            req = req.header("Chatgpt-Account-Id", account_id);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(err) => {
                tracing::debug!("账号 [{email}] 额度查询网络错误: {err}");
                return CheckOutcome::Failed { email };
            }
        };

        let status = resp.status().as_u16();
        let body = read_body_limited(resp, 1 << 20).await;
        let now_ms = now_unix_ms();

        if status == 200 {
            let raw_data = if serde_json::from_slice::<serde_json::Value>(&body).is_ok() {
                body
            } else {
                Vec::new()
            };

            acc.set_quota_info(QuotaInfo {
                valid: true,
                status_code: status,
                raw_data,
                checked_at_ms: now_ms,
            });

            return CheckOutcome::Valid { email };
        }

        let raw_data = if serde_json::from_slice::<serde_json::Value>(&body).is_ok() {
            body
        } else if body.is_empty() {
            Vec::new()
        } else {
            let mut truncated = String::from_utf8_lossy(&body).to_string();
            if truncated.len() > 200 {
                truncated.truncate(200);
            }
            serde_json::to_vec(&truncated).unwrap_or_default()
        };

        acc.set_quota_info(QuotaInfo {
            valid: false,
            status_code: status,
            raw_data,
            checked_at_ms: now_ms,
        });

        match status {
            401 | 403 => CheckOutcome::Invalid { email },
            _ => CheckOutcome::Failed { email },
        }
    }

    pub fn check_all_stream(
        &self,
        manager: Arc<Manager>,
    ) -> tokio::sync::mpsc::Receiver<ProgressEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel::<ProgressEvent>(100);
        let checker = self.clone();

        tokio::spawn(async move {
            let accounts = manager.accounts_snapshot();
            let total = accounts.len();
            if total == 0 {
                let _ = tx
                    .send(ProgressEvent {
                        event_type: "done",
                        email: None,
                        success: None,
                        message: Some("无账号".to_string()),
                        total: None,
                        success_count: None,
                        failed_count: None,
                        remaining: None,
                        duration: Some("0s".to_string()),
                        current: None,
                    })
                    .await;
                return;
            }

            let start = Instant::now();
            let sem = Arc::new(tokio::sync::Semaphore::new(checker.concurrency));
            let valid_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let invalid_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let failed_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let current = Arc::new(std::sync::atomic::AtomicUsize::new(0));

            let mut handles = Vec::with_capacity(total);
            for acc in accounts.iter() {
                let tx = tx.clone();
                let sem = sem.clone();
                let checker = checker.clone();
                let manager = manager.clone();
                let valid_count = valid_count.clone();
                let invalid_count = invalid_count.clone();
                let failed_count = failed_count.clone();
                let current = current.clone();
                let acc = acc.clone();

                handles.push(tokio::spawn(async move {
                    let _permit = sem.acquire_owned().await.unwrap();
                    let outcome = checker.check_account(acc.clone()).await;

                    let cur = current.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    let (email, ok, invalid) = match outcome {
                        CheckOutcome::Valid { email } => {
                            valid_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            (email, true, false)
                        }
                        CheckOutcome::Invalid { email } => {
                            invalid_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            manager.remove_account(acc.file_path(), "quota_invalid");
                            (email, false, true)
                        }
                        CheckOutcome::Failed { email } => {
                            failed_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            (email, false, false)
                        }
                    };

                    let _ = tx
                        .send(ProgressEvent {
                            event_type: "item",
                            email: Some(email),
                            success: Some(ok),
                            message: None,
                            total: Some(total),
                            success_count: None,
                            failed_count: None,
                            remaining: None,
                            duration: None,
                            current: Some(cur),
                        })
                        .await;

                    invalid
                }));
            }

            for h in handles {
                let _ = h.await;
            }

            let vc = valid_count.load(std::sync::atomic::Ordering::Relaxed);
            let ic = invalid_count.load(std::sync::atomic::Ordering::Relaxed);
            let fc = failed_count.load(std::sync::atomic::Ordering::Relaxed);
            let elapsed = start.elapsed();
            let _ = tx
                .send(ProgressEvent {
                    event_type: "done",
                    email: None,
                    success: None,
                    message: Some("额度查询完成".to_string()),
                    total: Some(total),
                    success_count: Some(vc),
                    failed_count: Some(ic + fc),
                    remaining: Some(manager.account_count()),
                    duration: Some(format!("{elapsed:?}")),
                    current: None,
                })
                .await;
        });

        rx
    }
}

#[derive(Debug)]
enum CheckOutcome {
    Valid { email: String },
    Invalid { email: String },
    Failed { email: String },
}

#[derive(Debug, Serialize, Clone)]
pub struct ProgressEvent {
    #[serde(rename = "type")]
    pub event_type: &'static str,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub success_count: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_count: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<usize>,
}

fn derive_usage_url(base_url: &str, backend_domain: &str) -> Result<Url, String> {
    let backend_domain = backend_domain.trim();
    let mut usage_url = Url::parse("https://chatgpt.com/backend-api/wham/usage")
        .expect("default usage url is valid");

    if !backend_domain.is_empty() {
        usage_url = Url::parse(&format!("https://{backend_domain}/backend-api/wham/usage"))
            .map_err(|e| format!("usage url 无效: {e}"))?;
    }

    if !base_url.trim().is_empty() {
        if let Ok(mut u) = Url::parse(base_url) {
            if u.host_str().is_some() {
                u.set_path("/backend-api/wham/usage");
                u.set_query(None);
                u.set_fragment(None);
                usage_url = u;
            }
        }
    }

    Ok(usage_url)
}

async fn read_body_limited(resp: reqwest::Response, max_bytes: usize) -> Vec<u8> {
    resp.bytes_stream()
        .take_while(|r| futures_util::future::ready(r.is_ok()))
        .fold(Vec::new(), |mut acc, chunk| async move {
            if acc.len() >= max_bytes {
                return acc;
            }
            if let Ok(bytes) = chunk {
                let take = max_bytes - acc.len();
                acc.extend_from_slice(&bytes[..bytes.len().min(take)]);
            }
            acc
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::routing::get;
    use axum::{Json, Router};
    use std::net::SocketAddr;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    #[derive(Clone)]
    struct AppState {
        calls: Arc<AtomicUsize>,
    }

    async fn usage(
        State(state): State<AppState>,
        headers: axum::http::HeaderMap,
    ) -> (axum::http::StatusCode, Json<serde_json::Value>) {
        state.calls.fetch_add(1, Ordering::Relaxed);
        let auth = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let body = match auth {
            "Bearer at-ok" => serde_json::json!({
                "rate_limit": { "primary_window": { "used_percent": 12.34 } }
            }),
            "Bearer at429" => serde_json::json!({
                "error": { "resets_in_seconds": 7 }
            }),
            _ => serde_json::json!({ "error": "invalid" }),
        };

        let status = match auth {
            "Bearer at-ok" => axum::http::StatusCode::OK,
            "Bearer at429" => axum::http::StatusCode::TOO_MANY_REQUESTS,
            _ => axum::http::StatusCode::UNAUTHORIZED,
        };

        (status, Json(body))
    }

    async fn start_server(calls: Arc<AtomicUsize>) -> Url {
        let app = Router::new()
            .route("/backend-api/wham/usage", get(usage))
            .with_state(AppState { calls });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Url::parse(&format!("http://{addr}")).unwrap()
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
    async fn quota_check_one_updates_cache_and_used_percent() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base = start_server(calls.clone()).await;
        let base_url = base.join("/backend-api/codex").expect("join base url");

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at-ok").await;

        let manager = Arc::new(Manager::new(dir.path()));
        manager.load_accounts().unwrap();
        let acc = manager.accounts_snapshot()[0].clone();

        let qc = QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap();
        let outcome = qc.check_account(acc.clone()).await;
        assert!(matches!(outcome, CheckOutcome::Valid { .. }));
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let info = acc.quota_info().expect("quota info");
        assert!(info.valid);
        assert_eq!(info.status_code, 200);
        assert!(!info.raw_data.is_empty());
        assert!((acc.used_percent() - 12.34).abs() < 0.02);
    }

    #[tokio::test]
    async fn quota_invalid_account_is_removed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base = start_server(calls.clone()).await;
        let base_url = base.join("/backend-api/codex").expect("join base url");

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at-bad").await;

        let manager = Arc::new(Manager::new(dir.path()));
        manager.load_accounts().unwrap();
        let acc = manager.accounts_snapshot()[0].clone();

        let qc = QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap();
        let outcome = qc.check_account(acc.clone()).await;
        assert!(matches!(outcome, CheckOutcome::Invalid { .. }));
        manager.remove_account(acc.file_path(), "quota_invalid");

        assert_eq!(manager.account_count(), 0);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(!Path::new(acc.file_path()).exists());
    }

    #[tokio::test]
    async fn quota_temporary_error_does_not_remove_account() {
        let calls = Arc::new(AtomicUsize::new(0));
        let base = start_server(calls.clone()).await;
        let base_url = base.join("/backend-api/codex").expect("join base url");

        let dir = tempfile::tempdir().unwrap();
        write_auth_file(dir.path(), "a.json", "at429").await;

        let manager = Arc::new(Manager::new(dir.path()));
        manager.load_accounts().unwrap();

        let qc = QuotaChecker::new(&base_url.to_string(), "", "", 1).unwrap();
        let mut rx = qc.check_all_stream(manager.clone());
        while let Some(evt) = rx.recv().await {
            if evt.event_type == "done" {
                break;
            }
        }

        assert_eq!(manager.account_count(), 1);
        assert!(dir.path().join("a.json").exists());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
