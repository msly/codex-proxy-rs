use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::core::{AccountStatsSnapshot, now_unix_ms};

const DEFAULT_QUEUE_CAPACITY: usize = 8192;
const WRITE_BATCH_SIZE: usize = 64;

#[derive(Debug, Clone)]
pub struct PersistStore {
    db_path: PathBuf,
    tx: SyncSender<PersistEvent>,
    runtime: Arc<PersistRuntime>,
}

#[derive(Debug, Default)]
struct PersistRuntime {
    dropped_events: AtomicU64,
    write_errors: AtomicU64,
    writer_running: AtomicBool,
}

#[derive(Debug, Clone)]
enum PersistEvent {
    Request(RequestLogInput),
    Usage(UsageLogInput),
    AccountStatus(AccountStatusInput),
    Cleanup { older_than_ms: i64 },
}

#[derive(Debug, Clone, Default)]
pub struct RequestLogInput {
    pub ts_ms: i64,
    pub endpoint: String,
    pub model: String,
    pub stream: bool,
    pub status: u16,
    pub attempts: usize,
    pub api_key: Option<String>,
    pub account_file_path: Option<String>,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct UsageLogInput {
    pub ts_ms: i64,
    pub endpoint: String,
    pub model: String,
    pub api_key: Option<String>,
    pub account_file_path: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Clone, Default)]
pub struct AccountStatusInput {
    pub ts_ms: i64,
    pub file_path: String,
    pub email: String,
    pub status: String,
    pub plan_type: Option<String>,
    pub used_percent: f64,
    pub successful_requests: i64,
    pub failed_requests: i64,
    pub attempt_requests: i64,
    pub attempt_errors: i64,
    pub consecutive_failures: i64,
    pub last_used_ms: i64,
    pub cooldown_until_ms: i64,
    pub quota_exhausted: bool,
    pub quota_resets_at_ms: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestLogRow {
    pub id: i64,
    pub ts_ms: i64,
    pub endpoint: String,
    pub model: String,
    pub stream: bool,
    pub status: u16,
    pub attempts: usize,
    pub api_key: Option<String>,
    pub account_file_path: Option<String>,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageLogRow {
    pub id: i64,
    pub ts_ms: i64,
    pub endpoint: String,
    pub model: String,
    pub api_key: Option<String>,
    pub account_file_path: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountStatusRow {
    pub file_path: String,
    pub ts_ms: i64,
    pub email: String,
    pub status: String,
    pub plan_type: Option<String>,
    pub used_percent: f64,
    pub successful_requests: i64,
    pub failed_requests: i64,
    pub attempt_requests: i64,
    pub attempt_errors: i64,
    pub consecutive_failures: i64,
    pub last_used_ms: i64,
    pub cooldown_until_ms: i64,
    pub quota_exhausted: bool,
    pub quota_resets_at_ms: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
    pub reasoning_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PersistStatus {
    pub writer_running: bool,
    pub dropped_events: u64,
    pub write_errors: u64,
}

impl PersistStore {
    pub fn start(db_path: impl AsRef<Path>) -> Result<Self, String> {
        Self::start_with_capacity(db_path, DEFAULT_QUEUE_CAPACITY)
    }

    pub fn start_with_capacity(
        db_path: impl AsRef<Path>,
        queue_capacity: usize,
    ) -> Result<Self, String> {
        let db_path = db_path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建 SQLite 目录失败: {e}"))?;
        }
        {
            let conn = Connection::open(&db_path).map_err(|e| format!("打开 SQLite 失败: {e}"))?;
            init_schema(&conn).map_err(|e| format!("初始化 SQLite schema 失败: {e}"))?;
        }

        let (tx, rx) = mpsc::sync_channel::<PersistEvent>(queue_capacity.max(1));
        let worker_path = db_path.clone();
        let runtime = Arc::new(PersistRuntime::default());
        runtime.writer_running.store(true, Ordering::Relaxed);
        let worker_runtime = runtime.clone();
        thread::Builder::new()
            .name("codex-proxy-sqlite-writer".to_string())
            .spawn(move || {
                let mut conn = match Connection::open(worker_path) {
                    Ok(conn) => conn,
                    Err(err) => {
                        tracing::error!("sqlite writer open failed: {err}");
                        worker_runtime
                            .writer_running
                            .store(false, Ordering::Relaxed);
                        return;
                    }
                };
                if let Err(err) = init_schema(&conn) {
                    tracing::error!("sqlite writer init failed: {err}");
                    worker_runtime
                        .writer_running
                        .store(false, Ordering::Relaxed);
                    return;
                }
                while let Ok(first) = rx.recv() {
                    let mut events = Vec::with_capacity(WRITE_BATCH_SIZE);
                    events.push(first);
                    while events.len() < WRITE_BATCH_SIZE {
                        match rx.try_recv() {
                            Ok(event) => events.push(event),
                            Err(_) => break,
                        }
                    }
                    if let Err(err) = write_events(&mut conn, events) {
                        worker_runtime.write_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("sqlite write failed: {err}");
                    }
                }
                worker_runtime
                    .writer_running
                    .store(false, Ordering::Relaxed);
            })
            .map_err(|e| format!("启动 SQLite writer 失败: {e}"))?;

        Ok(Self {
            db_path,
            tx,
            runtime,
        })
    }

    pub fn record_request(&self, input: RequestLogInput) {
        self.enqueue(PersistEvent::Request(input));
    }

    pub fn record_usage(&self, input: UsageLogInput) {
        self.enqueue(PersistEvent::Usage(input));
    }

    pub fn record_account_status(&self, input: AccountStatusInput) {
        self.enqueue(PersistEvent::AccountStatus(input));
    }

    pub fn cleanup_older_than_days(&self, days: u64) {
        let older_than_ms = now_unix_ms().saturating_sub((days as i64).saturating_mul(86_400_000));
        self.enqueue(PersistEvent::Cleanup { older_than_ms });
    }

    pub fn status(&self) -> PersistStatus {
        PersistStatus {
            writer_running: self.runtime.writer_running.load(Ordering::Relaxed),
            dropped_events: self.runtime.dropped_events.load(Ordering::Relaxed),
            write_errors: self.runtime.write_errors.load(Ordering::Relaxed),
        }
    }

    fn enqueue(&self, event: PersistEvent) {
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                let dropped = self.runtime.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped == 1 || dropped.is_power_of_two() {
                    tracing::warn!(dropped_events = dropped, "sqlite persist queue full");
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                self.runtime.writer_running.store(false, Ordering::Relaxed);
                let dropped = self.runtime.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(dropped_events = dropped, "sqlite persist writer stopped");
            }
        }
    }

    pub async fn list_request_logs(&self, limit: usize) -> Result<Vec<RequestLogRow>, String> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || query_request_logs(&db_path, limit))
            .await
            .map_err(|e| format!("查询 request_logs 任务失败: {e}"))?
    }

