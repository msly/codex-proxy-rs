use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;

use super::account::{Account, TokenData, TokenFile, parse_id_token_claims};
use super::selector::{RoundRobinSelector, Selector};

#[derive(Debug)]
pub struct Manager {
    auth_dir: PathBuf,
    accounts: ArcSwap<Vec<Arc<Account>>>,
    selector: RoundRobinSelector,
}

impl Manager {
    pub fn new(auth_dir: impl Into<PathBuf>) -> Self {
        Self {
            auth_dir: auth_dir.into(),
            accounts: ArcSwap::from_pointee(Vec::new()),
            selector: RoundRobinSelector::new(),
        }
    }

    pub fn load_accounts(&self) -> Result<usize, String> {
        let entries = fs::read_dir(&self.auth_dir)
            .map_err(|e| format!("读取账号目录失败: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取账号目录失败: {e}"))?;

        let mut accounts = Vec::new();
        for entry in entries {
            let path = entry.path();
            if !is_json_file(&path) {
                continue;
            }
            match load_account_from_file(&path) {
                Ok(acc) => accounts.push(acc),
                Err(err) => {
                    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                        tracing::warn!(file = name, "skip invalid auth file: {err}");
                    } else {
                        tracing::warn!("skip invalid auth file: {err}");
                    }
                }
            }
        }

        if accounts.is_empty() {
            return Err(format!(
                "在目录 {} 中未找到有效的账号文件",
                self.auth_dir.display()
            ));
        }

        let count = accounts.len();
        self.accounts.store(Arc::new(accounts));
        Ok(count)
    }

    pub fn accounts_snapshot(&self) -> Arc<Vec<Arc<Account>>> {
        self.accounts.load_full()
    }

    pub fn account_count(&self) -> usize {
        self.accounts.load_full().len()
    }

    pub fn pick(&self, model: &str) -> Result<Arc<Account>, String> {
        let accounts = self.accounts.load_full();
        self.selector.pick(model, &accounts)
    }

    pub fn pick_excluding(
        &self,
        model: &str,
        excluded_file_paths: &HashSet<String>,
    ) -> Result<Arc<Account>, String> {
        let accounts = self.accounts.load_full();
        if excluded_file_paths.is_empty() {
            return self.selector.pick(model, &accounts);
        }

        let filtered: Vec<Arc<Account>> = accounts
            .iter()
            .filter(|acc| !excluded_file_paths.contains(acc.file_path()))
            .cloned()
            .collect();

        if filtered.is_empty() {
            return Err(format!(
                "没有更多可用账号（已排除 {} 个）",
                excluded_file_paths.len()
            ));
        }

        self.selector.pick(model, &filtered)
    }

    pub fn remove_account(&self, file_path: &str, reason: &str) -> bool {
        let snap = self.accounts.load_full();
        if snap.is_empty() {
            return false;
        }

        let mut removed = false;
        let filtered: Vec<Arc<Account>> = snap
            .iter()
            .filter(|acc| {
                let keep = acc.file_path() != file_path;
                if !keep {
                    removed = true;
                }
                keep
            })
            .cloned()
            .collect();

        if !removed {
            return false;
        }

        self.accounts.store(Arc::new(filtered));

        match fs::remove_file(file_path) {
            Ok(()) => tracing::warn!(file_path, reason, "account removed (memory+disk)"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    file_path,
                    reason,
                    "account removed (memory only; disk missing)"
                )
            }
            Err(err) => tracing::warn!(
                file_path,
                reason,
                "account removed (disk delete failed: {err})"
            ),
        }

        true
    }

    /// Scan `auth_dir` and hot-load new `*.json` auth files.
    ///
    /// Unlike `load_accounts`, this will not replace existing in-memory accounts.
    pub fn scan_new_files(&self) -> Result<usize, String> {
        let entries = fs::read_dir(&self.auth_dir)
            .map_err(|e| format!("读取账号目录失败: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取账号目录失败: {e}"))?;

        let snap = self.accounts.load_full();
        let existing: HashSet<String> = snap.iter().map(|a| a.file_path().to_string()).collect();

        let mut added = Vec::new();
        for entry in entries {
            let path = entry.path();
            if !is_json_file(&path) {
                continue;
            }
            let file_path = path.to_string_lossy().to_string();
            if existing.contains(&file_path) {
                continue;
            }

            match load_account_from_file(&path) {
                Ok(acc) => added.push(acc),
                Err(err) => {
                    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                        tracing::warn!(file = name, "skip invalid auth file: {err}");
                    } else {
                        tracing::warn!("skip invalid auth file: {err}");
                    }
                }
            }
        }

        if added.is_empty() {
            return Ok(0);
        }

        let mut merged = Vec::with_capacity(snap.len() + added.len());
        merged.extend(snap.iter().cloned());
        merged.extend(added);

        let count = merged.len() - snap.len();
        self.accounts.store(Arc::new(merged));
        Ok(count)
    }
}

