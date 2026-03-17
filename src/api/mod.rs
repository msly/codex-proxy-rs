use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::{CloseCode, CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::http::header;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use futures_util::stream::unfold;
use serde::Serialize;
use serde_json::json;

use crate::core::Manager;
use crate::quota::QuotaChecker;
use crate::refresh::{Refresher, SaveQueue, refresh_account};
use crate::thinking::apply_thinking;
use crate::translate::{
    ClaudeStreamState, StreamState, build_reverse_tool_name_map, convert_claude_request_to_openai,
    convert_codex_full_sse_to_claude_response_with_meta, convert_codex_stream_to_claude_events,
    convert_non_stream_response, convert_openai_request_to_codex, convert_stream_chunk,
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
    pub refresher: Refresher,
    pub save_queue: SaveQueue,
    pub refresh_concurrency: usize,
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
        )
        .await
    {
        Ok(v) => v,
        Err(err) => return send_upstream_error(err),
    };

    account.record_success(crate::core::now_unix_ms());

    if stream {
        let status = upstream.status();
        let stream = upstream
            .bytes_stream()
            .map(|chunk| chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)));

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
                            Ok(()) => return,
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

    account.record_success(crate::core::now_unix_ms());
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
        )
        .await
    {
        Ok(v) => v,
        Err(err) => return send_claude_upstream_error(err),
    };

    account.record_success(crate::core::now_unix_ms());

    if stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
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
        )
        .await
    {
        Ok(v) => v,
        Err(err) => return send_upstream_error(err),
    };

    account.record_success(crate::core::now_unix_ms());

    if stream {
        let status = upstream.status();
        let headers = upstream.headers().clone();
        let stream = upstream
            .bytes_stream()
            .map(|chunk| chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e)));

        let mut resp = Response::new(Body::from_stream(stream));
        *resp.status_mut() = status;
        for (k, v) in headers.iter() {
            resp.headers_mut().append(k.clone(), v.clone());
        }
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
            return send_error(
                StatusCode::BAD_REQUEST,
                "empty response",
                "invalid_response",
            );
        }

        let mut resp = Response::new(Body::from(out));
        *resp.status_mut() = StatusCode::OK;
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        return resp;
    }

    send_error(
        StatusCode::BAD_GATEWAY,
        "上游响应缺少 response.completed",
        "api_error",
    )
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
        Some((Ok(Event::default().data(json)), rx))
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
        Some((Ok(Event::default().data(json)), rx))
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
            refresher: Refresher::new("").unwrap(),
            save_queue: SaveQueue::start(1),
            refresh_concurrency: 1,
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
            api_keys: Arc::new(HashSet::new()),
            max_retry: 0,
            refresher: Refresher::new("").unwrap(),
            save_queue: SaveQueue::start(1),
            refresh_concurrency: 1,
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
