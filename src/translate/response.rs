use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const DATA_PREFIX: &[u8] = b"data:";

#[derive(Debug, Clone)]
pub struct StreamState {
    pub response_id: String,
    pub created_at: i64,
    pub model: String,
    pub function_call_index: i64,
    pub has_text: bool,
    pub has_tool_call: bool,
    pub has_reasoning: bool,
    pub completed: bool,
    pub usage_input: i64,
    pub usage_output: i64,
    pub usage_cached: i64,
    pub usage_reasoning: i64,
    pub usage_total: i64,
    has_received_args_delta: bool,
    has_tool_call_announced: bool,
    reasoning_delta_by_item: HashMap<String, String>,
    has_reasoning_summary_delta: bool,
}

impl StreamState {
    pub fn new(model: &str) -> Self {
        Self {
            response_id: String::new(),
            created_at: 0,
            model: model.to_string(),
            function_call_index: -1,
            has_text: false,
            has_tool_call: false,
            has_reasoning: false,
            completed: false,
            usage_input: 0,
            usage_output: 0,
            usage_cached: 0,
            usage_reasoning: 0,
            usage_total: 0,
            has_received_args_delta: false,
            has_tool_call_announced: false,
            reasoning_delta_by_item: HashMap::new(),
            has_reasoning_summary_delta: false,
        }
    }
}

