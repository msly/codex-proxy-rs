use std::collections::{HashMap, HashSet};

use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::{CloseCode, CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::http::HeaderMap;
use axum::http::header;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::SinkExt;
use futures_util::StreamExt;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message as UpstreamWsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::core::Account;
use crate::state::RuntimeStateStore;
use crate::thinking::apply::apply_thinking_to_value;
use crate::translate::request::{
    convert_openai_value_to_codex_value, normalize_codex_instructions,
};
use crate::upstream::codex::UpstreamError;

use super::{
    AppState, api_key_from_req, bind_response_account_from_json_bytes,
    bind_response_account_from_value, build_passthrough_sse_response, execute_codex_request,
    extract_codex_passthrough_headers, log_request_completed, log_response_read_failed,
    log_upstream_request_error, parse_stream_field, previous_response_id_from_value,
    record_client_success, record_persist_account_error, record_persist_error,
    record_persist_request, record_persist_usage, record_usage_from_json_bytes,
    request_limit_guard_from_req, send_error, send_upstream_error, sse, trim_ascii,
    validate_function_call_output_context, validate_function_call_output_context_with_known_ids,
};

pub(super) async fn v1_responses(State(state): State<AppState>, req: Request<Body>) -> Response {
    let api_key = api_key_from_req(&req);
    let request_limit_guard = request_limit_guard_from_req(&req);
    let started_ms = crate::core::now_unix_ms();
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
    let stream = match parse_stream_field(&body_value) {
        Ok(stream) => stream,
        Err(message) => {
            return send_error(StatusCode::BAD_REQUEST, message, "invalid_request_error");
        }
    };
    if body_value.get("input").is_none() {
        return send_error(
            StatusCode::BAD_REQUEST,
            "缺少 input 字段",
            "invalid_request_error",
        );
    }

    tracing::info!(model = %model, stream, "received /v1/responses request");

    if let Err(message) = validate_function_call_output_context(&body_value) {
        return send_error(StatusCode::BAD_REQUEST, &message, "invalid_request_error");
    }

    let base_model = apply_thinking_to_value(&mut body_value, &model);
    let codex_value = convert_openai_value_to_codex_value(&base_model, body_value, stream);
    let session_keys = session_keys_from_request(passthrough_headers.as_ref(), &codex_value);
    let initial_excluded = sticky_initial_excluded_for_request(
        &state,
        previous_response_id_from_value(&codex_value),
        &session_keys,
    );
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
    let upstream_result = match execute_codex_request(
        &state,
        &base_model,
        url,
        codex_body,
        stream,
        passthrough_headers.as_ref(),
        &initial_excluded,
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            log_upstream_request_error(endpoint, &base_model, stream, &err);
            record_persist_error(
                state.persist_store.as_ref(),
                endpoint,
                &base_model,
                stream,
                StatusCode::BAD_GATEWAY,
                api_key,
                err.to_string(),
                crate::core::now_unix_ms().saturating_sub(started_ms),
            );
            return send_upstream_error(err);
        }
    };
    let upstream = upstream_result.response;
    let account = upstream_result.account;
    let attempts = upstream_result.attempts;
    let account_limit_guard = upstream_result.account_limit_guard;

    if stream {
        let status = upstream.status();
        let now_ms = crate::core::now_unix_ms();
        bind_session_accounts(
            state.runtime_state.as_ref(),
            account.as_ref(),
            &session_keys,
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
            true,
            status,
            attempts,
            account.as_ref(),
        );
        record_persist_request(
            state.persist_store.as_ref(),
            endpoint,
            &base_model,
            true,
            status,
            attempts,
            api_key.clone(),
            Some(account.as_ref()),
            crate::core::now_unix_ms().saturating_sub(started_ms),
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
            account_limit_guard,
            request_limit_guard,
            state.persist_store.clone(),
            api_key,
        );
    }

    let status = upstream.status();
    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => {
            log_response_read_failed(endpoint, &base_model, false, account.as_ref(), &err);
            record_persist_account_error(
                state.persist_store.as_ref(),
                endpoint,
                &base_model,
                false,
                StatusCode::BAD_GATEWAY,
                attempts,
                api_key,
                account.as_ref(),
                format!("读取上游响应失败: {err}"),
                crate::core::now_unix_ms().saturating_sub(started_ms),
            );
            return send_error(
                StatusCode::BAD_GATEWAY,
                &format!("读取上游响应失败: {err}"),
                "api_error",
            );
        }
    };
    let now_ms = crate::core::now_unix_ms();
    bind_session_accounts(
        state.runtime_state.as_ref(),
        account.as_ref(),
        &session_keys,
    );
    bind_response_account_from_json_bytes(state.runtime_state.as_ref(), account.as_ref(), &bytes);
    if let Some(usage) = record_usage_from_json_bytes(
        account.as_ref(),
        state.runtime_state.as_ref(),
        now_ms,
        bytes.as_ref(),
    ) {
        record_persist_usage(
            state.persist_store.as_ref(),
            endpoint,
            &base_model,
            api_key.clone(),
            account.as_ref(),
            usage,
        );
    }
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
    record_persist_request(
        state.persist_store.as_ref(),
        endpoint,
        &base_model,
        false,
        status,
        attempts,
        api_key,
        Some(account.as_ref()),
        now_ms.saturating_sub(started_ms),
    );

    let mut resp = Response::new(Body::from(bytes));
    let _request_limit_guard = request_limit_guard;
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

