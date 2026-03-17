use std::collections::HashSet;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use codex_proxy_rs::health::{HealthChecker, HealthCheckerConfig, KeepAlive, KeepAliveConfig};
use codex_proxy_rs::refresh::{
    RefreshLoop, RefreshLoopConfig, Refresher, SaveQueue, refresh_account_with_remove_reason,
};
use codex_proxy_rs::{
    api,
    config::Config,
    core::{Account, Manager},
    net,
    quota::QuotaChecker,
    upstream::codex::{CodexClient, On401Hook},
};
use tracing_subscriber::EnvFilter;
use url::Url;

#[tokio::main]
async fn main() -> Result<(), String> {
    let config_path = parse_config_path();
    let cfg = Config::load(&config_path)?;

    init_tracing(&cfg.log_level);

    let manager = Arc::new(Manager::new(&cfg.auth_dir));
    if cfg.startup_async_load {
        tracing::info!("startup_async_load enabled; accounts will load in background");
        let manager = manager.clone();
        let auth_dir = cfg.auth_dir.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            loop {
                match manager.scan_new_files() {
                    Ok(_) => {
                        let count = manager.account_count();
                        if count == 0 {
                            tracing::warn!(
                                auth_dir = %auth_dir,
                                "后台加载账号失败: 未找到有效账号文件，10 秒后重试"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                            continue;
                        }
                        tracing::info!(
                            auth_dir = %auth_dir,
                            count,
                            elapsed = ?start.elapsed(),
                            "accounts loaded"
                        );
                        return;
                    }
                    Err(err) => {
                        tracing::warn!("后台加载账号失败: {err}，10 秒后重试");
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    }
                }
            }
        });
    } else {
        let start = Instant::now();
        let count = manager.load_accounts()?;
        tracing::info!(
            auth_dir = %cfg.auth_dir,
            count,
            elapsed = ?start.elapsed(),
            "accounts loaded"
        );
    }

    let base_url = Url::parse(&cfg.base_url).map_err(|e| format!("base-url 无效: {e}"))?;
    let codex_http = net::build_backend_reqwest_client(&cfg, Duration::from_secs(5 * 60))?;
    let codex_client = Arc::new(CodexClient::new_with_http(base_url.clone(), codex_http));

    let quota_http = net::build_backend_reqwest_client(&cfg, Duration::from_secs(20))?;
    let quota_checker = Arc::new(QuotaChecker::new_with_config(&cfg, 50, quota_http)?);

    let refresh_http = net::build_generic_reqwest_client(&cfg, Duration::from_secs(30))?;
    let refresher = Refresher::new_with_http(refresh_http);
    let save_queue = SaveQueue::start(4);

    let on_401: On401Hook = {
        let manager = manager.clone();
        let refresher = refresher.clone();
        let save_queue = save_queue.clone();
        Arc::new(move |acc: Arc<Account>| {
            let manager = manager.clone();
            let refresher = refresher.clone();
            let save_queue = save_queue.clone();
            tokio::spawn(async move {
                let _ = refresh_account_with_remove_reason(
                    manager.as_ref(),
                    &refresher,
                    &save_queue,
                    acc,
                    2,
                    "auth_401",
                )
                .await;
            });
        })
    };

    let api_keys: HashSet<String> = cfg
        .api_keys
        .iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();

    tracing::info!(
        listen = %cfg.listen,
        bind_addr = %cfg.bind_addr(),
        auth_dir = %cfg.auth_dir,
        base_url = %cfg.base_url,
        refresh_interval = cfg.refresh_interval,
        max_retry = cfg.max_retry,
        "codex-proxy-rs starting"
    );

    let app = api::router(api::AppState {
        manager: manager.clone(),
        quota_checker,
        codex_client: codex_client.clone(),
        api_keys: Arc::new(api_keys),
        max_retry: cfg.max_retry,
        refresher: refresher.clone(),
        save_queue: save_queue.clone(),
        refresh_concurrency: cfg.refresh_concurrency as usize,
        on_401: Some(on_401),
    });
    let listener = tokio::net::TcpListener::bind(cfg.bind_addr())
        .await
        .map_err(|e| format!("监听失败: {e}"))?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    if cfg.refresh_interval > 0 {
        let refresh_interval = Duration::from_secs(cfg.refresh_interval);
        let scan_interval = Duration::from_secs(30.min(cfg.refresh_interval));

        match RefreshLoop::new(
            manager.clone(),
            refresher.clone(),
            save_queue.clone(),
            RefreshLoopConfig {
                refresh_interval,
                scan_interval,
                refresh_concurrency: cfg.refresh_concurrency as usize,
                max_retries: 3,
            },
        ) {
            Ok(loop_) => {
                let rx = shutdown_rx.clone();
                tokio::spawn(async move {
                    loop_.start_loop(rx).await;
                });
            }
            Err(err) => tracing::warn!("refresh loop disabled: {err}"),
        }
    } else {
        tracing::info!("refresh_interval=0; refresh loop disabled");
    }

    if cfg.health_check_interval > 0 {
        let health_http = net::build_backend_reqwest_client(
            &cfg,
            Duration::from_secs(cfg.health_check_request_timeout),
        )?;
        let hc = Arc::new(HealthChecker::new_with_http(
            base_url.clone(),
            health_http,
            HealthCheckerConfig {
                check_interval: Duration::from_secs(cfg.health_check_interval),
                max_consecutive_failures: cfg.health_check_max_failures as i64,
                concurrency: cfg.health_check_concurrency as usize,
                start_delay: Duration::from_secs(cfg.health_check_start_delay),
                batch_size: cfg.health_check_batch_size as usize,
                request_timeout: Duration::from_secs(cfg.health_check_request_timeout),
            },
        ));

        let manager = manager.clone();
        let rx = shutdown_rx.clone();
        tokio::spawn(async move {
            hc.start_loop(manager, rx).await;
        });
    } else {
        tracing::info!("health_check_interval=0; health checker disabled");
    }

    let mut ping_url = base_url.clone();
    let base_path = ping_url.path().trim_end_matches('/').to_string();
    if let Some(stripped) = base_path.strip_suffix("/codex") {
        ping_url.set_path(if stripped.is_empty() { "/" } else { stripped });
    } else {
        ping_url.set_path("/");
    }
    ping_url.set_query(None);
    ping_url.set_fragment(None);

    match net::build_backend_reqwest_client(&cfg, Duration::from_secs(10)) {
        Ok(keepalive_http) => {
            let ka = KeepAlive::new_with_http(
                ping_url,
                keepalive_http,
                KeepAliveConfig {
                    interval: Duration::from_secs(60),
                    request_timeout: Duration::from_secs(10),
                },
            );
            let rx = shutdown_rx.clone();
            tokio::spawn(async move {
                ka.start_loop(rx).await;
            });
        }
        Err(err) => tracing::warn!("keepalive disabled: {err}"),
    }

    let shutdown = async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown signal received");
        let _ = shutdown_tx.send(true);
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| format!("server error: {e}"))?;

    Ok(())
}

