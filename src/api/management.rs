use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::unfold;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::core::Manager;
use crate::quota::QuotaChecker;
use crate::refresh::{Refresher, SaveQueue, refresh_account};

use super::{AppState, admin_auth_swap, extract_api_key, send_error};

#[derive(Debug, Serialize)]
pub(super) struct HealthResponse {
    status: &'static str,
    accounts: usize,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminLoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminSetupRequest {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct AdminChangePasswordRequest {
    current_password: String,
    new_password: String,
}

pub(super) async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        accounts: state.manager.account_count(),
    })
}

pub(super) async fn check_quota(
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

pub(super) async fn refresh(
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
pub(super) struct StatsResponse {
    summary: StatsSummary,
    trend: StatsTrend,
    accounts: Vec<StatsAccount>,
}

#[derive(Debug, serde::Deserialize)]
pub(super) struct LogQuery {
    #[serde(default = "default_log_limit")]
    limit: usize,
}

fn default_log_limit() -> usize {
    100
}

pub(super) async fn admin_request_logs(
    State(state): State<AppState>,
    Query(query): Query<LogQuery>,
) -> Response {
    let Some(store) = state.persist_store.as_ref() else {
        return send_error(
            StatusCode::NOT_FOUND,
            "persistence is disabled",
            "not_found",
        );
    };
    match store.list_request_logs(query.limit).await {
        Ok(data) => Json(json!({ "data": data })).into_response(),
        Err(err) => send_error(StatusCode::INTERNAL_SERVER_ERROR, &err, "server_error"),
    }
}

pub(super) async fn admin_status() -> Response {
    Json(json!({ "data": admin_auth_swap().load().status() })).into_response()
}

pub(super) async fn admin_setup(Json(input): Json<AdminSetupRequest>) -> Response {
    let auth = admin_auth_swap().load();
    match auth.setup(&input.username, &input.password) {
        Ok(token) => Json(json!({
            "data": {
                "token": token,
                "status": auth.status(),
            }
        }))
        .into_response(),
        Err(err) => send_error(StatusCode::BAD_REQUEST, &err, "invalid_request_error"),
    }
}

pub(super) async fn admin_login(Json(input): Json<AdminLoginRequest>) -> Response {
    let auth = admin_auth_swap().load();
    match auth.login(&input.username, &input.password) {
        Ok(token) => Json(json!({
            "data": {
                "token": token,
                "status": auth.status(),
            }
        }))
        .into_response(),
        Err(err) => send_error(StatusCode::UNAUTHORIZED, &err, "invalid_request_error"),
    }
}

pub(super) async fn admin_logout(headers: HeaderMap) -> Response {
    if let (Some(token), _) = extract_api_key(&headers) {
        admin_auth_swap().load().logout(&token);
    }
    Json(json!({ "data": { "ok": true } })).into_response()
}

pub(super) async fn admin_change_password(
    headers: HeaderMap,
    Json(input): Json<AdminChangePasswordRequest>,
) -> Response {
    let Some((token, _)) = extract_api_key(&headers).0.map(|token| (token, ())) else {
        return send_error(
            StatusCode::UNAUTHORIZED,
            "invalid admin session",
            "invalid_request_error",
        );
    };
    let auth = admin_auth_swap().load();
    match auth.change_password(&token, &input.current_password, &input.new_password) {
        Ok(()) => Json(json!({ "data": auth.status() })).into_response(),
        Err(err) => send_error(StatusCode::BAD_REQUEST, &err, "invalid_request_error"),
    }
}

pub(super) async fn admin_usage_logs(
    State(state): State<AppState>,
    Query(query): Query<LogQuery>,
) -> Response {
    let Some(store) = state.persist_store.as_ref() else {
        return send_error(
            StatusCode::NOT_FOUND,
            "persistence is disabled",
            "not_found",
        );
    };
    match store.list_usage_logs(query.limit).await {
        Ok(data) => Json(json!({ "data": data })).into_response(),
        Err(err) => send_error(StatusCode::INTERNAL_SERVER_ERROR, &err, "server_error"),
    }
}

pub(super) async fn admin_account_status(State(state): State<AppState>) -> Response {
    let Some(store) = state.persist_store.as_ref() else {
        return send_error(
            StatusCode::NOT_FOUND,
            "persistence is disabled",
            "not_found",
        );
    };
    match store.list_account_status().await {
        Ok(data) => Json(json!({ "data": data })).into_response(),
        Err(err) => send_error(StatusCode::INTERNAL_SERVER_ERROR, &err, "server_error"),
    }
}

pub(super) async fn admin_rate_limits(State(state): State<AppState>) -> Response {
    Json(json!({ "data": state.rate_limiter.snapshot() })).into_response()
}

pub(super) async fn admin_persistence(State(state): State<AppState>) -> Response {
    let Some(store) = state.persist_store.as_ref() else {
        return Json(json!({
            "data": {
                "enabled": false,
                "writer_running": false,
                "dropped_events": 0,
                "write_errors": 0,
            }
        }))
        .into_response();
    };
    let status = store.status();
    Json(json!({
        "data": {
            "enabled": true,
            "writer_running": status.writer_running,
            "dropped_events": status.dropped_events,
            "write_errors": status.write_errors,
        }
    }))
    .into_response()
}

pub(super) async fn stats(State(state): State<AppState>) -> Json<StatsResponse> {
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
        if let Some(store) = state.persist_store.as_ref() {
            store.record_account_status((&snap).into());
        }
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