pub(super) async fn v1_responses_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let passthrough_headers = extract_codex_passthrough_headers(&headers);
    ws.on_upgrade(move |socket| handle_responses_ws(socket, state, passthrough_headers))
        .into_response()
}

pub(super) async fn v1_responses_compact(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Response {
    let api_key = api_key_from_req(&req);
    let request_limit_guard = request_limit_guard_from_req(&req);
    let started_ms = crate::core::now_unix_ms();
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
    let stream = match parse_stream_field(&body_value) {
        Ok(stream) => stream,
        Err(message) => {
            return send_error(StatusCode::BAD_REQUEST, message, "invalid_request_error");
        }
    };

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
    let upstream_result = match execute_codex_request(
        &state,
        &base_model,
        url,
        codex_body,
        stream,
        passthrough_headers.as_ref(),
        &HashSet::new(),
    )
    .await
    {
        Ok(v) => v,
        Err(err) => {
            log_upstream_request_error(endpoint, &base_model, stream, &err);
            record_persist_error(
                state.persist_store.as_ref(),
                endpoint,
                &base_model,
                stream,
                StatusCode::BAD_GATEWAY,
                api_key,
                err.to_string(),
                crate::core::now_unix_ms().saturating_sub(started_ms),
            );
            return send_upstream_error(err);
        }
    };
    let upstream = upstream_result.response;
    let account = upstream_result.account;
    let attempts = upstream_result.attempts;
    let account_limit_guard = upstream_result.account_limit_guard;

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
        record_persist_request(
            state.persist_store.as_ref(),
            endpoint,
            &base_model,
            true,
            status,
            attempts,
            api_key.clone(),
            Some(account.as_ref()),
            crate::core::now_unix_ms().saturating_sub(started_ms),
        );
        return build_passthrough_sse_response(
            endpoint,
            base_model,
            upstream,
            account,
            state.runtime_state.clone(),
            headers,
            account_limit_guard,
            request_limit_guard,
            state.persist_store.clone(),
            api_key,
        );
    }

    let bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => {
            log_response_read_failed(endpoint, &base_model, false, account.as_ref(), &err);
            record_persist_account_error(
                state.persist_store.as_ref(),
                endpoint,
                &base_model,
                false,
                StatusCode::BAD_GATEWAY,
                attempts,
                api_key,
                account.as_ref(),
                format!("读取上游响应失败: {err}"),
                crate::core::now_unix_ms().saturating_sub(started_ms),
            );
            return send_error(
                StatusCode::BAD_GATEWAY,
                &format!("读取上游响应失败: {err}"),
                "api_error",
            );
        }
    };
    let now_ms = crate::core::now_unix_ms();
    if let Some(usage) = record_usage_from_json_bytes(
        account.as_ref(),
        state.runtime_state.as_ref(),
        now_ms,
        bytes.as_ref(),
    ) {
        record_persist_usage(
            state.persist_store.as_ref(),
            endpoint,
            &base_model,
            api_key.clone(),
            account.as_ref(),
            usage,
        );
    }
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
    record_persist_request(
        state.persist_store.as_ref(),
        endpoint,
        &base_model,
        false,
        StatusCode::OK,
        attempts,
        api_key,
        Some(account.as_ref()),
        now_ms.saturating_sub(started_ms),
    );

    let mut resp = Response::new(Body::from(bytes));
    let _request_limit_guard = request_limit_guard;
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

