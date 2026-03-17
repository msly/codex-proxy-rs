use std::convert::Infallible;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::unfold;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::json;

use crate::core::Manager;
use crate::quota::QuotaChecker;
use crate::thinking::apply_thinking;
use crate::translate::{
    StreamState, build_reverse_tool_name_map, convert_non_stream_response, convert_openai_request_to_codex,
    convert_stream_chunk,
};
use crate::upstream::codex::CodexClient;
use crate::upstream::codex::UpstreamError;

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    accounts: usize,
}

#[derive(Clone)]
pub struct AppState {
    pub manager: Arc<Manager>,
    pub quota_checker: Arc<QuotaChecker>,
    pub codex_client: Arc<CodexClient>,
    pub api_keys: Arc<HashSet<String>>,
    pub max_retry: usize,
}

pub fn router(state: AppState) -> Router {
    let mgmt = Router::new()
        .route("/stats", get(stats))
        .route("/check-quota", post(check_quota))
        .route_layer(middleware::from_fn_with_state(state.clone(), api_key_auth));

    let v1 = Router::new()
        .route("/responses", post(v1_responses))
        .route("/models", get(v1_models))
        .route("/chat/completions", post(v1_chat_completions))
        .route_layer(middleware::from_fn_with_state(state.clone(), api_key_auth));

    Router::new()
        .route("/health", get(health))
        .merge(mgmt)
        .nest("/v1", v1)
        .with_state(state)
}

async fn api_key_auth(State(state): State<AppState>, req: Request<Body>, next: Next) -> Response {
    if state.api_keys.is_empty() {
        return next.run(req).await;
    }

    let (token, token_source) = extract_api_key(req.headers());
    if let Some(token) = token.as_deref() {
        if state.api_keys.contains(token) {
            return next.run(req).await;
        }
    }

    tracing::debug!(
        path = %req.uri().path(),
        token_source,
        has_authorization = req.headers().get(axum::http::header::AUTHORIZATION).is_some(),
        has_x_api_key = req.headers().get("x-api-key").is_some(),
        has_api_key = req.headers().get("api-key").is_some(),
        token_len = token.as_deref().unwrap_or("").len(),
        "api key auth failed"
    );

    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": {
                "message": "无效的 API Key",
                "type": "invalid_request_error",
                "code": "invalid_api_key",
            }
        })),
    )
        .into_response()
}

fn extract_api_key(headers: &HeaderMap) -> (Option<String>, &'static str) {
    // Authorization: Bearer <key>
    if let Some(v) = headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(s) = v.to_str() {
            let parts: Vec<&str> = s.trim().split_whitespace().collect();
            if parts.len() == 2 && parts[0].eq_ignore_ascii_case("bearer") {
                let token = parts[1].trim();
                if !token.is_empty() {
                    return (Some(token.to_string()), "authorization_bearer");
                }
            }
        }
    }

    // Claude clients: x-api-key / api-key
    if let Some(v) = headers.get("x-api-key") {
        if let Ok(s) = v.to_str() {
            let token = s.trim();
            if !token.is_empty() {
                return (Some(token.to_string()), "x-api-key");
            }
        }
    }
    if let Some(v) = headers.get("api-key") {
        if let Ok(s) = v.to_str() {
            let token = s.trim();
            if !token.is_empty() {
                return (Some(token.to_string()), "api-key");
            }
        }
    }

    (None, "none")
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        accounts: state.manager.account_count(),
    })
}

fn send_error(status: StatusCode, message: &str, err_type: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": err_type,
            }
        })),
    )
        .into_response()
}

fn send_upstream_error(err: UpstreamError) -> Response {
    match err {
        UpstreamError::Status { code, body } => {
            let status = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
            (
                status,
                Json(json!({
                    "error": {
                        "message": String::from_utf8_lossy(&body),
                        "type": "api_error",
                        "code": format!("upstream_{code}"),
                    }
                })),
            )
                .into_response()
        }
        UpstreamError::Pick(msg) | UpstreamError::Network(msg) => {
            send_error(StatusCode::INTERNAL_SERVER_ERROR, &msg, "server_error")
        }
    }
}

