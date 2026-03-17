use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::{RwLock, RwLockReadGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenFile {
    #[serde(default, rename = "id_token")]
    pub id_token: String,
    #[serde(default, rename = "access_token")]
    pub access_token: String,
    #[serde(default, rename = "refresh_token")]
    pub refresh_token: String,
    #[serde(default, rename = "account_id")]
    pub account_id: String,
    #[serde(default, rename = "last_refresh")]
    pub last_refresh: String,
    #[serde(default)]
    pub email: String,
    #[serde(default, rename = "type")]
    pub token_type: String,
    #[serde(default, rename = "expired")]
    pub expired: String,
}

#[derive(Debug, Clone)]
pub struct TokenData {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
    pub email: String,
    pub expired: String,
    pub plan_type: String,
}

#[derive(Debug, Clone)]
pub struct QuotaInfo {
    pub valid: bool,
    pub status_code: u16,
    pub raw_data: Vec<u8>,
    pub checked_at_ms: i64,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum AccountStatus {
    Active = 0,
    Cooldown = 1,
    Disabled = 2,
}

impl AccountStatus {
    fn from_i32(v: i32) -> Self {
        match v {
            1 => Self::Cooldown,
            2 => Self::Disabled,
            _ => Self::Active,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Cooldown => "cooldown",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug)]
pub struct Account {
    file_path: String,
    token: RwLock<TokenData>,
    quota: RwLock<Option<QuotaInfo>>,

    status: AtomicI32,
    cooldown_until_ms: AtomicI64,
    used_percent_x100: AtomicI64, // -100 means unknown (-1.00%)

    last_used_ms: AtomicI64,

    quota_exhausted: AtomicI32,
    quota_resets_at_ms: AtomicI64,

    consecutive_failures: AtomicI64,
    total_requests: AtomicI64,
    total_errors: AtomicI64,

    total_completions: AtomicI64,
    input_tokens: AtomicI64,
    output_tokens: AtomicI64,
    total_tokens: AtomicI64,

    refreshing: AtomicI32,
    last_refresh_ms: AtomicI64,
}

impl Account {
    pub fn new(file_path: String, token: TokenData) -> Self {
        Self {
            file_path,
            token: RwLock::new(token),
            quota: RwLock::new(None),
            status: AtomicI32::new(AccountStatus::Active as i32),
            cooldown_until_ms: AtomicI64::new(0),
            used_percent_x100: AtomicI64::new(-100),
            last_used_ms: AtomicI64::new(0),
            quota_exhausted: AtomicI32::new(0),
            quota_resets_at_ms: AtomicI64::new(0),
            consecutive_failures: AtomicI64::new(0),
            total_requests: AtomicI64::new(0),
            total_errors: AtomicI64::new(0),
            total_completions: AtomicI64::new(0),
            input_tokens: AtomicI64::new(0),
            output_tokens: AtomicI64::new(0),
            total_tokens: AtomicI64::new(0),
            refreshing: AtomicI32::new(0),
            last_refresh_ms: AtomicI64::new(0),
        }
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    pub fn token(&self) -> RwLockReadGuard<'_, TokenData> {
        self.token.read().expect("token lock poisoned")
    }

    pub fn token_clone(&self) -> TokenData {
        self.token().clone()
    }

    pub fn status(&self) -> AccountStatus {
        AccountStatus::from_i32(self.status.load(Ordering::Relaxed))
    }

    pub fn is_available(&self, now_ms: i64) -> bool {
        match self.status() {
            AccountStatus::Active => true,
            AccountStatus::Disabled => false,
            AccountStatus::Cooldown => now_ms >= self.cooldown_until_ms.load(Ordering::Relaxed),
        }
    }

    pub fn used_percent_x100(&self) -> i64 {
        self.used_percent_x100.load(Ordering::Relaxed)
    }

    pub fn used_percent(&self) -> f64 {
        self.used_percent_x100() as f64 / 100.0
    }

    pub fn cooldown_until_ms(&self) -> i64 {
        self.cooldown_until_ms.load(Ordering::Relaxed)
    }

    pub fn last_used_ms(&self) -> i64 {
        self.last_used_ms.load(Ordering::Relaxed)
    }

    pub fn quota_exhausted(&self) -> bool {
        self.quota_exhausted.load(Ordering::Relaxed) != 0
    }

    pub fn quota_resets_at_ms(&self) -> i64 {
        self.quota_resets_at_ms.load(Ordering::Relaxed)
    }

    pub fn quota_exhausted_effective(&self, now_ms: i64) -> bool {
        if !self.quota_exhausted() {
            return false;
        }
        let resets_at = self.quota_resets_at_ms();
        if resets_at > 0 && now_ms >= resets_at {
            return false;
        }
        true
    }

    pub fn record_usage(&self, input_tokens: i64, output_tokens: i64, total_tokens: i64) {
        self.total_completions.fetch_add(1, Ordering::Relaxed);
        if input_tokens > 0 {
            self.input_tokens.fetch_add(input_tokens, Ordering::Relaxed);
        }
        if output_tokens > 0 {
            self.output_tokens
                .fetch_add(output_tokens, Ordering::Relaxed);
        }
        if total_tokens > 0 {
            self.total_tokens.fetch_add(total_tokens, Ordering::Relaxed);
        } else if input_tokens > 0 || output_tokens > 0 {
            self.total_tokens.fetch_add(
                input_tokens.saturating_add(output_tokens),
                Ordering::Relaxed,
            );
        }
    }

    pub fn set_used_percent_for_test(&self, percent: Option<f64>) {
        let v = match percent {
            None => -100,
            Some(p) => (p.clamp(0.0, 100.0) * 100.0).round() as i64,
        };
        self.used_percent_x100.store(v, Ordering::Relaxed);
    }

    pub fn set_status_for_test(&self, status: AccountStatus) {
        self.status.store(status as i32, Ordering::Relaxed);
    }

    pub fn set_cooldown_until_ms_for_test(&self, until_ms: i64) {
        self.cooldown_until_ms.store(until_ms, Ordering::Relaxed);
    }

    pub fn set_cooldown(&self, duration_ms: i64, now_ms: i64) {
        let until_ms = now_ms.saturating_add(duration_ms);
        self.status
            .store(AccountStatus::Cooldown as i32, Ordering::Relaxed);
        self.cooldown_until_ms.store(until_ms, Ordering::Relaxed);
    }

    pub fn set_quota_cooldown(&self, duration_ms: i64, now_ms: i64) {
        let until_ms = now_ms.saturating_add(duration_ms);
        self.set_cooldown(duration_ms, now_ms);
        self.quota_exhausted.store(1, Ordering::Relaxed);
        self.quota_resets_at_ms.store(until_ms, Ordering::Relaxed);
        self.used_percent_x100.store(10000, Ordering::Relaxed); // 100.00%
    }

    fn set_status_active(&self) {
        self.status
            .store(AccountStatus::Active as i32, Ordering::Relaxed);
        self.cooldown_until_ms.store(0, Ordering::Relaxed);
    }

    pub fn set_active(&self) {
        self.set_status_active();
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.quota_exhausted.store(0, Ordering::Relaxed);
        self.quota_resets_at_ms.store(0, Ordering::Relaxed);
    }

    pub fn last_refresh_ms(&self) -> i64 {
        self.last_refresh_ms.load(Ordering::Relaxed)
    }

    pub fn set_last_refresh_ms_for_test(&self, v: i64) {
        self.last_refresh_ms.store(v, Ordering::Relaxed);
    }

    pub fn is_refreshing(&self) -> bool {
        self.refreshing.load(Ordering::Relaxed) != 0
    }

    pub fn try_begin_refresh(&self) -> bool {
        self.refreshing
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    pub fn end_refresh(&self) {
        self.refreshing.store(0, Ordering::Release);
    }

    pub fn update_token(&self, token: TokenData, now_ms: i64) {
        let mut guard = self.token.write().expect("token lock poisoned");
        *guard = token;
        drop(guard);

        self.set_status_active();
        self.last_refresh_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn set_quota_info(&self, info: QuotaInfo) {
        {
            let mut guard = self.quota.write().expect("quota lock poisoned");
            *guard = Some(info.clone());
        }
        self.refresh_used_percent_from_quota(&info);
    }

    pub fn quota_info(&self) -> Option<QuotaInfo> {
        self.quota
            .read()
            .expect("quota lock poisoned")
            .as_ref()
            .cloned()
    }

    fn refresh_used_percent_from_quota(&self, info: &QuotaInfo) {
        if !info.valid || info.raw_data.is_empty() {
            self.used_percent_x100.store(-100, Ordering::Relaxed);
            return;
        }

        let used = extract_used_percent_x100(&info.raw_data);
        self.used_percent_x100
            .store(used.unwrap_or(-100), Ordering::Relaxed);
    }

    pub fn record_success(&self, now_ms: i64) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.last_used_ms.store(now_ms, Ordering::Relaxed);
    }

    pub fn record_failure(&self, now_ms: i64) -> i64 {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.total_errors.fetch_add(1, Ordering::Relaxed);
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        self.last_used_ms.store(now_ms, Ordering::Relaxed);
        failures
    }

    pub fn total_requests(&self) -> i64 {
        self.total_requests.load(Ordering::Relaxed)
    }

    pub fn total_errors(&self) -> i64 {
        self.total_errors.load(Ordering::Relaxed)
    }

    pub fn consecutive_failures(&self) -> i64 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    pub fn stats_snapshot(&self) -> AccountStatsSnapshot {
        let token = self.token();
        AccountStatsSnapshot {
            file_path: self.file_path.clone(),
            email: token.email.clone(),
            status: self.status(),
            plan_type: token.plan_type.clone(),
            used_percent: self.used_percent(),
            total_requests: self.total_requests(),
            total_errors: self.total_errors(),
            consecutive_failures: self.consecutive_failures(),
            last_used_ms: self.last_used_ms(),
            last_refreshed_ms: self.last_refresh_ms(),
            cooldown_until_ms: self.cooldown_until_ms(),
            quota_exhausted: self.quota_exhausted(),
            quota_resets_at_ms: self.quota_resets_at_ms(),
            usage_total_completions: self.total_completions.load(Ordering::Relaxed),
            usage_input_tokens: self.input_tokens.load(Ordering::Relaxed),
            usage_output_tokens: self.output_tokens.load(Ordering::Relaxed),
            usage_total_tokens: self.total_tokens.load(Ordering::Relaxed),
            token_expire: token.expired.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AccountStatsSnapshot {
    pub file_path: String,
    pub email: String,
    pub status: AccountStatus,
    pub plan_type: String,
    pub used_percent: f64,
    pub total_requests: i64,
    pub total_errors: i64,
    pub consecutive_failures: i64,
    pub last_used_ms: i64,
    pub last_refreshed_ms: i64,
    pub cooldown_until_ms: i64,
    pub quota_exhausted: bool,
    pub quota_resets_at_ms: i64,
    pub usage_total_completions: i64,
    pub usage_input_tokens: i64,
    pub usage_output_tokens: i64,
    pub usage_total_tokens: i64,
    pub token_expire: String,
}

pub fn now_unix_ms() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_millis() as i64
}

fn extract_used_percent_x100(raw_json: &[u8]) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_slice(raw_json).ok()?;
    let used = v
        .get("rate_limit")?
        .get("primary_window")?
        .get("used_percent")?
        .as_f64()?;
    Some((used * 100.0) as i64)
}

pub fn parse_id_token_claims(id_token: &str) -> (String, String, String) {
    if id_token.is_empty() {
        return (String::new(), String::new(), String::new());
    }
    let mut parts = id_token.split('.');
    let _header = parts.next();
    let payload = match parts.next() {
        Some(p) if !p.is_empty() => p,
        _ => return (String::new(), String::new(), String::new()),
    };

    let decoded = match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload) {
        Ok(v) => v,
        Err(_) => return (String::new(), String::new(), String::new()),
    };

    #[derive(Debug, Default, Deserialize)]
    struct Claims {
        #[serde(default)]
        email: String,
        #[serde(default, rename = "https://api.openai.com/auth")]
        auth: AuthClaims,
    }

    #[derive(Debug, Default, Deserialize)]
    struct AuthClaims {
        #[serde(default)]
        chatgpt_account_id: String,
        #[serde(default)]
        chatgpt_plan_type: String,
        #[serde(default)]
        organization_id: String,
        #[serde(default)]
        organizations: Vec<Org>,
    }

    #[derive(Debug, Default, Deserialize)]
    struct Org {
        #[serde(default)]
        id: String,
    }

    let claims: Claims = serde_json::from_slice(&decoded).unwrap_or_default();

    let mut account_id = claims.auth.chatgpt_account_id;
    if account_id.is_empty() && !claims.auth.organization_id.is_empty() {
        account_id = claims.auth.organization_id;
    }
    if account_id.is_empty() {
        if let Some(org) = claims.auth.organizations.first() {
            account_id = org.id.clone();
        }
    }

    (account_id, claims.email, claims.auth.chatgpt_plan_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_account_stats_snapshot_reflects_counters() {
        let acc = Account::new(
            "a.json".to_string(),
            TokenData {
                id_token: String::new(),
                access_token: String::new(),
                refresh_token: "rt".to_string(),
                account_id: String::new(),
                email: "a@example.com".to_string(),
                expired: "2099-01-01T00:00:00Z".to_string(),
                plan_type: String::new(),
            },
        );

        let now = now_unix_ms();
        assert_eq!(acc.record_failure(now), 1);
        assert_eq!(acc.record_failure(now), 2);
        acc.record_success(now);

        acc.set_used_percent_for_test(Some(12.34));
        acc.set_status_for_test(AccountStatus::Cooldown);

        let snap = acc.stats_snapshot();
        assert_eq!(snap.file_path, "a.json");
        assert_eq!(snap.email, "a@example.com");
        assert_eq!(snap.status, AccountStatus::Cooldown);
        assert_eq!(snap.total_requests, 3);
        assert_eq!(snap.total_errors, 2);
        assert_eq!(snap.consecutive_failures, 0);
        assert!(
            (snap.used_percent - 12.34).abs() < 1e-6,
            "used_percent={}",
            snap.used_percent
        );
        assert_eq!(snap.token_expire, "2099-01-01T00:00:00Z");
    }

    #[test]
    fn core_account_set_quota_info_updates_used_percent() {
        let acc = Account::new(
            "a.json".to_string(),
            TokenData {
                id_token: String::new(),
                access_token: String::new(),
                refresh_token: "rt".to_string(),
                account_id: String::new(),
                email: "a@example.com".to_string(),
                expired: "2099-01-01T00:00:00Z".to_string(),
                plan_type: String::new(),
            },
        );

        acc.set_quota_info(QuotaInfo {
            valid: true,
            status_code: 200,
            raw_data: serde_json::json!({
              "rate_limit": {
                "primary_window": {
                  "used_percent": 12.34
                }
              }
            })
            .to_string()
            .into_bytes(),
            checked_at_ms: now_unix_ms(),
        });

        assert!(
            (acc.used_percent() - 12.34).abs() < 0.02,
            "used={}",
            acc.used_percent()
        );
    }
}
