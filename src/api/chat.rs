use std::collections::HashSet;

use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use bytes::BytesMut;
use futures_util::StreamExt;
use futures_util::stream::unfold;
use memchr::memchr;

use crate::thinking::apply::apply_thinking_to_value;
use crate::translate::request::{
    build_reverse_tool_name_map_from_value, convert_openai_value_to_codex_value,
};
use crate::translate::{
    StreamState, convert_chat_completion_chunk_to_completion_chunk,
    convert_chat_completion_to_completion, convert_completions_request_to_chat_value,
    convert_stream_chunk,
};

use super::{
    AppState, ChatNonStreamOutcome, UsageTokens, api_key_from_req, execute_codex_request,
    extract_codex_passthrough_headers, log_request_completed, log_response_read_failed,
    log_stream_incomplete, log_stream_read_failed, log_upstream_request_error,
    openai_stream_error_event, parse_chat_non_stream_response, parse_stream_field,
    record_client_success, record_hourly_usage, record_persist_account_error, record_persist_error,
    record_persist_request, record_persist_usage, record_stream_account_failure,
    request_limit_guard_from_req, response_failed_error_from_sse_line, send_error,
    send_upstream_error, trim_ascii,
};

pub(super) async fn v1_chat_completions(
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
        let upstream_result = match execute_codex_request(
            &state,
            &base_model,
            url,
            codex_body,
            true,
            passthrough_headers.as_ref(),
            &HashSet::new(),
        )
        .await
        {
            Ok(v) => v,
            Err(err) => {
                log_upstream_request_error(endpoint, &base_model, true, &err);
                record_persist_error(
                    state.persist_store.as_ref(),
                    endpoint,
                    &base_model,
                    true,
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

        let status = upstream.status();
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
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
        let account = account.clone();
        let request_stats = state.request_stats.clone();
        let runtime_state = state.runtime_state.clone();
        let model_for_log = base_model.clone();
        let persist_store = state.persist_store.clone();
        let api_key_for_persist = api_key.clone();
        let started_ms_for_stream = started_ms;
        tokio::spawn(async move {
            let _account_limit_guard = account_limit_guard;
            let _request_limit_guard = request_limit_guard;
            let mut buf = BytesMut::new();
            let mut state = StreamState::new(&base_model);
            let mut upstream_stream = upstream.bytes_stream();

            while let Some(chunk) = upstream_stream.next().await {
                let chunk = match chunk {
                    Ok(b) => b,
                    Err(err) => {
                        log_stream_read_failed(endpoint, &model_for_log, account.as_ref(), &err);
                        record_persist_account_error(
                            persist_store.as_ref(),
                            endpoint,
                            &model_for_log,
                            true,
                            StatusCode::BAD_GATEWAY,
                            attempts,
                            api_key_for_persist.clone(),
                            account.as_ref(),
                            format!("stream read from upstream failed: {err}"),
                            0,
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
                    if let Some((message, err_type)) = response_failed_error_from_sse_line(line) {
                        log_stream_incomplete(
                            endpoint,
                            &model_for_log,
                            account.as_ref(),
                            "response.failed",
                        );
                        record_stream_account_failure(
                            persist_store.as_ref(),
                            endpoint,
                            &model_for_log,
                            attempts,
                            api_key_for_persist.clone(),
                            account.as_ref(),
                            runtime_state.as_ref(),
                            message.clone(),
                            started_ms_for_stream,
                        );
                        let _ = tx
                            .send(Ok(openai_stream_error_event(&message, &err_type)))
                            .await;
                        return;
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
                    record_persist_usage(
                        persist_store.as_ref(),
                        endpoint,
                        &base_model,
                        api_key_for_persist,
                        account.as_ref(),
                        usage,
                    );
                }
                record_client_success(
                    account.as_ref(),
                    request_stats.as_ref(),
                    runtime_state.as_ref(),
                    now_ms,
                );
                let _ = tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
            } else {
                let (message, err_type, reason) = if state.completed {
                    ("empty response", "invalid_response", "empty response")
                } else {
                    (
                        "上游响应缺少 response.completed",
                        "api_error",
                        "missing response.completed",
                    )
                };
                log_stream_incomplete(endpoint, &model_for_log, account.as_ref(), reason);
                record_stream_account_failure(
                    persist_store.as_ref(),
                    endpoint,
                    &model_for_log,
                    attempts,
                    api_key_for_persist.clone(),
                    account.as_ref(),
                    runtime_state.as_ref(),
                    message.to_string(),
                    started_ms_for_stream,
                );
                let _ = tx
                    .send(Ok(openai_stream_error_event(message, err_type)))
                    .await;
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
        let upstream_result = match execute_codex_request(
            &state,
            &base_model,
            url.clone(),
            codex_body.clone(),
            true,
            passthrough_headers.as_ref(),
            &excluded_for_empty,
        )
        .await
        {
            Ok(v) => v,
            Err(err) => {
                log_upstream_request_error(endpoint, &base_model, false, &err);
                record_persist_error(
                    state.persist_store.as_ref(),
                    endpoint,
                    &base_model,
                    false,
                    StatusCode::BAD_GATEWAY,
                    api_key.clone(),
                    err.to_string(),
                    crate::core::now_unix_ms().saturating_sub(started_ms),
                );
                return send_upstream_error(err);
            }
        };
        let upstream = upstream_result.response;
        let account = upstream_result.account;
        let attempts = upstream_result.attempts;
        let _account_limit_guard = upstream_result.account_limit_guard;

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
                    api_key.clone(),
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
        match parse_chat_non_stream_response(
            &bytes,
            &reverse_tool_map,
            account.as_ref(),
            state.runtime_state.as_ref(),
            now_ms,
        ) {
            ChatNonStreamOutcome::Success { body, usage } => {
                if let Some(usage) = usage {
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
                let mut resp = Response::new(Body::from(body));
                let _request_limit_guard = request_limit_guard;
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
                record_persist_account_error(
                    state.persist_store.as_ref(),
                    endpoint,
                    &base_model,
                    false,
                    StatusCode::BAD_REQUEST,
                    attempts,
                    api_key,
                    account.as_ref(),
                    "empty response".to_string(),
                    crate::core::now_unix_ms().saturating_sub(started_ms),
                );
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
                record_persist_account_error(
                    state.persist_store.as_ref(),
                    endpoint,
                    &base_model,
                    false,
                    StatusCode::BAD_GATEWAY,
                    attempts,
                    api_key,
                    account.as_ref(),
                    "上游响应缺少 response.completed".to_string(),
                    crate::core::now_unix_ms().saturating_sub(started_ms),
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

pub(super) async fn v1_completions(State(state): State<AppState>, req: Request<Body>) -> Response {
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
    let stream = match parse_stream_field(&body_value) {
        Ok(stream) => stream,
        Err(message) => {
            return send_error(StatusCode::BAD_REQUEST, message, "invalid_request_error");
        }
    };

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
    let upstream_result = match execute_codex_request(
        &state,
        &base_model,
        url,
        codex_body,
        true,
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
                api_key.clone(),
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
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);
        let account = account.clone();
        let request_stats = state.request_stats.clone();
        let runtime_state = state.runtime_state.clone();
        let model_for_log = base_model.clone();
        let persist_store = state.persist_store.clone();
        let api_key_for_persist = api_key.clone();
        let started_ms_for_stream = started_ms;
        tokio::spawn(async move {
            let _account_limit_guard = account_limit_guard;
            let _request_limit_guard = request_limit_guard;
            let mut buf = BytesMut::new();
            let mut state = StreamState::new(&base_model);
            let mut upstream_stream = upstream.bytes_stream();

            while let Some(chunk) = upstream_stream.next().await {
                let chunk = match chunk {
                    Ok(b) => b,
                    Err(err) => {
                        log_stream_read_failed(endpoint, &model_for_log, account.as_ref(), &err);
                        record_persist_account_error(
                            persist_store.as_ref(),
                            endpoint,
                            &model_for_log,
                            true,
                            StatusCode::BAD_GATEWAY,
                            attempts,
                            api_key_for_persist.clone(),
                            account.as_ref(),
                            format!("stream read from upstream failed: {err}"),
                            0,
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
                    if let Some((message, err_type)) = response_failed_error_from_sse_line(line) {
                        log_stream_incomplete(
                            endpoint,
                            &model_for_log,
                            account.as_ref(),
                            "response.failed",
                        );
                        record_stream_account_failure(
                            persist_store.as_ref(),
                            endpoint,
                            &model_for_log,
                            attempts,
                            api_key_for_persist.clone(),
                            account.as_ref(),
                            runtime_state.as_ref(),
                            message.clone(),
                            started_ms_for_stream,
                        );
                        let _ = tx
                            .send(Ok(openai_stream_error_event(&message, &err_type)))
                            .await;
                        return;
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
                    record_persist_usage(
                        persist_store.as_ref(),
                        endpoint,
                        &base_model,
                        api_key_for_persist,
                        account.as_ref(),
                        usage,
                    );
                }
                record_client_success(
                    account.as_ref(),
                    request_stats.as_ref(),
                    runtime_state.as_ref(),
                    now_ms,
                );
                let _ = tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await;
            } else {
                let (message, err_type, reason) = if state.completed {
                    ("empty response", "invalid_response", "empty response")
                } else {
                    (
                        "上游响应缺少 response.completed",
                        "api_error",
                        "missing response.completed",
                    )
                };
                log_stream_incomplete(endpoint, &model_for_log, account.as_ref(), reason);
                record_stream_account_failure(
                    persist_store.as_ref(),
                    endpoint,
                    &model_for_log,
                    attempts,
                    api_key_for_persist.clone(),
                    account.as_ref(),
                    runtime_state.as_ref(),
                    message.to_string(),
                    started_ms_for_stream,
                );
                let _ = tx
                    .send(Ok(openai_stream_error_event(message, err_type)))
                    .await;
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
            record_persist_account_error(
                state.persist_store.as_ref(),
                endpoint,
                &base_model,
                false,
                StatusCode::BAD_GATEWAY,
                attempts,
                api_key.clone(),
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
    match parse_chat_non_stream_response(
        &bytes,
        &reverse_tool_map,
        account.as_ref(),
        state.runtime_state.as_ref(),
        now_ms,
    ) {
        ChatNonStreamOutcome::Success { body, usage } => {
            if let Some(usage) = usage {
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
            let mut resp = Response::new(Body::from(convert_chat_completion_to_completion(&body)));
            let _request_limit_guard = request_limit_guard;
            *resp.status_mut() = StatusCode::OK;
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            );
            resp
        }
        ChatNonStreamOutcome::Empty => {
            record_persist_account_error(
                state.persist_store.as_ref(),
                endpoint,
                &base_model,
                false,
                StatusCode::BAD_REQUEST,
                attempts,
                api_key,
                account.as_ref(),
                "empty response".to_string(),
                crate::core::now_unix_ms().saturating_sub(started_ms),
            );
            send_error(
                StatusCode::BAD_REQUEST,
                "empty response",
                "invalid_response",
            )
        }
        ChatNonStreamOutcome::MissingCompleted => {
            record_persist_account_error(
                state.persist_store.as_ref(),
                endpoint,
                &base_model,
                false,
                StatusCode::BAD_GATEWAY,
                attempts,
                api_key,
                account.as_ref(),
                "上游响应缺少 response.completed".to_string(),
                crate::core::now_unix_ms().saturating_sub(started_ms),
            );
            send_error(
                StatusCode::BAD_GATEWAY,
                "上游响应缺少 response.completed",
                "api_error",
            )
        }
    }
}
