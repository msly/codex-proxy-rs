use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use futures_util::stream::unfold;
use serde_json::json;
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

use crate::admin::AdminAuth;
use crate::core::{Account, Manager};
use crate::limit::{AccountLimitGuard, RateLimiter, RequestLimitGuard};
use crate::persist::PersistStore;
use crate::quota::QuotaChecker;
use crate::refresh::{Refresher, SaveQueue};
use crate::state::RuntimeStateStore;
use crate::upstream::codex::{CodexClient, UpstreamError, UpstreamRequest, UpstreamResponse};

mod management;
pub mod models;
mod responses;
mod sse;
mod telemetry;

#[cfg(test)]
use responses::{
    ResponsesWsSession, build_responses_ws_request, clean_compact_value_to_vec,
    update_responses_ws_session_from_event,
};

use telemetry::{
    record_client_success, record_persist_account_error, record_persist_error,
    record_persist_request, record_persist_usage, record_usage_from_json_bytes,
    record_usage_from_sse_payload,
};

const FRONTEND_DIST_DIR: &str = "frontend/dist";
const INVALID_STREAM_FIELD_TYPE_MESSAGE: &str = "stream field must be a boolean when provided";

#[derive(Clone)]
pub struct AppState {
    pub manager: Arc<Manager>,
    pub quota_checker: Arc<QuotaChecker>,
    pub codex_client: Arc<CodexClient>,
    pub request_stats: Arc<RequestStats>,
    pub api_keys: Arc<HashSet<String>>,
    pub max_retry: usize,
    pub refresher: Refresher,
    pub save_queue: SaveQueue,
    pub refresh_concurrency: usize,
    pub runtime_state: Arc<RuntimeStateStore>,
    pub on_401: Option<crate::upstream::codex::On401Hook>,
    pub rate_limiter: Arc<RateLimiter>,
    pub persist_store: Option<Arc<PersistStore>>,
}

#[derive(Debug, Clone)]
struct RequestApiKey(Option<String>);

static ADMIN_AUTH: OnceLock<ArcSwap<AdminAuth>> = OnceLock::new();

pub fn set_admin_auth(auth: Arc<AdminAuth>) {
    admin_auth_swap().store(auth);
}

pub(super) fn admin_auth_swap() -> &'static ArcSwap<AdminAuth> {
    ADMIN_AUTH.get_or_init(|| {
        ArcSwap::from_pointee(AdminAuth::new(
            "config.yaml",
            "admin".to_string(),
            String::new(),
        ))
    })
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

pub(super) fn api_key_from_req(req: &Request<Body>) -> Option<String> {
    req.extensions()
        .get::<RequestApiKey>()
        .and_then(|v| v.0.clone())
}

pub(super) fn request_limit_guard_from_req(req: &Request<Body>) -> Option<Arc<RequestLimitGuard>> {
    req.extensions().get::<Arc<RequestLimitGuard>>().cloned()
}

pub(super) fn parse_stream_field(value: &serde_json::Value) -> Result<bool, &'static str> {
    match value.get("stream") {
        None | Some(serde_json::Value::Null) => Ok(false),
        Some(serde_json::Value::Bool(stream)) => Ok(*stream),
        Some(_) => Err(INVALID_STREAM_FIELD_TYPE_MESSAGE),
    }
}

pub(super) fn bind_response_account_from_value(
    runtime_state: &RuntimeStateStore,
    account: &Account,
    value: &serde_json::Value,
) {
    if let Some(response_id) = extract_response_id_from_value(value) {
        runtime_state.bind_response_account(response_id, account.file_path());
    }
}

pub(super) fn bind_response_account_from_json_bytes(
    runtime_state: &RuntimeStateStore,
    account: &Account,
    bytes: &[u8],
) {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        bind_response_account_from_value(runtime_state, account, &value);
    }
}

fn bind_response_account_from_sse_payload(
    runtime_state: &RuntimeStateStore,
    account: &Account,
    payload: &[u8],
) {
    bind_response_account_from_json_bytes(runtime_state, account, payload);
}