    pub async fn list_usage_logs(&self, limit: usize) -> Result<Vec<UsageLogRow>, String> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || query_usage_logs(&db_path, limit))
            .await
            .map_err(|e| format!("查询 usage_logs 任务失败: {e}"))?
    }

    pub async fn list_account_status(&self) -> Result<Vec<AccountStatusRow>, String> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || query_account_status(&db_path))
            .await
            .map_err(|e| format!("查询 account_status 任务失败: {e}"))?
    }
}

impl From<&AccountStatsSnapshot> for AccountStatusInput {
    fn from(snap: &AccountStatsSnapshot) -> Self {
        Self {
            ts_ms: now_unix_ms(),
            file_path: snap.file_path.clone(),
            email: snap.email.clone(),
            status: snap.status.as_str().to_string(),
            plan_type: if snap.plan_type.is_empty() {
                None
            } else {
                Some(snap.plan_type.clone())
            },
            used_percent: snap.used_percent,
            successful_requests: snap.successful_requests,
            failed_requests: snap.failed_requests,
            attempt_requests: snap.total_requests,
            attempt_errors: snap.total_errors,
            consecutive_failures: snap.consecutive_failures,
            last_used_ms: snap.last_used_ms,
            cooldown_until_ms: snap.cooldown_until_ms,
            quota_exhausted: snap.quota_exhausted,
            quota_resets_at_ms: snap.quota_resets_at_ms,
            input_tokens: snap.usage_input_tokens,
            output_tokens: snap.usage_output_tokens,
            cached_tokens: snap.usage_cached_tokens,
            reasoning_tokens: snap.usage_reasoning_tokens,
            total_tokens: snap.usage_total_tokens,
        }
    }
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS request_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms INTEGER NOT NULL,
    endpoint TEXT NOT NULL,
    model TEXT NOT NULL,
    stream INTEGER NOT NULL,
    status INTEGER NOT NULL,
    attempts INTEGER NOT NULL,
    api_key TEXT,
    account_file_path TEXT,
    error_type TEXT,
    error_message TEXT,
    duration_ms INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_request_logs_ts ON request_logs(ts_ms DESC);
CREATE INDEX IF NOT EXISTS idx_request_logs_api_key_ts ON request_logs(api_key, ts_ms DESC);
CREATE INDEX IF NOT EXISTS idx_request_logs_account_ts ON request_logs(account_file_path, ts_ms DESC);

CREATE TABLE IF NOT EXISTS usage_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms INTEGER NOT NULL,
    endpoint TEXT NOT NULL,
    model TEXT NOT NULL,
    api_key TEXT,
    account_file_path TEXT NOT NULL,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cached_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_usage_logs_ts ON usage_logs(ts_ms DESC);
CREATE INDEX IF NOT EXISTS idx_usage_logs_account_ts ON usage_logs(account_file_path, ts_ms DESC);

CREATE TABLE IF NOT EXISTS account_status (
    file_path TEXT PRIMARY KEY,
    ts_ms INTEGER NOT NULL,
    email TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL,
    plan_type TEXT,
    used_percent REAL NOT NULL DEFAULT -1,
    successful_requests INTEGER NOT NULL DEFAULT 0,
    failed_requests INTEGER NOT NULL DEFAULT 0,
    attempt_requests INTEGER NOT NULL DEFAULT 0,
    attempt_errors INTEGER NOT NULL DEFAULT 0,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    last_used_ms INTEGER NOT NULL DEFAULT 0,
    cooldown_until_ms INTEGER NOT NULL DEFAULT 0,
    quota_exhausted INTEGER NOT NULL DEFAULT 0,
    quota_resets_at_ms INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cached_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens INTEGER NOT NULL DEFAULT 0
);
"#,
    )
}

