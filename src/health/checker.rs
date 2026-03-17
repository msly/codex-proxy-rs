use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reqwest::Url;
use serde_json::Value;

use crate::core::{Account, Manager, now_unix_ms};
use crate::upstream::codex::CODEX_USER_AGENT;

#[derive(Debug, Clone)]
pub struct HealthCheckerConfig {
    pub check_interval: Duration,
    pub max_consecutive_failures: i64,
    pub concurrency: usize,
    pub start_delay: Duration,
    pub batch_size: usize,
    pub request_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct HealthChecker {
    http: reqwest::Client,
    responses_url: Url,
    cfg: HealthCheckerConfig,
    cursor: Arc<AtomicU64>,
}

impl HealthChecker {
    pub fn new(base_url: Url, proxy_url: &str, cfg: HealthCheckerConfig) -> Result<Self, String> {
        let mut responses_url = base_url;
        let base_path = responses_url.path().trim_end_matches('/');
        responses_url.set_path(&format!("{base_path}/responses"));
        responses_url.set_query(None);
        responses_url.set_fragment(None);

        let mut builder = reqwest::Client::builder().timeout(cfg.request_timeout);
        if !proxy_url.trim().is_empty() {
            match reqwest::Proxy::all(proxy_url) {
                Ok(proxy) => builder = builder.proxy(proxy),
                Err(err) => tracing::warn!("代理地址解析失败: {err}"),
            }
        }

        let http = builder
            .build()
            .map_err(|e| format!("构建 health HTTP client 失败: {e}"))?;

        Ok(Self {
            http,
            responses_url,
            cfg,
            cursor: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn responses_url(&self) -> &Url {
        &self.responses_url
    }

    pub async fn start_loop(
        self: Arc<Self>,
        manager: Arc<Manager>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) {
        tracing::info!(
            interval = ?self.cfg.check_interval,
            concurrency = self.cfg.concurrency,
            batch_size = self.cfg.batch_size,
            start_delay = ?self.cfg.start_delay,
            max_failures = self.cfg.max_consecutive_failures,
            timeout = ?self.cfg.request_timeout,
            "health checker started"
        );

        if self.cfg.start_delay > Duration::from_secs(0) {
            tokio::select! {
                _ = tokio::time::sleep(self.cfg.start_delay) => {}
                _ = shutdown.changed() => {
                    tracing::info!("health checker stopped (before start)");
                    return;
                }
            }
        }

        let mut ticker = tokio::time::interval(self.cfg.check_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("health checker stopped");
                        return;
                    }
                }
                _ = ticker.tick() => {
                    self.check_all_once(manager.clone()).await;
                }
            }
        }
    }

    pub async fn check_all_once(&self, manager: Arc<Manager>) {
        let accounts = manager.accounts_snapshot();
        if accounts.is_empty() {
            return;
        }

        let selected = pick_batch(&accounts, self.cfg.batch_size, &self.cursor);
        if selected.is_empty() {
            return;
        }

        let sem = Arc::new(tokio::sync::Semaphore::new(self.cfg.concurrency.max(1)));
        let mut handles = Vec::with_capacity(selected.len());

        for acc in selected {
            let checker = self.clone();
            let manager = manager.clone();
            let sem = sem.clone();
            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.unwrap();
                checker.check_account(&manager, acc).await;
            }));
        }

        for h in handles {
            let _ = h.await;
        }
    }

    async fn check_account(&self, manager: &Manager, acc: Arc<Account>) {
        let (access_token, account_id, email) = {
            let token = acc.token();
            (
                token.access_token.clone(),
                token.account_id.clone(),
                token.email.clone(),
            )
        };

        if access_token.trim().is_empty() {
            manager.remove_account(acc.file_path(), "empty_access_token");
            return;
        }

        let body = r#"{"model":"gpt-5","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"ping"}]}],"stream":false,"store":false,"max_output_tokens":1,"reasoning":{"effort":"minimal"}}"#;

        let mut req = self
            .http
            .post(self.responses_url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {access_token}"),
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, CODEX_USER_AGENT)
            .header("Origin", "https://chatgpt.com")
            .header(reqwest::header::REFERER, "https://chatgpt.com/")
            .header("X-Health-Check", "1")
            .body(body);

        if !account_id.is_empty() {
            req = req.header("Chatgpt-Account-Id", account_id);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(err) => {
                tracing::debug!("health check network error [{email}]: {err}");
                return;
            }
        };

        let status = resp.status().as_u16();
        let body = read_body_limited(resp, 1 << 20).await;
        let now_ms = now_unix_ms();

        match status {
            401 => {
                let failures = acc.record_failure(now_ms);
                if failures >= self.cfg.max_consecutive_failures {
                    manager.remove_account(acc.file_path(), "auth_401");
                } else {
                    tracing::debug!(
                        email,
                        failures,
                        threshold = self.cfg.max_consecutive_failures,
                        "health check 401"
                    );
                }
            }
            403 => {
                acc.set_cooldown(5 * 60_000, now_ms);
            }
            400 => {
                if should_ignore_400(&body) {
                    return;
                }
            }
            429 => {
                let cooldown_ms = parse_retry_after_ms(&body, 60_000);
                acc.set_cooldown(cooldown_ms, now_ms);
                tracing::info!(email, cooldown_ms, "health check 429 (cooldown)");
            }
            200..=299 => {
                acc.record_success(now_ms);
            }
            500..=599 => {
                tracing::debug!(email, status, "health check upstream 5xx");
            }
            _ => {
                let mut preview = String::from_utf8_lossy(&body).to_string();
                if preview.len() > 120 {
                    preview.truncate(120);
                }
                tracing::warn!(
                    email,
                    status,
                    body = preview,
                    "health check unexpected status"
                );
            }
        }
    }
}

fn pick_batch(
    accounts: &Arc<Vec<Arc<Account>>>,
    batch_size: usize,
    cursor: &Arc<AtomicU64>,
) -> Vec<Arc<Account>> {
    if batch_size == 0 || batch_size >= accounts.len() {
        return accounts.iter().cloned().collect();
    }

    let start = (cursor.fetch_add(batch_size as u64, Ordering::Relaxed) as usize) % accounts.len();
    let mut selected = Vec::with_capacity(batch_size);
    for i in 0..batch_size {
        selected.push(accounts[(start + i) % accounts.len()].clone());
    }
    selected
}

fn should_ignore_400(body: &[u8]) -> bool {
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let msg = v
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_lowercase();

    msg.contains("model") || msg.contains("reason")
}

fn parse_retry_after_ms(body: &[u8], default_ms: i64) -> i64 {
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return default_ms,
    };

    if let Some(seconds) = v
        .get("error")
        .and_then(|e| e.get("resets_in_seconds"))
        .and_then(Value::as_i64)
    {
        if seconds > 0 {
            return seconds.saturating_mul(1000);
        }
    }
    if let Some(resets_at) = v
        .get("error")
        .and_then(|e| e.get("resets_at"))
        .and_then(Value::as_i64)
    {
        let now_s = (now_unix_ms() / 1000).max(0);
        if resets_at > now_s {
            return (resets_at - now_s).saturating_mul(1000);
        }
    }

    default_ms
}

async fn read_body_limited(resp: reqwest::Response, max_bytes: usize) -> Vec<u8> {
    use futures_util::StreamExt;

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
