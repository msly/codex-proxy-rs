use std::fs;
use std::path::Path;

use serde::Deserialize;
use url::Url;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen: String,

    #[serde(rename = "auth-dir")]
    pub auth_dir: String,

    #[serde(rename = "proxy-url")]
    pub proxy_url: String,

    #[serde(rename = "backend-domain")]
    pub backend_domain: String,

    #[serde(rename = "backend-resolve-address")]
    pub backend_resolve_address: String,

    #[serde(rename = "base-url")]
    pub base_url: String,

    #[serde(rename = "log-level")]
    pub log_level: String,

    #[serde(rename = "refresh-interval")]
    pub refresh_interval: u64,

    #[serde(rename = "max-retry")]
    pub max_retry: usize,

    #[serde(rename = "health-check-interval")]
    pub health_check_interval: u64,

    #[serde(rename = "health-check-max-failures")]
    pub health_check_max_failures: u64,

    #[serde(rename = "health-check-concurrency")]
    pub health_check_concurrency: u64,

    #[serde(rename = "health-check-start-delay")]
    pub health_check_start_delay: u64,

    #[serde(rename = "health-check-batch-size")]
    pub health_check_batch_size: u64,

    #[serde(rename = "health-check-request-timeout")]
    pub health_check_request_timeout: u64,

    #[serde(rename = "refresh-concurrency")]
    pub refresh_concurrency: u64,

    #[serde(rename = "max-conns-per-host")]
    pub max_conns_per_host: u64,

    #[serde(rename = "max-idle-conns")]
    pub max_idle_conns: u64,

    #[serde(rename = "max-idle-conns-per-host")]
    pub max_idle_conns_per_host: u64,

    #[serde(rename = "enable-http2")]
    pub enable_http2: bool,

    #[serde(rename = "startup-async-load")]
    pub startup_async_load: bool,

    #[serde(default)]
    pub accounts: Vec<String>,

    #[serde(rename = "api-keys")]
    #[serde(default)]
    pub api_keys: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: ":8080".to_string(),
            auth_dir: "./auths".to_string(),
            proxy_url: String::new(),
            backend_domain: String::new(),
            backend_resolve_address: String::new(),
            base_url: String::new(),
            log_level: "info".to_string(),
            refresh_interval: 3000,
            max_retry: 2,
            health_check_interval: 300,
            health_check_max_failures: 3,
            health_check_concurrency: 5,
            health_check_start_delay: 45,
            health_check_batch_size: 20,
            health_check_request_timeout: 8,
            refresh_concurrency: 50,
            max_conns_per_host: 512,
            max_idle_conns: 1024,
            max_idle_conns_per_host: 512,
            enable_http2: false,
            startup_async_load: true,
            accounts: Vec::new(),
            api_keys: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let data =
            fs::read_to_string(path.as_ref()).map_err(|e| format!("读取配置文件失败: {e}"))?;
        Self::load_from_str(&data)
    }

    pub fn load_from_str(yaml: &str) -> Result<Self, String> {
        let mut cfg: Config =
            serde_yaml::from_str(yaml).map_err(|e| format!("解析配置文件失败: {e}"))?;
        cfg.sanitize();
        Ok(cfg)
    }

    pub fn sanitize(&mut self) {
        self.listen = self.listen.trim().to_string();
        self.auth_dir = self.auth_dir.trim().to_string();
        self.proxy_url = self.proxy_url.trim().to_string();
        self.backend_domain = self.backend_domain.trim().to_string();
        self.backend_resolve_address = self.backend_resolve_address.trim().to_string();
        self.base_url = self.base_url.trim().to_string();
        self.log_level = self.log_level.trim().to_lowercase();

        if self.listen.is_empty() {
            self.listen = ":8080".to_string();
        }
        if self.auth_dir.is_empty() {
            self.auth_dir = "./auths".to_string();
        }

        // 优先级：base-url（若配置） > backend-domain（自动拼接）
        if !self.base_url.is_empty() {
            if !(self.base_url.to_lowercase().starts_with("http://")
                || self.base_url.to_lowercase().starts_with("https://"))
            {
                self.base_url = format!("https://{}", self.base_url);
            }
            if let Ok(u) = Url::parse(&self.base_url) {
                if let Some(host) = u.host_str() {
                    if !host.is_empty() {
                        self.backend_domain = host.to_string();
                    }
                }
            }
        } else {
            if self.backend_domain.is_empty() {
                self.backend_domain = "chatgpt.com".to_string();
            }
            self.base_url = format!("https://{}/backend-api/codex", self.backend_domain);
        }

        if self.refresh_interval == 0 {
            self.refresh_interval = 3000;
        }
        // max_retry: usize, negative not possible; keep as-is

        // health_check_interval: 0 disables
        if self.health_check_max_failures == 0 {
            self.health_check_max_failures = 3;
        }
        if self.health_check_concurrency == 0 {
            self.health_check_concurrency = 5;
        }
        if self.health_check_concurrency > 128 {
            self.health_check_concurrency = 128;
        }
        if self.health_check_batch_size > 0
            && self.health_check_concurrency > self.health_check_batch_size
        {
            self.health_check_concurrency = self.health_check_batch_size;
        }
        if self.health_check_request_timeout == 0 {
            self.health_check_request_timeout = 8;
        }
        if self.refresh_concurrency == 0 {
            self.refresh_concurrency = 50;
        }

        match self.log_level.as_str() {
            "debug" | "info" | "warn" | "error" => {}
            _ => self.log_level = "info".to_string(),
        }
    }

    // Go 允许 ":8080" 形式，Rust bind 需要明确 host。
    pub fn bind_addr(&self) -> String {
        let listen = self.listen.trim();
        if listen.starts_with(':') {
            format!("0.0.0.0{listen}")
        } else {
            listen.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_and_base_url_derivation() {
        let cfg = Config::load_from_str("{}").expect("load empty config");
        assert_eq!(cfg.listen, ":8080");
        assert_eq!(cfg.auth_dir, "./auths");
        assert_eq!(cfg.backend_domain, "chatgpt.com");
        assert_eq!(cfg.base_url, "https://chatgpt.com/backend-api/codex");
        assert_eq!(cfg.log_level, "info");
        assert_eq!(cfg.bind_addr(), "0.0.0.0:8080");
    }

    #[test]
    fn config_base_url_overrides_backend_domain_and_adds_scheme() {
        let cfg = Config::load_from_str(
            r#"
base-url: "example.com/backend-api/codex"
backend-domain: "should-be-overridden.example"
"#,
        )
        .expect("load config");
        assert_eq!(cfg.base_url, "https://example.com/backend-api/codex");
        assert_eq!(cfg.backend_domain, "example.com");
    }
}
