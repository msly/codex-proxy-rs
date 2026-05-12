use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{CloseCode, CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, State};
use axum::http::header;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Html;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use bytes::BytesMut;
use futures_util::StreamExt;
use futures_util::stream::unfold;
use memchr::memchr;
use serde::Serialize;
use serde_json::json;
use tower_http::compression::CompressionLayer;

use crate::core::{Account, Manager};
use crate::quota::QuotaChecker;
use crate::refresh::{Refresher, SaveQueue, refresh_account};
use crate::state::RuntimeStateStore;
use crate::thinking::apply::apply_thinking_to_value;
use crate::translate::request::{
    build_reverse_tool_name_map_from_value, convert_openai_value_to_codex_value,
    normalize_codex_instructions,
};
use crate::translate::{
    ClaudeStreamState, StreamState, convert_chat_completion_chunk_to_completion_chunk,
    convert_chat_completion_to_completion, convert_claude_request_to_openai,
    convert_codex_full_sse_to_claude_response_with_meta, convert_codex_stream_to_claude_events,
    convert_completions_request_to_chat_value, convert_image_request_to_responses_value,
    convert_non_stream_response, convert_responses_sse_to_images_json, convert_stream_chunk,
    extract_completed_response_payload,
};
use crate::upstream::codex::CodexClient;
use crate::upstream::codex::UpstreamError;

const INDEX_HTML: &str = include_str!("../../assets/index.html");

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
    pub request_stats: Arc<RequestStats>,
    pub api_keys: Arc<HashSet<String>>,
    pub max_retry: usize,
    pub empty_retry_max: usize,
    pub refresher: Refresher,
    pub save_queue: SaveQueue,
    pub refresh_concurrency: usize,
    pub runtime_state: Arc<RuntimeStateStore>,
    pub on_401: Option<crate::upstream::codex::On401Hook>,
}

#[derive(Debug, Default)]
pub struct RequestStats {
    last_minute_start: AtomicI64,
    last_minute_count: AtomicI64,
}

impl RequestStats {
    pub fn record_request(&self) {
        let minute_start = crate::core::now_unix_ms() / 60_000;
        loop {
            let prev = self.last_minute_start.load(Ordering::Relaxed);
            if prev == minute_start {
                self.last_minute_count.fetch_add(1, Ordering::Relaxed);
                return;
            }
            if self
                .last_minute_start
                .compare_exchange(prev, minute_start, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                self.last_minute_count.store(1, Ordering::Relaxed);
                return;
            }
        }
    }

    pub fn rpm(&self) -> i64 {
        let minute_start = crate::core::now_unix_ms() / 60_000;
        if self.last_minute_start.load(Ordering::Relaxed) != minute_start {
            return 0;
        }
        self.last_minute_count.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct UsageTokens {
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
    reasoning_tokens: i64,
    total_tokens: i64,
}

impl UsageTokens {
    fn has_activity(&self) -> bool {
        self.input_tokens > 0
            || self.output_tokens > 0
            || self.cached_tokens > 0
            || self.reasoning_tokens > 0
            || self.total_tokens > 0
    }
}

fn record_hourly_request(runtime_state: &RuntimeStateStore, now_ms: i64) {
    runtime_state.record_hourly_request(now_ms);
}

fn record_hourly_usage(runtime_state: &RuntimeStateStore, now_ms: i64, usage: UsageTokens) {
    if usage.has_activity() {
        runtime_state.record_hourly_usage(
            now_ms,
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_tokens,
            usage.reasoning_tokens,
            usage.total_tokens,
        );
    }
}

fn record_client_success(
    account: &Account,
    request_stats: &RequestStats,
    runtime_state: &RuntimeStateStore,
    now_ms: i64,
) {
    account.record_success(now_ms);
    account.record_client_success();
    request_stats.record_request();
    record_hourly_request(runtime_state, now_ms);
}

fn extract_usage_tokens(value: &serde_json::Value) -> Option<UsageTokens> {
    let usage = value.get("usage").or_else(|| {
        value
            .get("response")
            .and_then(|response| response.get("usage"))
    })?;
    let input_tokens = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);
    let output_tokens = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);
    let cached_tokens = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .or_else(|| {
            usage
                .get("prompt_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
        })
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);
    let reasoning_tokens = usage
        .get("output_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .or_else(|| {
            usage
                .get("completion_tokens_details")
                .and_then(|details| details.get("reasoning_tokens"))
        })
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens))
        .max(0);

    let usage = UsageTokens {
        input_tokens,
        output_tokens,
        cached_tokens,
        reasoning_tokens,
        total_tokens,
    };

    if !usage.has_activity() {
        return None;
    }

    Some(usage)
}

fn record_usage_from_value(
    account: &Account,
    runtime_state: &RuntimeStateStore,
    now_ms: i64,
    value: &serde_json::Value,
) {
    let Some(usage) = extract_usage_tokens(value) else {
        return;
    };
    account.record_usage_detail(
        usage.input_tokens,
        usage.output_tokens,
        usage.cached_tokens,
        usage.reasoning_tokens,
        usage.total_tokens,
    );
    record_hourly_usage(runtime_state, now_ms, usage);
}

fn record_usage_from_json_bytes(
    account: &Account,
    runtime_state: &RuntimeStateStore,
    now_ms: i64,
    bytes: &[u8],
) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return;
    };
    record_usage_from_value(account, runtime_state, now_ms, &value);
}

fn record_usage_from_sse_line(
    account: &Account,
    runtime_state: &RuntimeStateStore,
    now_ms: i64,
    line: &[u8],
) -> bool {
    if !line.starts_with(b"data:") {
        return false;
    }
    let payload = trim_ascii(&line[5..]);
    if payload.is_empty() || payload == b"[DONE]" {
        return false;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return false;
    };
    let had_usage = extract_usage_tokens(&value).is_some();
    if had_usage {
        record_usage_from_value(account, runtime_state, now_ms, &value);
    }
    had_usage
}

fn record_usage_from_sse_bytes(
    account: &Account,
    runtime_state: &RuntimeStateStore,
    now_ms: i64,
    bytes: &[u8],
) -> bool {
    for line in bytes.split(|b| *b == b'\n') {
        let line = trim_ascii(line);
        if line.is_empty() {
            continue;
        }
        if record_usage_from_sse_line(account, runtime_state, now_ms, line) {
            return true;
        }
    }
    false
}

fn build_passthrough_sse_response(
    endpoint: &'static str,
    model: String,
    upstream: reqwest::Response,
    account: Arc<Account>,
    runtime_state: Arc<RuntimeStateStore>,
    headers: HeaderMap,
) -> Response {
    let status = upstream.status();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);

    tokio::spawn(async move {
        let mut upstream_stream = upstream.bytes_stream();
        let mut buf = BytesMut::new();
        let mut recorded_usage = false;

        while let Some(chunk) = upstream_stream.next().await {
            let chunk = match chunk {
                Ok(b) => b,
                Err(err) => {
                    log_stream_read_failed(endpoint, &model, account.as_ref(), &err);
                    let _ = tx
                        .send(Err(std::io::Error::new(std::io::ErrorKind::Other, err)))
                        .await;
                    return;
                }
            };

            if tx.send(Ok(chunk.to_vec())).await.is_err() {
                return;
            }

            if recorded_usage {
                continue;
            }

            buf.extend_from_slice(&chunk);
            while let Some(pos) = memchr(b'\n', buf.as_ref()) {
                let mut line = buf.split_to(pos + 1);
                line.truncate(pos);

                let line = trim_ascii(line.as_ref());
                if line.is_empty() {
                    continue;
                }
                if record_usage_from_sse_line(
                    account.as_ref(),
                    runtime_state.as_ref(),
                    crate::core::now_unix_ms(),
                    line,
                ) {
                    recorded_usage = true;
                    break;
                }
            }
        }

        if !recorded_usage {
            let line = trim_ascii(buf.as_ref());
            if !line.is_empty() {
                let _ = record_usage_from_sse_line(
                    account.as_ref(),
                    runtime_state.as_ref(),
                    crate::core::now_unix_ms(),
                    line,
                );
            }
        }
    });

    let stream = unfold(rx, |mut rx| async move {
        let item = rx.recv().await?;
        Some((item, rx))
    });

    let mut resp = Response::new(Body::from_stream(stream));
    *resp.status_mut() = status;
    for (k, v) in headers.iter() {
        resp.headers_mut().append(k.clone(), v.clone());
    }
    resp
}

