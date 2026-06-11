use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::RateLimitConfig;
use crate::core::now_unix_ms;

#[derive(Debug)]
pub struct RateLimiter {
    cfg: RateLimitConfig,
    key_windows: Arc<Mutex<HashMap<String, FixedWindow>>>,
    account_windows: Arc<Mutex<HashMap<String, FixedWindow>>>,
    key_semaphores: Arc<Mutex<HashMap<String, CachedSemaphore>>>,
    account_semaphores: Arc<Mutex<HashMap<String, CachedSemaphore>>>,
    cache_ttl_ms: i64,
    last_prune_ms: AtomicI64,
}

#[derive(Debug, Clone, Copy, Default)]
struct FixedWindow {
    window_start_ms: i64,
    last_seen_ms: i64,
    count: u64,
}

#[derive(Debug, Clone)]
struct CachedSemaphore {
    semaphore: Arc<Semaphore>,
    last_seen_ms: i64,
}

#[derive(Debug, Serialize)]
pub struct RateLimitSnapshot {
    pub key_rpm: u64,
    pub key_concurrency: usize,
    pub account_rpm: u64,
    pub account_concurrency: usize,
    pub cache_ttl_sec: u64,
}

#[derive(Debug)]
pub struct RequestLimitGuard {
    _key_permit: Option<OwnedSemaphorePermit>,
}

impl RequestLimitGuard {
    pub fn disabled() -> Self {
        Self { _key_permit: None }
    }
}

#[derive(Debug)]
pub struct AccountLimitGuard {
    _account_permit: Option<OwnedSemaphorePermit>,
}

#[derive(Debug, Clone)]
pub struct LimitError {
    pub scope: &'static str,
    pub message: String,
}

impl std::fmt::Display for LimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for LimitError {}

impl RateLimiter {
    pub fn new(cfg: RateLimitConfig) -> Self {
        let cache_ttl_sec = if cfg.cache_ttl_sec == 0 {
            300
        } else {
            cfg.cache_ttl_sec.max(1)
        };
        let cache_ttl_ms = (cache_ttl_sec as i64).saturating_mul(1000);
        Self {
            cfg,
            key_windows: Arc::new(Mutex::new(HashMap::new())),
            account_windows: Arc::new(Mutex::new(HashMap::new())),
            key_semaphores: Arc::new(Mutex::new(HashMap::new())),
            account_semaphores: Arc::new(Mutex::new(HashMap::new())),
            cache_ttl_ms,
            last_prune_ms: AtomicI64::new(0),
        }
    }

    pub fn snapshot(&self) -> RateLimitSnapshot {
        RateLimitSnapshot {
            key_rpm: self.cfg.key_rpm,
            key_concurrency: self.cfg.key_concurrency,
            account_rpm: self.cfg.account_rpm,
            account_concurrency: self.cfg.account_concurrency,
            cache_ttl_sec: self.cache_ttl_ms.div_euclid(1000) as u64,
        }
    }

    pub fn check_request(&self, api_key: Option<&str>) -> Result<RequestLimitGuard, LimitError> {
        self.prune_if_due();
        if let Some(api_key) = api_key.filter(|v| !v.trim().is_empty()) {
            if self.cfg.key_rpm > 0 {
                check_fixed_window(
                    &self.key_windows,
                    api_key,
                    self.cfg.key_rpm,
                    "api_key",
                    "API key RPM limit exceeded",
                )?;
            }
        }

        let key_permit = if let Some(api_key) = api_key.filter(|v| !v.trim().is_empty())
            && self.cfg.key_concurrency > 0
        {
            Some(try_acquire_named(
                &self.key_semaphores,
                api_key,
                self.cfg.key_concurrency,
                "api_key",
                "API key concurrency limit exceeded",
            )?)
        } else {
            None
        };

        Ok(RequestLimitGuard {
            _key_permit: key_permit,
        })
    }

    pub fn check_account(&self, account_key: &str) -> Result<AccountLimitGuard, LimitError> {
        self.prune_if_due();
        if self.cfg.account_rpm > 0 {
            check_fixed_window(
                &self.account_windows,
                account_key,
                self.cfg.account_rpm,
                "account",
                "Account RPM limit exceeded",
            )?;
        }

        let account_permit = if self.cfg.account_concurrency > 0 {
            Some(try_acquire_named(
                &self.account_semaphores,
                account_key,
                self.cfg.account_concurrency,
                "account",
                "Account concurrency limit exceeded",
            )?)
        } else {
            None
        };

        Ok(AccountLimitGuard {
            _account_permit: account_permit,
        })
    }

    fn prune_if_due(&self) {
        let now_ms = now_unix_ms();
        let last = self.last_prune_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) < 60_000 {
            return;
        }
        if self
            .last_prune_ms
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        prune_windows(&self.key_windows, now_ms, self.cache_ttl_ms);
        prune_windows(&self.account_windows, now_ms, self.cache_ttl_ms);
        prune_semaphores(&self.key_semaphores, now_ms, self.cache_ttl_ms);
        prune_semaphores(&self.account_semaphores, now_ms, self.cache_ttl_ms);
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(RateLimitConfig::default())
    }
}

