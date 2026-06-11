use std::collections::HashSet;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, State};
use axum::http::header;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use base64::Engine;

use crate::thinking::apply::apply_thinking_to_value;
use crate::translate::request::convert_openai_value_to_codex_value;
use crate::translate::{
    convert_image_request_to_responses_value, convert_responses_sse_to_images_json,
};

use super::{
    AppState, api_key_from_req, execute_codex_request, extract_codex_passthrough_headers,
    log_request_completed, log_response_read_failed, log_upstream_request_error,
    record_client_success, record_persist_account_error, record_persist_error,
    record_persist_request, record_persist_usage, record_usage_from_sse_bytes,
    request_limit_guard_from_req, response_failed_error_from_sse_bytes, send_error,
    send_upstream_error,
};

pub(super) async fn v1_images_generations(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Response {
    v1_images(state, req, false).await
}

pub(super) async fn v1_images_edits(State(state): State<AppState>, req: Request<Body>) -> Response {
    v1_images(state, req, true).await
}

async fn v1_images(state: AppState, req: Request<Body>, edit: bool) -> Response {
    let api_key = api_key_from_req(&req);
    let request_limit_guard = request_limit_guard_from_req(&req);
    let started_ms = crate::core::now_unix_ms();
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
    if let Some((message, err_type)) = response_failed_error_from_sse_bytes(&bytes) {
        record_persist_account_error(
            state.persist_store.as_ref(),
            endpoint,
            &base_model,
            false,
            StatusCode::BAD_GATEWAY,
            attempts,
            api_key,
            account.as_ref(),
            message.clone(),
            crate::core::now_unix_ms().saturating_sub(started_ms),
        );
        return send_error(StatusCode::BAD_GATEWAY, &message, &err_type);
    }
    let Some(out) = convert_responses_sse_to_images_json(&bytes, &response_format) else {
        record_persist_account_error(
            state.persist_store.as_ref(),
            endpoint,
            &base_model,
            false,
            StatusCode::BAD_GATEWAY,
            attempts,
            api_key,
            account.as_ref(),
            "上游响应缺少 image_generation_call 输出".to_string(),
            crate::core::now_unix_ms().saturating_sub(started_ms),
        );
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

    let mut resp = Response::new(Body::from(out));
    let _request_limit_guard = request_limit_guard;
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