pub fn router(state: AppState) -> Router {
    let mgmt = Router::new()
        .route("/stats", get(stats))
        .route("/refresh", post(refresh))
        .route_layer(middleware::from_fn_with_state(state.clone(), api_key_auth));

    let mgmt_sse = Router::new()
        .route("/check-quota", post(check_quota))
        .route_layer(middleware::from_fn_with_state(state.clone(), api_key_auth));

    let v1 = Router::new()
        .route("/responses", post(v1_responses).get(v1_responses_ws))
        .route("/responses/compact", post(v1_responses_compact))
        .route("/models", get(v1_models))
        .route("/chat/completions", post(v1_chat_completions))
        .route("/completions", post(v1_completions))
        .route("/images/generations", post(v1_images_generations))
        .route("/images/edits", post(v1_images_edits))
        .route("/messages", post(v1_messages))
        .route("/messages/count_tokens", post(v1_messages_count_tokens))
        .route_layer(middleware::from_fn_with_state(state.clone(), api_key_auth));

    let non_v1 = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .merge(mgmt)
        .layer(CompressionLayer::new());

    Router::new()
        .merge(non_v1)
        .merge(mgmt_sse)
        .nest("/v1", v1)
        .layer(middleware::from_fn(cors_and_options))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn cors_and_options(req: Request<Body>, next: Next) -> Response {
    let origin = req
        .headers()
        .get("Origin")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("*")
        .to_string();

    if req.method() == axum::http::Method::OPTIONS {
        let allow_methods = req
            .headers()
            .get("Access-Control-Request-Method")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("GET, POST, PUT, PATCH, DELETE, OPTIONS");

        let allow_headers = req
            .headers()
            .get("Access-Control-Request-Headers")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Authorization, Content-Type");

        let mut resp = Response::new(Body::empty());
        *resp.status_mut() = StatusCode::NO_CONTENT;
        apply_cors_headers(resp.headers_mut(), &origin);
        resp.headers_mut().insert(
            "Access-Control-Allow-Methods",
            axum::http::HeaderValue::from_str(allow_methods).unwrap_or(
                axum::http::HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
            ),
        );
        resp.headers_mut().insert(
            "Access-Control-Allow-Headers",
            axum::http::HeaderValue::from_str(allow_headers).unwrap_or(
                axum::http::HeaderValue::from_static("Authorization, Content-Type"),
            ),
        );
        resp.headers_mut().insert(
            "Access-Control-Max-Age",
            axum::http::HeaderValue::from_static("86400"),
        );
        return resp;
    }

    let mut resp = next.run(req).await;
    apply_cors_headers(resp.headers_mut(), &origin);
    resp
}

fn apply_cors_headers(headers: &mut HeaderMap, origin: &str) {
    headers.insert(
        "Access-Control-Allow-Origin",
        axum::http::HeaderValue::from_str(origin)
            .unwrap_or(axum::http::HeaderValue::from_static("*")),
    );
    headers.insert(header::VARY, axum::http::HeaderValue::from_static("Origin"));
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

fn send_claude_error(status: StatusCode, err_type: &str, message: &str) -> Response {
    (
        status,
        Json(json!({
            "type": "error",
            "error": {
                "type": err_type,
                "message": message,
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

fn send_claude_upstream_error(err: UpstreamError) -> Response {
    match err {
        UpstreamError::Status { code, body } => {
            let status = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
            send_claude_error(status, "api_error", &String::from_utf8_lossy(&body))
        }
        UpstreamError::Pick(msg) | UpstreamError::Network(msg) => {
            send_claude_error(StatusCode::INTERNAL_SERVER_ERROR, "api_error", &msg)
        }
    }
}

fn truncate_for_log(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

fn preview_body_for_log(body: &[u8], max_chars: usize) -> String {
    truncate_for_log(&String::from_utf8_lossy(body), max_chars)
}

fn log_upstream_request_error(
    endpoint: &'static str,
    model: &str,
    stream: bool,
    err: &UpstreamError,
) {
    match err {
        UpstreamError::Status { code, body } => tracing::warn!(
            endpoint,
            model = %model,
            stream,
            status = *code,
            body = %preview_body_for_log(body, 240),
            "request failed upstream"
        ),
        UpstreamError::Pick(msg) => tracing::warn!(
            endpoint,
            model = %model,
            stream,
            error = %truncate_for_log(msg, 240),
            "request failed before upstream send"
        ),
        UpstreamError::Network(msg) => tracing::warn!(
            endpoint,
            model = %model,
            stream,
            error = %truncate_for_log(msg, 240),
            "request failed before upstream send"
        ),
    }
}

fn log_request_completed(
    endpoint: &'static str,
    model: &str,
    stream: bool,
    status: StatusCode,
    attempts: usize,
    account: &Account,
) {
    if stream {
        tracing::info!(
            endpoint,
            model = %model,
            stream,
            status = status.as_u16(),
            attempts,
            account = account.file_path(),
            "stream request accepted by upstream"
        );
    } else {
        tracing::info!(
            endpoint,
            model = %model,
            stream,
            status = status.as_u16(),
            attempts,
            account = account.file_path(),
            "request completed"
        );
    }
}

fn log_response_read_failed(
    endpoint: &'static str,
    model: &str,
    stream: bool,
    account: &Account,
    err: &reqwest::Error,
) {
    tracing::warn!(
        endpoint,
        model = %model,
        stream,
        account = account.file_path(),
        error = %err,
        "read upstream response failed"
    );
}

fn log_stream_read_failed(
    endpoint: &'static str,
    model: &str,
    account: &Account,
    err: &reqwest::Error,
) {
    tracing::warn!(
        endpoint,
        model = %model,
        stream = true,
        account = account.file_path(),
        error = %err,
        "stream read from upstream failed"
    );
}

fn log_stream_incomplete(
    endpoint: &'static str,
    model: &str,
    account: &Account,
    reason: &'static str,
) {
    tracing::warn!(
        endpoint,
        model = %model,
        stream = true,
        account = account.file_path(),
        reason,
        "stream ended without a complete response"
    );
}

fn extract_codex_passthrough_headers(headers: &HeaderMap) -> Option<HeaderMap> {
    let mut passthrough = HeaderMap::new();
    for name in [
        "Version",
        "Session_id",
        "Originator",
        "X-Codex-Turn-Metadata",
        "X-Client-Request-Id",
    ] {
        if let Some(value) = headers.get(name) {
            passthrough.insert(name, value.clone());
        }
    }
    if passthrough.is_empty() {
        None
    } else {
        Some(passthrough)
    }
}

async fn v1_responses(State(state): State<AppState>, req: Request<Body>) -> Response {
    let passthrough_headers = extract_codex_passthrough_headers(req.headers());
    let raw = match axum::body::to_bytes(req.into_body(), 50 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return send_error(
                StatusCode::BAD_REQUEST,
                "读取请求体失败",
                "invalid_request_error",
            );
        }
    };

    let mut body_value: serde_json::Value = serde_json::from_slice(&raw)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
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

    if let Err(message) = validate_function_call_output_context(&body_value) {
        return send_error(StatusCode::BAD_REQUEST, &message, "invalid_request_error");
    }

    let base_model = apply_thinking_to_value(&mut body_value, &model);
    let codex_value = convert_openai_value_to_codex_value(&base_model, body_value, stream);
    let codex_body = serde_json::to_vec(&codex_value).unwrap_or_else(|_| b"{}".to_vec());

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

    let endpoint = "/v1/responses";
    let (upstream, account, attempts) = match state
        .codex_client
        .send_with_retry(
            &state.manager,
            &base_model,
            url,
            codex_body,
            stream,
            state.max_retry,
            passthrough_headers.as_ref(),
            state.on_401.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(err) => {
            log_upstream_request_error(endpoint, &base_model, stream, &err);
            return send_upstream_error(err);
        }
    };

    if stream {
        let status = upstream.status();
        let now_ms = crate::core::now_unix_ms();
        record_client_success(
            account.as_ref(),
            state.request_stats.as_ref(),
            state.runtime_state.as_ref(),
            now_ms,
        );
        log_request_completed(
            endpoint,
            &base_model,
            true,
            status,
            attempts,
            account.as_ref(),
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache"),
        );
        headers.insert(
            header::CONNECTION,
            axum::http::HeaderValue::from_static("keep-alive"),
        );
        return build_passthrough_sse_response(
            endpoint,
            base_model,
            upstream,
            account,
            state.runtime_state.clone(),
            headers,
        );
    }

    let status = upstream.status();
    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => {
            log_response_read_failed(endpoint, &base_model, false, account.as_ref(), &err);
            return send_error(
                StatusCode::BAD_GATEWAY,
                &format!("读取上游响应失败: {err}"),
                "api_error",
            );
        }
    };
    let now_ms = crate::core::now_unix_ms();
    record_usage_from_json_bytes(
        account.as_ref(),
        state.runtime_state.as_ref(),
        now_ms,
        bytes.as_ref(),
    );
    record_client_success(
        account.as_ref(),
        state.request_stats.as_ref(),
        state.runtime_state.as_ref(),
        now_ms,
    );
    log_request_completed(
        endpoint,
        &base_model,
        false,
        status,
        attempts,
        account.as_ref(),
    );

    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

async fn v1_responses_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let passthrough_headers = extract_codex_passthrough_headers(&headers);
    ws.on_upgrade(move |socket| handle_responses_ws(socket, state, passthrough_headers))
        .into_response()
}

#[derive(Default)]
struct ResponsesWsSession {
    last_request: Option<serde_json::Value>,
    last_model: Option<String>,
    last_response_id: Option<String>,
    tool_call_ids: HashSet<String>,
}

async fn handle_responses_ws(
    mut socket: WebSocket,
    state: AppState,
    passthrough_headers: Option<HeaderMap>,
) {
    let mut session = ResponsesWsSession::default();
    loop {
        let msg = match socket.recv().await {
            Some(Ok(m)) => m,
            Some(Err(_)) => return,
            None => return,
        };

        match msg {
            Message::Text(text) => {
                let value: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => {
                        write_ws_error(&mut socket, "invalid_request_error", "非法 JSON").await;
                        continue;
                    }
                };
                let event_type = value
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();

                match event_type {
                    "response.create" | "response.append" => {
                        let (request_body, model) =
                            match build_responses_ws_request(&value, &mut session) {
                                Ok(v) => v,
                                Err(message) => {
                                    write_ws_error(&mut socket, "invalid_request_error", &message)
                                        .await;
                                    continue;
                                }
                            };
                        let stream_result = forward_responses_sse_as_ws(
                            &mut socket,
                            &state,
                            request_body,
                            &model,
                            passthrough_headers.as_ref(),
                            &mut session,
                        )
                        .await;
                        match stream_result {
                            Ok(()) => continue,
                            Err(ResponsesWsError::EmptyResponse) => {
                                write_ws_error(&mut socket, "invalid_response", "empty response")
                                    .await;
                                close_ws(&mut socket, close_code::POLICY, "empty response").await;
                                return;
                            }
                            Err(ResponsesWsError::Upstream(err)) => {
                                write_ws_error(&mut socket, "api_error", &err.to_string()).await;
                                return;
                            }
                            Err(ResponsesWsError::Local(msg)) => {
                                write_ws_error(&mut socket, "api_error", &msg).await;
                                return;
                            }
                        }
                    }
                    "response.cancel" | "response.close" => {
                        close_ws(&mut socket, close_code::NORMAL, "closed").await;
                        return;
                    }
                    _ => {
                        write_ws_error(&mut socket, "invalid_request_error", "不支持的事件类型")
                            .await;
                    }
                }
            }
            Message::Close(_) => return,
            Message::Ping(v) => {
                let _ = socket.send(Message::Pong(v)).await;
            }
            Message::Pong(_) => {}
            _ => {
                write_ws_error(&mut socket, "invalid_request_error", "仅支持文本帧").await;
            }
        }
    }
}

#[derive(Debug)]
enum ResponsesWsError {
    EmptyResponse,
    Upstream(UpstreamError),
    Local(String),
}

fn build_responses_ws_request(
    value: &serde_json::Value,
    session: &mut ResponsesWsSession,
) -> Result<(Vec<u8>, String), String> {
    let event_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let response_value = value
        .get("response")
        .ok_or_else(|| "缺少 response 字段".to_string())?;
    let response_object = response_value
        .as_object()
        .ok_or_else(|| "response 必须是对象".to_string())?;

    let mut request_value = match event_type {
        "response.create" => serde_json::Value::Object(response_object.clone()),
        "response.append" => {
            let Some(previous) = session.last_request.clone() else {
                return Err("response.append 之前必须先发送 response.create".to_string());
            };
            let mut merged = previous;
            let merged_object = merged
                .as_object_mut()
                .ok_or_else(|| "response 必须是对象".to_string())?;
            for (key, value) in response_object {
                merged_object.insert(key.clone(), value.clone());
            }
            merged
        }
        _ => return Err("不支持的事件类型".to_string()),
    };

    let existing_model = request_value
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .map(str::to_string);
    let model = response_object
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .map(str::to_string)
        .or(existing_model)
        .or_else(|| session.last_model.clone())
        .ok_or_else(|| "缺少 model 字段".to_string())?;

    let request_object = request_value
        .as_object_mut()
        .ok_or_else(|| "response 必须是对象".to_string())?;
    if event_type == "response.append"
        && request_object.get("previous_response_id").is_none()
        && let Some(previous_response_id) = session.last_response_id.clone()
    {
        request_object.insert(
            "previous_response_id".to_string(),
            serde_json::Value::String(previous_response_id),
        );
    }
    request_object.insert("stream".to_string(), serde_json::Value::Bool(true));
    request_object.insert(
        "model".to_string(),
        serde_json::Value::String(model.clone()),
    );

    validate_function_call_output_context_with_known_ids(&request_value, &session.tool_call_ids)?;

    session.last_request = Some(request_value.clone());
    session.last_model = Some(model.clone());

    let request_body =
        serde_json::to_vec(&request_value).map_err(|_| "序列化请求失败".to_string())?;
    Ok((request_body, model))
}

async fn forward_responses_sse_as_ws(
    socket: &mut WebSocket,
    state: &AppState,
    request_body: Vec<u8>,
    model: &str,
    passthrough_headers: Option<&HeaderMap>,
    session: &mut ResponsesWsSession,
) -> Result<(), ResponsesWsError> {
    tracing::info!(model = %model, "responses ws: fallback to HTTP/SSE forwarding");

    let mut body_value: serde_json::Value = serde_json::from_slice(&request_body)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
    let base_model = apply_thinking_to_value(&mut body_value, model);
    let codex_value = convert_openai_value_to_codex_value(&base_model, body_value, true);
    let codex_body = serde_json::to_vec(&codex_value).unwrap_or_else(|_| b"{}".to_vec());

    let url = state
        .codex_client
        .responses_url()
        .map_err(ResponsesWsError::Local)?;

    let (upstream, account, _attempts) = state
        .codex_client
        .send_with_retry(
            &state.manager,
            &base_model,
            url,
            codex_body,
            true,
            state.max_retry,
            passthrough_headers,
            state.on_401.clone(),
        )
        .await
        .map_err(ResponsesWsError::Upstream)?;

    let mut has_text = false;
    let mut has_tool = false;
    let mut has_completed_output = false;
    let mut buf = BytesMut::new();
    let mut upstream_stream = upstream.bytes_stream();

    while let Some(chunk) = upstream_stream.next().await {
        let chunk = chunk.map_err(|e| ResponsesWsError::Local(format!("读取上游响应失败: {e}")))?;
        buf.extend_from_slice(&chunk);

        while let Some(pos) = memchr(b'\n', buf.as_ref()) {
            let mut line = buf.split_to(pos + 1);
            line.truncate(pos);
            let line = trim_ascii(line.as_ref());
            if line.is_empty() {
                continue;
            }
            if !line.starts_with(b"data:") {
                continue;
            }
            let payload = trim_ascii(&line[5..]);
            if payload.is_empty() || payload == b"[DONE]" {
                continue;
            }

            let mut outbound_text = None;
            if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(payload) {
                update_responses_ws_session_from_event(session, &v);
                let had_stream_output = has_text || has_tool;
                if let Some(typ) = v.get("type").and_then(|v| v.as_str()) {
                    match typ {
                        "response.output_text.delta" => {
                            if v.get("delta").and_then(|v| v.as_str()).unwrap_or_default() != "" {
                                has_text = true;
                            }
                        }
                        "response.output_item.added"
                        | "response.function_call_arguments.delta"
                        | "response.function_call_arguments.done"
                        | "response.output_item.done" => {
                            has_tool = true;
                        }
                        "response.completed" => {
                            let (_, completed_has_output) = convert_non_stream_response(
                                payload,
                                &std::collections::HashMap::new(),
                            );
                            if completed_has_output {
                                has_completed_output = true;
                            }
                            if completed_has_output && had_stream_output {
                                if let Some(response) = v
                                    .get_mut("response")
                                    .and_then(|value| value.as_object_mut())
                                {
                                    response.remove("output");
                                }
                                outbound_text = Some(v.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }

            if socket
                .send(Message::Text(
                    outbound_text
                        .unwrap_or_else(|| String::from_utf8_lossy(payload).into_owned())
                        .into(),
                ))
                .await
                .is_err()
            {
                return Ok(());
            }
        }
    }

    if !has_text && !has_tool && !has_completed_output {
        return Err(ResponsesWsError::EmptyResponse);
    }

    let now_ms = crate::core::now_unix_ms();
    record_client_success(
        account.as_ref(),
        state.request_stats.as_ref(),
        state.runtime_state.as_ref(),
        now_ms,
    );
    Ok(())
}

async fn write_ws_error(socket: &mut WebSocket, err_type: &str, message: &str) {
    let body = json!({
        "type": "error",
        "error": {
            "type": err_type,
            "message": message,
        }
    });
    let _ = socket.send(Message::Text(body.to_string().into())).await;
}

async fn close_ws(socket: &mut WebSocket, code: CloseCode, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await;
}

async fn v1_messages(State(state): State<AppState>, req: Request<Body>) -> Response {
    let passthrough_headers = extract_codex_passthrough_headers(req.headers());
    let raw = match axum::body::to_bytes(req.into_body(), 50 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return send_claude_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "读取请求体失败",
            );
        }
    };

    let (openai_body, model, stream) = convert_claude_request_to_openai(&raw);
    if model.trim().is_empty() {
        return send_claude_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "缺少 model 字段",
        );
    }

    tracing::info!(model = %model, stream, "received /v1/messages request");

    let mut body_value: serde_json::Value = serde_json::from_slice(&openai_body)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
    let base_model = apply_thinking_to_value(&mut body_value, &model);
    let codex_value = convert_openai_value_to_codex_value(&base_model, body_value, true);
    let codex_body = serde_json::to_vec(&codex_value).unwrap_or_else(|_| b"{}".to_vec());

    let url = match state.codex_client.responses_url() {
        Ok(u) => u,
        Err(err) => {
            return send_claude_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                &format!("构建上游 URL 失败: {err}"),
            );
        }
    };

    let endpoint = "/v1/messages";
    let (upstream, account, attempts) = match state
        .codex_client
        .send_with_retry(
            &state.manager,
            &base_model,
            url,
            codex_body,
            true,
            state.max_retry,
            passthrough_headers.as_ref(),
            state.on_401.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(err) => {
            log_upstream_request_error(endpoint, &base_model, stream, &err);
            return send_claude_upstream_error(err);
        }
    };

    if stream {
        let status = upstream.status();
        log_request_completed(
            endpoint,
            &base_model,
            true,
            status,
            attempts,
            account.as_ref(),
        );
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
        let request_stats = state.request_stats.clone();
        let runtime_state = state.runtime_state.clone();
        let account_for_log = account.clone();
        let model_for_log = base_model.clone();
        tokio::spawn(async move {
            let mut buf = BytesMut::new();
            let mut state = ClaudeStreamState::new(&base_model);
            let mut upstream_stream = upstream.bytes_stream();

            while let Some(chunk) = upstream_stream.next().await {
                let chunk = match chunk {
                    Ok(b) => b,
                    Err(err) => {
                        log_stream_read_failed(
                            endpoint,
                            &model_for_log,
                            account_for_log.as_ref(),
                            &err,
                        );
                        let _ = tx
                            .send(Err(std::io::Error::new(std::io::ErrorKind::Other, err)))
                            .await;
                        return;
                    }
                };

                buf.extend_from_slice(&chunk);
                while let Some(pos) = memchr(b'\n', buf.as_ref()) {
                    let mut line = buf.split_to(pos + 1);
                    line.truncate(pos);
                    let line = trim_ascii(line.as_ref());
                    if line.is_empty() {
                        continue;
                    }

                    let events = convert_codex_stream_to_claude_events(line, &mut state);
                    for evt in events {
                        if tx.send(Ok(evt.into_bytes())).await.is_err() {
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

            if state.completed {
                let now_ms = crate::core::now_unix_ms();
                record_client_success(
                    account.as_ref(),
                    request_stats.as_ref(),
                    runtime_state.as_ref(),
                    now_ms,
                );
            } else {
                log_stream_incomplete(
                    endpoint,
                    &model_for_log,
                    account_for_log.as_ref(),
                    "missing response.completed",
                );
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
            log_response_read_failed(endpoint, &base_model, false, account.as_ref(), &err);
            return send_claude_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("读取上游响应失败: {err}"),
            );
        }
    };

    let result = convert_codex_full_sse_to_claude_response_with_meta(&bytes, &base_model);
    if !result.found_completed || result.json.is_empty() {
        tracing::warn!(
            endpoint,
            model = %base_model,
            stream = false,
            account = account.file_path(),
            "messages non-stream response missing response.completed"
        );
        return send_claude_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "未收到 response.completed 事件",
        );
    }
    if !result.has_text && !result.has_tool_use {
        tracing::warn!(
            endpoint,
            model = %base_model,
            stream = false,
            account = account.file_path(),
            "messages non-stream response was empty"
        );
        return send_claude_error(
            StatusCode::BAD_REQUEST,
            "invalid_response",
            "empty response",
        );
    }

    let now_ms = crate::core::now_unix_ms();
    record_client_success(
        account.as_ref(),
        state.request_stats.as_ref(),
        state.runtime_state.as_ref(),
        now_ms,
    );
    log_request_completed(
        endpoint,
        &base_model,
        false,
        StatusCode::OK,
        attempts,
        account.as_ref(),
    );

    let mut resp = Response::new(Body::from(result.json));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

async fn v1_messages_count_tokens(req: Request<Body>) -> Response {
    let raw = match axum::body::to_bytes(req.into_body(), 50 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return send_claude_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "读取请求体失败",
            );
        }
    };

    let value: serde_json::Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => {
            return send_claude_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "非法 JSON",
            );
        }
    };
    if value
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        return send_claude_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "缺少 model 字段",
        );
    }

    let tokens = estimate_claude_input_tokens(&value).max(1);
    (
        StatusCode::OK,
        Json(json!({
            "input_tokens": tokens,
        })),
    )
        .into_response()
}

fn estimate_claude_input_tokens(value: &serde_json::Value) -> i64 {
    let mut chars = 0usize;
    if let Some(system) = value.get("system") {
        chars += count_text_chars(system);
    }
    if let Some(messages) = value.get("messages").and_then(|v| v.as_array()) {
        for message in messages {
            chars += count_text_chars(message.get("content").unwrap_or(&serde_json::Value::Null));
        }
    }
    if let Some(tools) = value.get("tools") {
        chars += tools.to_string().chars().count();
    }
    ((chars as i64) + 3) / 4
}

fn count_text_chars(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(s) => s.chars().count(),
        serde_json::Value::Array(items) => items.iter().map(count_text_chars).sum(),
        serde_json::Value::Object(obj) => {
            let mut n = 0usize;
            for key in ["text", "content", "name", "input"] {
                if let Some(v) = obj.get(key) {
                    n += count_text_chars(v);
                }
            }
            n
        }
        _ => 0,
    }
}

fn validate_function_call_output_context(v: &serde_json::Value) -> Result<(), String> {
    validate_function_call_output_context_with_known_ids(v, &HashSet::new())
}

fn validate_function_call_output_context_with_known_ids(
    v: &serde_json::Value,
    known_call_ids: &HashSet<String>,
) -> Result<(), String> {
    let Some(input) = v.get("input").and_then(|v| v.as_array()) else {
        return Ok(());
    };

    let mut has_function_call_output = false;
    let mut has_tool_call_context = false;
    for item in input {
        match item
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            "function_call_output" => has_function_call_output = true,
            "function_call" | "tool_call" => {
                if !item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
                {
                    has_tool_call_context = true;
                }
            }
            _ => {}
        }
        if has_function_call_output && has_tool_call_context {
            return Ok(());
        }
    }

    if !has_function_call_output || has_tool_call_context {
        return Ok(());
    }
    if !v
        .get("previous_response_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        return Ok(());
    }

    let mut call_ids = HashSet::<String>::new();
    let mut reference_ids = HashSet::<String>::new();
    let mut missing_call_id = false;
    for item in input {
        match item
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            "function_call_output" => {
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .trim();
                if call_id.is_empty() {
                    missing_call_id = true;
                } else {
                    call_ids.insert(call_id.to_string());
                }
            }
            "item_reference" => {
                let id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .trim();
                if !id.is_empty() {
                    reference_ids.insert(id.to_string());
                }
            }
            _ => {}
        }
    }

    if missing_call_id {
        return Err("function_call_output requires call_id on HTTP requests; continuation via previous_response_id requires a response id".to_string());
    }

    if !call_ids.is_empty()
        && call_ids
            .iter()
            .all(|call_id| reference_ids.contains(call_id) || known_call_ids.contains(call_id))
    {
        return Ok(());
    }

    Err("function_call_output requires matching function_call context, item_reference ids, or previous_response_id on HTTP requests".to_string())
}

fn update_responses_ws_session_from_event(
    session: &mut ResponsesWsSession,
    event: &serde_json::Value,
) {
    let event_type = event
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    match event_type {
        "response.created" | "response.completed" => {
            if let Some(response_id) = event
                .get("response")
                .and_then(|response| response.get("id"))
                .and_then(|value| value.as_str())
                .filter(|value| !value.trim().is_empty())
            {
                session.last_response_id = Some(response_id.to_string());
            }
            if let Some(output) = event
                .get("response")
                .and_then(|response| response.get("output"))
                .and_then(|value| value.as_array())
            {
                collect_tool_call_ids(output.iter(), &mut session.tool_call_ids);
            }
        }
        "response.output_item.added" | "response.output_item.done" => {
            if let Some(item) = event.get("item") {
                collect_tool_call_id(item, &mut session.tool_call_ids);
            }
        }
        _ => {}
    }
}

fn collect_tool_call_ids<'a>(
    items: impl IntoIterator<Item = &'a serde_json::Value>,
    ids: &mut HashSet<String>,
) {
    for item in items {
        collect_tool_call_id(item, ids);
    }
}

fn collect_tool_call_id(item: &serde_json::Value, ids: &mut HashSet<String>) {
    let item_type = item
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if item_type != "function_call" && item_type != "tool_call" {
        return;
    }
    if let Some(call_id) = item
        .get("call_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
    {
        ids.insert(call_id.to_string());
    }
}

async fn v1_responses_compact(State(state): State<AppState>, req: Request<Body>) -> Response {
    let passthrough_headers = extract_codex_passthrough_headers(req.headers());
    let raw = match axum::body::to_bytes(req.into_body(), 50 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return send_error(
                StatusCode::BAD_REQUEST,
                "读取请求体失败",
                "invalid_request_error",
            );
        }
    };

    let mut body_value: serde_json::Value = serde_json::from_slice(&raw)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
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

    tracing::info!(model = %model, stream, "received /v1/responses/compact request");

    let base_model = apply_thinking_to_value(&mut body_value, &model);
    let codex_body = clean_compact_value_to_vec(body_value, &base_model);

    let url = match state.codex_client.responses_compact_url() {
        Ok(u) => u,
        Err(err) => {
            return send_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("构建上游 URL 失败: {err}"),
                "server_error",
            );
        }
    };

    let endpoint = "/v1/responses/compact";
    let (upstream, account, attempts) = match state
        .codex_client
        .send_with_retry(
            &state.manager,
            &base_model,
            url,
            codex_body,
            stream,
            state.max_retry,
            passthrough_headers.as_ref(),
            state.on_401.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(err) => {
            log_upstream_request_error(endpoint, &base_model, stream, &err);
            return send_upstream_error(err);
        }
    };

    if stream {
        let headers = upstream.headers().clone();
        let status = upstream.status();
        let now_ms = crate::core::now_unix_ms();
        record_client_success(
            account.as_ref(),
            state.request_stats.as_ref(),
            state.runtime_state.as_ref(),
            now_ms,
        );
        log_request_completed(
            endpoint,
            &base_model,
            true,
            status,
            attempts,
            account.as_ref(),
        );
        return build_passthrough_sse_response(
            endpoint,
            base_model,
            upstream,
            account,
            state.runtime_state.clone(),
            headers,
        );
    }

    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => {
            log_response_read_failed(endpoint, &base_model, false, account.as_ref(), &err);
            return send_error(
                StatusCode::BAD_GATEWAY,
                &format!("读取上游响应失败: {err}"),
                "api_error",
            );
        }
    };
    let now_ms = crate::core::now_unix_ms();
    record_usage_from_json_bytes(
        account.as_ref(),
        state.runtime_state.as_ref(),
        now_ms,
        bytes.as_ref(),
    );
    record_client_success(
        account.as_ref(),
        state.request_stats.as_ref(),
        state.runtime_state.as_ref(),
        now_ms,
    );
    log_request_completed(
        endpoint,
        &base_model,
        false,
        StatusCode::OK,
        attempts,
        account.as_ref(),
    );

    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

fn clean_compact_value_to_vec(mut v: serde_json::Value, base_model: &str) -> Vec<u8> {
    {
        let obj = match v.as_object_mut() {
            Some(m) => m,
            None => {
                v = serde_json::Value::Object(Default::default());
                v.as_object_mut().unwrap()
            }
        };

        obj.insert(
            "model".to_string(),
            serde_json::Value::String(base_model.to_string()),
        );

        for key in [
            "stream",
            "stream_options",
            "parallel_tool_calls",
            "reasoning",
            "include",
            "previous_response_id",
            "prompt_cache_retention",
            "safety_identifier",
            "generate",
            "store",
            "reasoning_effort",
            "max_output_tokens",
            "max_completion_tokens",
            "temperature",
            "top_p",
            "truncation",
            "context_management",
            "user",
            "service_tier",
        ] {
            obj.remove(key);
        }
    }

    normalize_codex_instructions(&mut v);

    serde_json::to_vec(&v).unwrap_or_else(|_| b"{}".to_vec())
}

async fn v1_chat_completions(State(state): State<AppState>, req: Request<Body>) -> Response {
    let passthrough_headers = extract_codex_passthrough_headers(req.headers());
    let raw = match axum::body::to_bytes(req.into_body(), 50 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return send_error(
                StatusCode::BAD_REQUEST,
                "读取请求体失败",
                "invalid_request_error",
            );
        }
    };

    let mut body_value: serde_json::Value = serde_json::from_slice(&raw)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
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

    let reverse_tool_map = build_reverse_tool_name_map_from_value(&body_value);
    let base_model = apply_thinking_to_value(&mut body_value, &model);
    let codex_value = convert_openai_value_to_codex_value(&base_model, body_value, true);
    let codex_body = serde_json::to_vec(&codex_value).unwrap_or_else(|_| b"{}".to_vec());

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

    let endpoint = "/v1/chat/completions";
    if stream {
        let (upstream, account, attempts) = match state
            .codex_client
            .send_with_retry(
                &state.manager,
                &base_model,
                url,
                codex_body,
                true,
                state.max_retry,
                passthrough_headers.as_ref(),
                state.on_401.clone(),
            )
            .await
        {
            Ok(v) => v,
            Err(err) => {
                log_upstream_request_error(endpoint, &base_model, true, &err);
                return send_upstream_error(err);
            }
        };

        let status = upstream.status();
        log_request_completed(
            endpoint,
            &base_model,
            true,
            status,
            attempts,
            account.as_ref(),
        );
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
        let account = account.clone();
        let request_stats = state.request_stats.clone();
        let runtime_state = state.runtime_state.clone();
        let model_for_log = base_model.clone();
        tokio::spawn(async move {
            let mut buf = BytesMut::new();
            let mut state = StreamState::new(&base_model);
            let mut upstream_stream = upstream.bytes_stream();

            while let Some(chunk) = upstream_stream.next().await {
                let chunk = match chunk {
                    Ok(b) => b,
                    Err(err) => {
                        log_stream_read_failed(endpoint, &model_for_log, account.as_ref(), &err);
                        let _ = tx
                            .send(Err(std::io::Error::new(std::io::ErrorKind::Other, err)))
                            .await;
                        return;
                    }
                };

                buf.extend_from_slice(&chunk);
                while let Some(pos) = memchr(b'\n', buf.as_ref()) {
                    let mut line = buf.split_to(pos + 1);
                    line.truncate(pos);
                    let line = trim_ascii(line.as_ref());
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

            if state.completed && (state.has_text || state.has_tool_call || state.has_reasoning) {
                let now_ms = crate::core::now_unix_ms();
                let usage = UsageTokens {
                    input_tokens: state.usage_input,
                    output_tokens: state.usage_output,
                    cached_tokens: state.usage_cached,
                    reasoning_tokens: state.usage_reasoning,
                    total_tokens: state.usage_total,
                };
                if usage.has_activity() {
                    account.record_usage_detail(
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cached_tokens,
                        usage.reasoning_tokens,
                        usage.total_tokens,
                    );
                    record_hourly_usage(runtime_state.as_ref(), now_ms, usage);
                }
                record_client_success(
                    account.as_ref(),
                    request_stats.as_ref(),
                    runtime_state.as_ref(),
                    now_ms,
                );
                let _ = tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
            } else {
                log_stream_incomplete(
                    endpoint,
                    &model_for_log,
                    account.as_ref(),
                    "missing usable output",
                );
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

    let mut excluded_for_empty = HashSet::new();
    for empty_attempt in 0..=state.empty_retry_max {
        let (upstream, account, attempts) = match state
            .codex_client
            .send_with_retry_excluding(
                &state.manager,
                &base_model,
                url.clone(),
                codex_body.clone(),
                true,
                state.max_retry,
                passthrough_headers.as_ref(),
                state.on_401.clone(),
                &excluded_for_empty,
            )
            .await
        {
            Ok(v) => v,
            Err(err) => {
                log_upstream_request_error(endpoint, &base_model, false, &err);
                return send_upstream_error(err);
            }
        };

        let bytes = match upstream.bytes().await {
            Ok(b) => b,
            Err(err) => {
                log_response_read_failed(endpoint, &base_model, false, account.as_ref(), &err);
                return send_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("读取上游响应失败: {err}"),
                    "api_error",
                );
            }
        };

        let now_ms = crate::core::now_unix_ms();
        match parse_chat_non_stream_response(
            &bytes,
            &reverse_tool_map,
            account.as_ref(),
            state.runtime_state.as_ref(),
            now_ms,
        ) {
            ChatNonStreamOutcome::Success(out) => {
                record_client_success(
                    account.as_ref(),
                    state.request_stats.as_ref(),
                    state.runtime_state.as_ref(),
                    now_ms,
                );
                log_request_completed(
                    endpoint,
                    &base_model,
                    false,
                    StatusCode::OK,
                    attempts,
                    account.as_ref(),
                );
                let mut resp = Response::new(Body::from(out));
                *resp.status_mut() = StatusCode::OK;
                resp.headers_mut().insert(
                    header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/json"),
                );
                return resp;
            }
            ChatNonStreamOutcome::Empty => {
                excluded_for_empty.insert(account.file_path().to_string());
                if empty_attempt < state.empty_retry_max {
                    tracing::warn!(
                        account = account.file_path(),
                        attempt = empty_attempt + 1,
                        total = state.empty_retry_max + 1,
                        "chat non-stream empty response; retrying with another account"
                    );
                    continue;
                }
                return send_error(
                    StatusCode::BAD_REQUEST,
                    "empty response",
                    "invalid_response",
                );
            }
            ChatNonStreamOutcome::MissingCompleted => {
                tracing::warn!(
                    endpoint,
                    model = %base_model,
                    stream = false,
                    account = account.file_path(),
                    "chat non-stream response missing response.completed"
                );
                return send_error(
                    StatusCode::BAD_GATEWAY,
                    "上游响应缺少 response.completed",
                    "api_error",
                );
            }
        }
    }

    send_error(
        StatusCode::BAD_REQUEST,
        "empty response",
        "invalid_response",
    )
}

async fn v1_completions(State(state): State<AppState>, req: Request<Body>) -> Response {
    let passthrough_headers = extract_codex_passthrough_headers(req.headers());
    let raw = match axum::body::to_bytes(req.into_body(), 50 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return send_error(
                StatusCode::BAD_REQUEST,
                "读取请求体失败",
                "invalid_request_error",
            );
        }
    };

    let raw_value: serde_json::Value = serde_json::from_slice(&raw)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
    let mut body_value = convert_completions_request_to_chat_value(raw_value);
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

    tracing::info!(model = %model, stream, "received /v1/completions request");

    let reverse_tool_map = build_reverse_tool_name_map_from_value(&body_value);
    let base_model = apply_thinking_to_value(&mut body_value, &model);
    let codex_value = convert_openai_value_to_codex_value(&base_model, body_value, true);
    let codex_body = serde_json::to_vec(&codex_value).unwrap_or_else(|_| b"{}".to_vec());

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

    let endpoint = "/v1/completions";
    let (upstream, account, attempts) = match state
        .codex_client
        .send_with_retry(
            &state.manager,
            &base_model,
            url,
            codex_body,
            true,
            state.max_retry,
            passthrough_headers.as_ref(),
            state.on_401.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(err) => {
            log_upstream_request_error(endpoint, &base_model, stream, &err);
            return send_upstream_error(err);
        }
    };

    if stream {
        let status = upstream.status();
        log_request_completed(
            endpoint,
            &base_model,
            true,
            status,
            attempts,
            account.as_ref(),
        );
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
        let account = account.clone();
        let request_stats = state.request_stats.clone();
        let runtime_state = state.runtime_state.clone();
        let model_for_log = base_model.clone();
        tokio::spawn(async move {
            let mut buf = BytesMut::new();
            let mut state = StreamState::new(&base_model);
            let mut upstream_stream = upstream.bytes_stream();

            while let Some(chunk) = upstream_stream.next().await {
                let chunk = match chunk {
                    Ok(b) => b,
                    Err(err) => {
                        log_stream_read_failed(endpoint, &model_for_log, account.as_ref(), &err);
                        let _ = tx
                            .send(Err(std::io::Error::new(std::io::ErrorKind::Other, err)))
                            .await;
                        return;
                    }
                };

                buf.extend_from_slice(&chunk);
                while let Some(pos) = memchr(b'\n', buf.as_ref()) {
                    let mut line = buf.split_to(pos + 1);
                    line.truncate(pos);
                    let line = trim_ascii(line.as_ref());
                    if line.is_empty() {
                        continue;
                    }
                    let chunks = convert_stream_chunk(line, &mut state, &reverse_tool_map);
                    for chunk in chunks {
                        let completion_chunk =
                            convert_chat_completion_chunk_to_completion_chunk(&chunk);
                        let msg = format!("data: {completion_chunk}\n\n").into_bytes();
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

            if state.completed && (state.has_text || state.has_tool_call || state.has_reasoning) {
                let now_ms = crate::core::now_unix_ms();
                let usage = UsageTokens {
                    input_tokens: state.usage_input,
                    output_tokens: state.usage_output,
                    cached_tokens: state.usage_cached,
                    reasoning_tokens: state.usage_reasoning,
                    total_tokens: state.usage_total,
                };
                if usage.has_activity() {
                    account.record_usage_detail(
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cached_tokens,
                        usage.reasoning_tokens,
                        usage.total_tokens,
                    );
                    record_hourly_usage(runtime_state.as_ref(), now_ms, usage);
                }
                record_client_success(
                    account.as_ref(),
                    request_stats.as_ref(),
                    runtime_state.as_ref(),
                    now_ms,
                );
                let _ = tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
            } else {
                log_stream_incomplete(
                    endpoint,
                    &model_for_log,
                    account.as_ref(),
                    "missing usable output",
                );
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
            log_response_read_failed(endpoint, &base_model, false, account.as_ref(), &err);
            return send_error(
                StatusCode::BAD_GATEWAY,
                &format!("读取上游响应失败: {err}"),
                "api_error",
            );
        }
    };

    let now_ms = crate::core::now_unix_ms();
    match parse_chat_non_stream_response(
        &bytes,
        &reverse_tool_map,
        account.as_ref(),
        state.runtime_state.as_ref(),
        now_ms,
    ) {
        ChatNonStreamOutcome::Success(out) => {
            record_client_success(
                account.as_ref(),
                state.request_stats.as_ref(),
                state.runtime_state.as_ref(),
                now_ms,
            );
            log_request_completed(
                endpoint,
                &base_model,
                false,
                StatusCode::OK,
                attempts,
                account.as_ref(),
            );
            let mut resp = Response::new(Body::from(convert_chat_completion_to_completion(&out)));
            *resp.status_mut() = StatusCode::OK;
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            );
            resp
        }
        ChatNonStreamOutcome::Empty => send_error(
            StatusCode::BAD_REQUEST,
            "empty response",
            "invalid_response",
        ),
        ChatNonStreamOutcome::MissingCompleted => send_error(
            StatusCode::BAD_GATEWAY,
            "上游响应缺少 response.completed",
            "api_error",
        ),
    }
}

async fn v1_images_generations(State(state): State<AppState>, req: Request<Body>) -> Response {
    v1_images(state, req, false).await
}

async fn v1_images_edits(State(state): State<AppState>, req: Request<Body>) -> Response {
    v1_images(state, req, true).await
}

async fn v1_images(state: AppState, req: Request<Body>, edit: bool) -> Response {
    let passthrough_headers = extract_codex_passthrough_headers(req.headers());
    let raw_value = match parse_image_request_value(req).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let response_format = raw_value
        .get("response_format")
        .and_then(|value| value.as_str())
        .unwrap_or("url")
        .to_string();
    let mut body_value = convert_image_request_to_responses_value(raw_value, edit);
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

    let endpoint = if edit {
        "/v1/images/edits"
    } else {
        "/v1/images/generations"
    };
    tracing::info!(model = %model, endpoint, "received images request");

    let base_model = apply_thinking_to_value(&mut body_value, &model);
    let codex_value = convert_openai_value_to_codex_value(&base_model, body_value, true);
    let codex_body = serde_json::to_vec(&codex_value).unwrap_or_else(|_| b"{}".to_vec());

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

    let (upstream, account, attempts) = match state
        .codex_client
        .send_with_retry(
            &state.manager,
            &base_model,
            url,
            codex_body,
            true,
            state.max_retry,
            passthrough_headers.as_ref(),
            state.on_401.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(err) => {
            log_upstream_request_error(endpoint, &base_model, true, &err);
            return send_upstream_error(err);
        }
    };

    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => {
            log_response_read_failed(endpoint, &base_model, false, account.as_ref(), &err);
            return send_error(
                StatusCode::BAD_GATEWAY,
                &format!("读取上游响应失败: {err}"),
                "api_error",
            );
        }
    };

    let now_ms = crate::core::now_unix_ms();
    record_usage_from_sse_bytes(
        account.as_ref(),
        state.runtime_state.as_ref(),
        now_ms,
        &bytes,
    );
    let Some(out) = convert_responses_sse_to_images_json(&bytes, &response_format) else {
        return send_error(
            StatusCode::BAD_GATEWAY,
            "上游响应缺少 image_generation_call 输出",
            "api_error",
        );
    };

    record_client_success(
        account.as_ref(),
        state.request_stats.as_ref(),
        state.runtime_state.as_ref(),
        now_ms,
    );
    log_request_completed(
        endpoint,
        &base_model,
        false,
        StatusCode::OK,
        attempts,
        account.as_ref(),
    );

    let mut resp = Response::new(Body::from(out));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

async fn parse_image_request_value(req: Request<Body>) -> Result<serde_json::Value, Response> {
    let is_multipart = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .to_ascii_lowercase()
                .starts_with("multipart/form-data")
        });

    if is_multipart {
        return parse_multipart_image_request_value(req).await;
    }

    let raw = axum::body::to_bytes(req.into_body(), 50 * 1024 * 1024)
        .await
        .map_err(|_| {
            send_error(
                StatusCode::BAD_REQUEST,
                "读取请求体失败",
                "invalid_request_error",
            )
        })?;

    Ok(serde_json::from_slice(&raw)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default())))
}

async fn parse_multipart_image_request_value(
    mut req: Request<Body>,
) -> Result<serde_json::Value, Response> {
    DefaultBodyLimit::max(50 * 1024 * 1024).apply(&mut req);
    let mut multipart = Multipart::from_request(req, &()).await.map_err(|_| {
        send_error(
            StatusCode::BAD_REQUEST,
            "解析 multipart 请求失败",
            "invalid_request_error",
        )
    })?;
    let mut object = serde_json::Map::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(_) => {
                return Err(send_error(
                    StatusCode::BAD_REQUEST,
                    "读取 multipart 字段失败",
                    "invalid_request_error",
                ));
            }
        };
        let Some(name) = field.name().map(str::to_string) else {
            continue;
        };

        if name == "image" || name == "image[]" || name == "mask" {
            let content_type = field.content_type().map(str::to_string);
            let bytes = field.bytes().await.map_err(|_| {
                send_error(
                    StatusCode::BAD_REQUEST,
                    "读取 multipart 文件失败",
                    "invalid_request_error",
                )
            })?;
            let mime = content_type.unwrap_or_else(|| "image/png".to_string());
            let data_url = format!(
                "data:{};base64,{}",
                mime,
                base64::engine::general_purpose::STANDARD.encode(bytes)
            );
            let target_name = if name == "image[]" { "image" } else { &name };
            insert_multipart_value(
                &mut object,
                target_name,
                serde_json::Value::String(data_url),
            );
        } else {
            let text = field.text().await.map_err(|_| {
                send_error(
                    StatusCode::BAD_REQUEST,
                    "读取 multipart 文本字段失败",
                    "invalid_request_error",
                )
            })?;
            insert_multipart_value(&mut object, &name, parse_multipart_text_value(text));
        }
    }

    Ok(serde_json::Value::Object(object))
}

fn parse_multipart_text_value(text: String) -> serde_json::Value {
    let trimmed = text.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return value;
        }
    }
    serde_json::Value::String(text)
}

