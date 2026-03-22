use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::core::{AccountRuntimeSnapshot, Manager, now_unix_ms};

const STATE_FILE_NAME: &str = ".codex-proxy-state.json";
const HOUR_MS: i64 = 3_600_000;
const MAX_HOURLY_BUCKETS: usize = 168;
const DEFAULT_SAVE_INTERVAL_SECS: u64 = 5;
const SAVE_INTERVAL_ENV: &str = "CODEX_PROXY_RUNTIME_STATE_SAVE_INTERVAL_SECS";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    saved_at_ms: i64,
    #[serde(default)]
    accounts: HashMap<String, AccountRuntimeSnapshot>,
    #[serde(default)]
    hourly: Vec<HourlyTrendPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HourlyTrendPoint {
    pub hour_start_ms: i64,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    #[serde(default)]
    pub cached_tokens: i64,
    #[serde(default)]
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Default)]
struct RuntimeState {
    hourly: BTreeMap<i64, HourlyTrendPoint>,
}

#[derive(Debug)]
pub struct RuntimeStateStore {
    state_file_path: PathBuf,
    inner: RwLock<RuntimeState>,
    dirty: AtomicBool,
    save_interval: Duration,
}

impl RuntimeStateStore {
    pub fn new(auth_dir: impl AsRef<Path>) -> Self {
        let save_interval = env::var(SAVE_INTERVAL_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(|secs| secs.max(1))
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(DEFAULT_SAVE_INTERVAL_SECS));

        Self {
            state_file_path: auth_dir.as_ref().join(STATE_FILE_NAME),
            inner: RwLock::new(RuntimeState::default()),
            dirty: AtomicBool::new(false),
            save_interval,
        }
    }

    pub fn load_and_apply(&self, manager: &Manager) -> Result<(), String> {
        let persisted = match self.load_from_disk()? {
            Some(v) => v,
            None => return Ok(()),
        };
        let now_ms = now_unix_ms();

        for account in manager.accounts_snapshot().iter() {
            if let Some(snapshot) = persisted.accounts.get(account.file_path()) {
                account.apply_runtime_snapshot(snapshot, now_ms);
            }
        }

        let mut inner = self.inner.write().expect("runtime state lock poisoned");
        inner.hourly.clear();
        for point in persisted.hourly {
            inner.hourly.insert(point.hour_start_ms, point);
        }
        trim_hourly_map(&mut inner.hourly);
        Ok(())
    }

    pub fn record_hourly_request(&self, now_ms: i64) {
        let mut inner = self.inner.write().expect("runtime state lock poisoned");
        let hour_start_ms = truncate_to_hour(now_ms);
        let entry = inner
            .hourly
            .entry(hour_start_ms)
            .or_insert_with(|| HourlyTrendPoint {
                hour_start_ms,
                ..HourlyTrendPoint::default()
            });
        entry.requests += 1;
        trim_hourly_map(&mut inner.hourly);
        self.dirty.store(true, Ordering::Release);
    }

    pub fn record_hourly_usage(
        &self,
        now_ms: i64,
        input_tokens: i64,
        output_tokens: i64,
        cached_tokens: i64,
        reasoning_tokens: i64,
        total_tokens: i64,
    ) {
        let mut inner = self.inner.write().expect("runtime state lock poisoned");
        let hour_start_ms = truncate_to_hour(now_ms);
        let entry = inner
            .hourly
            .entry(hour_start_ms)
            .or_insert_with(|| HourlyTrendPoint {
                hour_start_ms,
                ..HourlyTrendPoint::default()
            });
        entry.input_tokens += input_tokens.max(0);
        entry.output_tokens += output_tokens.max(0);
        entry.cached_tokens += cached_tokens.max(0);
        entry.reasoning_tokens += reasoning_tokens.max(0);
        entry.total_tokens += total_tokens.max(0);
        trim_hourly_map(&mut inner.hourly);
        self.dirty.store(true, Ordering::Release);
    }

    pub fn hourly_trend(&self) -> Vec<HourlyTrendPoint> {
        let inner = self.inner.read().expect("runtime state lock poisoned");
        inner.hourly.values().cloned().collect()
    }

    pub fn save_now(&self, manager: &Manager) -> Result<(), String> {
        let mut accounts = HashMap::new();
        for account in manager.accounts_snapshot().iter() {
            let snapshot = account.stats_snapshot();
            accounts.insert(snapshot.file_path.clone(), snapshot.runtime_snapshot());
        }
        let hourly = self.hourly_trend();
        let payload = PersistedState {
            saved_at_ms: now_unix_ms(),
            accounts,
            hourly,
        };
        let data = serde_json::to_string_pretty(&payload)
            .map_err(|e| format!("序列化状态文件失败: {e}"))?;

        let parent = self
            .state_file_path
            .parent()
            .ok_or_else(|| format!("无效状态文件路径: {}", self.state_file_path.display()))?;
        fs::create_dir_all(parent).map_err(|e| format!("创建状态目录失败: {e}"))?;
        let tmp_path = format!("{}.tmp", self.state_file_path.to_string_lossy());
        fs::write(&tmp_path, data).map_err(|e| format!("写入状态临时文件失败: {e}"))?;
        fs::rename(&tmp_path, &self.state_file_path).map_err(|e| {
            let _ = fs::remove_file(&tmp_path);
            format!("重命名状态文件失败: {e}")
        })?;
        Ok(())
    }

    pub fn start_save_loop(self: Arc<Self>, manager: Arc<Manager>) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.save_interval);
            loop {
                ticker.tick().await;
                if !self.dirty.swap(false, Ordering::AcqRel) {
                    continue;
                }
                if let Err(err) = self.save_now(manager.as_ref()) {
                    self.dirty.store(true, Ordering::Release);
                    tracing::warn!("runtime state save failed: {err}");
                }
            }
        });
    }

    fn load_from_disk(&self) -> Result<Option<PersistedState>, String> {
        let data = match fs::read_to_string(&self.state_file_path) {
            Ok(v) => v,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(format!("读取状态文件失败: {err}")),
        };
        let mut persisted: PersistedState =
            serde_json::from_str(&data).map_err(|e| format!("解析状态文件失败: {e}"))?;
        persisted.hourly.sort_by_key(|point| point.hour_start_ms);
        if persisted.hourly.len() > MAX_HOURLY_BUCKETS {
            let drain = persisted.hourly.len() - MAX_HOURLY_BUCKETS;
            persisted.hourly.drain(0..drain);
        }
        Ok(Some(persisted))
    }
}

fn truncate_to_hour(ts_ms: i64) -> i64 {
    ts_ms.div_euclid(HOUR_MS) * HOUR_MS
}

fn trim_hourly_map(hourly: &mut BTreeMap<i64, HourlyTrendPoint>) {
    while hourly.len() > MAX_HOURLY_BUCKETS {
        let Some(first_key) = hourly.keys().next().copied() else {
            break;
        };
        hourly.remove(&first_key);
    }
}