fn write_events(conn: &mut Connection, events: Vec<PersistEvent>) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    for event in events {
        write_event(&tx, event)?;
    }
    tx.commit()
}

fn write_event(conn: &Connection, event: PersistEvent) -> rusqlite::Result<()> {
    match event {
        PersistEvent::Request(input) => {
            conn.execute(
                r#"INSERT INTO request_logs
(ts_ms, endpoint, model, stream, status, attempts, api_key, account_file_path, error_type, error_message, duration_ms)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"#,
                params![
                    input.ts_ms,
                    input.endpoint,
                    input.model,
                    input.stream as i64,
                    input.status as i64,
                    input.attempts as i64,
                    input.api_key,
                    input.account_file_path,
                    input.error_type,
                    input.error_message,
                    input.duration_ms,
                ],
            )?;
        }
        PersistEvent::Usage(input) => {
            conn.execute(
                r#"INSERT INTO usage_logs
(ts_ms, endpoint, model, api_key, account_file_path, input_tokens, output_tokens, cached_tokens, reasoning_tokens, total_tokens)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
                params![
                    input.ts_ms,
                    input.endpoint,
                    input.model,
                    input.api_key,
                    input.account_file_path,
                    input.input_tokens,
                    input.output_tokens,
                    input.cached_tokens,
                    input.reasoning_tokens,
                    input.total_tokens,
                ],
            )?;
        }
        PersistEvent::AccountStatus(input) => {
            conn.execute(
                r#"INSERT INTO account_status
(file_path, ts_ms, email, status, plan_type, used_percent, successful_requests, failed_requests, attempt_requests, attempt_errors, consecutive_failures, last_used_ms, cooldown_until_ms, quota_exhausted, quota_resets_at_ms, input_tokens, output_tokens, cached_tokens, reasoning_tokens, total_tokens)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
ON CONFLICT(file_path) DO UPDATE SET
    ts_ms=excluded.ts_ms,
    email=excluded.email,
    status=excluded.status,
    plan_type=excluded.plan_type,
    used_percent=excluded.used_percent,
    successful_requests=excluded.successful_requests,
    failed_requests=excluded.failed_requests,
    attempt_requests=excluded.attempt_requests,
    attempt_errors=excluded.attempt_errors,
    consecutive_failures=excluded.consecutive_failures,
    last_used_ms=excluded.last_used_ms,
    cooldown_until_ms=excluded.cooldown_until_ms,
    quota_exhausted=excluded.quota_exhausted,
    quota_resets_at_ms=excluded.quota_resets_at_ms,
    input_tokens=excluded.input_tokens,
    output_tokens=excluded.output_tokens,
    cached_tokens=excluded.cached_tokens,
    reasoning_tokens=excluded.reasoning_tokens,
    total_tokens=excluded.total_tokens"#,
                params![
                    input.file_path,
                    input.ts_ms,
                    input.email,
                    input.status,
                    input.plan_type,
                    input.used_percent,
                    input.successful_requests,
                    input.failed_requests,
                    input.attempt_requests,
                    input.attempt_errors,
                    input.consecutive_failures,
                    input.last_used_ms,
                    input.cooldown_until_ms,
                    input.quota_exhausted as i64,
                    input.quota_resets_at_ms,
                    input.input_tokens,
                    input.output_tokens,
                    input.cached_tokens,
                    input.reasoning_tokens,
                    input.total_tokens,
                ],
            )?;
        }
        PersistEvent::Cleanup { older_than_ms } => {
            conn.execute(
                "DELETE FROM request_logs WHERE ts_ms < ?1",
                params![older_than_ms],
            )?;
            conn.execute(
                "DELETE FROM usage_logs WHERE ts_ms < ?1",
                params![older_than_ms],
            )?;
        }
    }
    Ok(())
}

