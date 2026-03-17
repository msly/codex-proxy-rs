use std::fs;
use std::path::Path;
use std::sync::Arc;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::core::{Account, TokenFile};

pub fn save_token_to_file(account: &Account) -> Result<(), String> {
    let token = account.token_clone();
    let last_refresh_ms = account.last_refresh_ms();
    let last_refresh = if last_refresh_ms > 0 {
        let dt = OffsetDateTime::from_unix_timestamp_nanos((last_refresh_ms as i128) * 1_000_000)
            .map_err(|e| format!("last_refresh 时间戳无效: {e}"))?;
        dt.format(&Rfc3339)
            .unwrap_or_else(|_| String::new())
    } else {
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| String::new())
    };

    let tf = TokenFile {
        id_token: token.id_token,
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        account_id: token.account_id,
        last_refresh,
        email: token.email,
        token_type: "codex".to_string(),
        expired: token.expired,
    };

    let data = serde_json::to_string_pretty(&tf).map_err(|e| format!("序列化 Token 失败: {e}"))?;

    let file_path = account.file_path();
    let dir = Path::new(file_path)
        .parent()
        .ok_or_else(|| format!("无效路径: {file_path}"))?;
    fs::create_dir_all(dir).map_err(|e| format!("创建目录失败: {e}"))?;

    let tmp_path = format!("{file_path}.tmp");
    fs::write(&tmp_path, data).map_err(|e| format!("写入临时文件失败: {e}"))?;
    fs::rename(&tmp_path, file_path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        format!("重命名文件失败: {e}")
    })?;

    Ok(())
}

#[derive(Clone, Debug)]
pub struct SaveQueue {
    tx: tokio::sync::mpsc::Sender<Arc<Account>>,
}

impl SaveQueue {
    pub fn start(concurrency: usize) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Arc<Account>>(4096);
        let permits = Arc::new(tokio::sync::Semaphore::new(concurrency.max(1)));

        tokio::spawn(async move {
            while let Some(acc) = rx.recv().await {
                let permits = permits.clone();
                tokio::spawn(async move {
                    let _permit = permits.acquire_owned().await.unwrap();
                    let _ = tokio::task::spawn_blocking(move || save_token_to_file(&acc)).await;
                });
            }
        });

        Self { tx }
    }

    pub fn enqueue(&self, acc: Arc<Account>) {
        let _ = self.tx.try_send(acc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{now_unix_ms, Account, TokenData};

    #[test]
    fn refresh_save_token_to_file_writes_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("a.json");

        let acc = Account::new(
            file_path.to_string_lossy().to_string(),
            TokenData {
                id_token: "id".to_string(),
                access_token: "at".to_string(),
                refresh_token: "rt".to_string(),
                account_id: "acc".to_string(),
                email: "a@example.com".to_string(),
                expired: "2099-01-01T00:00:00Z".to_string(),
                plan_type: String::new(),
            },
        );
        acc.set_last_refresh_ms_for_test(now_unix_ms());

        save_token_to_file(&acc).expect("save");

        let data = fs::read_to_string(&file_path).expect("read saved file");
        let tf: TokenFile = serde_json::from_str(&data).expect("parse json");

        assert_eq!(tf.access_token, "at");
        assert_eq!(tf.refresh_token, "rt");
        assert_eq!(tf.account_id, "acc");
        assert_eq!(tf.email, "a@example.com");
        assert_eq!(tf.token_type, "codex");
        assert_eq!(tf.expired, "2099-01-01T00:00:00Z");
        assert!(!tf.last_refresh.is_empty());

        assert!(
            !Path::new(&format!("{}.tmp", file_path.to_string_lossy())).exists(),
            "tmp file should be renamed away"
        );
    }
}
