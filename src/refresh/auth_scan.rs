use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use crate::core::Manager;
use crate::state::RuntimeStateStore;

#[derive(Debug, Clone)]
pub struct AuthScanLoopConfig {
    pub scan_interval: Duration,
}

#[derive(Clone)]
pub struct AuthScanLoop {
    manager: Arc<Manager>,
    cfg: AuthScanLoopConfig,
    runtime_state: Option<Arc<RuntimeStateStore>>,
}

impl AuthScanLoop {
    pub fn new(manager: Arc<Manager>, cfg: AuthScanLoopConfig) -> Result<Self, String> {
        if cfg.scan_interval.is_zero() {
            return Err("scan_interval must be > 0".to_string());
        }
        Ok(Self {
            manager,
            cfg,
            runtime_state: None,
        })
    }

    pub fn with_runtime_state(mut self, runtime_state: Arc<RuntimeStateStore>) -> Self {
        self.runtime_state = Some(runtime_state);
        self
    }

    pub async fn start_loop(self, mut shutdown: watch::Receiver<bool>) {
        let mut scan_ticker = tokio::time::interval(self.cfg.scan_interval);
        scan_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        self.scan_once();

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("auth scan loop stopped");
                        return;
                    }
                }
                _ = scan_ticker.tick() => {
                    self.scan_once();
                }
            }
        }
    }

    fn scan_once(&self) {
        match self.manager.scan_new_files() {
            Ok(outcome) if outcome.added == 0 && outcome.removed == 0 => {}
            Ok(outcome) => {
                if let Some(runtime_state) = &self.runtime_state {
                    runtime_state.mark_dirty();
                }
                tracing::info!(
                    added = outcome.added,
                    removed = outcome.removed,
                    "auth scan updated accounts"
                );
            }
            Err(err) => tracing::warn!("auth scan failed: {err}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auth_scan_loop_stops_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(Manager::new(dir.path()));

        let loop_ = AuthScanLoop::new(
            manager,
            AuthScanLoopConfig {
                scan_interval: Duration::from_secs(3600),
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
    fn auth_scan_loop_hot_loads_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(Manager::new(dir.path()));

        let loop_ = AuthScanLoop::new(
            manager.clone(),
            AuthScanLoopConfig {
                scan_interval: Duration::from_secs(3600),
            },
        )
        .unwrap();

        assert_eq!(manager.account_count(), 0);
        loop_.scan_once();
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

        loop_.scan_once();
        assert_eq!(manager.account_count(), 2);
        loop_.scan_once();
        assert_eq!(manager.account_count(), 2);
    }

    #[tokio::test]
    async fn auth_scan_loop_prunes_deleted_auth_file_and_marks_runtime_state_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(Manager::new(dir.path()));

        let a_path = dir.path().join("a.json");
        let b_path = dir.path().join("b.json");
        std::fs::write(
            &a_path,
            r#"{"access_token":"at-a","refresh_token":"rt-a","account_id":"","email":"a@example.com","type":"codex","expired":"2000-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::write(
            &b_path,
            r#"{"access_token":"at-b","refresh_token":"rt-b","account_id":"","email":"b@example.com","type":"codex","expired":"2000-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        manager.load_accounts().unwrap();
        let runtime_state = Arc::new(RuntimeStateStore::new(dir.path()));
        runtime_state.save_now(manager.as_ref()).unwrap();

        std::fs::remove_file(&b_path).unwrap();

        let loop_ = AuthScanLoop::new(
            manager.clone(),
            AuthScanLoopConfig {
                scan_interval: Duration::from_secs(3600),
            },
        )
        .unwrap()
        .with_runtime_state(runtime_state.clone());

        loop_.scan_once();

        assert_eq!(manager.account_count(), 1);
        assert!(runtime_state.save_now_if_dirty(manager.as_ref()).unwrap());

        let state_path = dir.path().join(".codex-proxy-state.json");
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
        let accounts = state["accounts"].as_object().unwrap();
        assert_eq!(accounts.len(), 1);
        assert!(accounts.contains_key(&a_path.to_string_lossy().to_string()));
        assert!(!accounts.contains_key(&b_path.to_string_lossy().to_string()));
    }
}