fn query_request_logs(path: &Path, limit: usize) -> Result<Vec<RequestLogRow>, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            r#"SELECT id, ts_ms, endpoint, model, stream, status, attempts, api_key,
account_file_path, error_type, error_message, duration_ms
FROM request_logs ORDER BY ts_ms DESC, id DESC LIMIT ?1"#,
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![limit.min(1000) as i64], |row| {
            Ok(RequestLogRow {
                id: row.get(0)?,
                ts_ms: row.get(1)?,
                endpoint: row.get(2)?,
                model: row.get(3)?,
                stream: row.get::<_, i64>(4)? != 0,
                status: row.get::<_, i64>(5)? as u16,
                attempts: row.get::<_, i64>(6)? as usize,
                api_key: row.get(7)?,
                account_file_path: row.get(8)?,
                error_type: row.get(9)?,
                error_message: row.get(10)?,
                duration_ms: row.get(11)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())
}

fn query_usage_logs(path: &Path, limit: usize) -> Result<Vec<UsageLogRow>, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            r#"SELECT id, ts_ms, endpoint, model, api_key, account_file_path, input_tokens,
output_tokens, cached_tokens, reasoning_tokens, total_tokens
FROM usage_logs ORDER BY ts_ms DESC, id DESC LIMIT ?1"#,
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![limit.min(1000) as i64], |row| {
            Ok(UsageLogRow {
                id: row.get(0)?,
                ts_ms: row.get(1)?,
                endpoint: row.get(2)?,
                model: row.get(3)?,
                api_key: row.get(4)?,
                account_file_path: row.get(5)?,
                input_tokens: row.get(6)?,
                output_tokens: row.get(7)?,
                cached_tokens: row.get(8)?,
                reasoning_tokens: row.get(9)?,
                total_tokens: row.get(10)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())
}

fn query_account_status(path: &Path) -> Result<Vec<AccountStatusRow>, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            r#"SELECT file_path, ts_ms, email, status, plan_type, used_percent,
successful_requests, failed_requests, attempt_requests, attempt_errors, consecutive_failures,
last_used_ms, cooldown_until_ms, quota_exhausted, quota_resets_at_ms, input_tokens, output_tokens,
cached_tokens, reasoning_tokens, total_tokens
FROM account_status ORDER BY ts_ms DESC"#,
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(AccountStatusRow {
                file_path: row.get(0)?,
                ts_ms: row.get(1)?,
                email: row.get(2)?,
                status: row.get(3)?,
                plan_type: row.get(4)?,
                used_percent: row.get(5)?,
                successful_requests: row.get(6)?,
                failed_requests: row.get(7)?,
                attempt_requests: row.get(8)?,
                attempt_errors: row.get(9)?,
                consecutive_failures: row.get(10)?,
                last_used_ms: row.get(11)?,
                cooldown_until_ms: row.get(12)?,
                quota_exhausted: row.get::<_, i64>(13)? != 0,
                quota_resets_at_ms: row.get(14)?,
                input_tokens: row.get(15)?,
                output_tokens: row.get(16)?,
                cached_tokens: row.get(17)?,
                reasoning_tokens: row.get(18)?,
                total_tokens: row.get(19)?,
            })
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_queue_full_drops_without_blocking_request_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = PersistStore::start_with_capacity(dir.path().join("persist.sqlite3"), 1)
            .expect("persist store");

        for i in 0..50_000 {
            store.record_request(RequestLogInput {
                ts_ms: i,
                endpoint: "/test".to_string(),
                model: "gpt-test".to_string(),
                status: 200,
                ..RequestLogInput::default()
            });
        }

        assert!(
            store.status().dropped_events > 0,
            "expected bounded queue to drop events under burst load"
        );
        assert!(store.status().writer_running);
    }
}