pub fn convert_stream_chunk(
    raw_line: &[u8],
    state: &mut StreamState,
    reverse_tool_map: &HashMap<String, String>,
) -> Vec<String> {
    let line = raw_line.trim_ascii();
    if !line.starts_with(DATA_PREFIX) {
        return Vec::new();
    }
    let payload = line[DATA_PREFIX.len()..].trim_ascii();
    if payload.is_empty() || payload == b"[DONE]" {
        return Vec::new();
    }

    let root: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let typ = root.get("type").and_then(Value::as_str).unwrap_or_default();
    if typ == "response.created" {
        let resp = root.get("response").unwrap_or(&Value::Null);
        state.response_id = resp
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        state.created_at = resp.get("created_at").and_then(Value::as_i64).unwrap_or(0);
        if let Some(m) = resp.get("model").and_then(Value::as_str) {
            if !m.is_empty() {
                state.model = m.to_string();
            }
        }
        state.reasoning_delta_by_item.clear();
        state.has_reasoning = false;
        state.has_reasoning_summary_delta = false;
        state.usage_input = 0;
        state.usage_output = 0;
        state.usage_cached = 0;
        state.usage_reasoning = 0;
        state.usage_total = 0;
        return Vec::new();
    }

    let mut chunk = base_chunk(state);

    match typ {
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = root.get("delta").and_then(Value::as_str) {
                if delta.is_empty() {
                    return Vec::new();
                }
                state.has_reasoning_summary_delta = true;
                state.has_reasoning = true;
                set_delta_role_assistant(&mut chunk);
                set_choice_delta(
                    &mut chunk,
                    "reasoning_content",
                    Value::String(delta.to_string()),
                );
                return vec![chunk.to_string()];
            }
            Vec::new()
        }
        "response.reasoning_summary_text.done" => {
            set_delta_role_assistant(&mut chunk);
            let text = root.get("text").and_then(Value::as_str).unwrap_or_default();
            if !text.is_empty() && !state.has_reasoning_summary_delta {
                state.has_reasoning = true;
                set_choice_delta(
                    &mut chunk,
                    "reasoning_content",
                    Value::String(text.to_string()),
                );
            } else {
                set_choice_delta(
                    &mut chunk,
                    "reasoning_content",
                    Value::String("\n\n".to_string()),
                );
            }
            vec![chunk.to_string()]
        }
        "response.reasoning.delta" | "response.reasoning_text.delta" => {
            let item_key = reasoning_item_key(&root);
            let delta = root
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            state
                .reasoning_delta_by_item
                .entry(item_key)
                .or_default()
                .push_str(delta);
            if delta.is_empty() {
                return Vec::new();
            }
            state.has_reasoning = true;
            set_delta_role_assistant(&mut chunk);
            set_choice_delta(
                &mut chunk,
                "reasoning_content",
                Value::String(delta.to_string()),
            );
            vec![chunk.to_string()]
        }
        "response.reasoning_text.done" => {
            let full = root.get("text").and_then(Value::as_str).unwrap_or_default();
            if full.is_empty() {
                return Vec::new();
            }

            let item_key = reasoning_item_key(&root);
            let accumulated = state
                .reasoning_delta_by_item
                .remove(&item_key)
                .unwrap_or_default();

            let to_emit = if accumulated.is_empty() {
                full.to_string()
            } else if full.starts_with(&accumulated) && full.len() > accumulated.len() {
                full[accumulated.len()..].to_string()
            } else if full != accumulated {
                full.to_string()
            } else {
                String::new()
            };

            if to_emit.is_empty() {
                return Vec::new();
            }

            state.has_reasoning = true;
            set_delta_role_assistant(&mut chunk);
            set_choice_delta(&mut chunk, "reasoning_content", Value::String(to_emit));
            vec![chunk.to_string()]
        }
        "response.content_part.added" => {
            let part = root.get("part").unwrap_or(&Value::Null);
            if part.get("type").and_then(Value::as_str) == Some("reasoning_text") {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        state.has_reasoning = true;
                        set_delta_role_assistant(&mut chunk);
                        set_choice_delta(
                            &mut chunk,
                            "reasoning_content",
                            Value::String(text.to_string()),
                        );
                        return vec![chunk.to_string()];
                    }
                }
            }
            Vec::new()
        }
        "response.output_text.delta" => {
            if let Some(delta) = root.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    state.has_text = true;
                }
                set_delta_role_assistant(&mut chunk);
                set_choice_delta(&mut chunk, "content", Value::String(delta.to_string()));
                return vec![chunk.to_string()];
            }
            Vec::new()
        }
        "response.output_item.added" => {
            let item = root.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                return Vec::new();
            }

            state.has_tool_call = true;
            state.function_call_index += 1;
            state.has_received_args_delta = false;
            state.has_tool_call_announced = true;

            let mut name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if let Some(orig) = reverse_tool_map.get(&name) {
                name = orig.clone();
            }

            let tool_call = json!({
                "index": state.function_call_index,
                "id": item.get("call_id").and_then(Value::as_str).unwrap_or_default(),
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": ""
                }
            });

            set_delta_role_assistant(&mut chunk);
            set_choice_delta(&mut chunk, "tool_calls", Value::Array(vec![tool_call]));
            vec![chunk.to_string()]
        }
        "response.function_call_arguments.delta" => {
            state.has_tool_call = true;
            state.has_received_args_delta = true;
            let tool_call = json!({
                "index": state.function_call_index,
                "function": {
                    "arguments": root.get("delta").and_then(Value::as_str).unwrap_or_default()
                }
            });
            set_choice_delta(&mut chunk, "tool_calls", Value::Array(vec![tool_call]));
            vec![chunk.to_string()]
        }
        "response.function_call_arguments.done" => {
            state.has_tool_call = true;
            if state.has_received_args_delta {
                return Vec::new();
            }
            let tool_call = json!({
                "index": state.function_call_index,
                "function": {
                    "arguments": root.get("arguments").and_then(Value::as_str).unwrap_or_default()
                }
            });
            set_choice_delta(&mut chunk, "tool_calls", Value::Array(vec![tool_call]));
            vec![chunk.to_string()]
        }
        "response.output_item.done" => {
            let item = root.get("item").unwrap_or(&Value::Null);
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                return Vec::new();
            }

            state.has_tool_call = true;
            if state.has_tool_call_announced {
                state.has_tool_call_announced = false;
                return Vec::new();
            }

            state.function_call_index += 1;

            let mut name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if let Some(orig) = reverse_tool_map.get(&name) {
                name = orig.clone();
            }

            let tool_call = json!({
                "index": state.function_call_index,
                "id": item.get("call_id").and_then(Value::as_str).unwrap_or_default(),
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": item.get("arguments").and_then(Value::as_str).unwrap_or_default()
                }
            });

            set_delta_role_assistant(&mut chunk);
            set_choice_delta(&mut chunk, "tool_calls", Value::Array(vec![tool_call]));
            vec![chunk.to_string()]
        }
        "response.completed" => {
            state.completed = true;
            let finish_reason = if state.function_call_index != -1 {
                "tool_calls"
            } else {
                "stop"
            };
            set_finish_reason(&mut chunk, finish_reason);

            if let Some(usage) = root.get("response").and_then(|r| r.get("usage")) {
                state.usage_input = usage
                    .get("input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                state.usage_output = usage
                    .get("output_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                state.usage_cached = usage
                    .get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                state.usage_reasoning = usage
                    .get("output_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                state.usage_total = usage
                    .get("total_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);

                let mut usage_out = json!({});
                if let Some(v) = usage.get("output_tokens").and_then(Value::as_i64) {
                    usage_out["completion_tokens"] = Value::Number(v.into());
                }
                if let Some(v) = usage.get("total_tokens").and_then(Value::as_i64) {
                    usage_out["total_tokens"] = Value::Number(v.into());
                }
                if let Some(v) = usage.get("input_tokens").and_then(Value::as_i64) {
                    usage_out["prompt_tokens"] = Value::Number(v.into());
                }

                if let Some(v) = usage
                    .get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(Value::as_i64)
                {
                    usage_out["prompt_tokens_details"] = json!({ "cached_tokens": v });
                }
                if let Some(v) = usage
                    .get("output_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))
                    .and_then(Value::as_i64)
                {
                    usage_out["completion_tokens_details"] = json!({ "reasoning_tokens": v });
                }

                chunk["usage"] = usage_out;
            }

            vec![chunk.to_string()]
        }
        _ => {
            if typ.contains("reasoning") && typ.ends_with(".delta") {
                let delta = root
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if delta.is_empty() {
                    return Vec::new();
                }
                state.has_reasoning = true;
                set_delta_role_assistant(&mut chunk);
                set_choice_delta(
                    &mut chunk,
                    "reasoning_content",
                    Value::String(delta.to_string()),
                );
                return vec![chunk.to_string()];
            }
            Vec::new()
        }
    }
}

pub fn convert_non_stream_response(
    raw_json: &[u8],
    reverse_tool_map: &HashMap<String, String>,
) -> (String, bool) {
    let root: Value = match serde_json::from_slice(raw_json) {
        Ok(v) => v,
        Err(_) => return (String::new(), false),
    };
    if root.get("type").and_then(Value::as_str) != Some("response.completed") {
        return (String::new(), false);
    }

    let resp = root.get("response").unwrap_or(&Value::Null);
    let id = resp.get("id").and_then(Value::as_str).unwrap_or_default();
    let model = resp
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let created = resp
        .get("created_at")
        .and_then(Value::as_i64)
        .unwrap_or_else(now_unix);

    let mut out = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": Value::Null,
                "reasoning_content": Value::Null,
                "tool_calls": Value::Null,
            },
            "finish_reason": Value::Null,
        }]
    });

    if let Some(usage) = resp.get("usage") {
        let mut usage_out = json!({});
        if let Some(v) = usage.get("output_tokens").and_then(Value::as_i64) {
            usage_out["completion_tokens"] = Value::Number(v.into());
        }
        if let Some(v) = usage.get("total_tokens").and_then(Value::as_i64) {
            usage_out["total_tokens"] = Value::Number(v.into());
        }
        if let Some(v) = usage.get("input_tokens").and_then(Value::as_i64) {
            usage_out["prompt_tokens"] = Value::Number(v.into());
        }
        if let Some(v) = usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_i64)
        {
            usage_out["prompt_tokens_details"] = json!({ "cached_tokens": v });
        }
        if let Some(v) = usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_i64)
        {
            usage_out["completion_tokens_details"] = json!({ "reasoning_tokens": v });
        }
        out["usage"] = usage_out;
    }

    let mut reasoning = String::new();
    if let Some(t) = resp
        .get("reasoning_summary")
        .and_then(|v| v.get("text"))
        .and_then(Value::as_str)
    {
        if !t.is_empty() {
            reasoning.push_str(t);
        }
    }
    if reasoning.is_empty() {
        if let Some(t) = resp.get("reasoning_summary").and_then(Value::as_str) {
            if !t.is_empty() {
                reasoning.push_str(t);
            }
        }
    }

    let mut has_output = false;

    if let Some(output) = resp.get("output").and_then(Value::as_array) {
        let mut content = String::new();
        let mut tool_calls: Vec<Value> = Vec::new();

        for item in output {
            match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                "reasoning" => {
                    if let Some(summary) = item.get("summary").and_then(Value::as_array) {
                        for si in summary {
                            if si.get("type").and_then(Value::as_str) == Some("summary_text") {
                                if let Some(t) = si.get("text").and_then(Value::as_str) {
                                    if !t.is_empty() {
                                        reasoning.push_str(t);
                                    }
                                }
                            }
                        }
                    }
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            match part.get("type").and_then(Value::as_str).unwrap_or_default() {
                                "reasoning_text" | "text" => {
                                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                                        if !t.is_empty() {
                                            reasoning.push_str(t);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    if let Some(t) = item.get("text").and_then(Value::as_str) {
                        if !t.is_empty() {
                            reasoning.push_str(t);
                        }
                    }
                }
                "reasoning_text" => {
                    if let Some(t) = item.get("text").and_then(Value::as_str) {
                        if !t.is_empty() {
                            reasoning.push_str(t);
                        }
                    }
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                if !t.is_empty() {
                                    reasoning.push_str(t);
                                }
                            }
                        }
                    }
                }
                "content_part" => {
                    let part = item.get("part").unwrap_or(&Value::Null);
                    if part.get("type").and_then(Value::as_str) == Some("reasoning_text") {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            if !t.is_empty() {
                                reasoning.push_str(t);
                            }
                        }
                    }
                }
                "message" => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for ci in parts {
                            if ci.get("type").and_then(Value::as_str) == Some("output_text") {
                                if let Some(t) = ci.get("text").and_then(Value::as_str) {
                                    if !t.is_empty() {
                                        content.push_str(t);
                                    }
                                }
                            }
                        }
                    }
                }
                "function_call" => {
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let mut name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if let Some(orig) = reverse_tool_map.get(&name) {
                        name = orig.clone();
                    }
                    let args = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    tool_calls.push(json!({
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": args,
                        }
                    }));
                }
                _ => {}
            }
        }

        if !content.is_empty() {
            has_output = true;
            out["choices"][0]["message"]["content"] = Value::String(content);
        }
        if !tool_calls.is_empty() {
            has_output = true;
            out["choices"][0]["message"]["tool_calls"] = Value::Array(tool_calls);
        }
    }

    if !reasoning.is_empty() {
        has_output = true;
        out["choices"][0]["message"]["reasoning_content"] = Value::String(reasoning);
    }

    if resp.get("status").and_then(Value::as_str) == Some("completed") {
        out["choices"][0]["finish_reason"] = Value::String("stop".to_string());
    }

    (out.to_string(), has_output)
}