async fn v1_responses(State(state): State<AppState>, req: Request<Body>) -> Response {
    let raw = match axum::body::to_bytes(req.into_body(), 50 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return send_error(StatusCode::BAD_REQUEST, "读取请求体失败", "invalid_request_error"),
    };

    let body_value: serde_json::Value =
        serde_json::from_slice(&raw).unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
    let model = body_value
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if model.trim().is_empty() {
        return send_error(
            StatusCode::BAD_REQUEST,
            "缺少 model 字段",
            "invalid_request_error",
        );
    }
    let stream = body_value
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    tracing::info!(model = %model, stream, "received /v1/responses request");

    let (body, base_model) = apply_thinking(&raw, &model);
    let codex_body = convert_openai_request_to_codex(&base_model, &body, stream);

    let url = match state.codex_client.responses_url() {
        Ok(u) => u,
        Err(err) => {
            return send_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("构建上游 URL 失败: {err}"),
                "server_error",
            );
        }
    };

    let (upstream, account, _attempts) = match state
        .codex_client
        .send_with_retry(
            &state.manager,
            &base_model,
            url,
            codex_body,
            stream,
            state.max_retry,
        )
        .await
    {
        Ok(v) => v,
        Err(err) => return send_upstream_error(err),
    };

    account.record_success(crate::core::now_unix_ms());

    if stream {
        let status = upstream.status();
        let stream = upstream.bytes_stream().map(|chunk| {
            chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
        });

        let mut resp = Response::new(Body::from_stream(stream));
        *resp.status_mut() = status;
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/event-stream"),
        );
        resp.headers_mut().insert(
            header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache"),
        );
        resp.headers_mut().insert(
            header::CONNECTION,
            axum::http::HeaderValue::from_static("keep-alive"),
        );
        return resp;
    }

    let status = upstream.status();
    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => {
            return send_error(
                StatusCode::BAD_GATEWAY,
                &format!("读取上游响应失败: {err}"),
                "api_error",
            );
        }
    };

    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