pub(super) fn clean_compact_value_to_vec(mut v: serde_json::Value, base_model: &str) -> Vec<u8> {
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
                        let stream_result = forward_responses_request_as_ws(
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
                            Err(ResponsesWsError::NativeFallback(_)) => unreachable!(),
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

fn completed_response_has_output(payload: &[u8]) -> bool {
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return false;
    };
    let Some(output) = root
        .get("response")
        .and_then(|response| response.get("output"))
        .and_then(|output| output.as_array())
    else {
        return false;
    };

    output.iter().any(output_item_has_visible_content)
}

fn output_item_has_visible_content(item: &serde_json::Value) -> bool {
    match item
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
    {
        "function_call" | "custom_tool_call" | "tool_search_call" | "image_generation_call" => true,
        "reasoning" | "reasoning_text" => {
            item.get("encrypted_content")
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.is_empty())
                || item
                    .get("summary")
                    .and_then(|value| value.as_array())
                    .is_some_and(|items| {
                        items.iter().any(|part| {
                            part.get("text")
                                .and_then(|value| value.as_str())
                                .is_some_and(|value| !value.is_empty())
                        })
                    })
                || item
                    .get("content")
                    .and_then(|value| value.as_array())
                    .is_some_and(|items| {
                        items.iter().any(|part| {
                            part.get("text")
                                .and_then(|value| value.as_str())
                                .is_some_and(|value| !value.is_empty())
                        })
                    })
        }
        "message" => item
            .get("content")
            .and_then(|value| value.as_array())
            .is_some_and(|items| {
                items.iter().any(
                    |part| match part.get("type").and_then(|value| value.as_str()) {
                        Some("output_text") | Some("text") => part
                            .get("text")
                            .and_then(|value| value.as_str())
                            .is_some_and(|value| !value.is_empty()),
                        Some("refusal") => part
                            .get("refusal")
                            .and_then(|value| value.as_str())
                            .is_some_and(|value| !value.is_empty()),
                        _ => false,
                    },
                )
            }),
        _ => false,
    }
}

#[derive(Debug)]
enum ResponsesWsError {
    EmptyResponse,
    Upstream(UpstreamError),
    Local(String),
    NativeFallback(String),
}

async fn forward_responses_request_as_ws(
    socket: &mut WebSocket,
    state: &AppState,
    request_body: Vec<u8>,
    model: &str,
    passthrough_headers: Option<&HeaderMap>,
    session: &mut ResponsesWsSession,
) -> Result<(), ResponsesWsError> {
    if state.codex_client.native_responses_websocket_enabled() {
        match forward_responses_native_ws_as_ws(
            socket,
            state,
            request_body.clone(),
            model,
            passthrough_headers,
            session,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(ResponsesWsError::NativeFallback(message)) => {
                tracing::warn!(model = %model, error = %message, "native responses websocket failed before downstream write; falling back to HTTP/SSE");
            }
            Err(err) => return Err(err),
        }
    }

    forward_responses_sse_as_ws(
        socket,
        state,
        request_body,
        model,
        passthrough_headers,
        session,
    )
    .await
}

