use std::collections::HashSet;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use codex_proxy_rs::health::{HealthChecker, HealthCheckerConfig, KeepAlive, KeepAliveConfig};
use codex_proxy_rs::refresh::{
    RefreshLoop, RefreshLoopConfig, Refresher, SaveQueue, refresh_account_with_options,
};
use codex_proxy_rs::{
    api,
    config::Config,
    core::{Account, Manager, QuotaFirstSelector, RoundRobinSelector, Selector},
    net,
    quota::QuotaChecker,
    state::RuntimeStateStore,
    upstream::codex::{CodexClient, On401Hook, RetryPolicy},
};
use tracing_subscriber::EnvFilter;
use url::Url;

#[tokio::main]
async fn main() -> Result<(), String> {
    let config_path = parse_config_path();
    let cfg = Config::load(&config_path)?;

    init_tracing(&cfg.log_level);

    let selector: Arc<dyn Selector> = if cfg.selector == "quota-first" {
        Arc::new(QuotaFirstSelector::new())
    } else {
        Arc::new(RoundRobinSelector::new())
    };
    let manager = Arc::new(Manager::new_with_selector(&cfg.auth_dir, selector));
    let runtime_state = Arc::new(RuntimeStateStore::new(&cfg.auth_dir));
    if cfg.startup_async_load {
        tracing::info!("startup_async_load enabled; accounts will load in background");
        let manager = manager.clone();
        let auth_dir = cfg.auth_dir.clone();
        let runtime_state = runtime_state.clone();
        let retry_interval = cfg.startup_load_retry_interval.max(1);
        tokio::spawn(async move {
            let start = Instant::now();
            loop {
                match manager.scan_new_files() {
                    Ok(_) => {
                        let count = manager.account_count();
                        if count == 0 {
                            tracing::warn!(
                                auth_dir = %auth_dir,
                                retry_interval,
                                "后台加载账号失败: 未找到有效账号文件，稍后重试"
                            );
                            tokio::time::sleep(Duration::from_secs(retry_interval)).await;
                            continue;
                        }
                        tracing::info!(
                            auth_dir = %auth_dir,
                            count,
                            elapsed = ?start.elapsed(),
                            "accounts loaded"
                        );
                        if let Err(err) = runtime_state.load_and_apply(manager.as_ref()) {
                            tracing::warn!("restore runtime state failed: {err}");
                        }
                        return;
                    }
                    Err(err) => {
                        tracing::warn!(retry_interval, "后台加载账号失败: {err}，稍后重试");
                        tokio::time::sleep(Duration::from_secs(retry_interval)).await;
                    }
                }
            }
        });
    } else {
        let start = Instant::now();
        let count = manager.load_accounts()?;
        if let Err(err) = runtime_state.load_and_apply(manager.as_ref()) {
            tracing::warn!("restore runtime state failed: {err}");
        }
        tracing::info!(
            auth_dir = %cfg.auth_dir,
            count,
            elapsed = ?start.elapsed(),
            "accounts loaded"
        );
    }

    let base_url = Url::parse(&cfg.base_url).map_err(|e| format!("base-url 无效: {e}"))?;
    let codex_http = net::build_backend_reqwest_client(&cfg, Duration::from_secs(5 * 60))?;
    let codex_client = Arc::new(CodexClient::new_with_http_and_policy(
        base_url.clone(),
        codex_http,
        RetryPolicy {
            cooldown_401_ms: (cfg.cooldown_401_sec as i64).saturating_mul(1000),
            default_cooldown_429_ms: (cfg.cooldown_429_sec as i64).saturating_mul(1000),
            header_timeout: if cfg.upstream_timeout_sec > 0 {
                Some(Duration::from_secs(cfg.upstream_timeout_sec))
            } else {
                None
            },
        },
    ));

    let quota_http = net::build_backend_reqwest_client(&cfg, Duration::from_secs(20))?;
    let quota_checker = Arc::new(
        QuotaChecker::new_with_config(&cfg, cfg.quota_check_concurrency as usize, quota_http)?
            .with_runtime_state(runtime_state.clone()),
    );

    let refresh_http = net::build_generic_reqwest_client(
        &cfg,
        Duration::from_secs(cfg.refresh_single_timeout_sec.max(1)),
    )?;
    let refresher = Refresher::new_with_http(refresh_http);
    let save_queue = SaveQueue::start(cfg.save_workers as usize);
    runtime_state.clone().start_save_loop(manager.clone());

    let on_401: On401Hook = {
        let manager = manager.clone();
        let refresher = refresher.clone();
        let save_queue = save_queue.clone();
        let refresh_timeout = Duration::from_secs(cfg.refresh_single_timeout_sec.max(1));
        let cooldown_429_ms = (cfg.cooldown_429_sec as i64).saturating_mul(1000);
        Arc::new(move |acc: Arc<Account>| {
            let manager = manager.clone();
            let refresher = refresher.clone();
            let save_queue = save_queue.clone();
            let refresh_timeout = refresh_timeout;
            let cooldown_429_ms = cooldown_429_ms;
            tokio::spawn(async move {
                let file_path = acc.file_path().to_string();
                let refresh = refresh_account_with_options(
                    manager.as_ref(),
                    &refresher,
                    &save_queue,
                    acc,
                    2,
                    "auth_401",
                    cooldown_429_ms,
                );
                if tokio::time::timeout(refresh_timeout, refresh)
                    .await
                    .is_err()
                {
                    tracing::warn!(file_path, "401 background refresh timed out");
                    manager.remove_account(&file_path, "auth_401");
                }
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
        request_stats: Arc::new(api::RequestStats::default()),
        api_keys: Arc::new(api_keys),
        max_retry: cfg.max_retry,
        empty_retry_max: cfg.empty_retry_max,
        refresher: refresher.clone(),
        save_queue: save_queue.clone(),
        refresh_concurrency: cfg.refresh_concurrency as usize,
        runtime_state: runtime_state.clone(),
        on_401: Some(on_401),
    });
    let listener = tokio::net::TcpListener::bind(cfg.bind_addr())
        .await
        .map_err(|e| format!("监听失败: {e}"))?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    if cfg.refresh_interval > 0 {
        let refresh_interval = Duration::from_secs(cfg.refresh_interval);
        let scan_interval = Duration::from_secs(cfg.auth_scan_interval.min(cfg.refresh_interval));

        match RefreshLoop::new(
            manager.clone(),
            refresher.clone(),
            save_queue.clone(),
            RefreshLoopConfig {
                refresh_interval,
                scan_interval,
                refresh_concurrency: cfg.refresh_concurrency as usize,
                max_retries: 3,
                refresh_batch_size: cfg.refresh_batch_size as usize,
                rate_limit_cooldown_ms: (cfg.cooldown_429_sec as i64).saturating_mul(1000),
            },
        ) {
            Ok(loop_) => {
                let loop_ = loop_.with_runtime_state(runtime_state.clone());
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
        let hc = Arc::new(
            HealthChecker::new_with_http(
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
            )
            .with_runtime_state(runtime_state.clone()),
        );

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
                    interval: Duration::from_secs(cfg.keepalive_interval),
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

    let mut server_shutdown = shutdown_rx.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                loop {
                    if server_shutdown.changed().await.is_err() {
                        break;
                    }
                    if *server_shutdown.borrow() {
                        break;
                    }
                }
            })
            .await
            .map_err(|e| format!("server error: {e}"))
    });

    tokio::select! {
        result = &mut server => {
            return result.map_err(|e| format!("server task join failed: {e}"))?;
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutdown signal received");
        }
    }

    let _ = shutdown_tx.send(true);
    let shutdown_timeout = Duration::from_secs(cfg.shutdown_timeout.max(1));
    match tokio::time::timeout(shutdown_timeout, &mut server).await {
        Ok(result) => {
            result
                .map_err(|e| format!("server task join failed: {e}"))?
                .map_err(|e| format!("server error: {e}"))?;
        }
        Err(_) => {
            tracing::warn!(timeout = ?shutdown_timeout, "server shutdown timed out; aborting");
            server.abort();
            let _ = server.await;
        }
    }

    if let Err(err) = runtime_state.save_now(manager.as_ref()) {
        tracing::warn!("final runtime state save failed: {err}");
    }

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