fn init_tracing(log_level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn parse_config_path() -> String {
    parse_config_path_from(env::args().skip(1))
}

fn parse_config_path_from<I, S>(args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut args = args.into_iter();
    let mut config = "config.yaml".to_string();

    while let Some(arg) = args.next() {
        let arg = arg.as_ref();
        match arg {
            "--config" | "-config" => {
                if let Some(next) = args.next() {
                    config = next.as_ref().to_string();
                }
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--config=") {
                    config = value.to_string();
                } else if let Some(value) = arg.strip_prefix("-config=") {
                    config = value.to_string();
                }
            }
        }
    }

    config
}

#[cfg(test)]
mod tests {
    use super::parse_config_path_from;

    #[test]
    fn config_cli_default_path() {
        assert_eq!(parse_config_path_from([] as [&str; 0]), "config.yaml");
    }

    #[test]
    fn config_cli_parses_space_separated_flag() {
        assert_eq!(
            parse_config_path_from(["--config", "config.dev.yaml"]),
            "config.dev.yaml"
        );
        assert_eq!(
            parse_config_path_from(["-config", "config.dev.yaml"]),
            "config.dev.yaml"
        );
    }

    #[test]
    fn config_cli_parses_equals_form() {
        assert_eq!(
            parse_config_path_from(["--config=config.dev.yaml"]),
            "config.dev.yaml"
        );
        assert_eq!(
            parse_config_path_from(["-config=config.dev.yaml"]),
            "config.dev.yaml"
        );
    }

    #[test]
    fn config_cli_prefers_last_value() {
        assert_eq!(
            parse_config_path_from(["--config", "a.yaml", "--config=b.yaml"]),
            "b.yaml"
        );
    }
}