async fn forward_responses_native_ws_as_ws(
    socket: &mut WebSocket,
    state: &AppState,
    request_body: Vec<u8>,
    model: &str,
    passthrough_headers: Option<&HeaderMap>,
    session: &mut ResponsesWsSession,
) -> Result<(), ResponsesWsError> {
    let codex_value = serde_json::from_slice::<serde_json::Value>(&request_body)
        .map_err(|err| ResponsesWsError::Local(format!("构建上游 websocket 请求失败: {err}")))?;
    let session_keys = session_keys_from_request(passthrough_headers, &codex_value);
    let initial_excluded = sticky_initial_excluded_for_request(
        state,
        previous_response_id_from_value(&codex_value),
        &session_keys,
    );
    let account = state
        .manager
        .pick_excluding(model, &initial_excluded)
        .map_err(|err| ResponsesWsError::Upstream(UpstreamError::Pick(err)))?;
    let _account_limit_guard = state
        .rate_limiter
        .check_account(account.file_path())
        .map_err(|err| ResponsesWsError::NativeFallback(err.message))?;
    let ws_url = state
        .codex_client
        .responses_websocket_url()
        .map_err(ResponsesWsError::NativeFallback)?;
    let headers = state
        .codex_client
        .websocket_headers(account.as_ref(), passthrough_headers);
    let mut request = ws_url
        .as_str()
        .into_client_request()
        .map_err(|err| ResponsesWsError::NativeFallback(err.to_string()))?;
    for (name, value) in headers.iter() {
        request.headers_mut().insert(name.clone(), value.clone());
    }

    let (mut upstream_ws, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|err| ResponsesWsError::NativeFallback(err.to_string()))?;
    let upstream_request = build_codex_websocket_request_body(&request_body);
    upstream_ws
        .send(UpstreamWsMessage::Text(
            String::from_utf8_lossy(&upstream_request)
                .into_owned()
                .into(),
        ))
        .await
        .map_err(|err| ResponsesWsError::NativeFallback(err.to_string()))?;

    bind_session_accounts(
        state.runtime_state.as_ref(),
        account.as_ref(),
        &session_keys,
    );

    let mut saw_event = false;
    let mut has_text = false;
    let mut has_tool = false;
    let mut has_completed_output = false;
    while let Some(message) = upstream_ws.next().await {
        let message = message.map_err(|err| {
            if saw_event {
                ResponsesWsError::Local(format!("读取上游 websocket 失败: {err}"))
            } else {
                ResponsesWsError::NativeFallback(err.to_string())
            }
        })?;
        let payload = match message {
            UpstreamWsMessage::Text(text) => text.to_string().into_bytes(),
            UpstreamWsMessage::Binary(bytes) => bytes.to_vec(),
            UpstreamWsMessage::Ping(payload) => {
                let _ = upstream_ws.send(UpstreamWsMessage::Pong(payload)).await;
                continue;
            }
            UpstreamWsMessage::Pong(_) => continue,
            UpstreamWsMessage::Close(_) => break,
            _ => continue,
        };
        let payload = trim_ascii(&payload);
        if payload.is_empty() {
            continue;
        }
        let normalized_payload = normalize_codex_websocket_completion(payload);
        let outbound = process_responses_ws_sse_payload(
            state,
            account.as_ref(),
            session,
            &normalized_payload,
            &mut has_text,
            &mut has_tool,
            &mut has_completed_output,
        );
        saw_event = true;
        if socket.send(Message::Text(outbound.into())).await.is_err() {
            return Ok(());
        }
        if serde_json::from_slice::<serde_json::Value>(&normalized_payload)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            })
            .is_some_and(|event_type| event_type == "response.completed")
        {
            break;
        }
    }

    if !saw_event {
        return Err(ResponsesWsError::NativeFallback(
            "native websocket returned no events".to_string(),
        ));
    }
    record_client_success(
        account.as_ref(),
        state.request_stats.as_ref(),
        state.runtime_state.as_ref(),
        crate::core::now_unix_ms(),
    );
    Ok(())
}

fn build_codex_websocket_request_body(body: &[u8]) -> Vec<u8> {
    let mut value = serde_json::from_slice::<serde_json::Value>(body)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "type".to_string(),
            serde_json::Value::String("response.create".to_string()),
        );
    }
    serde_json::to_vec(&value).unwrap_or_else(|_| body.to_vec())
}

