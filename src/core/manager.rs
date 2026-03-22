use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;

use super::account::{Account, TokenData, TokenFile, parse_id_token_claims};
use super::selector::{RoundRobinSelector, Selector};

const RUNTIME_STATE_FILE_NAME: &str = ".codex-proxy-state.json";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    pub added: usize,
    pub removed: usize,
}

pub struct Manager {
    auth_dir: PathBuf,
    accounts: ArcSwap<Vec<Arc<Account>>>,
    selector: Arc<dyn Selector>,
}

impl Manager {
    pub fn new(auth_dir: impl Into<PathBuf>) -> Self {
        Self::new_with_selector(auth_dir, Arc::new(RoundRobinSelector::new()))
    }

    pub fn new_with_selector(auth_dir: impl Into<PathBuf>, selector: Arc<dyn Selector>) -> Self {
        Self {
            auth_dir: auth_dir.into(),
            accounts: ArcSwap::from_pointee(Vec::new()),
            selector,
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
                    warn_invalid_auth_file(&path, &err);
                    if err.should_delete_file() {
                        delete_invalid_auth_file(&path, "missing_access_token");
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
    pub fn scan_new_files(&self) -> Result<ScanOutcome, String> {
        let entries = fs::read_dir(&self.auth_dir)
            .map_err(|e| format!("读取账号目录失败: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取账号目录失败: {e}"))?;

        let snap = self.accounts.load_full();
        let existing: HashSet<String> = snap.iter().map(|a| a.file_path().to_string()).collect();
        let mut disk_files = HashSet::new();

        let mut added = Vec::new();
        for entry in entries {
            let path = entry.path();
            if !is_json_file(&path) {
                continue;
            }
            let file_path = path.to_string_lossy().to_string();
            disk_files.insert(file_path.clone());
            if existing.contains(&file_path) {
                continue;
            }

            match load_account_from_file(&path) {
                Ok(acc) => added.push(acc),
                Err(err) => {
                    warn_invalid_auth_file(&path, &err);
                    if err.should_delete_file() {
                        delete_invalid_auth_file(&path, "missing_access_token");
                    }
                }
            }
        }

        let retained: Vec<Arc<Account>> = snap
            .iter()
            .filter(|acc| disk_files.contains(acc.file_path()))
            .cloned()
            .collect();
        let removed = snap.len().saturating_sub(retained.len());
        let added_count = added.len();

        if added.is_empty() && removed == 0 {
            return Ok(ScanOutcome::default());
        }

        let mut merged = Vec::with_capacity(retained.len() + added.len());
        merged.extend(retained);
        merged.extend(added);

        self.accounts.store(Arc::new(merged));
        Ok(ScanOutcome {
            added: added_count,
            removed,
        })
    }
}

fn is_json_file(path: &Path) -> bool {
    if path
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| name == RUNTIME_STATE_FILE_NAME)
    {
        return false;
    }
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

#[derive(Debug)]
enum LoadAccountError {
    Read(String),
    Parse(String),
    MissingAccessToken,
}

impl LoadAccountError {
    fn message(&self) -> String {
        match self {
            Self::Read(err) => format!("读取文件失败: {err}"),
            Self::Parse(err) => format!("解析 JSON 失败: {err}"),
            Self::MissingAccessToken => "文件中缺少 access_token".to_string(),
        }
    }

    fn should_delete_file(&self) -> bool {
        matches!(self, Self::MissingAccessToken)
    }
}

fn warn_invalid_auth_file(path: &Path, err: &LoadAccountError) {
    let err = err.message();
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        tracing::warn!(file = name, "skip invalid auth file: {err}");
    } else {
        tracing::warn!("skip invalid auth file: {err}");
    }
}

fn delete_invalid_auth_file(path: &Path, reason: &str) {
    match fs::remove_file(path) {
        Ok(()) => tracing::warn!(file_path = %path.display(), reason, "invalid auth file deleted"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                file_path = %path.display(),
                reason,
                "invalid auth file already missing"
            )
        }
        Err(err) => tracing::warn!(
            file_path = %path.display(),
            reason,
            "failed to delete invalid auth file: {err}"
        ),
    }
}

fn load_account_from_file(path: &Path) -> Result<Arc<Account>, LoadAccountError> {
    let data = fs::read_to_string(path).map_err(|e| LoadAccountError::Read(e.to_string()))?;
    let tf: TokenFile =
        serde_json::from_str(&data).map_err(|e| LoadAccountError::Parse(e.to_string()))?;

    if tf.access_token.trim().is_empty() {
        return Err(LoadAccountError::MissingAccessToken);
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
  "access_token": "",
  "refresh_token": "rt-x",
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
    fn core_manager_loads_access_token_only_auth_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("a.json");
        fs::write(
            &auth_path,
            r#"{
  "access_token": "at-a",
  "refresh_token": "",
  "account_id": "acc-a",
  "email": "a@example.com",
  "type": "codex",
  "expired": "2099-01-01T00:00:00Z"
}"#,
        )
        .expect("write a.json");

        let manager = Manager::new(dir.path());
        let count = manager.load_accounts().expect("load accounts");
        assert_eq!(count, 1);
        assert_eq!(manager.account_count(), 1);
        assert_eq!(manager.accounts_snapshot()[0].token().access_token, "at-a");
        assert_eq!(manager.accounts_snapshot()[0].token().refresh_token, "");
    }