fn insert_multipart_value(
    object: &mut serde_json::Map<String, serde_json::Value>,
    name: &str,
    value: serde_json::Value,
) {
    match object.get_mut(name) {
        Some(existing) => match existing {
            serde_json::Value::Array(values) => values.push(value),
            _ => {
                let first = std::mem::replace(existing, serde_json::Value::Null);
                *existing = serde_json::Value::Array(vec![first, value]);
            }
        },
        None => {
            object.insert(name.to_string(), value);
        }
    }
}

#[derive(Debug)]
enum ChatNonStreamOutcome {
    Success(String),
    Empty,
    MissingCompleted,
}

fn parse_chat_non_stream_response(
    bytes: &[u8],
    reverse_tool_map: &std::collections::HashMap<String, String>,
    account: &crate::core::Account,
    runtime_state: &RuntimeStateStore,
    now_ms: i64,
) -> ChatNonStreamOutcome {
    let Some(completed_payload) = extract_completed_response_payload(bytes) else {
        return ChatNonStreamOutcome::MissingCompleted;
    };

    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&completed_payload) {
        if let Some(usage) = extract_usage_tokens(&v) {
            account.record_usage_detail(
                usage.input_tokens,
                usage.output_tokens,
                usage.cached_tokens,
                usage.reasoning_tokens,
                usage.total_tokens,
            );
            record_hourly_usage(runtime_state, now_ms, usage);
        }
    }

    let (out, has_output) = convert_non_stream_response(&completed_payload, reverse_tool_map);
    if has_output && !out.is_empty() {
        ChatNonStreamOutcome::Success(out)
    } else {
        ChatNonStreamOutcome::Empty
    }
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

