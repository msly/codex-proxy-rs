use std::collections::HashSet;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::BytesMut;
use futures_util::StreamExt;
use futures_util::stream::unfold;
use memchr::memchr;
use serde_json::json;

use crate::thinking::apply::apply_thinking_to_value;
use crate::translate::request::convert_openai_value_to_codex_value;
use crate::translate::{
    ClaudeStreamState, convert_claude_request_to_openai,
    convert_codex_full_sse_to_claude_response_with_meta, convert_codex_stream_to_claude_events,
};

use super::{
    AppState, api_key_from_req, claude_stream_error_event, execute_codex_request,
    extract_codex_passthrough_headers, log_request_completed, log_response_read_failed,
    log_stream_incomplete, log_stream_read_failed, log_upstream_request_error,
    record_client_success, record_persist_account_error, record_persist_error,
    record_persist_request, record_persist_usage, record_stream_account_failure,
    record_usage_from_sse_bytes, record_usage_from_sse_line, request_limit_guard_from_req,
    response_failed_error_from_sse_line, send_claude_error, send_claude_upstream_error, trim_ascii,
};

pub(super) async fn v1_messages(State(state): State<AppState>, req: Request<Body>) -> Response {
    let api_key = api_key_from_req(&req);
    let request_limit_guard = request_limit_guard_from_req(&req);
    let started_ms = crate::core::now_unix_ms();
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
                api_key,
                err.to_string(),
                crate::core::now_unix_ms().saturating_sub(started_ms),
            );
            return send_claude_upstream_error(err);
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
        let request_stats = state.request_stats.clone();
        let runtime_state = state.runtime_state.clone();
        let account_for_log = account.clone();
        let model_for_log = base_model.clone();
        let persist_store = state.persist_store.clone();
        let api_key_for_persist = api_key.clone();
        let started_ms_for_stream = started_ms;
        tokio::spawn(async move {
            let _account_limit_guard = account_limit_guard;
            let _request_limit_guard = request_limit_guard;
            let mut buf = BytesMut::new();
            let mut state = ClaudeStreamState::new(&base_model);
            let mut upstream_stream = upstream.bytes_stream();
            let mut usage_for_persist = None;

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
                        record_persist_account_error(
                            persist_store.as_ref(),
                            endpoint,
                            &model_for_log,
                            true,
                            StatusCode::BAD_GATEWAY,
                            attempts,
                            api_key_for_persist.clone(),
                            account_for_log.as_ref(),
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

                    if usage_for_persist.is_none() {
                        usage_for_persist = record_usage_from_sse_line(
                            account.as_ref(),
                            runtime_state.as_ref(),
                            crate::core::now_unix_ms(),
                            line,
                        );
                    }

                    if let Some((message, err_type)) = response_failed_error_from_sse_line(line) {
                        log_stream_incomplete(
                            endpoint,
                            &model_for_log,
                            account_for_log.as_ref(),
                            "response.failed",
                        );
                        record_stream_account_failure(
                            persist_store.as_ref(),
                            endpoint,
                            &model_for_log,
                            attempts,
                            api_key_for_persist.clone(),
                            account_for_log.as_ref(),
                            runtime_state.as_ref(),
                            message.clone(),
                            started_ms_for_stream,
                        );
                        let _ = tx
                            .send(Ok(claude_stream_error_event(&message, &err_type)))
                            .await;
                        return;
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
                if let Some(usage) = usage_for_persist {
                    record_persist_usage(
                        persist_store.as_ref(),
                        endpoint,
                        &base_model,
                        api_key_for_persist,
                        account.as_ref(),
                        usage,
                    );
                }
            } else {
                let message = "上游响应缺少 response.completed";
                log_stream_incomplete(
                    endpoint,
                    &model_for_log,
                    account_for_log.as_ref(),
                    "missing response.completed",
                );
                record_stream_account_failure(
                    persist_store.as_ref(),
                    endpoint,
                    &model_for_log,
                    attempts,
                    api_key_for_persist.clone(),
                    account_for_log.as_ref(),
                    runtime_state.as_ref(),
                    message.to_string(),
                    started_ms_for_stream,
                );
                let _ = tx
                    .send(Ok(claude_stream_error_event(message, "api_error")))
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
                api_key,
                account.as_ref(),
                format!("读取上游响应失败: {err}"),
                crate::core::now_unix_ms().saturating_sub(started_ms),
            );
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
        record_persist_account_error(
            state.persist_store.as_ref(),
            endpoint,
            &base_model,
            false,
            StatusCode::BAD_GATEWAY,
            attempts,
            api_key,
            account.as_ref(),
            "未收到 response.completed 事件".to_string(),
            crate::core::now_unix_ms().saturating_sub(started_ms),
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
        return send_claude_error(
            StatusCode::BAD_REQUEST,
            "invalid_response",
            "empty response",
        );
    }

    let now_ms = crate::core::now_unix_ms();
    if let Some(usage) = record_usage_from_sse_bytes(
        account.as_ref(),
        state.runtime_state.as_ref(),
        now_ms,
        &bytes,
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

    let mut resp = Response::new(Body::from(result.json));
    let _request_limit_guard = request_limit_guard;
    *resp.status_mut() = StatusCode::OK;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

pub(super) async fn v1_messages_count_tokens(req: Request<Body>) -> Response {
    let request_limit_guard = request_limit_guard_from_req(&req);
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
    let _request_limit_guard = request_limit_guard;
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