fn check_fixed_window(
    windows: &Mutex<HashMap<String, FixedWindow>>,
    key: &str,
    limit: u64,
    scope: &'static str,
    message: &'static str,
) -> Result<(), LimitError> {
    let now_ms = now_unix_ms();
    let window_start_ms = now_ms.div_euclid(60_000) * 60_000;
    let mut windows = windows.lock().expect("rate limit window lock poisoned");
    let entry = windows.entry(key.to_string()).or_default();
    entry.last_seen_ms = now_ms;
    if entry.window_start_ms != window_start_ms {
        entry.window_start_ms = window_start_ms;
        entry.count = 0;
    }
    if entry.count >= limit {
        return Err(LimitError {
            scope,
            message: message.to_string(),
        });
    }
    entry.count += 1;
    Ok(())
}

fn try_acquire_named(
    semaphores: &Mutex<HashMap<String, CachedSemaphore>>,
    key: &str,
    limit: usize,
    scope: &'static str,
    message: &'static str,
) -> Result<OwnedSemaphorePermit, LimitError> {
    let now_ms = now_unix_ms();
    let semaphore = {
        let mut semaphores = semaphores
            .lock()
            .expect("rate limit semaphore lock poisoned");
        semaphores
            .entry(key.to_string())
            .and_modify(|cached| cached.last_seen_ms = now_ms)
            .or_insert_with(|| CachedSemaphore {
                semaphore: Arc::new(Semaphore::new(limit)),
                last_seen_ms: now_ms,
            })
            .semaphore
            .clone()
    };
    semaphore.try_acquire_owned().map_err(|_| LimitError {
        scope,
        message: message.to_string(),
    })
}

fn prune_windows(windows: &Mutex<HashMap<String, FixedWindow>>, now_ms: i64, ttl_ms: i64) {
    let mut windows = windows.lock().expect("rate limit window lock poisoned");
    windows.retain(|_, window| now_ms.saturating_sub(window.last_seen_ms) <= ttl_ms);
}

fn prune_semaphores(
    semaphores: &Mutex<HashMap<String, CachedSemaphore>>,
    now_ms: i64,
    ttl_ms: i64,
) {
    let mut semaphores = semaphores
        .lock()
        .expect("rate limit semaphore lock poisoned");
    semaphores.retain(|_, cached| {
        now_ms.saturating_sub(cached.last_seen_ms) <= ttl_ms
            || Arc::strong_count(&cached.semaphore) > 1
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(cfg: RateLimitConfig) -> RateLimiter {
        RateLimiter::new(cfg)
    }

    #[test]
    fn key_rpm_rejects_after_limit() {
        let limiter = limiter(RateLimitConfig {
            key_rpm: 1,
            ..RateLimitConfig::default()
        });

        assert!(limiter.check_request(Some("k1")).is_ok());
        let err = limiter.check_request(Some("k1")).unwrap_err();
        assert_eq!(err.scope, "api_key");
    }

    #[test]
    fn key_concurrency_is_released_when_guard_drops() {
        let limiter = limiter(RateLimitConfig {
            key_concurrency: 1,
            ..RateLimitConfig::default()
        });

        let guard = limiter.check_request(Some("k1")).unwrap();
        assert!(limiter.check_request(Some("k1")).is_err());
        drop(guard);
        assert!(limiter.check_request(Some("k1")).is_ok());
    }

    #[test]
    fn account_concurrency_is_per_account() {
        let limiter = limiter(RateLimitConfig {
            account_concurrency: 1,
            ..RateLimitConfig::default()
        });

        let guard = limiter.check_account("a.json").unwrap();
        assert!(limiter.check_account("a.json").is_err());
        assert!(limiter.check_account("b.json").is_ok());
        drop(guard);
        assert!(limiter.check_account("a.json").is_ok());
    }

    #[test]
    fn stale_key_limit_cache_entries_are_pruned() {
        let limiter = limiter(RateLimitConfig {
            key_rpm: 10,
            key_concurrency: 1,
            cache_ttl_sec: 1,
            ..RateLimitConfig::default()
        });

        limiter.check_request(Some("old")).unwrap();
        let stale_ms = now_unix_ms().saturating_sub(2_000);
        limiter
            .key_windows
            .lock()
            .unwrap()
            .get_mut("old")
            .unwrap()
            .last_seen_ms = stale_ms;
        limiter
            .key_semaphores
            .lock()
            .unwrap()
            .get_mut("old")
            .unwrap()
            .last_seen_ms = stale_ms;
        limiter
            .last_prune_ms
            .store(now_unix_ms().saturating_sub(61_000), Ordering::Relaxed);

        limiter.check_request(Some("new")).unwrap();

        assert!(!limiter.key_windows.lock().unwrap().contains_key("old"));
        assert!(!limiter.key_semaphores.lock().unwrap().contains_key("old"));
    }
}
