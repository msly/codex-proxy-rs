use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use crate::config::Config;

pub fn build_backend_reqwest_client(
    cfg: &Config,
    timeout: Duration,
) -> Result<reqwest::Client, String> {
    build_reqwest_client(cfg, timeout, true)
}

pub fn build_generic_reqwest_client(
    cfg: &Config,
    timeout: Duration,
) -> Result<reqwest::Client, String> {
    build_reqwest_client(cfg, timeout, false)
}

fn build_reqwest_client(
    cfg: &Config,
    timeout: Duration,
    apply_backend_resolve: bool,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(Some(Duration::from_secs(120)))
        .tcp_keepalive(Some(Duration::from_secs(60)));

    if let Some(max) = pick_pool_max_idle_per_host(cfg) {
        builder = builder.pool_max_idle_per_host(max);
    }

    if !cfg.enable_http2 {
        builder = builder.http1_only();
    }

    if !cfg.proxy_url.trim().is_empty() {
        match reqwest::Proxy::all(&cfg.proxy_url) {
            Ok(proxy) => builder = builder.proxy(proxy),
            Err(err) => tracing::warn!("代理地址解析失败: {err}"),
        }
    }

    if apply_backend_resolve {
        let domain = cfg.backend_domain.trim();
        let resolve_address = cfg.backend_resolve_address.trim();
        if !domain.is_empty() && !resolve_address.is_empty() {
            let addr = parse_resolve_address(resolve_address)?;
            builder = builder.resolve(domain, addr);
            tracing::debug!(backend_domain = domain, resolve = %addr, "backend resolve override enabled");
        }
    }

    builder
        .build()
        .map_err(|e| format!("构建 HTTP client 失败: {e}"))
}

fn pick_pool_max_idle_per_host(cfg: &Config) -> Option<usize> {
    let per_host = cfg.max_idle_conns_per_host as usize;
    if per_host > 0 {
        return Some(per_host);
    }
    None
}

fn parse_resolve_address(resolve_address: &str) -> Result<SocketAddr, String> {
    let resolve_address = resolve_address.trim();
    if resolve_address.is_empty() {
        return Err("resolve address 不能为空".to_string());
    }

    if let Ok(addr) = resolve_address.parse::<SocketAddr>() {
        return Ok(addr);
    }

    if let Ok(ip) = resolve_address.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, 0));
    }

    // hostname[:port] — resolve eagerly to an IP since reqwest DNS overrides require SocketAddr.
    let (host, port) = match resolve_address.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => match p.parse::<u16>() {
            Ok(port) => (h, port),
            Err(_) => (resolve_address, 0),
        },
        _ => (resolve_address, 0),
    };

    let mut addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("解析 resolve address 失败: {e}"))?;
    addrs
        .next()
        .ok_or_else(|| format!("解析 resolve address 失败: 无可用地址 ({resolve_address})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use tokio::net::TcpListener;

    #[test]
    fn net_parse_resolve_address_accepts_ip_without_port() {
        let addr = parse_resolve_address("127.0.0.1").unwrap();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), 0);
    }

    #[test]
    fn net_parse_resolve_address_accepts_ip_with_port() {
        let addr = parse_resolve_address("127.0.0.1:12345").unwrap();
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_eq!(addr.port(), 12345);
    }

    #[tokio::test]
    async fn net_backend_resolve_override_connects_to_override_addr() {
        async fn ok() -> &'static str {
            "ok"
        }

        let app = Router::new().route("/ping", get(ok));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut cfg = Config::default();
        cfg.backend_domain = "backend.test".to_string();
        cfg.backend_resolve_address = format!("127.0.0.1:{}", addr.port());
        cfg.proxy_url = String::new();
        cfg.enable_http2 = false;

        let client = build_backend_reqwest_client(&cfg, Duration::from_secs(5)).unwrap();
        let resp = client.get("http://backend.test/ping").send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "ok");
    }

    #[test]
    fn net_pool_max_idle_per_host_does_not_fallback_to_removed_global_limit() {
        let mut cfg = Config::default();
        cfg.max_idle_conns_per_host = 0;

        assert_eq!(pick_pool_max_idle_per_host(&cfg), None);
    }
}
