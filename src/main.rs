use std::env;

use codex_proxy_rs::{api, config::Config};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), String> {
    let config_path = parse_config_path();
    let cfg = Config::load(&config_path)?;

    init_tracing(&cfg.log_level);

    tracing::info!(
        listen = %cfg.listen,
        bind_addr = %cfg.bind_addr(),
        auth_dir = %cfg.auth_dir,
        base_url = %cfg.base_url,
        refresh_interval = cfg.refresh_interval,
        max_retry = cfg.max_retry,
        "codex-proxy-rs starting"
    );

    let app = api::router();
    let listener = tokio::net::TcpListener::bind(cfg.bind_addr())
        .await
        .map_err(|e| format!("监听失败: {e}"))?;
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown signal received");
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
