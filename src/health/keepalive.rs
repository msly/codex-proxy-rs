use std::time::Duration;

use reqwest::Url;

use crate::upstream::codex::CODEX_USER_AGENT;

#[derive(Debug, Clone)]
pub struct KeepAliveConfig {
    pub interval: Duration,
    pub request_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct KeepAlive {
    http: reqwest::Client,
    ping_url: Url,
    cfg: KeepAliveConfig,
}

impl KeepAlive {
    pub fn new(ping_url: Url, proxy_url: &str, cfg: KeepAliveConfig) -> Result<Self, String> {
        let mut builder = reqwest::Client::builder().timeout(cfg.request_timeout);
        if !proxy_url.trim().is_empty() {
            match reqwest::Proxy::all(proxy_url) {
                Ok(proxy) => builder = builder.proxy(proxy),
                Err(err) => tracing::warn!("代理地址解析失败: {err}"),
            }
        }
        let http = builder
            .build()
            .map_err(|e| format!("构建 keepalive HTTP client 失败: {e}"))?;

        Ok(Self::new_with_http(ping_url, http, cfg))
    }

    pub fn new_with_http(ping_url: Url, http: reqwest::Client, cfg: KeepAliveConfig) -> Self {
        Self {
            http,
            ping_url,
            cfg,
        }
    }

    pub fn ping_url(&self) -> &Url {
        &self.ping_url
    }

    pub async fn start_loop(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        tracing::info!(ping = %self.ping_url, interval = ?self.cfg.interval, "keepalive started");
        let mut ticker = tokio::time::interval(self.cfg.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("keepalive stopped");
                        return;
                    }
                }
                _ = ticker.tick() => {
                    let _ = self.ping_once().await;
                }
            }
        }
    }

    pub async fn ping_once(&self) -> Result<(), String> {
        let resp = self
            .http
            .head(self.ping_url.clone())
            .header(reqwest::header::USER_AGENT, CODEX_USER_AGENT)
            .header(reqwest::header::CONNECTION, "Keep-Alive")
            .send()
            .await
            .map_err(|e| format!("keepalive ping failed: {e}"))?;
        let status = resp.status().as_u16();
        drop(resp);
        tracing::debug!(status, "keepalive ping done");
        Ok(())
    }
}
