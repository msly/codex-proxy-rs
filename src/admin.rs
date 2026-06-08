use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Argon2, password_hash::rand_core::OsRng};
use serde::Serialize;
use uuid::Uuid;

const ADMIN_SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Debug)]
pub struct AdminAuth {
    config_path: PathBuf,
    username: Mutex<String>,
    password_hash: Mutex<String>,
    sessions: Mutex<HashMap<String, AdminSession>>,
}

#[derive(Debug, Clone)]
struct AdminSession {
    username: String,
    expires_at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdminStatus {
    pub username: String,
    pub initialized: bool,
}

impl AdminAuth {
    pub fn new(config_path: impl AsRef<Path>, username: String, password_hash: String) -> Self {
        Self {
            config_path: config_path.as_ref().to_path_buf(),
            username: Mutex::new(username),
            password_hash: Mutex::new(password_hash),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn status(&self) -> AdminStatus {
        let username = self
            .username
            .lock()
            .expect("admin username mutex poisoned")
            .clone();
        let initialized = !self
            .password_hash
            .lock()
            .expect("admin password mutex poisoned")
            .trim()
            .is_empty();
        AdminStatus {
            username,
            initialized,
        }
    }

    pub fn setup(&self, username: &str, password: &str) -> Result<String, String> {
        if self.status().initialized {
            return Err("admin password has already been initialized".to_string());
        }
        self.set_credentials(username, password)?;
        self.login(username, password)
    }

    pub fn login(&self, username: &str, password: &str) -> Result<String, String> {
        let current_username = self
            .username
            .lock()
            .expect("admin username mutex poisoned")
            .clone();
        if username.trim() != current_username {
            return Err("invalid username or password".to_string());
        }

        let hash = self
            .password_hash
            .lock()
            .expect("admin password mutex poisoned")
            .clone();
        if hash.trim().is_empty() {
            return Err("admin password is not initialized".to_string());
        }
        verify_password(password, &hash)?;

        let token = Uuid::new_v4().to_string();
        let expires_at_ms = now_ms().saturating_add(ADMIN_SESSION_TTL.as_millis() as i64);
        self.sessions
            .lock()
            .expect("admin sessions mutex poisoned")
            .insert(
                token.clone(),
                AdminSession {
                    username: current_username,
                    expires_at_ms,
                },
            );
        Ok(token)
    }

    pub fn logout(&self, token: &str) {
        self.sessions
            .lock()
            .expect("admin sessions mutex poisoned")
            .remove(token);
    }

    pub fn is_valid_token(&self, token: &str) -> bool {
        let now = now_ms();
        let mut sessions = self.sessions.lock().expect("admin sessions mutex poisoned");
        sessions.retain(|_, session| session.expires_at_ms > now);
        sessions
            .get(token)
            .map(|session| !session.username.is_empty())
            .unwrap_or(false)
    }

    pub fn change_password(
        &self,
        token: &str,
        current_password: &str,
        new_password: &str,
    ) -> Result<(), String> {
        if !self.is_valid_token(token) {
            return Err("invalid admin session".to_string());
        }
        let hash = self
            .password_hash
            .lock()
            .expect("admin password mutex poisoned")
            .clone();
        verify_password(current_password, &hash)?;
        let username = self
            .username
            .lock()
            .expect("admin username mutex poisoned")
            .clone();
        self.set_credentials(&username, new_password)
    }

    fn set_credentials(&self, username: &str, password: &str) -> Result<(), String> {
        let username = username.trim();
        validate_username(username)?;
        validate_password(password)?;
        let hash = hash_password(password)?;
        write_admin_config(&self.config_path, username, &hash)?;
        *self.username.lock().expect("admin username mutex poisoned") = username.to_string();
        *self
            .password_hash
            .lock()
            .expect("admin password mutex poisoned") = hash;
        self.sessions
            .lock()
            .expect("admin sessions mutex poisoned")
            .clear();
        Ok(())
    }
}

fn validate_username(username: &str) -> Result<(), String> {
    if username.is_empty() {
        return Err("username is required".to_string());
    }
    if username.len() > 64 {
        return Err("username is too long".to_string());
    }
    Ok(())
}

fn validate_password(password: &str) -> Result<(), String> {
    if password.len() < 8 {
        return Err("password must be at least 8 characters".to_string());
    }
    if password.len() > 256 {
        return Err("password is too long".to_string());
    }
    Ok(())
}

fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| format!("hash password failed: {e}"))
}

fn verify_password(password: &str, hash: &str) -> Result<(), String> {
    let parsed = PasswordHash::new(hash).map_err(|_| "invalid username or password".to_string())?;
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .map_err(|_| "invalid username or password".to_string())
}

fn write_admin_config(path: &Path, username: &str, password_hash: &str) -> Result<(), String> {
    let data = fs::read_to_string(path).map_err(|e| format!("read config failed: {e}"))?;
    serde_yaml::from_str::<serde_yaml::Value>(&data)
        .map_err(|e| format!("parse config failed: {e}"))?;
    let next = patch_admin_config_text(&data, username, password_hash);
    fs::write(path, next).map_err(|e| format!("write config failed: {e}"))
}

fn patch_admin_config_text(data: &str, username: &str, password_hash: &str) -> String {
    let trimmed = data.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return admin_block(username, password_hash);
    }

