use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::account::{Account, TokenData, TokenFile, parse_id_token_claims};
use super::selector::{RoundRobinSelector, Selector};

const RUNTIME_STATE_FILE_NAME: &str = ".codex-proxy-state.json";
const SUB2API_SPLIT_NAME_MAX_LEN: usize = 80;

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
            match load_accounts_from_auth_file(&path) {
                Ok(loaded) => accounts.extend(loaded),
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

            match load_accounts_from_auth_file(&path) {
                Ok(accounts) => {
                    for acc in accounts {
                        let file_path = acc.file_path().to_string();
                        disk_files.insert(file_path.clone());
                        if existing.contains(&file_path) {
                            continue;
                        }
                        added.push(acc);
                    }
                }
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

fn load_accounts_from_auth_file(path: &Path) -> Result<Vec<Arc<Account>>, LoadAccountError> {
    let paths = match migrate_sub2api_multi_account_export(path)? {
        Some(paths) => paths,
        None => vec![path.to_path_buf()],
    };

    let mut accounts = Vec::with_capacity(paths.len());
    for path in paths {
        accounts.push(load_account_from_file(&path)?);
    }
    Ok(accounts)
}

fn load_account_from_file(path: &Path) -> Result<Arc<Account>, LoadAccountError> {
    let data = fs::read_to_string(path).map_err(|e| LoadAccountError::Read(e.to_string()))?;
    let tf = parse_token_file(&data)?;

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

fn parse_token_file(data: &str) -> Result<TokenFile, LoadAccountError> {
    let tf: TokenFile =
        serde_json::from_str(data).map_err(|e| LoadAccountError::Parse(e.to_string()))?;
    if !tf.access_token.trim().is_empty() {
        return Ok(tf);
    }

    match parse_sub2api_token_file(data)? {
        Some(sub2api_tf) => Ok(sub2api_tf),
        None => Ok(tf),
    }
}

fn migrate_sub2api_multi_account_export(
    path: &Path,
) -> Result<Option<Vec<PathBuf>>, LoadAccountError> {
    let data = fs::read_to_string(path).map_err(|e| LoadAccountError::Read(e.to_string()))?;
    let value: serde_json::Value =
        serde_json::from_str(&data).map_err(|e| LoadAccountError::Parse(e.to_string()))?;
    let account_count = value
        .get("accounts")
        .and_then(|accounts| accounts.as_array())
        .map(|accounts| accounts.len())
        .unwrap_or(0);
    if account_count <= 1 {
        return Ok(None);
    }
    let export: Sub2ApiExport =
        serde_json::from_value(value).map_err(|e| LoadAccountError::Parse(e.to_string()))?;

    let parent = path
        .parent()
        .ok_or_else(|| LoadAccountError::Read(format!("无效路径: {}", path.display())))?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("sub2api");

    let mut split_payloads = Vec::with_capacity(export.accounts.len());
    for (idx, account) in export.accounts.into_iter().enumerate() {
        let tf = sub2api_account_to_token_file(account).ok_or_else(|| {
            LoadAccountError::Parse(format!(
                "sub2api 多账号导出第 {} 个账号缺少 access_token，跳过迁移以保留原文件",
                idx + 1
            ))
        })?;
        let target = next_available_split_path(parent, stem, idx, &tf);
        let data = serde_json::to_string_pretty(&tf)
            .map_err(|e| LoadAccountError::Parse(format!("序列化拆分账号失败: {e}")))?;
        split_payloads.push((target, data));
    }

    let mut split_files = Vec::with_capacity(split_payloads.len());
    for (target, data) in split_payloads {
        if let Err(err) = fs::write(&target, data) {
            cleanup_split_files(&split_files);
            return Err(LoadAccountError::Read(err.to_string()));
        }
        split_files.push(target);
    }

    if let Err(err) = fs::remove_file(path) {
        cleanup_split_files(&split_files);
        return Err(LoadAccountError::Read(format!(
            "删除 sub2api 原始导出文件失败: {err}"
        )));
    }

    tracing::info!(
        source = %path.display(),
        split_count = split_files.len(),
        "sub2api multi-account export migrated"
    );
    Ok(Some(split_files))
}

fn cleanup_split_files(split_files: &[PathBuf]) {
    for split_file in split_files {
        let _ = fs::remove_file(split_file);
    }
}

#[derive(Debug, Deserialize)]
struct Sub2ApiExport {
    #[serde(default)]
    accounts: Vec<Sub2ApiAccount>,
}

#[derive(Debug, Deserialize)]
struct Sub2ApiAccount {
    #[serde(default)]
    name: String,
    #[serde(default)]
    credentials: Sub2ApiCredentials,
    #[serde(default)]
    extra: Sub2ApiExtra,
}

#[derive(Debug, Default, Deserialize)]
struct Sub2ApiCredentials {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    chatgpt_account_id: String,
    #[serde(default)]
    organization_id: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    expires_at: serde_json::Value,
}

#[derive(Debug, Default, Deserialize)]
struct Sub2ApiExtra {
    #[serde(default)]
    email: String,
    #[serde(default)]
    last_refresh: String,
}

fn sub2api_account_to_token_file(account: Sub2ApiAccount) -> Option<TokenFile> {
    let credentials = account.credentials;
    if credentials.access_token.trim().is_empty() {
        return None;
    }

    let account_id = if credentials.chatgpt_account_id.trim().is_empty() {
        credentials.organization_id
    } else {
        credentials.chatgpt_account_id
    };
    let email = if !credentials.email.trim().is_empty() {
        credentials.email
    } else if !account.extra.email.trim().is_empty() {
        account.extra.email
    } else {
        account.name
    };

    Some(TokenFile {
        id_token: credentials.id_token,
        access_token: credentials.access_token,
        refresh_token: credentials.refresh_token,
        account_id,
        last_refresh: account.extra.last_refresh,
        email,
        token_type: "codex".to_string(),
        expired: format_sub2api_expires_at(&credentials.expires_at),
    })
}

fn format_sub2api_expires_at(expires_at: &serde_json::Value) -> String {
    if let Some(v) = expires_at.as_i64() {
        return format_expires_at(v);
    }
    if let Some(v) = expires_at.as_u64().and_then(|v| i64::try_from(v).ok()) {
        return format_expires_at(v);
    }
    let Some(raw) = expires_at.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
        return String::new();
    };
    if let Ok(v) = raw.parse::<i64>() {
        return format_expires_at(v);
    }
    OffsetDateTime::parse(raw, &Rfc3339)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_else(|| raw.to_string())
}

fn next_available_split_path(parent: &Path, stem: &str, idx: usize, tf: &TokenFile) -> PathBuf {
    let label = if !tf.email.trim().is_empty() {
        tf.email.as_str()
    } else if !tf.account_id.trim().is_empty() {
        tf.account_id.as_str()
    } else {
        "account"
    };
    let label = sanitize_split_file_component(label);
    let base_name = format!("{stem}-{:04}-{label}", idx + 1);
    let mut path = parent.join(format!("{base_name}.json"));
    let mut suffix = 2;
    while path.exists() {
        path = parent.join(format!("{base_name}-{suffix}.json"));
        suffix += 1;
    }
    path
}

fn sanitize_split_file_component(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.trim().chars() {
        let next = if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            last_dash = false;
            ch
        } else if !last_dash {
            last_dash = true;
            '-'
        } else {
            continue;
        };
        out.push(next);
        if out.len() >= SUB2API_SPLIT_NAME_MAX_LEN {
            break;
        }
    }
    let out = out.trim_matches(['.', '-', '_']).to_string();
    if out.is_empty() {
        "account".to_string()
    } else {
        out
    }
}

fn parse_sub2api_token_file(data: &str) -> Result<Option<TokenFile>, LoadAccountError> {
    let export: Sub2ApiExport =
        serde_json::from_str(data).map_err(|e| LoadAccountError::Parse(e.to_string()))?;
    if export.accounts.is_empty() {
        return Ok(None);
    }
    if export.accounts.len() != 1 {
        return Err(LoadAccountError::Parse(format!(
            "sub2api 导出包含 {} 个 accounts，当前仅支持单账号文件",
            export.accounts.len()
        )));
    }

    let account = export
        .accounts
        .into_iter()
        .next()
        .expect("checked len == 1");
    Ok(sub2api_account_to_token_file(account))
}

fn format_expires_at(expires_at_unix_seconds: i64) -> String {
    if expires_at_unix_seconds <= 0 {
        return String::new();
    }

    OffsetDateTime::from_unix_timestamp(expires_at_unix_seconds)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_else(|| expires_at_unix_seconds.to_string())
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

        assert_eq!(
            manager.scan_new_files().expect("initial scan"),
            ScanOutcome::default()
        );

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
        assert_eq!(
            manager.accounts_snapshot()[0].file_path(),
            a_path.to_string_lossy()
        );
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

        assert_eq!(
            manager.scan_new_files().expect("scan"),
            ScanOutcome::default()
        );
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

    #[test]
    fn core_manager_loads_sub2api_single_account_export() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("sub.json");
        fs::write(
            &auth_path,
            r#"{
  "accounts": [
    {
      "name": "sub@example.com",
      "type": "oauth",
      "credentials": {
        "access_token": "at-sub",
        "refresh_token": "rt-sub",
        "chatgpt_account_id": "acc-sub",
        "organization_id": "org-sub",
        "expires_at": 4070908800
      }
    }
  ],
  "proxies": [],
  "exported_at": "2026-04-11T11:35:09Z"
}"#,
        )
        .expect("write sub.json");

        let manager = Manager::new(dir.path());
        let count = manager.load_accounts().expect("load accounts");
        assert_eq!(count, 1);

        let token = manager.accounts_snapshot()[0].token_clone();
        assert_eq!(token.access_token, "at-sub");
        assert_eq!(token.refresh_token, "rt-sub");
        assert_eq!(token.account_id, "acc-sub");
        assert_eq!(token.email, "sub@example.com");
        assert_eq!(token.expired, "2099-01-01T00:00:00Z");
    }

    #[test]
    fn core_manager_loads_sub2api_string_expires_at() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("sub.json");
        fs::write(
            &auth_path,
            r#"{
  "accounts": [
    {
      "name": "sub@example.com",
      "type": "oauth",
      "credentials": {
        "access_token": "at-sub",
        "refresh_token": "rt-sub",
        "chatgpt_account_id": "acc-sub",
        "organization_id": "org-sub",
        "email": "sub@example.com",
        "expires_at": "2026-06-17T22:36:07.000Z"
      }
    }
  ],
  "proxies": [],
  "exported_at": "2026-04-11T11:35:09Z"
}"#,
        )
        .expect("write sub.json");

        let manager = Manager::new(dir.path());
        let count = manager.load_accounts().expect("load accounts");
        assert_eq!(count, 1);

        let token = manager.accounts_snapshot()[0].token_clone();
        assert_eq!(token.access_token, "at-sub");
        assert_eq!(token.refresh_token, "rt-sub");
        assert_eq!(token.account_id, "acc-sub");
        assert_eq!(token.email, "sub@example.com");
        assert_eq!(token.expired, "2026-06-17T22:36:07Z");
    }

    #[test]
    fn core_manager_migrates_sub2api_multi_account_export() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("sub.json");
        fs::write(
            &auth_path,
            r#"{
  "accounts": [
    {
      "name": "a@example.com",
      "type": "oauth",
      "credentials": {
        "access_token": "at-a",
        "refresh_token": "rt-a",
        "chatgpt_account_id": "acc-a",
        "expires_at": 4070908800
      }
    },
    {
      "name": "b@example.com",
      "type": "oauth",
      "credentials": {
        "access_token": "at-b",
        "refresh_token": "rt-b",
        "chatgpt_account_id": "acc-b",
        "expires_at": 4070908800
      }
    }
  ],
  "proxies": [],
  "exported_at": "2026-04-11T11:35:09Z"
}"#,
        )
        .expect("write sub.json");

        let manager = Manager::new(dir.path());
        let count = manager.load_accounts().expect("load accounts");
        assert_eq!(count, 2);
        assert!(
            !auth_path.exists(),
            "multi-account export should be removed after migration"
        );

        let mut split_files: Vec<_> = fs::read_dir(dir.path())
            .expect("read dir")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| is_json_file(path))
            .collect();
        split_files.sort();
        assert_eq!(split_files.len(), 2);
        assert!(
            split_files[0]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("a-example.com")
        );
        assert!(
            split_files[1]
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("b-example.com")
        );

        let mut tokens: Vec<_> = manager
            .accounts_snapshot()
            .iter()
            .map(|acc| acc.token_clone())
            .collect();
        tokens.sort_by(|a, b| a.email.cmp(&b.email));
        assert_eq!(tokens[0].access_token, "at-a");
        assert_eq!(tokens[0].refresh_token, "rt-a");
        assert_eq!(tokens[0].account_id, "acc-a");
        assert_eq!(tokens[0].email, "a@example.com");
        assert_eq!(tokens[1].access_token, "at-b");
        assert_eq!(tokens[1].refresh_token, "rt-b");
        assert_eq!(tokens[1].account_id, "acc-b");
        assert_eq!(tokens[1].email, "b@example.com");
    }

    #[test]
    fn core_manager_scan_new_files_migrates_sub2api_multi_account_export() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = Manager::new(dir.path());

        assert_eq!(
            manager.scan_new_files().expect("initial scan"),
            ScanOutcome::default()
        );

        let auth_path = dir.path().join("sub.json");
        fs::write(
            &auth_path,
            r#"{
  "accounts": [
    {
      "name": "a@example.com",
      "type": "oauth",
      "credentials": {
        "access_token": "at-a",
        "refresh_token": "rt-a",
        "chatgpt_account_id": "acc-a",
        "expires_at": 4070908800
      }
    },
    {
      "name": "b@example.com",
      "type": "oauth",
      "credentials": {
        "access_token": "at-b",
        "refresh_token": "rt-b",
        "chatgpt_account_id": "acc-b",
        "expires_at": 4070908800
      }
    }
  ],
  "proxies": [],
  "exported_at": "2026-04-11T11:35:09Z"
}"#,
        )
        .expect("write sub.json");

        assert_eq!(
            manager.scan_new_files().expect("hot load"),
            ScanOutcome {
                added: 2,
                removed: 0
            }
        );
        assert_eq!(manager.account_count(), 2);
        assert!(
            !auth_path.exists(),
            "multi-account export should be removed after hot-scan migration"
        );
    }

    #[test]
    fn core_manager_sub2api_multi_account_migration_keeps_source_on_invalid_account() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("sub.json");
        fs::write(
            &auth_path,
            r#"{
  "accounts": [
    {
      "name": "a@example.com",
      "type": "oauth",
      "credentials": {
        "access_token": "at-a",
        "refresh_token": "rt-a",
        "chatgpt_account_id": "acc-a",
        "expires_at": 4070908800
      }
    },
    {
      "name": "b@example.com",
      "type": "oauth",
      "credentials": {
        "access_token": "",
        "refresh_token": "rt-b",
        "chatgpt_account_id": "acc-b",
        "expires_at": 4070908800
      }
    }
  ],
  "proxies": [],
  "exported_at": "2026-04-11T11:35:09Z"
}"#,
        )
        .expect("write sub.json");

        let manager = Manager::new(dir.path());
        let err = manager.load_accounts().expect_err("should error");
        assert!(err.contains("未找到有效"), "got err: {err}");
        assert!(
            auth_path.exists(),
            "source export should be kept when migration fails"
        );

        let json_files: Vec<_> = fs::read_dir(dir.path())
            .expect("read dir")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| is_json_file(path))
            .collect();
        assert_eq!(json_files, vec![auth_path]);
    }
}