fn reasoning_item_key(root: &Value) -> String {
    if let Some(item_id) = root.get("item_id").and_then(Value::as_str) {
        if !item_id.is_empty() {
            return item_id.to_string();
        }
    }
    format!(
        "_idx:{}",
        root.get("output_index")
            .and_then(Value::as_i64)
            .unwrap_or(0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_stream_reasoning_text_done_emits_when_no_delta() {
        let mut state = StreamState::new("gpt-5.4");
        let reverse = HashMap::new();

        let _ = convert_stream_chunk(
            br#"data: {"type":"response.created","response":{"id":"r1","created_at":1,"model":"gpt-5.4"}}"#,
            &mut state,
            &reverse,
        );
        let out = convert_stream_chunk(
            br#"data: {"type":"response.reasoning_text.done","item_id":"it1","text":"chain"}"#,
            &mut state,
            &reverse,
        );

        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains(r#""reasoning_content":"chain""#),
            "{}",
            out[0]
        );
        assert!(state.has_reasoning);
    }

    #[test]
    fn response_non_stream_reasoning_only_counts_as_output() {
        let reverse = HashMap::new();
        let raw = br#"{
          "type":"response.completed",
          "response":{
            "id":"r1",
            "created_at":1,
            "model":"gpt-5.4",
            "status":"completed",
            "reasoning_summary":{"text":"think"},
            "usage":{"input_tokens":1,"output_tokens":0,"total_tokens":1}
          }
        }"#;

        let (out, has_output) = convert_non_stream_response(raw, &reverse);
        assert!(has_output);
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["choices"][0]["message"]["reasoning_content"], "think");
    }
}