struct ModelListEntry {
    base: &'static str,
    suffixes: &'static [&'static str],
}

const MODEL_LIST: &[ModelListEntry] = &[
    ModelListEntry {
        base: "gpt-5",
        suffixes: &["low", "medium", "high", "auto"],
    },
    ModelListEntry {
        base: "gpt-5-codex",
        suffixes: &["low", "medium", "high", "auto"],
    },
    ModelListEntry {
        base: "gpt-5-codex-mini",
        suffixes: &["low", "medium", "high", "auto"],
    },
    ModelListEntry {
        base: "gpt-5.1",
        suffixes: &["low", "medium", "high", "none", "auto"],
    },
    ModelListEntry {
        base: "gpt-5.1-codex",
        suffixes: &["low", "medium", "high", "max", "auto"],
    },
    ModelListEntry {
        base: "gpt-5.1-codex-mini",
        suffixes: &["low", "medium", "high", "auto"],
    },
    ModelListEntry {
        base: "gpt-5.1-codex-max",
        suffixes: &["low", "medium", "high", "xhigh", "auto"],
    },
    ModelListEntry {
        base: "gpt-5.2",
        suffixes: &["low", "medium", "high", "xhigh", "none", "auto"],
    },
    ModelListEntry {
        base: "gpt-5.2-codex",
        suffixes: &["low", "medium", "high", "xhigh", "auto"],
    },
    ModelListEntry {
        base: "gpt-5.3-codex",
        suffixes: &["low", "medium", "high", "xhigh", "none", "auto"],
    },
    ModelListEntry {
        base: "gpt-5.4",
        suffixes: &["low", "medium", "high", "xhigh", "none", "auto"],
    },
    ModelListEntry {
        base: "gpt-5.4-mini",
        suffixes: &["low", "medium", "high", "xhigh", "none", "auto"],
    },
    ModelListEntry {
        base: "gpt-5.5",
        suffixes: &["low", "medium", "high", "xhigh", "none", "auto"],
    },
];

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
    let mut capacity = 0usize;
    for entry in MODEL_LIST {
        capacity += 2 + entry.suffixes.len() * 2;
    }
    let mut data = Vec::with_capacity(capacity);

    for entry in MODEL_LIST {
        let base = entry.base;
        data.push(ModelItem {
            id: base.to_string(),
            object: "model",
            owned_by: "openai",
        });
        data.push(ModelItem {
            id: format!("{base}-fast"),
            object: "model",
            owned_by: "openai",
        });
        for suffix in entry.suffixes {
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

    Json(ModelsResponse {
        object: "list",
        data,
    })
}

async fn check_quota(
    State(state): State<AppState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.quota_checker.check_all_stream(state.manager);

    let stream = unfold(rx, |mut rx| async move {
        let evt = rx.recv().await?;
        let json = serde_json::to_string(&evt).unwrap_or_else(|_| "{}".to_string());
        Some((Ok(Event::default().event(evt.event_type).data(json)), rx))
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

async fn refresh(
    State(state): State<AppState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let rx = force_refresh_all_stream(
        state.manager,
        state.refresher,
        state.save_queue,
        state.quota_checker,
        state.refresh_concurrency,
    );

    let stream = unfold(rx, |mut rx| async move {
        let evt = rx.recv().await?;
        let json = serde_json::to_string(&evt).unwrap_or_else(|_| "{}".to_string());
        Some((Ok(Event::default().event(evt.event_type).data(json)), rx))
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        return format!("{ms}ms");
    }
    format!("{:.3}s", d.as_secs_f64())
}

fn force_refresh_all_stream(
    manager: Arc<Manager>,
    refresher: Refresher,
    save_queue: SaveQueue,
    quota_checker: Arc<QuotaChecker>,
    refresh_concurrency: usize,
) -> tokio::sync::mpsc::Receiver<crate::quota::ProgressEvent> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    let (tx, rx) = tokio::sync::mpsc::channel::<crate::quota::ProgressEvent>(100);

    tokio::spawn(async move {
        let accounts = manager.accounts_snapshot();
        let total = accounts.len();
        if total == 0 {
            let _ = tx
                .send(crate::quota::ProgressEvent {
                    event_type: "done",
                    email: None,
                    success: None,
                    message: Some("无账号".to_string()),
                    total: None,
                    success_count: None,
                    failed_count: None,
                    remaining: None,
                    duration: Some("0s".to_string()),
                    current: None,
                })
                .await;
            return;
        }

        let start = Instant::now();
        let sem = Arc::new(tokio::sync::Semaphore::new(refresh_concurrency.max(1)));
        let success_count = Arc::new(AtomicUsize::new(0));
        let fail_count = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(total);

        for acc in accounts.iter() {
            if tx.is_closed() {
                break;
            }

            let tx = tx.clone();
            let sem = sem.clone();
            let manager = manager.clone();
            let refresher = refresher.clone();
            let save_queue = save_queue.clone();
            let quota_checker = quota_checker.clone();
            let success_count = success_count.clone();
            let fail_count = fail_count.clone();
            let current = current.clone();
            let acc = acc.clone();

            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire_owned().await.unwrap();
                let email = acc.token().email.clone();

                // Go parity: force refresh regardless of expiry window.
                let ok = refresh_account(manager.as_ref(), &refresher, &save_queue, acc.clone(), 3)
                    .await
                    .is_ok();

                if ok {
                    let _ = quota_checker.check_one(acc).await;
                    success_count.fetch_add(1, Ordering::Relaxed);
                } else {
                    fail_count.fetch_add(1, Ordering::Relaxed);
                }

                let cur = current.fetch_add(1, Ordering::Relaxed) + 1;
                let _ = tx
                    .send(crate::quota::ProgressEvent {
                        event_type: "item",
                        email: Some(email),
                        success: Some(ok),
                        message: None,
                        total: Some(total),
                        success_count: None,
                        failed_count: None,
                        remaining: None,
                        duration: None,
                        current: Some(cur),
                    })
                    .await;
            }));
        }

        for h in handles {
            let _ = h.await;
        }

        let remaining = manager.account_count();
        let elapsed = start.elapsed();
        let sc = success_count.load(Ordering::Relaxed);
        let fc = fail_count.load(Ordering::Relaxed);

        let _ = tx
            .send(crate::quota::ProgressEvent {
                event_type: "done",
                email: None,
                success: None,
                message: Some("刷新完成".to_string()),
                total: Some(total),
                success_count: Some(sc),
                failed_count: Some(fc),
                remaining: Some(remaining),
                duration: Some(format_duration(elapsed)),
                current: None,
            })
            .await;
    });

    rx
}

#[derive(Debug, Serialize)]
struct StatsSummary {
    total: usize,
    active: usize,
    cooldown: usize,
    disabled: usize,
    rpm: i64,
    total_input_tokens: i64,
    total_output_tokens: i64,
    total_cached_tokens: i64,
    total_reasoning_tokens: i64,
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
struct StatsUsage {
    total_completions: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
    reasoning_tokens: i64,
    total_tokens: i64,
}

#[derive(Debug, Serialize)]
struct StatsTrendPoint {
    hour_start_ms: i64,
    hour_label: String,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
    reasoning_tokens: i64,
    total_tokens: i64,
}

#[derive(Debug, Serialize)]
struct StatsTrend {
    hourly: Vec<StatsTrendPoint>,
}

#[derive(Debug, Serialize)]
struct StatsAccount {
    file_path: String,
    email: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    plan_type: Option<String>,
    used_percent: f64,
    successful_requests: i64,
    failed_requests: i64,
    attempt_requests: i64,
    attempt_errors: i64,
    consecutive_failures: i64,
    last_used_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_used_at: Option<String>,
    cooldown_until_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_refreshed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cooldown_until: Option<String>,
    quota_exhausted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota_resets_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_expire: Option<String>,
    usage: StatsUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    quota: Option<StatsQuota>,
}

#[derive(Debug, Serialize)]
struct StatsResponse {
    summary: StatsSummary,
    trend: StatsTrend,
    accounts: Vec<StatsAccount>,
}

async fn stats(State(state): State<AppState>) -> Json<StatsResponse> {
    let accounts = state.manager.accounts_snapshot();
    let now_ms = crate::core::now_unix_ms();

    let mut out = Vec::with_capacity(accounts.len());
    let mut active = 0usize;
    let mut cooldown = 0usize;
    let mut disabled = 0usize;
    let mut total_input_tokens = 0i64;
    let mut total_output_tokens = 0i64;
    let mut total_cached_tokens = 0i64;
    let mut total_reasoning_tokens = 0i64;

    for acc in accounts.iter() {
        let snap = acc.stats_snapshot();
        match snap.status {
            crate::core::AccountStatus::Active => active += 1,
            crate::core::AccountStatus::Cooldown => cooldown += 1,
            crate::core::AccountStatus::Disabled => disabled += 1,
        }
        total_input_tokens += snap.usage_input_tokens;
        total_output_tokens += snap.usage_output_tokens;
        total_cached_tokens += snap.usage_cached_tokens;
        total_reasoning_tokens += snap.usage_reasoning_tokens;

        fn ms_to_rfc3339(ms: i64) -> Option<String> {
            if ms <= 0 {
                return None;
            }
            let dt = time::OffsetDateTime::from_unix_timestamp_nanos(
                (ms as i128).saturating_mul(1_000_000),
            )
            .ok()?;
            dt.format(&time::format_description::well_known::Rfc3339)
                .ok()
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

        let mut quota_exhausted = snap.quota_exhausted;
        if quota_exhausted && snap.quota_resets_at_ms > 0 && now_ms >= snap.quota_resets_at_ms {
            quota_exhausted = false;
        }

        out.push(StatsAccount {
            file_path: snap.file_path,
            email: snap.email,
            status: snap.status.as_str().to_string(),
            plan_type: if snap.plan_type.is_empty() {
                None
            } else {
                Some(snap.plan_type)
            },
            used_percent: snap.used_percent,
            successful_requests: snap.successful_requests,
            failed_requests: snap.failed_requests,
            attempt_requests: snap.total_requests,
            attempt_errors: snap.total_errors,
            consecutive_failures: snap.consecutive_failures,
            last_used_ms: snap.last_used_ms,
            last_used_at: ms_to_rfc3339(snap.last_used_ms),
            cooldown_until_ms: snap.cooldown_until_ms,
            last_refreshed_at: ms_to_rfc3339(snap.last_refreshed_ms),
            cooldown_until: ms_to_rfc3339(snap.cooldown_until_ms),
            quota_exhausted,
            quota_resets_at: if quota_exhausted {
                ms_to_rfc3339(snap.quota_resets_at_ms)
            } else {
                None
            },
            token_expire: if snap.token_expire.is_empty() {
                None
            } else {
                Some(snap.token_expire)
            },
            usage: StatsUsage {
                total_completions: snap.usage_total_completions,
                input_tokens: snap.usage_input_tokens,
                output_tokens: snap.usage_output_tokens,
                cached_tokens: snap.usage_cached_tokens,
                reasoning_tokens: snap.usage_reasoning_tokens,
                total_tokens: snap.usage_total_tokens,
            },
            quota,
        });
    }

    fn hour_label(ms: i64) -> String {
        if ms <= 0 {
            return String::new();
        }
        let Ok(dt) = time::OffsetDateTime::from_unix_timestamp_nanos((ms as i128) * 1_000_000)
        else {
            return String::new();
        };
        let Ok(fmt) = time::format_description::parse("[month]-[day] [hour]:00") else {
            return String::new();
        };
        dt.format(&fmt).unwrap_or_else(|_| String::new())
    }

    let trend = StatsTrend {
        hourly: state
            .runtime_state
            .hourly_trend()
            .into_iter()
            .map(|point| StatsTrendPoint {
                hour_start_ms: point.hour_start_ms,
                hour_label: hour_label(point.hour_start_ms),
                requests: point.requests,
                input_tokens: point.input_tokens,
                output_tokens: point.output_tokens,
                cached_tokens: point.cached_tokens,
                reasoning_tokens: point.reasoning_tokens,
                total_tokens: point.total_tokens,
            })
            .collect(),
    };

    Json(StatsResponse {
        summary: StatsSummary {
            total: accounts.len(),
            active,
            cooldown,
            disabled,
            rpm: state.request_stats.rpm(),
            total_input_tokens,
            total_output_tokens,
            total_cached_tokens,
            total_reasoning_tokens,
        },
        trend,
        accounts: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Account, TokenData};
    use crate::state::RuntimeStateStore;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::{Value, json};
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
            request_stats: Arc::new(RequestStats::default()),
            api_keys: Arc::new(HashSet::new()),
            max_retry: 0,
            empty_retry_max: 0,
            refresher: Refresher::new("").unwrap(),
            save_queue: SaveQueue::start(1),
            refresh_concurrency: 1,
            runtime_state: Arc::new(RuntimeStateStore::new(dir.path())),
            on_401: None,
        };

        let app = router(state);
        let res = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
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
            request_stats: Arc::new(RequestStats::default()),
            api_keys: Arc::new(HashSet::new()),
            max_retry: 0,
            empty_retry_max: 0,
            refresher: Refresher::new("").unwrap(),
            save_queue: SaveQueue::start(1),
            refresh_concurrency: 1,
            runtime_state: Arc::new(RuntimeStateStore::new(dir.path())),
            on_401: None,
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
            body.contains("event: done"),
            "expected event: done framing, got body: {body}"
        );
        assert!(
            body.contains("\"type\":\"done\""),
            "expected done payload, got body: {body}"
        );
    }

    #[test]
    fn api_clean_compact_value_to_vec_normalizes_null_instructions() {
        let body = json!({
            "instructions": null,
            "stream": true,
            "stream_options": {"include_usage": true}
        });

        let out = clean_compact_value_to_vec(body, "gpt-5.4");
        let value: Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(value["model"], "gpt-5.4");
        assert_eq!(value["instructions"], "");
        assert!(value.get("stream").is_none());
        assert!(value.get("stream_options").is_none());
    }

    #[test]
    fn api_parse_chat_non_stream_response_uses_output_item_done_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime_state = RuntimeStateStore::new(dir.path());
        let account = Account::new(
            "a.json".to_string(),
            TokenData {
                id_token: String::new(),
                access_token: "at".to_string(),
                refresh_token: "rt".to_string(),
                account_id: String::new(),
                email: "a@example.com".to_string(),
                expired: String::new(),
                plan_type: String::new(),
            },
        );
        let raw = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"model\":\"gpt-5.4\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"model\":\"gpt-5.4\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2,\"total_tokens\":3}}}\n\n",
        );

        let reverse = std::collections::HashMap::new();
        let out =
            parse_chat_non_stream_response(raw.as_bytes(), &reverse, &account, &runtime_state, 1);

        match out {
            ChatNonStreamOutcome::Success(json) => {
                let value: Value = serde_json::from_str(&json).unwrap();
                assert_eq!(value["choices"][0]["message"]["content"], "hi");
                assert_eq!(value["usage"]["completion_tokens"], 2);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn api_build_responses_ws_request_append_reuses_previous_model_and_replaces_input() {
        let mut session = ResponsesWsSession::default();

        let create = json!({
            "type": "response.create",
            "response": {
                "model": "gpt-5.4",
                "input": "hi",
                "instructions": "keep"
            }
        });
        let (create_body, create_model) =
            build_responses_ws_request(&create, &mut session).expect("create request");
        let create_value: Value = serde_json::from_slice(&create_body).unwrap();
        assert_eq!(create_model, "gpt-5.4");
        assert_eq!(create_value["model"], "gpt-5.4");
        assert_eq!(create_value["input"], "hi");
        assert_eq!(create_value["stream"], true);

        let append = json!({
            "type": "response.append",
            "response": {
                "input": "next turn"
            }
        });
        let (append_body, append_model) =
            build_responses_ws_request(&append, &mut session).expect("append request");
        let append_value: Value = serde_json::from_slice(&append_body).unwrap();
        assert_eq!(append_model, "gpt-5.4");
        assert_eq!(append_value["model"], "gpt-5.4");
        assert_eq!(append_value["input"], "next turn");
        assert_eq!(append_value["instructions"], "keep");
        assert_eq!(append_value["stream"], true);
    }

    #[test]
    fn api_build_responses_ws_request_append_adds_previous_response_id() {
        let mut session = ResponsesWsSession::default();

        let create = json!({
            "type": "response.create",
            "response": {
                "model": "gpt-5.4",
                "input": "hi"
            }
        });
        build_responses_ws_request(&create, &mut session).expect("create request");
        update_responses_ws_session_from_event(
            &mut session,
            &json!({"type":"response.created","response":{"id":"resp_1"}}),
        );

        let append = json!({
            "type": "response.append",
            "response": {
                "input": "next turn"
            }
        });
        let (append_body, _) =
            build_responses_ws_request(&append, &mut session).expect("append request");
        let append_value: Value = serde_json::from_slice(&append_body).unwrap();

        assert_eq!(append_value["previous_response_id"], "resp_1");
    }

    #[test]
    fn api_build_responses_ws_request_append_allows_known_function_call_output() {
        let mut session = ResponsesWsSession::default();
        session.tool_call_ids.insert("call_1".to_string());

        let create = json!({
            "type": "response.create",
            "response": {
                "model": "gpt-5.4",
                "input": "hi"
            }
        });
        build_responses_ws_request(&create, &mut session).expect("create request");

        let append = json!({
            "type": "response.append",
            "response": {
                "input": [
                    {"type":"function_call_output","call_id":"call_1","output":"ok"}
                ]
            }
        });

        let (append_body, _) =
            build_responses_ws_request(&append, &mut session).expect("append request");
        let append_value: Value = serde_json::from_slice(&append_body).unwrap();
        assert_eq!(append_value["input"][0]["type"], "function_call_output");
    }

    #[test]
    fn api_build_responses_ws_request_append_rejects_unknown_function_call_output() {
        let mut session = ResponsesWsSession::default();

        let create = json!({
            "type": "response.create",
            "response": {
                "model": "gpt-5.4",
                "input": "hi"
            }
        });
        build_responses_ws_request(&create, &mut session).expect("create request");

        let append = json!({
            "type": "response.append",
            "response": {
                "input": [
                    {"type":"function_call_output","call_id":"call_1","output":"ok"}
                ]
            }
        });

        let err = build_responses_ws_request(&append, &mut session).unwrap_err();
        assert!(err.contains("function_call_output"));
    }

    #[test]
    fn api_update_responses_ws_session_tracks_response_and_tool_call_ids() {
        let mut session = ResponsesWsSession::default();
        update_responses_ws_session_from_event(
            &mut session,
            &json!({
                "type":"response.completed",
                "response":{
                    "id":"resp_1",
                    "output":[{"type":"function_call","call_id":"call_1"}]
                }
            }),
        );

        assert_eq!(session.last_response_id.as_deref(), Some("resp_1"));
        assert!(session.tool_call_ids.contains("call_1"));
    }

    #[test]
    fn api_preview_body_for_log_truncates_long_payloads() {
        let preview = preview_body_for_log("你好abcdef".as_bytes(), 4);
        assert_eq!(preview, "你好ab...");
    }
}