fn is_json_file(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

fn load_account_from_file(path: &Path) -> Result<Arc<Account>, String> {
    let data = fs::read_to_string(path).map_err(|e| format!("读取文件失败: {e}"))?;
    let tf: TokenFile = serde_json::from_str(&data).map_err(|e| format!("解析 JSON 失败: {e}"))?;

    if tf.refresh_token.trim().is_empty() {
        return Err("文件中缺少 refresh_token".to_string());
    }

    let mut account_id = tf.account_id;
    let mut email = tf.email;
    let mut plan_type = String::new();
    if !tf.id_token.is_empty() {
        let (jwt_account_id, jwt_email, jwt_plan_type) = parse_id_token_claims(&tf.id_token);
        if account_id.is_empty() {
            account_id = jwt_account_id;
        }
        if email.is_empty() {
            email = jwt_email;
        }
        plan_type = jwt_plan_type;
    }

    Ok(Arc::new(Account::new(
        path.to_string_lossy().to_string(),
        TokenData {
            id_token: tf.id_token,
            access_token: tf.access_token,
            refresh_token: tf.refresh_token,
            account_id,
            email,
            expired: tf.expired,
            plan_type,
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn core_manager_loads_auth_dir_and_picks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a_path = dir.path().join("a.json");
        let b_path = dir.path().join("b.json");
        let invalid_path = dir.path().join("invalid.json");

        fs::write(
            &a_path,
            r#"{
  "access_token": "at-a",
  "refresh_token": "rt-a",
  "account_id": "acc-a",
  "email": "a@example.com",
  "type": "codex",
  "expired": "2099-01-01T00:00:00Z"
}"#,
        )
        .expect("write a.json");
        fs::write(
            &b_path,
            r#"{
  "access_token": "at-b",
  "refresh_token": "rt-b",
  "account_id": "acc-b",
  "email": "b@example.com",
  "type": "codex",
  "expired": "2099-01-01T00:00:00Z"
}"#,
        )
        .expect("write b.json");
        fs::write(
            &invalid_path,
            r#"{
  "access_token": "at-x",
  "refresh_token": "",
  "account_id": "acc-x",
  "email": "x@example.com",
  "type": "codex",
  "expired": "2099-01-01T00:00:00Z"
}"#,
        )
        .expect("write invalid.json");

        let manager = Manager::new(dir.path());
        let count = manager.load_accounts().expect("load accounts");
        assert_eq!(count, 2);

        let picked = manager.pick("gpt-4.1").expect("pick");
        assert!(
            picked.file_path().ends_with("a.json"),
            "expected deterministic sort by file name, got {}",
            picked.file_path()
        );

        let mut excluded = HashSet::new();
        excluded.insert(picked.file_path().to_string());
        let picked2 = manager
            .pick_excluding("gpt-4.1", &excluded)
            .expect("pick excluding");
        assert!(
            picked2.file_path().ends_with("b.json"),
            "expected the other account, got {}",
            picked2.file_path()
        );
    }

    #[test]
    fn core_manager_errors_when_no_valid_accounts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let invalid_path = dir.path().join("invalid.json");
        fs::write(
            &invalid_path,
            r#"{
  "access_token": "at-x",
  "refresh_token": "",
  "account_id": "acc-x",
  "email": "x@example.com",
  "type": "codex",
  "expired": "2099-01-01T00:00:00Z"
}"#,
        )
        .expect("write invalid.json");

        let manager = Manager::new(dir.path());
        let err = manager.load_accounts().expect_err("should error");
        assert!(err.contains("未找到有效"), "got err: {err}");
    }
}