fn base_chunk(state: &StreamState) -> Value {
    json!({
        "id": state.response_id,
        "object": "chat.completion.chunk",
        "created": state.created_at,
        "model": state.model,
        "choices": [{
            "index": 0,
            "delta": {
                "role": Value::Null,
                "content": Value::Null,
                "reasoning_content": Value::Null,
                "tool_calls": Value::Null,
            },
            "finish_reason": Value::Null,
        }]
    })
}

fn set_choice_delta(chunk: &mut Value, field: &str, value: Value) {
    if let Some(delta) = chunk
        .get_mut("choices")
        .and_then(Value::as_array_mut)
        .and_then(|a| a.get_mut(0))
        .and_then(|c| c.get_mut("delta"))
        .and_then(Value::as_object_mut)
    {
        delta.insert(field.to_string(), value);
    }
}

fn set_delta_role_assistant(chunk: &mut Value) {
    set_choice_delta(chunk, "role", Value::String("assistant".to_string()));
}

fn set_finish_reason(chunk: &mut Value, reason: &str) {
    if let Some(choice) = chunk
        .get_mut("choices")
        .and_then(Value::as_array_mut)
        .and_then(|a| a.get_mut(0))
        .and_then(Value::as_object_mut)
    {
        choice.insert(
            "finish_reason".to_string(),
            Value::String(reason.to_string()),
        );
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