    let lines: Vec<&str> = data.lines().collect();
    let Some(admin_start) = lines
        .iter()
        .position(|line| line.trim_start().starts_with("admin:"))
    else {
        let mut next = trim_trailing_newlines(data).to_string();
        if !next.is_empty() {
            next.push_str("\n\n");
        }
        next.push_str(&admin_block(username, password_hash));
        return next;
    };

    let admin_indent = leading_spaces(lines[admin_start]);
    let admin_end = lines
        .iter()
        .enumerate()
        .skip(admin_start + 1)
        .find_map(|(idx, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            if leading_spaces(line) <= admin_indent {
                return Some(idx);
            }
            if trimmed.starts_with('#') {
                let next_key = lines
                    .iter()
                    .skip(idx + 1)
                    .find(|next| !next.trim().is_empty());
                if next_key
                    .map(|next| {
                        leading_spaces(next) <= admin_indent && !next.trim_start().starts_with('#')
                    })
                    .unwrap_or(false)
                {
                    return Some(idx);
                }
            }
            None
        })
        .unwrap_or(lines.len());

    let mut out = Vec::with_capacity(lines.len() + 4);
    out.extend(lines[..admin_start].iter().map(|line| (*line).to_string()));
    let indent = " ".repeat(admin_indent);
    let child_indent = " ".repeat(admin_indent + 2);
    out.push(format!("{indent}admin:"));
    out.push(format!(
        "{child_indent}username: \"{}\"",
        escape_yaml_double_quoted(username)
    ));
    out.push(format!(
        "{child_indent}password-hash: \"{}\"",
        escape_yaml_double_quoted(password_hash)
    ));
    out.extend(lines[admin_end..].iter().map(|line| (*line).to_string()));
    let mut next = out.join("\n");
    next.push('\n');
    next
}

fn admin_block(username: &str, password_hash: &str) -> String {
    format!(
        "admin:\n  username: \"{}\"\n  password-hash: \"{}\"\n",
        escape_yaml_double_quoted(username),
        escape_yaml_double_quoted(password_hash)
    )
}

fn trim_trailing_newlines(data: &str) -> &str {
    data.trim_end_matches(['\r', '\n'])
}

fn leading_spaces(line: &str) -> usize {
    line.chars().take_while(|ch| *ch == ' ').count()
}

fn escape_yaml_double_quoted(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::patch_admin_config_text;

    #[test]
    fn admin_patch_empty_object_config_writes_admin_block() {
        let patched = patch_admin_config_text("{}\n", "admin", "$argon2id$hash");
        assert_eq!(
            patched,
            "admin:\n  username: \"admin\"\n  password-hash: \"$argon2id$hash\"\n"
        );
    }

    #[test]
    fn admin_patch_preserves_surrounding_comments() {
        let patched = patch_admin_config_text(
            r#"# top
listen: ":18080"

# admin comment
admin:
  # old nested comment
  username: "admin"
  password-hash: ""

# keep
api-keys:
  - "sk"
"#,
            "admin",
            "$argon2id$hash",
        );
        assert!(patched.contains("# top"));
        assert!(patched.contains("# admin comment"));
        assert!(patched.contains("# keep"));
        assert!(patched.contains("api-keys:"));
        assert!(patched.contains("password-hash: \"$argon2id$hash\""));
        assert!(!patched.contains("old nested comment"));
    }
}
