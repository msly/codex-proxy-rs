use std::collections::{HashMap, HashSet};

use axum::http::HeaderMap;

use crate::core::Account;
use crate::state::RuntimeStateStore;

use super::{AppState, validate_function_call_output_context_with_known_ids};

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
