use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use crate::core::Manager;

use super::{Refresher, SaveQueue, filter_need_refresh, refresh_account_with_options};

#[derive(Debug, Clone)]
pub struct RefreshLoopConfig {
    pub refresh_interval: Duration,
    pub scan_interval: Duration,
    pub refresh_concurrency: usize,
    pub max_retries: usize,
    pub refresh_batch_size: usize,
    pub rate_limit_cooldown_ms: i64,
}

#[derive(Clone)]
pub struct RefreshLoop {
    manager: Arc<Manager>,
    refresher: Refresher,
    save_queue: SaveQueue,
    cfg: RefreshLoopConfig,
}

impl RefreshLoop {
    pub fn new(
        manager: Arc<Manager>,
        refresher: Refresher,
        save_queue: SaveQueue,
        cfg: RefreshLoopConfig,
    ) -> Result<Self, String> {
        if cfg.refresh_interval.is_zero() {
            return Err("refresh_interval must be > 0".to_string());
        }
        if cfg.scan_interval.is_zero() {
            return Err("scan_interval must be > 0".to_string());
        }
        Ok(Self {
            manager,
            refresher,
            save_queue,
            cfg: RefreshLoopConfig {
                refresh_concurrency: cfg.refresh_concurrency.max(1),
                refresh_batch_size: cfg.refresh_batch_size,
                max_retries: cfg.max_retries,
                rate_limit_cooldown_ms: cfg.rate_limit_cooldown_ms.max(0),
                ..cfg
            },
        })
    }

    pub async fn start_loop(self, mut shutdown: watch::Receiver<bool>) {
        let mut refresh_ticker = tokio::time::interval(self.cfg.refresh_interval);
        refresh_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut scan_ticker = tokio::time::interval(self.cfg.scan_interval);
        scan_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        // Go parity: startup does an immediate scan + refresh.
        self.scan_once();
        self.refresh_once().await;

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("refresh loop stopped");
                        return;
                    }
                }
                _ = scan_ticker.tick() => {
                    self.scan_once();
                }
                _ = refresh_ticker.tick() => {
                    self.scan_once();
                    self.refresh_once().await;
                }
            }
        }
    }

    fn scan_once(&self) {
        match self.manager.scan_new_files() {
            Ok(0) => {}
            Ok(n) => tracing::info!(count = n, "hot-load: added new accounts"),
            Err(err) => tracing::warn!("hot-load scan failed: {err}"),
        }
    }

    async fn refresh_once(&self) {
        let accounts = self.manager.accounts_snapshot();
        if accounts.is_empty() {
            return;
        }

        let need_refresh = filter_need_refresh(&accounts);
        if need_refresh.is_empty() {
            tracing::debug!("refresh: all tokens are still valid; skip");
            return;
        }

        tracing::info!(
            total = accounts.len(),
            need_refresh = need_refresh.len(),
            concurrency = self.cfg.refresh_concurrency,
            "refresh: start batch"
        );

        let batch_size = if self.cfg.refresh_batch_size == 0 {
            need_refresh.len().max(1)
        } else {
            self.cfg.refresh_batch_size.max(1)
        };

        for batch in need_refresh.chunks(batch_size) {
            let manager = self.manager.clone();
            let refresher = self.refresher.clone();
            let save_queue = self.save_queue.clone();
            let max_retries = self.cfg.max_retries;
            let rate_limit_cooldown_ms = self.cfg.rate_limit_cooldown_ms;

            let mut tasks = stream::iter(batch.iter().cloned())
                .map(|acc| {
                    let manager = manager.clone();
                    let refresher = refresher.clone();
                    let save_queue = save_queue.clone();
                    async move {
                        let _ = refresh_account_with_options(
                            manager.as_ref(),
                            &refresher,
                            &save_queue,
                            acc,
                            max_retries,
                            "refresh_failed",
                            rate_limit_cooldown_ms,
                        )
                        .await;
                    }
                })
                .buffer_unordered(self.cfg.refresh_concurrency);

            while tasks.next().await.is_some() {}
        }

        tracing::info!("refresh: batch done");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Manager;

    #[tokio::test]
    async fn refresh_loop_stops_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(Manager::new(dir.path()));
        let save_queue = SaveQueue::start(1);
        let refresher = Refresher::new("").unwrap();

        let loop_ = RefreshLoop::new(
            manager,
            refresher,
            save_queue,
            RefreshLoopConfig {
                refresh_interval: Duration::from_secs(3600),
                scan_interval: Duration::from_secs(3600),
                refresh_concurrency: 1,
                max_retries: 0,
                refresh_batch_size: 0,
                rate_limit_cooldown_ms: 60_000,
            },
        )
        .unwrap();

        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(async move {
            loop_.start_loop(rx).await;
        });

        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("loop should stop quickly")
            .unwrap();
    }

    #[test]
    fn refresh_scan_new_files_hot_loads_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Manager::new(dir.path());

        assert_eq!(manager.account_count(), 0);
        assert_eq!(manager.scan_new_files().unwrap(), 0);
        assert_eq!(manager.account_count(), 0);

        std::fs::write(
            dir.path().join("a.json"),
            r#"{"access_token":"at-a","refresh_token":"rt-a","account_id":"","email":"a@example.com","type":"codex","expired":"2000-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.json"),
            r#"{"access_token":"at-b","refresh_token":"rt-b","account_id":"","email":"b@example.com","type":"codex","expired":"2000-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        assert_eq!(manager.scan_new_files().unwrap(), 2);
        assert_eq!(manager.account_count(), 2);
        assert_eq!(manager.scan_new_files().unwrap(), 0);
        assert_eq!(manager.account_count(), 2);
    }
}