async fn v1_chat_completions(State(state): State<AppState>, req: Request<Body>) -> Response {
    let raw = match axum::body::to_bytes(req.into_body(), 50 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return send_error(StatusCode::BAD_REQUEST, "读取请求体失败", "invalid_request_error"),
    };

    let body_value: serde_json::Value =
        serde_json::from_slice(&raw).unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
    let model = body_value
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if model.trim().is_empty() {
        return send_error(
            StatusCode::BAD_REQUEST,
            "缺少 model 字段",
            "invalid_request_error",
        );
    }
    let stream = body_value
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    tracing::info!(model = %model, stream, "received /v1/chat/completions request");

    let (body, base_model) = apply_thinking(&raw, &model);
    let codex_body = convert_openai_request_to_codex(&base_model, &body, true);

    let url = match state.codex_client.responses_url() {
        Ok(u) => u,
        Err(err) => {
            return send_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("构建上游 URL 失败: {err}"),
                "server_error",
            );
        }
    };

    let (upstream, account, _attempts) = match state
        .codex_client
        .send_with_retry(
            &state.manager,
            &base_model,
            url,
            codex_body,
            true,
            state.max_retry,
        )
        .await
    {
        Ok(v) => v,
        Err(err) => return send_upstream_error(err),
    };

    account.record_success(crate::core::now_unix_ms());

    let reverse_tool_map = build_reverse_tool_name_map(&raw);

    if stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
        tokio::spawn(async move {
            let mut buf = Vec::<u8>::new();
            let mut state = StreamState::new(&base_model);
            let mut upstream_stream = upstream.bytes_stream();

            while let Some(chunk) = upstream_stream.next().await {
                let chunk = match chunk {
                    Ok(b) => b,
                    Err(err) => {
                        let _ = tx.send(Err(std::io::Error::new(std::io::ErrorKind::Other, err))).await;
                        return;
                    }
                };

                buf.extend_from_slice(&chunk);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let line = buf.drain(..pos).collect::<Vec<u8>>();
                    let _ = buf.drain(..1);
                    let line = trim_ascii(&line);
                    if line.is_empty() {
                        continue;
                    }
                    let chunks = convert_stream_chunk(line, &mut state, &reverse_tool_map);
                    for chunk in chunks {
                        let msg = format!("data: {chunk}\n\n").into_bytes();
                        if tx.send(Ok(msg)).await.is_err() {
                            return;
                        }
                    }
                    if state.completed {
                        break;
                    }
                }
                if state.completed {
                    break;
                }
            }

            if state.completed && (state.has_text || state.has_tool_call) {
                let _ = tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
            }
        });

        let stream = unfold(rx, |mut rx| async move {
            let item = rx.recv().await?;
            Some((item, rx))
        });

        let mut resp = Response::new(Body::from_stream(stream));
        *resp.status_mut() = StatusCode::OK;
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/event-stream"),
        );
        resp.headers_mut().insert(
            header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache"),
        );
        resp.headers_mut().insert(
            header::CONNECTION,
            axum::http::HeaderValue::from_static("keep-alive"),
        );
        return resp;
    }

    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => {
            return send_error(
                StatusCode::BAD_GATEWAY,
                &format!("读取上游响应失败: {err}"),
                "api_error",
            );
        }
    };

    for line in bytes.split(|&b| b == b'\n') {
        let line = trim_ascii(line);
        if !line.starts_with(b"data:") {
            continue;
        }
        let payload = trim_ascii(&line[5..]);
        if payload.is_empty() || payload == b"[DONE]" {
            continue;
        }

        let (out, has_output) = convert_non_stream_response(payload, &reverse_tool_map);
        if out.is_empty() {
            continue;
        }
        if !has_output {
            return send_error(StatusCode::BAD_REQUEST, "empty response", "invalid_response");
        }

        let mut resp = Response::new(Body::from(out));
        *resp.status_mut() = StatusCode::OK;
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        return resp;
    }

    send_error(StatusCode::BAD_GATEWAY, "上游响应缺少 response.completed", "api_error")
}

