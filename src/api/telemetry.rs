use std::sync::Arc;

use axum::http::StatusCode;

use crate::core::Account;
use crate::persist::{PersistStore, RequestLogInput, UsageLogInput};
use crate::state::RuntimeStateStore;

use super::{RequestStats, trim_ascii};

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct UsageTokens {
    pub(super) input_tokens: i64,
    pub(super) output_tokens: i64,
    pub(super) cached_tokens: i64,
    pub(super) reasoning_tokens: i64,
    pub(super) total_tokens: i64,
}

impl UsageTokens {
    pub(super) fn has_activity(&self) -> bool {
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

pub(super) fn record_hourly_usage(
    runtime_state: &RuntimeStateStore,
    now_ms: i64,
    usage: UsageTokens,
) {
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

pub(super) fn record_client_success(
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

pub(super) fn record_persist_request(
    persist_store: Option<&Arc<PersistStore>>,
    endpoint: &'static str,
    model: &str,
    stream: bool,
    status: StatusCode,
    attempts: usize,
    api_key: Option<String>,
    account: Option<&Account>,
    duration_ms: i64,
) {
    let Some(store) = persist_store else {
        return;
    };
    store.record_request(RequestLogInput {
        ts_ms: crate::core::now_unix_ms(),
        endpoint: endpoint.to_string(),
        model: model.to_string(),
        stream,
        status: status.as_u16(),
        attempts,
        api_key,
        account_file_path: account.map(|a| a.file_path().to_string()),
        duration_ms,
        ..RequestLogInput::default()
    });
    if let Some(account) = account {
        let snap = account.stats_snapshot();
        store.record_account_status((&snap).into());
    }
}

pub(super) fn record_persist_error(
    persist_store: Option<&Arc<PersistStore>>,
    endpoint: &'static str,
    model: &str,
    stream: bool,
    status: StatusCode,
    api_key: Option<String>,
    message: String,
    duration_ms: i64,
) {
    let Some(store) = persist_store else {
        return;
    };
    store.record_request(RequestLogInput {
        ts_ms: crate::core::now_unix_ms(),
        endpoint: endpoint.to_string(),
        model: model.to_string(),
        stream,
        status: status.as_u16(),
        api_key,
        error_type: Some("api_error".to_string()),
        error_message: Some(message),
        duration_ms,
        ..RequestLogInput::default()
    });
}

pub(super) fn record_persist_account_error(
    persist_store: Option<&Arc<PersistStore>>,
    endpoint: &'static str,
    model: &str,
    stream: bool,
    status: StatusCode,
    attempts: usize,
    api_key: Option<String>,
    account: &Account,
    message: String,
    duration_ms: i64,
) {
    let Some(store) = persist_store else {
        return;
    };
    store.record_request(RequestLogInput {
        ts_ms: crate::core::now_unix_ms(),
        endpoint: endpoint.to_string(),
        model: model.to_string(),
        stream,
        status: status.as_u16(),
        attempts,
        api_key,
        account_file_path: Some(account.file_path().to_string()),
        error_type: Some("api_error".to_string()),
        error_message: Some(message),
        duration_ms,
    });
    let snap = account.stats_snapshot();
    store.record_account_status((&snap).into());
}

pub(super) fn record_persist_usage(
    persist_store: Option<&Arc<PersistStore>>,
    endpoint: &'static str,
    model: &str,
    api_key: Option<String>,
    account: &Account,
    usage: UsageTokens,
) {
    if !usage.has_activity() {
        return;
    }
    let Some(store) = persist_store else {
        return;
    };
    store.record_usage(UsageLogInput {
        ts_ms: crate::core::now_unix_ms(),
        endpoint: endpoint.to_string(),
        model: model.to_string(),
        api_key,
        account_file_path: account.file_path().to_string(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_tokens: usage.cached_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        total_tokens: usage.total_tokens,
    });
    let snap = account.stats_snapshot();
    store.record_account_status((&snap).into());
}

pub(super) fn extract_usage_tokens(value: &serde_json::Value) -> Option<UsageTokens> {
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
) -> Option<UsageTokens> {
    let Some(usage) = extract_usage_tokens(value) else {
        return None;
    };
    account.record_usage_detail(
        usage.input_tokens,
        usage.output_tokens,
        usage.cached_tokens,
        usage.reasoning_tokens,
        usage.total_tokens,
    );
    record_hourly_usage(runtime_state, now_ms, usage);
    Some(usage)
}

pub(super) fn record_usage_from_json_bytes(
    account: &Account,
    runtime_state: &RuntimeStateStore,
    now_ms: i64,
    bytes: &[u8],
) -> Option<UsageTokens> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return None;
    };
    record_usage_from_value(account, runtime_state, now_ms, &value)
}

pub(super) fn record_usage_from_sse_line(
    account: &Account,
    runtime_state: &RuntimeStateStore,
    now_ms: i64,
    line: &[u8],
) -> Option<UsageTokens> {
    if !line.starts_with(b"data:") {
        return None;
    }
    let payload = trim_ascii(&line[5..]);
    if payload.is_empty() || payload == b"[DONE]" {
        return None;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return None;
    };
    record_usage_from_value(account, runtime_state, now_ms, &value)
}

pub(super) fn record_usage_from_sse_bytes(
    account: &Account,
    runtime_state: &RuntimeStateStore,
    now_ms: i64,
    bytes: &[u8],
) -> Option<UsageTokens> {
    for line in bytes.split(|b| *b == b'\n') {
        let line = trim_ascii(line);
        if line.is_empty() {
            continue;
        }
        if let Some(usage) = record_usage_from_sse_line(account, runtime_state, now_ms, line) {
            return Some(usage);
        }
    }
    None
}

pub(super) fn record_stream_account_failure(
    persist_store: Option<&Arc<PersistStore>>,
    endpoint: &'static str,
    model: &str,
    attempts: usize,
    api_key: Option<String>,
    account: &Account,
    runtime_state: &RuntimeStateStore,
    message: String,
    started_ms: i64,
) {
    let now_ms = crate::core::now_unix_ms();
    account.record_failure(now_ms);
    account.record_client_failure();
    runtime_state.mark_dirty();
    record_persist_account_error(
        persist_store,
        endpoint,
        model,
        true,
        StatusCode::BAD_GATEWAY,
        attempts,
        api_key,
        account,
        message,
        now_ms.saturating_sub(started_ms),
    );
}