fn normalize_codex_websocket_completion(payload: &[u8]) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return payload.to_vec();
    };
    if value.get("type").and_then(|value| value.as_str()) == Some("response.done") {
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "type".to_string(),
                serde_json::Value::String("response.completed".to_string()),
            );
        }
        return serde_json::to_vec(&value).unwrap_or_else(|_| payload.to_vec());
    }
    payload.to_vec()
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
    let session_keys = session_keys_from_request(passthrough_headers, &codex_value);
    let initial_excluded = sticky_initial_excluded_for_request(
        state,
        previous_response_id_from_value(&codex_value),
        &session_keys,
    );
    let codex_body = serde_json::to_vec(&codex_value).unwrap_or_else(|_| b"{}".to_vec());

    let url = state
        .codex_client
        .responses_url()
        .map_err(ResponsesWsError::Local)?;

    let upstream_result = execute_codex_request(
        state,
        &base_model,
        url,
        codex_body,
        true,
        passthrough_headers,
        &initial_excluded,
    )
    .await
    .map_err(ResponsesWsError::Upstream)?;
    let upstream = upstream_result.response;
    let account = upstream_result.account;
    let _account_limit_guard = upstream_result.account_limit_guard;
    bind_session_accounts(
        state.runtime_state.as_ref(),
        account.as_ref(),
        &session_keys,
    );

    let mut has_text = false;
    let mut has_tool = false;
    let mut has_completed_output = false;
    let mut parser = sse::SseDataParser::default();
    let mut upstream_stream = upstream.bytes_stream();

    while let Some(chunk) = upstream_stream.next().await {
        let chunk = chunk.map_err(|e| ResponsesWsError::Local(format!("读取上游响应失败: {e}")))?;
        let mut outbound = Vec::new();
        parser.push(&chunk, |payload| {
            outbound.push(process_responses_ws_sse_payload(
                state,
                account.as_ref(),
                session,
                payload,
                &mut has_text,
                &mut has_tool,
                &mut has_completed_output,
            ));
        });
        for outbound_text in outbound {
            if socket
                .send(Message::Text(outbound_text.into()))
                .await
                .is_err()
            {
                return Ok(());
            }
        }
    }
    let mut outbound = Vec::new();
    parser.finish(|payload| {
        outbound.push(process_responses_ws_sse_payload(
            state,
            account.as_ref(),
            session,
            payload,
            &mut has_text,
            &mut has_tool,
            &mut has_completed_output,
        ));
    });
    for outbound_text in outbound {
        if socket
            .send(Message::Text(outbound_text.into()))
            .await
            .is_err()
        {
            return Ok(());
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

fn process_responses_ws_sse_payload(
    state: &AppState,
    account: &Account,
    session: &mut ResponsesWsSession,
    payload: &[u8],
    has_text: &mut bool,
    has_tool: &mut bool,
    has_completed_output: &mut bool,
) -> String {
    let mut outbound_text = None;
    if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(payload) {
        update_responses_ws_session_from_event(session, &v);
        bind_response_account_from_value(state.runtime_state.as_ref(), account, &v);
        let had_stream_output = *has_text || *has_tool;
        if let Some(typ) = v.get("type").and_then(|v| v.as_str()) {
            match typ {
                "response.output_text.delta" => {
                    if v.get("delta").and_then(|v| v.as_str()).unwrap_or_default() != "" {
                        *has_text = true;
                    }
                }
                "response.output_item.added"
                | "response.function_call_arguments.delta"
                | "response.function_call_arguments.done"
                | "response.output_item.done" => {
                    *has_tool = true;
                }
                "response.completed" => {
                    let completed_has_output = completed_response_has_output(payload);
                    if completed_has_output {
                        *has_completed_output = true;
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

    outbound_text.unwrap_or_else(|| String::from_utf8_lossy(payload).into_owned())
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

pub(super) fn sticky_initial_excluded_for_request(
    state: &AppState,
    previous_response_id: Option<&str>,
    session_keys: &[String],
) -> HashSet<String> {
    if let Some(previous_response_id) = previous_response_id
        && let Some(sticky_account_path) = state
            .runtime_state
            .account_for_response(previous_response_id)
    {
        return sticky_initial_excluded_for_account(state, &sticky_account_path);
    }
    for session_key in session_keys {
        if let Some(sticky_account_path) = state.runtime_state.account_for_session(session_key) {
            return sticky_initial_excluded_for_account(state, &sticky_account_path);
        }
    }
    HashSet::new()
}

fn sticky_initial_excluded_for_account(
    state: &AppState,
    sticky_account_path: &str,
) -> HashSet<String> {
    let accounts = state.manager.accounts_snapshot();
    if !accounts
        .iter()
        .any(|account| account.file_path() == sticky_account_path)
    {
        return HashSet::new();
    }
    accounts
        .iter()
        .filter_map(|account| {
            let file_path = account.file_path();
            if file_path == sticky_account_path {
                None
            } else {
                Some(file_path.to_string())
            }
        })
        .collect()
}

pub(super) fn bind_session_accounts(
    runtime_state: &RuntimeStateStore,
    account: &Account,
    session_keys: &[String],
) {
    for session_key in session_keys {
        runtime_state.bind_session_account(session_key, account.file_path());
    }
}

pub(super) fn session_keys_from_request(
    headers: Option<&HeaderMap>,
    value: &serde_json::Value,
) -> Vec<String> {
    let mut keys = Vec::new();
    for header_name in [
        "Session_id",
        "X-Session-ID",
        "x-session-id",
        "Conversation_id",
        "conversation_id",
    ] {
        if let Some(session) = headers
            .and_then(|headers| headers.get(header_name))
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            push_unique_session_key(&mut keys, header_name, session);
        }
    }

    if let Some(user_id) = value
        .get("metadata")
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(|user_id| user_id.as_str())
        .map(str::trim)
        .filter(|user_id| !user_id.is_empty())
    {
        if let Some(session_id) = serde_json::from_str::<serde_json::Value>(user_id)
            .ok()
            .and_then(|v| {
                v.get("session_id")
                    .and_then(|session_id| session_id.as_str())
                    .map(str::trim)
                    .filter(|session_id| !session_id.is_empty())
                    .map(str::to_string)
            })
        {
            push_unique_session_key(&mut keys, "metadata.user_id.session_id", &session_id);
        } else {
            push_unique_session_key(&mut keys, "metadata.user_id", user_id);
        }
    }

    keys
}

fn push_unique_session_key(keys: &mut Vec<String>, source: &str, value: &str) {
    let key = format!("{source}:{}", value.trim());
    if !keys.iter().any(|existing| existing == &key) {
        keys.push(key);
    }
}

#[derive(Default)]
pub(super) struct ResponsesWsSession {
    pub(super) last_request: Option<serde_json::Value>,
    pub(super) last_model: Option<String>,
    pub(super) last_response_id: Option<String>,
    pub(super) tool_call_ids: HashSet<String>,
}

pub(super) fn build_responses_ws_request(
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

    dedupe_input_items_by_id(&mut request_value);
    validate_function_call_output_context_with_known_ids(&request_value, &session.tool_call_ids)?;

    session.last_request = Some(request_value.clone());
    session.last_model = Some(model.clone());

    let request_body =
        serde_json::to_vec(&request_value).map_err(|_| "序列化请求失败".to_string())?;
    Ok((request_body, model))
}

fn dedupe_input_items_by_id(value: &mut serde_json::Value) {
    let Some(input) = value
        .get_mut("input")
        .and_then(|input| input.as_array_mut())
    else {
        return;
    };
    if input.len() < 2 {
        return;
    }

    let mut last_index_by_id = HashMap::<String, usize>::new();
    for (index, item) in input.iter().enumerate() {
        if let Some(item_id) = item
            .get("id")
            .and_then(|id| id.as_str())
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            last_index_by_id.insert(item_id.to_string(), index);
        }
    }
    if last_index_by_id.is_empty() {
        return;
    }

    let mut index = 0usize;
    input.retain(|item| {
        let keep = item
            .get("id")
            .and_then(|id| id.as_str())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .and_then(|item_id| last_index_by_id.get(item_id).copied())
            .is_none_or(|last_index| last_index == index);
        index += 1;
        keep
    });
}

pub(super) fn update_responses_ws_session_from_event(
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
