use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use super::account::{Account, now_unix_ms};

pub trait Selector: Send + Sync {
    fn pick(&self, model: &str, accounts: &[Arc<Account>]) -> Result<Arc<Account>, String>;
}

#[derive(Debug, Default)]
pub struct RoundRobinSelector {
    cursor: AtomicU64,
}

impl RoundRobinSelector {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Selector for RoundRobinSelector {
    fn pick(&self, _model: &str, accounts: &[Arc<Account>]) -> Result<Arc<Account>, String> {
        let now_ms = now_unix_ms();
        let mut available: Vec<Arc<Account>> = accounts
            .iter()
            .filter(|acc| acc.is_available(now_ms))
            .cloned()
            .collect();

        if available.is_empty() {
            return Err("没有可用的 Codex 账号".to_string());
        }

        if available.len() > 1 {
            available.sort_by(|a, b| compare_used_percent(a, b));
        }

        let idx = self.cursor.fetch_add(1, AtomicOrdering::Relaxed) as usize;
        Ok(available[idx % available.len()].clone())
    }
}

fn compare_used_percent(a: &Account, b: &Account) -> Ordering {
    let pa = a.used_percent_x100();
    let pb = b.used_percent_x100();

    if pa < 0 && pb >= 0 {
        return Ordering::Greater;
    }
    if pa >= 0 && pb < 0 {
        return Ordering::Less;
    }

    if pa != pb {
        return pa.cmp(&pb);
    }
    a.file_path().cmp(b.file_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Account;
    use crate::core::account::{AccountStatus, TokenData, now_unix_ms};

    fn make_account(path: &str) -> Arc<Account> {
        Arc::new(Account::new(
            path.to_string(),
            TokenData {
                id_token: String::new(),
                access_token: String::new(),
                refresh_token: "rt".to_string(),
                account_id: String::new(),
                email: String::new(),
                expired: String::new(),
                plan_type: String::new(),
            },
        ))
    }

    #[test]
    fn core_selector_sorts_used_percent_and_round_robins() {
        let selector = RoundRobinSelector::new();

        let acc_b = make_account("b.json");
        acc_b.set_used_percent_for_test(Some(0.0));

        let acc_a = make_account("a.json");
        acc_a.set_used_percent_for_test(Some(50.0));

        let acc_unknown = make_account("c.json");
        acc_unknown.set_used_percent_for_test(None);

        let accounts = vec![acc_unknown.clone(), acc_a.clone(), acc_b.clone()];

        let p0 = selector.pick("gpt-4.1", &accounts).expect("pick 0");
        let p1 = selector.pick("gpt-4.1", &accounts).expect("pick 1");
        let p2 = selector.pick("gpt-4.1", &accounts).expect("pick 2");

        assert_eq!(p0.file_path(), "b.json");
        assert_eq!(p1.file_path(), "a.json");
        assert_eq!(p2.file_path(), "c.json");
    }

    #[test]
    fn core_selector_filters_cooldown_accounts() {
        let selector = RoundRobinSelector::new();

        let acc_ok = make_account("ok.json");
        acc_ok.set_used_percent_for_test(Some(0.0));

        let acc_cd = make_account("cooldown.json");
        let now = now_unix_ms();
        acc_cd.set_status_for_test(AccountStatus::Cooldown);
        acc_cd.set_cooldown_until_ms_for_test(now + 60_000);

        let accounts = vec![acc_ok.clone(), acc_cd.clone()];

        let picked = selector.pick("gpt-4.1", &accounts).expect("pick");
        assert_eq!(picked.file_path(), "ok.json");
    }
}
