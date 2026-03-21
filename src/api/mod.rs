use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::{CloseCode, CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::http::header;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Html;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use futures_util::stream::unfold;
use serde::Serialize;
use serde_json::json;
use tower_http::compression::CompressionLayer;

use crate::core::{Account, Manager};
use crate::quota::QuotaChecker;
use crate::refresh::{Refresher, SaveQueue, refresh_account};
use crate::state::RuntimeStateStore;
use crate::thinking::apply_thinking;
use crate::translate::{
    ClaudeStreamState, StreamState, build_reverse_tool_name_map, convert_claude_request_to_openai,
    convert_codex_full_sse_to_claude_response_with_meta, convert_codex_stream_to_claude_events,
    convert_non_stream_response, convert_openai_request_to_codex, convert_stream_chunk,
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

fn record_hourly_request(runtime_state: &RuntimeStateStore, now_ms: i64) {
    runtime_state.record_hourly_request(now_ms);
}

fn record_hourly_usage(
    runtime_state: &RuntimeStateStore,
    now_ms: i64,
    input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
) {
    if input_tokens > 0 || output_tokens > 0 {
        runtime_state.record_hourly_usage(now_ms, input_tokens, output_tokens, total_tokens);
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

fn extract_usage_tokens(value: &serde_json::Value) -> Option<(i64, i64, i64)> {
    let usage = value
        .get("usage")
        .or_else(|| value.get("response").and_then(|response| response.get("usage")))?;
    let input_tokens = usage
        .get("input_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens))
        .max(0);

    if input_tokens == 0 && output_tokens == 0 && total_tokens == 0 {
        return None;
    }

    Some((input_tokens, output_tokens, total_tokens))
}

fn record_usage_from_value(
    account: &Account,
    runtime_state: &RuntimeStateStore,
    now_ms: i64,
    value: &serde_json::Value,
) {
    let Some((input_tokens, output_tokens, total_tokens)) = extract_usage_tokens(value) else {
        return;
    };
    account.record_usage(input_tokens, output_tokens, total_tokens);
    record_hourly_usage(
        runtime_state,
        now_ms,
        input_tokens,
        output_tokens,
        total_tokens,
    );
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

fn build_passthrough_sse_response(
    upstream: reqwest::Response,
    account: Arc<Account>,
    runtime_state: Arc<RuntimeStateStore>,
    headers: HeaderMap,
) -> Response {
    let status = upstream.status();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);

    tokio::spawn(async move {
        let mut upstream_stream = upstream.bytes_stream();
        let mut buf = Vec::<u8>::new();
        let mut recorded_usage = false;

        while let Some(chunk) = upstream_stream.next().await {
            let chunk = match chunk {
                Ok(b) => b,
                Err(err) => {
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
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line = buf.drain(..pos).collect::<Vec<u8>>();
                let _ = buf.drain(..1);
                let line = trim_ascii(&line);
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
            let line = trim_ascii(&buf);
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
        .route("/check-quota", post(check_quota))
        .route_layer(middleware::from_fn_with_state(state.clone(), api_key_auth));

    let v1 = Router::new()
        .route("/responses", post(v1_responses).get(v1_responses_ws))
        .route("/responses/compact", post(v1_responses_compact))
        .route("/models", get(v1_models))
        .route("/chat/completions", post(v1_chat_completions))
        .route("/messages", post(v1_messages))
        .route_layer(middleware::from_fn_with_state(state.clone(), api_key_auth));

    let non_v1 = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .merge(mgmt)
        .layer(CompressionLayer::new());

    Router::new()
        .merge(non_v1)
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

async fn v1_responses(State(state): State<AppState>, req: Request<Body>) -> Response {
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

    let body_value: serde_json::Value = serde_json::from_slice(&raw)
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
            state.on_401.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(err) => return send_upstream_error(err),
    };

    if stream {
        let now_ms = crate::core::now_unix_ms();
        record_client_success(
            account.as_ref(),
            state.request_stats.as_ref(),
            state.runtime_state.as_ref(),
            now_ms,
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

    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

async fn v1_responses_ws(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| handle_responses_ws(socket, state))
        .into_response()
}

async fn handle_responses_ws(mut socket: WebSocket, state: AppState) {
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
                    "response.create" => {
                        let response_value = match value.get("response") {
                            Some(v) => v.clone(),
                            None => {
                                write_ws_error(
                                    &mut socket,
                                    "invalid_request_error",
                                    "缺少 response 字段",
                                )
                                .await;
                                continue;
                            }
                        };

                        let mut request_value = response_value;
                        match request_value.as_object_mut() {
                            Some(obj) => {
                                obj.insert("stream".to_string(), serde_json::Value::Bool(true));
                            }
                            None => {
                                write_ws_error(
                                    &mut socket,
                                    "invalid_request_error",
                                    "response 必须是对象",
                                )
                                .await;
                                continue;
                            }
                        }

                        let model = request_value
                            .get("model")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        if model.trim().is_empty() {
                            write_ws_error(&mut socket, "invalid_request_error", "缺少 model 字段")
                                .await;
                            continue;
                        }

                        let request_body = match serde_json::to_vec(&request_value) {
                            Ok(b) => b,
                            Err(_) => {
                                write_ws_error(
                                    &mut socket,
                                    "invalid_request_error",
                                    "序列化请求失败",
                                )
                                .await;
                                continue;
                            }
                        };

                        let stream_result =
                            forward_responses_sse_as_ws(&mut socket, &state, request_body, &model)
                                .await;
                        match stream_result {
                            Ok(()) => {
                                state.request_stats.record_request();
                                return;
                            }
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

async fn forward_responses_sse_as_ws(
    socket: &mut WebSocket,
    state: &AppState,
    request_body: Vec<u8>,
    model: &str,
) -> Result<(), ResponsesWsError> {
    tracing::info!(model = %model, "responses ws: fallback to HTTP/SSE forwarding");

    let (body, base_model) = apply_thinking(&request_body, model);
    let codex_body = convert_openai_request_to_codex(&base_model, &body, true);

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
            state.on_401.clone(),
        )
        .await
        .map_err(ResponsesWsError::Upstream)?;

    let mut has_text = false;
    let mut has_tool = false;
    let mut buf = Vec::<u8>::new();
    let mut upstream_stream = upstream.bytes_stream();

    while let Some(chunk) = upstream_stream.next().await {
        let chunk = chunk.map_err(|e| ResponsesWsError::Local(format!("读取上游响应失败: {e}")))?;
        buf.extend_from_slice(&chunk);

        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line = buf.drain(..pos).collect::<Vec<u8>>();
            let _ = buf.drain(..1);
            let line = trim_ascii(&line);
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

            if !has_text || !has_tool {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) {
                    if let Some(typ) = v.get("type").and_then(|v| v.as_str()) {
                        match typ {
                            "response.output_text.delta" => {
                                if v.get("delta").and_then(|v| v.as_str()).unwrap_or_default() != ""
                                {
                                    has_text = true;
                                }
                            }
                            "response.output_item.added"
                            | "response.function_call_arguments.delta"
                            | "response.function_call_arguments.done"
                            | "response.output_item.done" => {
                                has_tool = true;
                            }
                            _ => {}
                        }
                    }
                }
            }

            if socket
                .send(Message::Text(
                    String::from_utf8_lossy(payload).into_owned().into(),
                ))
                .await
                .is_err()
            {
                return Ok(());
            }
        }
    }

    if !has_text && !has_tool {
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

    let (body, base_model) = apply_thinking(&openai_body, &model);
    let codex_body = convert_openai_request_to_codex(&base_model, &body, true);

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

    let (upstream, account, _attempts) = match state
        .codex_client
        .send_with_retry(
            &state.manager,
            &base_model,
            url,
            codex_body,
            true,
            state.max_retry,
            state.on_401.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(err) => return send_claude_upstream_error(err),
    };

    if stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
        let request_stats = state.request_stats.clone();
        let runtime_state = state.runtime_state.clone();
        tokio::spawn(async move {
            let mut buf = Vec::<u8>::new();
            let mut state = ClaudeStreamState::new(&base_model);
            let mut upstream_stream = upstream.bytes_stream();

            while let Some(chunk) = upstream_stream.next().await {
                let chunk = match chunk {
                    Ok(b) => b,
                    Err(err) => {
                        let _ = tx
                            .send(Err(std::io::Error::new(std::io::ErrorKind::Other, err)))
                            .await;
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
            return send_claude_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("读取上游响应失败: {err}"),
            );
        }
    };

    let result = convert_codex_full_sse_to_claude_response_with_meta(&bytes, &base_model);
    if !result.found_completed || result.json.is_empty() {
        return send_claude_error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "未收到 response.completed 事件",
        );
    }
    if !result.has_text && !result.has_tool_use {
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

    let mut resp = Response::new(Body::from(result.json));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

async fn v1_responses_compact(State(state): State<AppState>, req: Request<Body>) -> Response {
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

    let body_value: serde_json::Value = serde_json::from_slice(&raw)
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

    let (body, base_model) = apply_thinking(&raw, &model);
    let codex_body = clean_compact_body(&body, &base_model);

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

    let (upstream, account, _attempts) = match state
        .codex_client
        .send_with_retry(
            &state.manager,
            &base_model,
            url,
            codex_body,
            stream,
            state.max_retry,
            state.on_401.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(err) => return send_upstream_error(err),
    };

    if stream {
        let headers = upstream.headers().clone();
        let now_ms = crate::core::now_unix_ms();
        record_client_success(
            account.as_ref(),
            state.request_stats.as_ref(),
            state.runtime_state.as_ref(),
            now_ms,
        );
        return build_passthrough_sse_response(
            upstream,
            account,
            state.runtime_state.clone(),
            headers,
        );
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

    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

fn clean_compact_body(raw: &[u8], base_model: &str) -> Vec<u8> {
    let mut v: serde_json::Value = serde_json::from_slice(raw)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));

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

    if !obj.contains_key("instructions") {
        obj.insert(
            "instructions".to_string(),
            serde_json::Value::String(String::new()),
        );
    }

    serde_json::to_vec(&v).unwrap_or_else(|_| b"{}".to_vec())
}

async fn v1_chat_completions(State(state): State<AppState>, req: Request<Body>) -> Response {
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

    let body_value: serde_json::Value = serde_json::from_slice(&raw)
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

    let reverse_tool_map = build_reverse_tool_name_map(&raw);

    if stream {
        let (upstream, account, _attempts) = match state
            .codex_client
            .send_with_retry(
                &state.manager,
                &base_model,
                url,
                codex_body,
                true,
                state.max_retry,
                state.on_401.clone(),
            )
            .await
        {
            Ok(v) => v,
            Err(err) => return send_upstream_error(err),
        };

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
        let account = account.clone();
        let request_stats = state.request_stats.clone();
        let runtime_state = state.runtime_state.clone();
        tokio::spawn(async move {
            let mut buf = Vec::<u8>::new();
            let mut state = StreamState::new(&base_model);
            let mut upstream_stream = upstream.bytes_stream();

            while let Some(chunk) = upstream_stream.next().await {
                let chunk = match chunk {
                    Ok(b) => b,
                    Err(err) => {
                        let _ = tx
                            .send(Err(std::io::Error::new(std::io::ErrorKind::Other, err)))
                            .await;
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

            if state.completed && (state.has_text || state.has_tool_call || state.has_reasoning) {
                let now_ms = crate::core::now_unix_ms();
                if state.usage_input > 0 || state.usage_output > 0 {
                    account.record_usage(state.usage_input, state.usage_output, state.usage_total);
                    record_hourly_usage(
                        runtime_state.as_ref(),
                        now_ms,
                        state.usage_input,
                        state.usage_output,
                        state.usage_total,
                    );
                }
                record_client_success(
                    account.as_ref(),
                    request_stats.as_ref(),
                    runtime_state.as_ref(),
                    now_ms,
                );
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

    let mut excluded_for_empty = HashSet::new();
    for empty_attempt in 0..=state.empty_retry_max {
        let (upstream, account, _attempts) = match state
            .codex_client
            .send_with_retry_excluding(
                &state.manager,
                &base_model,
                url.clone(),
                codex_body.clone(),
                true,
                state.max_retry,
                state.on_401.clone(),
                &excluded_for_empty,
            )
            .await
        {
            Ok(v) => v,
            Err(err) => return send_upstream_error(err),
        };

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
    for line in bytes.split(|&b| b == b'\n') {
        let line = trim_ascii(line);
        if !line.starts_with(b"data:") {
            continue;
        }
        let payload = trim_ascii(&line[5..]);
        if payload.is_empty() || payload == b"[DONE]" {
            continue;
        }

        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload) {
            if v.get("type").and_then(|v| v.as_str()) != Some("response.completed") {
                continue;
            }

            let usage = v.get("response").and_then(|r| r.get("usage"));
            let input_tokens = usage
                .and_then(|u| u.get("input_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let output_tokens = usage
                .and_then(|u| u.get("output_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let total_tokens = usage
                .and_then(|u| u.get("total_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            if input_tokens > 0 || output_tokens > 0 {
                account.record_usage(input_tokens, output_tokens, total_tokens);
                record_hourly_usage(
                    runtime_state,
                    now_ms,
                    input_tokens,
                    output_tokens,
                    total_tokens,
                );
            }
        }

        let (out, has_output) = convert_non_stream_response(payload, reverse_tool_map);
        if has_output && !out.is_empty() {
            return ChatNonStreamOutcome::Success(out);
        }
        return ChatNonStreamOutcome::Empty;
    }

    ChatNonStreamOutcome::MissingCompleted
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

        for acc in accounts.iter() {
            acc.set_active();
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
    total_tokens: i64,
}

#[derive(Debug, Serialize)]
struct StatsTrendPoint {
    hour_start_ms: i64,
    hour_label: String,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
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

    for acc in accounts.iter() {
        let snap = acc.stats_snapshot();
        match snap.status {
            crate::core::AccountStatus::Active => active += 1,
            crate::core::AccountStatus::Cooldown => cooldown += 1,
            crate::core::AccountStatus::Disabled => disabled += 1,
        }
        total_input_tokens += snap.usage_input_tokens;
        total_output_tokens += snap.usage_output_tokens;

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
        },
        trend,
        accounts: out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RuntimeStateStore;
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
}