    #[test]
    fn core_manager_scan_new_files_loads_access_token_only_auth_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = Manager::new(dir.path());

        assert_eq!(manager.scan_new_files().expect("initial scan"), ScanOutcome::default());

        fs::write(
            dir.path().join("a.json"),
            r#"{
  "access_token": "at-a",
  "refresh_token": "",
  "account_id": "acc-a",
  "email": "a@example.com",
  "type": "codex",
  "expired": "2099-01-01T00:00:00Z"
}"#,
        )
        .expect("write a.json");

        assert_eq!(
            manager.scan_new_files().expect("hot load"),
            ScanOutcome {
                added: 1,
                removed: 0
            }
        );
        assert_eq!(manager.account_count(), 1);
        assert_eq!(manager.accounts_snapshot()[0].token().access_token, "at-a");
        assert_eq!(manager.accounts_snapshot()[0].token().refresh_token, "");
    }

    #[test]
    fn core_manager_scan_new_files_prunes_deleted_auth_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a_path = dir.path().join("a.json");
        let b_path = dir.path().join("b.json");
        fs::write(
            &a_path,
            r#"{"access_token":"at-a","refresh_token":"rt-a","account_id":"","email":"a@example.com","type":"codex","expired":"2099-01-01T00:00:00Z"}"#,
        )
        .expect("write a.json");
        fs::write(
            &b_path,
            r#"{"access_token":"at-b","refresh_token":"rt-b","account_id":"","email":"b@example.com","type":"codex","expired":"2099-01-01T00:00:00Z"}"#,
        )
        .expect("write b.json");

        let manager = Manager::new(dir.path());
        manager.load_accounts().expect("load accounts");
        fs::remove_file(&b_path).expect("remove b.json");

        assert_eq!(
            manager.scan_new_files().expect("scan after delete"),
            ScanOutcome {
                added: 0,
                removed: 1
            }
        );
        assert_eq!(manager.account_count(), 1);
        assert_eq!(manager.accounts_snapshot()[0].file_path(), a_path.to_string_lossy());
    }

    #[test]
    fn core_manager_deletes_missing_access_token_file_on_startup_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let invalid_path = dir.path().join("invalid.json");
        fs::write(
            &invalid_path,
            r#"{
  "access_token": "",
  "refresh_token": "rt-x",
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
        assert!(
            !invalid_path.exists(),
            "missing-access-token auth file should be deleted"
        );
    }

    #[test]
    fn core_manager_deletes_missing_access_token_file_on_hot_load_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = Manager::new(dir.path());
        let invalid_path = dir.path().join("invalid.json");
        fs::write(
            &invalid_path,
            r#"{
  "access_token": "",
  "refresh_token": "rt-x",
  "account_id": "acc-x",
  "email": "x@example.com",
  "type": "codex",
  "expired": "2099-01-01T00:00:00Z"
}"#,
        )
        .expect("write invalid.json");

        assert_eq!(manager.scan_new_files().expect("scan"), ScanOutcome::default());
        assert!(
            !invalid_path.exists(),
            "missing-access-token auth file should be deleted"
        );
    }

    #[test]
    fn core_manager_keeps_malformed_json_file_on_startup_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let invalid_path = dir.path().join("invalid.json");
        fs::write(&invalid_path, r#"{"access_token":"at""#).expect("write invalid json");

        let manager = Manager::new(dir.path());
        let err = manager.load_accounts().expect_err("should error");
        assert!(err.contains("未找到有效"), "got err: {err}");
        assert!(
            invalid_path.exists(),
            "malformed json should be skipped but not deleted"
        );
    }

    #[test]
    fn core_manager_ignores_runtime_state_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("a.json");
        let state_path = dir.path().join(".codex-proxy-state.json");

        fs::write(
            &auth_path,
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
            &state_path,
            r#"{
  "saved_at_ms": 123,
  "accounts": {},
  "hourly": []
}"#,
        )
        .expect("write state file");

        let manager = Manager::new(dir.path());
        let count = manager.load_accounts().expect("load accounts");
        assert_eq!(count, 1);
        assert!(
            state_path.exists(),
            "runtime state file should be ignored instead of deleted"
        );
    }

    #[test]
    fn core_manager_errors_when_no_valid_accounts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let invalid_path = dir.path().join("invalid.json");
        fs::write(
            &invalid_path,
            r#"{
  "access_token": "",
  "refresh_token": "rt-x",
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