fn extract_response_id_from_value(value: &serde_json::Value) -> Option<&str> {
    value
        .get("response")
        .and_then(|response| response.get("id"))
        .and_then(|id| id.as_str())
        .or_else(|| value.get("id").and_then(|id| id.as_str()))
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

pub(super) fn previous_response_id_from_value(value: &serde_json::Value) -> Option<&str> {
    value
        .get("previous_response_id")
        .and_then(|id| id.as_str())
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

pub(super) fn build_passthrough_sse_response(
    endpoint: &'static str,
    model: String,
    upstream: reqwest::Response,
    account: Arc<Account>,
    runtime_state: Arc<RuntimeStateStore>,
    headers: HeaderMap,
    account_limit_guard: AccountLimitGuard,
    request_limit_guard: Option<Arc<RequestLimitGuard>>,
    persist_store: Option<Arc<PersistStore>>,
    api_key: Option<String>,
) -> Response {
    let status = upstream.status();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(256);

    tokio::spawn(async move {
        let _account_limit_guard = account_limit_guard;
        let _request_limit_guard = request_limit_guard;
        let mut upstream_stream = upstream.bytes_stream();
        let mut parser = sse::SseDataParser::default();
        let mut recorded_usage = false;

        while let Some(chunk) = upstream_stream.next().await {
            let chunk = match chunk {
                Ok(b) => b,
                Err(err) => {
                    log_stream_read_failed(endpoint, &model, account.as_ref(), &err);
                    record_persist_account_error(
                        persist_store.as_ref(),
                        endpoint,
                        &model,
                        true,
                        StatusCode::BAD_GATEWAY,
                        0,
                        api_key.clone(),
                        account.as_ref(),
                        format!("stream read from upstream failed: {err}"),
                        0,
                    );
                    let _ = tx
                        .send(Ok(responses_stream_failed_event(
                            &model,
                            "stream read from upstream failed",
                            "server_error",
                        )))
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

            parser.push(&chunk, |payload| {
                bind_response_account_from_sse_payload(
                    runtime_state.as_ref(),
                    account.as_ref(),
                    payload,
                );
                if recorded_usage {
                    return;
                }
                if let Some(usage) = record_usage_from_sse_payload(
                    account.as_ref(),
                    runtime_state.as_ref(),
                    crate::core::now_unix_ms(),
                    payload,
                ) {
                    record_persist_usage(
                        persist_store.as_ref(),
                        endpoint,
                        &model,
                        api_key.clone(),
                        account.as_ref(),
                        usage,
                    );
                    recorded_usage = true;
                }
            });
        }

        if !recorded_usage {
            parser.finish(|payload| {
                bind_response_account_from_sse_payload(
                    runtime_state.as_ref(),
                    account.as_ref(),
                    payload,
                );
                if let Some(usage) = record_usage_from_sse_payload(
                    account.as_ref(),
                    runtime_state.as_ref(),
                    crate::core::now_unix_ms(),
                    payload,
                ) {
                    record_persist_usage(
                        persist_store.as_ref(),
                        endpoint,
                        &model,
                        api_key.clone(),
                        account.as_ref(),
                        usage,
                    );
                    recorded_usage = true;
                }
            });
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
        .route("/stats", get(management::stats))
        .route("/admin/request-logs", get(management::admin_request_logs))
        .route("/admin/usage-logs", get(management::admin_usage_logs))
        .route(
            "/admin/account-status",
            get(management::admin_account_status),
        )
        .route("/admin/rate-limits", get(management::admin_rate_limits))
        .route("/admin/persistence", get(management::admin_persistence))
        .route("/admin/logout", post(management::admin_logout))
        .route(
            "/admin/change-password",
            post(management::admin_change_password),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            admin_session_auth,
        ));

    let admin_public = Router::new()
        .route("/admin/status", get(management::admin_status))
        .route("/admin/setup", post(management::admin_setup))
        .route("/admin/login", post(management::admin_login));

    let mgmt_sse = Router::new()
        .route("/refresh", post(management::refresh))
        .route("/check-quota", post(management::check_quota))
        .route_layer(middleware::from_fn_with_state(state.clone(), api_key_auth));

    let v1 = Router::new()
        .route(
            "/responses",
            post(responses::v1_responses).get(responses::v1_responses_ws),
        )
        .route("/responses/compact", post(responses::v1_responses_compact))
        .route("/models", get(models::v1_models))
        .route_layer(middleware::from_fn_with_state(state.clone(), api_key_auth));

    let frontend = ServeDir::new(FRONTEND_DIST_DIR)
        .fallback(ServeFile::new(format!("{FRONTEND_DIST_DIR}/index.html")));

    let non_v1 = Router::new()
        .route("/health", get(management::health))
        .merge(mgmt)
        .merge(admin_public)
        .fallback_service(frontend)
        .layer(CompressionLayer::new());

    Router::new()
        .merge(non_v1)
        .merge(mgmt_sse)
        .nest("/v1", v1)
        .layer(middleware::from_fn(cors_and_options))
        .with_state(state)
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
    let mut req = req;
    if state.api_keys.is_empty() {
        req.extensions_mut().insert(RequestApiKey(None));
        match check_request_limits(&state, &req, None) {
            Ok(guard) => {
                req.extensions_mut().insert(Arc::new(guard));
            }
            Err(resp) => return resp,
        }
        return next.run(req).await;
    }

    let (token, token_source) = extract_api_key(req.headers());
    if let Some(token) = token.as_deref() {
        if state.api_keys.contains(token) {
            req.extensions_mut()
                .insert(RequestApiKey(Some(token.to_string())));
            match check_request_limits(&state, &req, Some(token)) {
                Ok(guard) => {
                    req.extensions_mut().insert(Arc::new(guard));
                }
                Err(resp) => return resp,
            }
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

async fn admin_session_auth(
    State(_state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let (token, token_source) = extract_api_key(req.headers());
    if let Some(token) = token.as_deref() {
        if admin_auth_swap().load().is_valid_token(token) {
            return next.run(req).await;
        }
    }

    tracing::debug!(
        path = %req.uri().path(),
        token_source,
        has_authorization = req.headers().get(axum::http::header::AUTHORIZATION).is_some(),
        "admin session auth failed"
    );

    (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": {
                "message": "invalid admin session",
                "type": "invalid_request_error",
                "code": "invalid_admin_session",
            }
        })),
    )
        .into_response()
}

fn check_request_limits(
    state: &AppState,
    _req: &Request<Body>,
    api_key: Option<&str>,
) -> Result<RequestLimitGuard, Response> {
    state.rate_limiter.check_request(api_key).map_err(|err| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({
                "error": {
                    "message": err.message,
                    "type": "rate_limit_error",
                    "code": err.scope,
                }
            })),
        )
            .into_response()
    })
}

pub(super) fn extract_api_key(headers: &HeaderMap) -> (Option<String>, &'static str) {
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

pub(super) fn send_error(status: StatusCode, message: &str, err_type: &str) -> Response {
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

pub(super) fn send_upstream_error(err: UpstreamError) -> Response {
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

pub(super) async fn execute_codex_request(
    state: &AppState,
    model: &str,
    url: url::Url,
    body: Vec<u8>,
    stream: bool,
    passthrough_headers: Option<&HeaderMap>,
    initial_excluded: &HashSet<String>,
) -> Result<UpstreamResponse, UpstreamError> {
    let stable_headers = codex_passthrough_headers_with_stable_session(passthrough_headers, &body);
    state
        .codex_client
        .execute(UpstreamRequest {
            manager: state.manager.as_ref(),
            model,
            url,
            body,
            stream,
            max_retry: state.max_retry,
            passthrough_headers: stable_headers.as_ref(),
            on_401: state.on_401.clone(),
            initial_excluded,
            rate_limiter: state.rate_limiter.as_ref(),
        })
        .await
}

fn codex_passthrough_headers_with_stable_session(
    passthrough_headers: Option<&HeaderMap>,
    body: &[u8],
) -> Option<HeaderMap> {
    let mut headers = passthrough_headers.cloned().unwrap_or_default();
    if headers.get("Session_id").is_none()
        && let Some(seed) = codex_stable_session_seed(passthrough_headers, body)
    {
        let session_id = Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("codex-proxy-rs:codex-session:{seed}").as_bytes(),
        )
        .to_string();
        if let Ok(value) = HeaderValue::from_str(&session_id) {
            headers.insert("Session_id", value);
        }
    }
    if headers.is_empty() {
        None
    } else {
        Some(headers)
    }
}

fn codex_stable_session_seed(
    passthrough_headers: Option<&HeaderMap>,
    body: &[u8],
) -> Option<String> {
    if let Some(seed) = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("prompt_cache_key")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| format!("prompt_cache_key:{value}"))
        })
    {
        return Some(seed);
    }

    for header_name in [
        "Conversation_id",
        "conversation_id",
        "X-Codex-Turn-Metadata",
    ] {
        if let Some(value) = passthrough_headers
            .and_then(|headers| headers.get(header_name))
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(format!("header:{header_name}:{value}"));
        }
    }
    None
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

pub(super) fn log_upstream_request_error(
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

pub(super) fn log_request_completed(
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

pub(super) fn log_response_read_failed(
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

pub(super) fn log_stream_read_failed(
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

fn responses_stream_failed_event(model: &str, message: &str, code: &str) -> Vec<u8> {
    let id = format!("resp_{}", Uuid::new_v4().simple());
    let payload = json!({
        "type": "response.failed",
        "response": {
            "id": id,
            "object": "response",
            "model": model,
            "status": "failed",
            "output": [],
            "error": {
                "code": code,
                "message": message,
            },
        },
    });
    format!("event: response.failed\ndata: {payload}\n\n").into_bytes()
}

pub(super) fn extract_codex_passthrough_headers(headers: &HeaderMap) -> Option<HeaderMap> {
    let mut passthrough = HeaderMap::new();
    for name in [
        "Version",
        "Session_id",
        "X-Session-ID",
        "Conversation_id",
        "conversation_id",
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

pub(super) fn validate_function_call_output_context(v: &serde_json::Value) -> Result<(), String> {
    validate_function_call_output_context_with_known_ids(v, &HashSet::new())
}

pub(super) fn validate_function_call_output_context_with_known_ids(
    v: &serde_json::Value,
    known_call_ids: &HashSet<String>,
) -> Result<(), String> {
    let Some(input) = v.get("input").and_then(|v| v.as_array()) else {
        return Ok(());
    };

    let mut has_tool_call_output = false;
    let mut has_tool_call_context = false;
    for item in input {
        let item_type = item
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if is_codex_tool_output_item_type(item_type) {
            has_tool_call_output = true;
        } else if is_codex_tool_call_context_item_type(item_type)
            && !item
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .is_empty()
        {
            has_tool_call_context = true;
        }
        if has_tool_call_output && has_tool_call_context {
            return Ok(());
        }
    }

    if !has_tool_call_output || has_tool_call_context {
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
        let item_type = item
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if is_codex_tool_output_item_type(item_type) {
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
        } else if item_type == "item_reference" {
            let id = item
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim();
            if !id.is_empty() {
                reference_ids.insert(id.to_string());
            }
        }
    }

    if missing_call_id {
        return Err("tool output items require call_id on HTTP requests; continuation via previous_response_id requires a response id".to_string());
    }

    if !call_ids.is_empty()
        && call_ids
            .iter()
            .all(|call_id| reference_ids.contains(call_id) || known_call_ids.contains(call_id))
    {
        return Ok(());
    }

    Err("tool output items require matching tool call context, item_reference ids, or previous_response_id on HTTP requests".to_string())
}

fn is_codex_tool_call_context_item_type(typ: &str) -> bool {
    matches!(
        typ.trim(),
        "function_call"
            | "tool_call"
            | "local_shell_call"
            | "tool_search_call"
            | "custom_tool_call"
            | "mcp_tool_call"
    )
}

fn is_codex_tool_output_item_type(typ: &str) -> bool {
    matches!(
        typ.trim(),
        "function_call_output"
            | "tool_search_output"
            | "custom_tool_call_output"
            | "mcp_tool_call_output"
    )
}

pub(super) fn trim_ascii(input: &[u8]) -> &[u8] {
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

#[cfg(test)]
mod tests {
    use super::*;
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
            refresher: Refresher::new("").unwrap(),
            save_queue: SaveQueue::start(1),
            refresh_concurrency: 1,
            runtime_state: Arc::new(RuntimeStateStore::new(dir.path())),
            on_401: None,
            rate_limiter: Arc::new(RateLimiter::default()),
            persist_store: None,
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
            refresher: Refresher::new("").unwrap(),
            save_queue: SaveQueue::start(1),
            refresh_concurrency: 1,
            runtime_state: Arc::new(RuntimeStateStore::new(dir.path())),
            on_401: None,
            rate_limiter: Arc::new(RateLimiter::default()),
            persist_store: None,
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
        assert!(err.contains("tool output"));
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
