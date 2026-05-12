use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::RateLimitConfig;
use crate::core::now_unix_ms;

#[derive(Debug, Clone)]
pub struct RateLimiter {
    cfg: RateLimitConfig,
    key_windows: Arc<Mutex<HashMap<String, FixedWindow>>>,
    account_windows: Arc<Mutex<HashMap<String, FixedWindow>>>,
    key_semaphores: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    account_semaphores: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    image_semaphore: Option<Arc<Semaphore>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct FixedWindow {
    window_start_ms: i64,
    count: u64,
}

#[derive(Debug, Serialize)]
pub struct RateLimitSnapshot {
    pub key_rpm: u64,
    pub key_concurrency: usize,
    pub account_rpm: u64,
    pub account_concurrency: usize,
    pub image_concurrency: usize,
}

#[derive(Debug)]
pub struct RequestLimitGuard {
    _key_permit: Option<OwnedSemaphorePermit>,
    _image_permit: Option<OwnedSemaphorePermit>,
}

impl RequestLimitGuard {
    pub fn disabled() -> Self {
        Self {
            _key_permit: None,
            _image_permit: None,
        }
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
        Self {
            image_semaphore: if cfg.image_concurrency > 0 {
                Some(Arc::new(Semaphore::new(cfg.image_concurrency)))
            } else {
                None
            },
            cfg,
            key_windows: Arc::new(Mutex::new(HashMap::new())),
            account_windows: Arc::new(Mutex::new(HashMap::new())),
            key_semaphores: Arc::new(Mutex::new(HashMap::new())),
            account_semaphores: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn snapshot(&self) -> RateLimitSnapshot {
        RateLimitSnapshot {
            key_rpm: self.cfg.key_rpm,
            key_concurrency: self.cfg.key_concurrency,
            account_rpm: self.cfg.account_rpm,
            account_concurrency: self.cfg.account_concurrency,
            image_concurrency: self.cfg.image_concurrency,
        }
    }

    pub fn check_request(
        &self,
        api_key: Option<&str>,
        is_image: bool,
    ) -> Result<RequestLimitGuard, LimitError> {
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

        let image_permit =
            if is_image {
                match &self.image_semaphore {
                    Some(semaphore) => Some(semaphore.clone().try_acquire_owned().map_err(
                        |_| LimitError {
                            scope: "image",
                            message: "Image concurrency limit exceeded".to_string(),
                        },
                    )?),
                    None => None,
                }
            } else {
                None
            };

        Ok(RequestLimitGuard {
            _key_permit: key_permit,
            _image_permit: image_permit,
        })
    }

    pub fn check_account(&self, account_key: &str) -> Result<AccountLimitGuard, LimitError> {
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
    semaphores: &Mutex<HashMap<String, Arc<Semaphore>>>,
    key: &str,
    limit: usize,
    scope: &'static str,
    message: &'static str,
) -> Result<OwnedSemaphorePermit, LimitError> {
    let semaphore = {
        let mut semaphores = semaphores
            .lock()
            .expect("rate limit semaphore lock poisoned");
        semaphores
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(limit)))
            .clone()
    };
    semaphore.try_acquire_owned().map_err(|_| LimitError {
        scope,
        message: message.to_string(),
    })
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

        assert!(limiter.check_request(Some("k1"), false).is_ok());
        let err = limiter.check_request(Some("k1"), false).unwrap_err();
        assert_eq!(err.scope, "api_key");
    }

    #[test]
    fn key_concurrency_is_released_when_guard_drops() {
        let limiter = limiter(RateLimitConfig {
            key_concurrency: 1,
            ..RateLimitConfig::default()
        });

        let guard = limiter.check_request(Some("k1"), false).unwrap();
        assert!(limiter.check_request(Some("k1"), false).is_err());
        drop(guard);
        assert!(limiter.check_request(Some("k1"), false).is_ok());
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
    fn image_concurrency_is_global_for_image_requests() {
        let limiter = limiter(RateLimitConfig {
            image_concurrency: 1,
            ..RateLimitConfig::default()
        });

        let guard = limiter.check_request(Some("k1"), true).unwrap();
        assert!(limiter.check_request(Some("k2"), true).is_err());
        assert!(limiter.check_request(Some("k2"), false).is_ok());
        drop(guard);
        assert!(limiter.check_request(Some("k2"), true).is_ok());
    }
}