fn trim_ascii(input: &[u8]) -> &[u8] {
    let mut start = 0usize;
    let mut end = input.len();
    while start < end && input[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && input[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &input[start..end]
}

const BASE_MODEL_LIST: &[&str] = &[
    "gpt-5",
    "gpt-5-codex",
    "gpt-5-codex-mini",
    "gpt-5.1",
    "gpt-5.1-codex",
    "gpt-5.1-codex-mini",
    "gpt-5.1-codex-max",
    "gpt-5.2",
    "gpt-5.2-codex",
    "gpt-5.3-codex",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "codex-mini",
];

const THINKING_SUFFIXES: &[&str] = &["low", "medium", "high", "xhigh", "max", "none", "auto"];

#[derive(Debug, Serialize)]
struct ModelItem {
    id: String,
    object: &'static str,
    owned_by: &'static str,
}

#[derive(Debug, Serialize)]
struct ModelsResponse {
    object: &'static str,
    data: Vec<ModelItem>,
}

async fn v1_models() -> Json<ModelsResponse> {
    let mut data = Vec::with_capacity(BASE_MODEL_LIST.len() * (2 + THINKING_SUFFIXES.len() * 2));

    for base in BASE_MODEL_LIST {
        data.push(ModelItem {
            id: (*base).to_string(),
            object: "model",
            owned_by: "openai",
        });
        data.push(ModelItem {
            id: format!("{base}-fast"),
            object: "model",
            owned_by: "openai",
        });
        for suffix in THINKING_SUFFIXES {
            data.push(ModelItem {
                id: format!("{base}-{suffix}"),
                object: "model",
                owned_by: "openai",
            });
            data.push(ModelItem {
                id: format!("{base}-{suffix}-fast"),
                object: "model",
                owned_by: "openai",
            });
        }
    }

    Json(ModelsResponse { object: "list", data })
}

async fn check_quota(State(state): State<AppState>) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.quota_checker.check_all_stream(state.manager);

    let stream = unfold(rx, |mut rx| async move {
        let evt = rx.recv().await?;
        let json = serde_json::to_string(&evt).unwrap_or_else(|_| "{}".to_string());
        Some((Ok(Event::default().data(json)), rx))
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("ping"))
}

#[derive(Debug, Serialize)]
struct StatsSummary {
    total: usize,
    active: usize,
    cooldown: usize,
    disabled: usize,
}

#[derive(Debug, Serialize)]
struct StatsQuota {
    valid: bool,
    status_code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_data: Option<serde_json::Value>,
    checked_at_ms: i64,
}

#[derive(Debug, Serialize)]
struct StatsAccount {
    file_path: String,
    email: String,
    status: String,
    used_percent: f64,
    total_requests: i64,
    total_errors: i64,
    consecutive_failures: i64,
    token_expire: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota: Option<StatsQuota>,
}

#[derive(Debug, Serialize)]
struct StatsResponse {
    summary: StatsSummary,
    accounts: Vec<StatsAccount>,
}

async fn stats(State(state): State<AppState>) -> Json<StatsResponse> {
    let accounts = state.manager.accounts_snapshot();

    let mut out = Vec::with_capacity(accounts.len());
    let mut active = 0usize;
    let mut cooldown = 0usize;
    let mut disabled = 0usize;

    for acc in accounts.iter() {
        let snap = acc.stats_snapshot();
        match snap.status {
            crate::core::AccountStatus::Active => active += 1,
            crate::core::AccountStatus::Cooldown => cooldown += 1,
            crate::core::AccountStatus::Disabled => disabled += 1,
        }

        let quota = acc.quota_info().map(|info| StatsQuota {
            valid: info.valid,
            status_code: info.status_code,
            raw_data: if info.raw_data.is_empty() {
                None
            } else {
                serde_json::from_slice(&info.raw_data).ok()
            },
            checked_at_ms: info.checked_at_ms,
        });

        out.push(StatsAccount {
            file_path: snap.file_path,
            email: snap.email,
            status: snap.status.as_str().to_string(),
            used_percent: snap.used_percent,
            total_requests: snap.total_requests,
            total_errors: snap.total_errors,
            consecutive_failures: snap.consecutive_failures,
            token_expire: snap.token_expire,
            quota,
        });
    }

    Json(StatsResponse {
        summary: StatsSummary {
            total: accounts.len(),
            active,
            cooldown,
            disabled,
        },
        accounts: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::util::ServiceExt;
    use url::Url;

    #[tokio::test]
    async fn config_health_endpoint_returns_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = AppState {
            manager: Arc::new(Manager::new(dir.path())),
            quota_checker: Arc::new(
                QuotaChecker::new(
                    "https://chatgpt.com/backend-api/codex",
                    "chatgpt.com",
                    "",
                    1,
                )
                .unwrap(),
            ),
            codex_client: Arc::new(
                CodexClient::new(
                    Url::parse("https://chatgpt.com/backend-api/codex").unwrap(),
                    "",
                )
                .unwrap(),
            ),
            api_keys: Arc::new(HashSet::new()),
            max_retry: 0,
        };

        let app = router(state);
        let res = app
            .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
            .await
            .expect("request should succeed");
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn quota_check_quota_endpoint_streams_progress() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = Arc::new(Manager::new(dir.path()));

        let state = AppState {
            manager: manager.clone(),
            quota_checker: Arc::new(
                QuotaChecker::new(
                    "https://chatgpt.com/backend-api/codex",
                    "chatgpt.com",
                    "",
                    1,
                )
                .unwrap(),
            ),
            codex_client: Arc::new(
                CodexClient::new(
                    Url::parse("https://chatgpt.com/backend-api/codex").unwrap(),
                    "",
                )
                .unwrap(),
            ),
            api_keys: Arc::new(HashSet::new()),
            max_retry: 0,
        };

        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/check-quota")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("request should succeed");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(""),
            "text/event-stream"
        );

        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            body.contains("\"type\":\"done\""),
            "expected done event, got body: {body}"
        );
    }
}
