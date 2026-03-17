use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

pub fn convert_claude_request_to_openai(claude_body: &[u8]) -> (Vec<u8>, String, bool) {
    let v: Value =
        serde_json::from_slice(claude_body).unwrap_or_else(|_| Value::Object(Map::new()));

    let model = v
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let stream = v.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let mut out = json!({
        "model": model,
        "stream": stream,
        "messages": []
    });

    if let Some(v) = v.get("max_tokens").and_then(Value::as_i64) {
        set(&mut out, &["max_tokens"], Value::Number(v.into()));
    }
    if let Some(v) = v.get("temperature").and_then(Value::as_f64) {
        if let Some(n) = serde_json::Number::from_f64(v) {
            set(&mut out, &["temperature"], Value::Number(n));
        }
    }
    if let Some(v) = v.get("top_p").and_then(Value::as_f64) {
        if let Some(n) = serde_json::Number::from_f64(v) {
            set(&mut out, &["top_p"], Value::Number(n));
        }
    }

    // system -> messages[0] role=system
    if let Some(sys) = v.get("system") {
        let text = extract_text_from_claude_text_or_array(sys);
        let msg = json!({
            "role": "system",
            "content": text,
        });
        push_message(&mut out, msg);
    }

    // messages
    if let Some(messages) = v.get("messages").and_then(Value::as_array) {
        for m in messages {
            let role = m.get("role").and_then(Value::as_str).unwrap_or_default();
            let content = m.get("content");

            let mut msg = json!({
                "role": role,
            });

            if let Some(Value::String(s)) = content {
                set(&mut msg, &["content"], Value::String(s.clone()));
                push_message(&mut out, msg);
                continue;
            }

            if let Some(Value::Array(blocks)) = content {
                let mut content_parts = Vec::<Value>::new();
                let mut tool_calls = Vec::<Value>::new();

                let mut has_non_tool_result = false;
                for block in blocks {
                    let block_type = block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    match block_type {
                        "text" => {
                            has_non_tool_result = true;
                            let text = block
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            content_parts.push(json!({"type":"text","text":text}));
                        }
                        "image" => {
                            has_non_tool_result = true;
                            if let Some(src) = block.get("source") {
                                if src.get("type").and_then(Value::as_str) == Some("base64") {
                                    let media_type = src
                                        .get("media_type")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    let data =
                                        src.get("data").and_then(Value::as_str).unwrap_or_default();
                                    if !media_type.is_empty() && !data.is_empty() {
                                        let url = format!("data:{media_type};base64,{data}");
                                        content_parts.push(
                                            json!({"type":"image_url","image_url":{"url":url}}),
                                        );
                                    }
                                }
                            }
                        }
                        "tool_use" => {
                            has_non_tool_result = true;
                            let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let args = block.get("input").cloned().unwrap_or_else(|| json!({}));
                            let arguments =
                                serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": arguments,
                                }
                            }));
                        }
                        "tool_result" => {
                            let tool_use_id = block
                                .get("tool_use_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let content = block.get("content");

                            let mut text = String::new();
                            match content {
                                Some(Value::String(s)) => text = s.clone(),
                                Some(Value::Array(items)) => {
                                    for item in items {
                                        if item.get("type").and_then(Value::as_str) == Some("text")
                                        {
                                            if let Some(t) =
                                                item.get("text").and_then(Value::as_str)
                                            {
                                                text.push_str(t);
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }

                            let tool_msg = json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": text,
                            });
                            push_message(&mut out, tool_msg);
                        }
                        _ => {}
                    }
                }

                if role == "user" && !has_non_tool_result {
                    continue;
                }

                if !tool_calls.is_empty() {
                    set(&mut msg, &["tool_calls"], Value::Array(tool_calls));
                    if content_parts.is_empty() {
                        set(&mut msg, &["content"], Value::Null);
                    } else {
                        set(&mut msg, &["content"], Value::Array(content_parts));
                    }
                } else {
                    set(&mut msg, &["content"], Value::Array(content_parts));
                }

                push_message(&mut out, msg);
                continue;
            }

            push_message(&mut out, msg);
        }
    }

    // tools: Claude -> OpenAI
    if let Some(tools) = v.get("tools").and_then(Value::as_array) {
        if !tools.is_empty() {
            let mut out_tools = Vec::<Value>::new();
            for t in tools {
                let name = t.get("name").and_then(Value::as_str).unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                let mut tool = json!({"type":"function","function":{"name": name}});
                if let Some(desc) = t.get("description").and_then(Value::as_str) {
                    if !desc.is_empty() {
                        set(
                            &mut tool,
                            &["function", "description"],
                            Value::String(desc.to_string()),
                        );
                    }
                }
                if let Some(schema) = t.get("input_schema") {
                    set(&mut tool, &["function", "parameters"], schema.clone());
                }
                out_tools.push(tool);
            }
            if !out_tools.is_empty() {
                set(&mut out, &["tools"], Value::Array(out_tools));
            }
        }
    }

    // tool_choice
    if let Some(tc) = v.get("tool_choice").and_then(Value::as_object) {
        let tc_type = tc.get("type").and_then(Value::as_str).unwrap_or_default();
        match tc_type {
            "auto" => set(
                &mut out,
                &["tool_choice"],
                Value::String("auto".to_string()),
            ),
            "any" => set(
                &mut out,
                &["tool_choice"],
                Value::String("required".to_string()),
            ),
            "tool" => {
                let name = tc.get("name").and_then(Value::as_str).unwrap_or_default();
                if !name.is_empty() {
                    set(
                        &mut out,
                        &["tool_choice"],
                        json!({"type":"function","function":{"name": name}}),
                    );
                }
            }
            _ => {}
        }
    }

    let bytes = serde_json::to_vec(&out).unwrap_or_else(|_| b"{}".to_vec());
    (
        bytes,
        out.get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        stream,
    )
}

static CLAUDE_MESSAGE_COUNTER: AtomicU64 = AtomicU64::new(0);
static CLAUDE_TOOL_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct ClaudeStreamState {
    pub message_id: String,
    pub model: String,
    pub input_tokens: i64,
    pub content_block_index: i64,
    pub has_started_text_block: bool,
    pub has_text: bool,
    pub has_tool_use: bool,
    pub completed: bool,
}

impl ClaudeStreamState {
    pub fn new(model: &str) -> Self {
        Self {
            message_id: new_message_id(),
            model: model.to_string(),
            input_tokens: 0,
            content_block_index: -1,
            has_started_text_block: false,
            has_text: false,
            has_tool_use: false,
            completed: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeNonStreamResult {
    pub json: String,
    pub found_completed: bool,
    pub has_text: bool,
    pub has_tool_use: bool,
}

pub fn convert_codex_stream_to_claude_events(
    raw_line: &[u8],
    state: &mut ClaudeStreamState,
) -> Vec<String> {
    let line = trim_ascii(raw_line);
    if !line.starts_with(b"data:") {
        return Vec::new();
    }
    let payload = trim_ascii(&line[5..]);
    if payload.is_empty() {
        return Vec::new();
    }

    let v: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let typ = v.get("type").and_then(Value::as_str).unwrap_or_default();

    let mut events = Vec::<String>::new();

    match typ {
        "response.created" => {
            if let Some(m) = v
                .get("response")
                .and_then(|r| r.get("model"))
                .and_then(Value::as_str)
            {
                if !m.is_empty() {
                    state.model = m.to_string();
                }
            }

            if let Some(input_tokens) = v
                .get("response")
                .and_then(|r| r.get("usage"))
                .and_then(|u| u.get("input_tokens"))
                .and_then(Value::as_i64)
            {
                state.input_tokens = input_tokens;
            }

            let msg_start = json!({
                "type": "message_start",
                "message": {
                    "id": state.message_id.clone(),
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": state.model.clone(),
                    "stop_reason": Value::Null,
                    "usage": { "input_tokens": state.input_tokens, "output_tokens": 0 }
                }
            });
            events.push(format_claude_sse("message_start", &msg_start));
        }
        "response.output_text.delta" => {
            let delta = v.get("delta").and_then(Value::as_str).unwrap_or_default();
            if delta.is_empty() {
                return events;
            }
            state.has_text = true;

            if !state.has_started_text_block {
                state.content_block_index += 1;
                state.has_started_text_block = true;
                let block_start = json!({
                    "type": "content_block_start",
                    "index": state.content_block_index,
                    "content_block": { "type": "text", "text": "" }
                });
                events.push(format_claude_sse("content_block_start", &block_start));
            }

            let block_delta = json!({
                "type": "content_block_delta",
                "index": state.content_block_index,
                "delta": { "type": "text_delta", "text": delta }
            });
            events.push(format_claude_sse("content_block_delta", &block_delta));
        }
        "response.output_item.added" => {
            let item = v.get("item");
            if item.and_then(|i| i.get("type")).and_then(Value::as_str) != Some("function_call") {
                return events;
            }
            state.has_tool_use = true;

            if state.has_started_text_block {
                let block_stop = json!({
                    "type": "content_block_stop",
                    "index": state.content_block_index
                });
                events.push(format_claude_sse("content_block_stop", &block_stop));
                state.has_started_text_block = false;
            }

            state.content_block_index += 1;
            let call_id = item
                .and_then(|i| i.get("call_id"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let name = item
                .and_then(|i| i.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();

            let block_start = json!({
                "type": "content_block_start",
                "index": state.content_block_index,
                "content_block": {
                    "type": "tool_use",
                    "id": sanitize_claude_tool_id(call_id),
                    "name": name,
                    "input": {}
                }
            });
            events.push(format_claude_sse("content_block_start", &block_start));
        }
        "response.function_call_arguments.delta" => {
            let delta = v.get("delta").and_then(Value::as_str).unwrap_or_default();
            if delta.is_empty() {
                return events;
            }
            let block_delta = json!({
                "type": "content_block_delta",
                "index": state.content_block_index,
                "delta": { "type": "input_json_delta", "partial_json": delta }
            });
            events.push(format_claude_sse("content_block_delta", &block_delta));
        }
        "response.function_call_arguments.done" => {
            let block_stop = json!({
                "type": "content_block_stop",
                "index": state.content_block_index
            });
            events.push(format_claude_sse("content_block_stop", &block_stop));
        }
        "response.completed" => {
            state.completed = true;

            if state.has_started_text_block {
                let block_stop = json!({
                    "type": "content_block_stop",
                    "index": state.content_block_index
                });
                events.push(format_claude_sse("content_block_stop", &block_stop));
            }

            let stop_reason = if state.has_tool_use {
                "tool_use"
            } else {
                "end_turn"
            };
            let output_tokens = v
                .get("response")
                .and_then(|r| r.get("usage"))
                .and_then(|u| u.get("output_tokens"))
                .and_then(Value::as_i64)
                .unwrap_or(0);

            let msg_delta = json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason },
                "usage": { "output_tokens": output_tokens }
            });
            events.push(format_claude_sse("message_delta", &msg_delta));

            let msg_stop = json!({"type":"message_stop"});
            events.push(format_claude_sse("message_stop", &msg_stop));
        }
        _ => {}
    }

    events
}

pub fn convert_codex_full_sse_to_claude_response_with_meta(
    data: &[u8],
    model: &str,
) -> ClaudeNonStreamResult {
    for line in data.split(|&b| b == b'\n') {
        let line = trim_ascii(line);
        if !line.starts_with(b"data:") {
            continue;
        }
        let payload = trim_ascii(&line[5..]);
        if payload.is_empty() {
            continue;
        }

        let v: Value = match serde_json::from_slice(payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(Value::as_str) != Some("response.completed") {
            continue;
        }

        let mut has_text = false;
        let mut has_tool_use = false;
        if let Some(output) = v
            .get("response")
            .and_then(|r| r.get("output"))
            .and_then(Value::as_array)
        {
            for item in output {
                match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "message" => {
                        if let Some(content) = item.get("content").and_then(Value::as_array) {
                            for ci in content {
                                if ci.get("type").and_then(Value::as_str) == Some("output_text")
                                    && ci.get("text").and_then(Value::as_str).unwrap_or_default()
                                        != ""
                                {
                                    has_text = true;
                                    break;
                                }
                            }
                        }
                    }
                    "function_call" => has_tool_use = true,
                    _ => {}
                }
            }
        }

        let out = convert_codex_completed_to_claude(&v, model);
        let json = serde_json::to_string(&out).unwrap_or_else(|_| String::new());

        return ClaudeNonStreamResult {
            json,
            found_completed: true,
            has_text,
            has_tool_use,
        };
    }

    ClaudeNonStreamResult::default()
}

fn convert_codex_completed_to_claude(completed_event: &Value, model: &str) -> Value {
    let resp = completed_event.get("response").unwrap_or(&Value::Null);
    let msg_id = new_message_id();

    let mut out = json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "content": [],
        "model": model,
    });

    if let Some(m) = resp.get("model").and_then(Value::as_str) {
        if !m.is_empty() {
            set(&mut out, &["model"], Value::String(m.to_string()));
        }
    }

    let mut stop_reason = "end_turn";
    if let Some(output) = resp.get("output").and_then(Value::as_array) {
        for item in output {
            match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                "message" => {
                    if let Some(content) = item.get("content").and_then(Value::as_array) {
                        for ci in content {
                            if ci.get("type").and_then(Value::as_str) == Some("output_text") {
                                let text =
                                    ci.get("text").and_then(Value::as_str).unwrap_or_default();
                                let block = json!({"type":"text","text": text});
                                push_content(&mut out, block);
                            }
                        }
                    }
                }
                "function_call" => {
                    stop_reason = "tool_use";
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                    let input = item.get("arguments").cloned().unwrap_or_else(|| json!({}));

                    let block = json!({
                        "type": "tool_use",
                        "id": sanitize_claude_tool_id(call_id),
                        "name": name,
                        "input": input
                    });
                    push_content(&mut out, block);
                }
                _ => {}
            }
        }
    }

    set(
        &mut out,
        &["stop_reason"],
        Value::String(stop_reason.to_string()),
    );
    set(&mut out, &["stop_sequence"], Value::Null);

    if let Some(usage) = resp.get("usage") {
        let input_tokens = usage
            .get("input_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let output_tokens = usage
            .get("output_tokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        set(
            &mut out,
            &["usage", "input_tokens"],
            Value::Number(input_tokens.into()),
        );
        set(
            &mut out,
            &["usage", "output_tokens"],
            Value::Number(output_tokens.into()),
        );
    } else {
        set(
            &mut out,
            &["usage", "input_tokens"],
            Value::Number(0.into()),
        );
        set(
            &mut out,
            &["usage", "output_tokens"],
            Value::Number(0.into()),
        );
    }

    out
}

fn push_content(out: &mut Value, item: Value) {
    if let Some(arr) = out.get_mut("content").and_then(Value::as_array_mut) {
        arr.push(item);
    }
}

fn format_claude_sse(event_type: &str, data: &Value) -> String {
    let json = serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string());
    format!("event: {event_type}\ndata: {json}\n\n")
}

fn new_message_id() -> String {
    let n = CLAUDE_MESSAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("msg_{}_{}", crate::core::now_unix_ms(), n)
}

fn sanitize_claude_tool_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }

    if out.is_empty() {
        let n = CLAUDE_TOOL_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        out = format!("toolu_{}_{}", crate::core::now_unix_ms(), n);
    }

    out
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

fn extract_text_from_claude_text_or_array(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let mut out = String::new();
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        out.push_str(text);
                    }
                }
            }
            out
        }
        _ => String::new(),
    }
}

fn push_message(out: &mut Value, msg: Value) {
    if let Some(arr) = out.get_mut("messages").and_then(Value::as_array_mut) {
        arr.push(msg);
    }
}

fn set(root: &mut Value, path: &[&str], value: Value) {
    let mut cur = root;
    for (idx, key) in path.iter().enumerate() {
        let last = idx == path.len() - 1;
        if last {
            if let Some(obj) = cur.as_object_mut() {
                obj.insert((*key).to_string(), value);
            }
            return;
        }

        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        let obj = cur.as_object_mut().unwrap();
        cur = obj
            .entry((*key).to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn translate_claude_request_system_and_text_blocks() {
        let input = json!({
            "model":"claude-3.5",
            "stream": false,
            "system": "sys",
            "messages": [
                {"role":"user","content":[{"type":"text","text":"hi"}]},
            ]
        });

        let (out, model, stream) =
            convert_claude_request_to_openai(&serde_json::to_vec(&input).unwrap());
        assert_eq!(model, "claude-3.5");
        assert!(!stream);

        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "claude-3.5");
        assert_eq!(v["stream"], false);
        assert_eq!(v["messages"][0]["role"], "system");
        assert_eq!(v["messages"][0]["content"], "sys");
        assert_eq!(v["messages"][1]["role"], "user");
        assert_eq!(v["messages"][1]["content"][0]["type"], "text");
        assert_eq!(v["messages"][1]["content"][0]["text"], "hi");
    }

    #[test]
    fn translate_claude_request_tool_use_and_tool_result() {
        let input = json!({
            "model":"claude-3.5",
            "stream": true,
            "messages": [
                {"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"f","input":{"x":1}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":"ok"}]}
            ]
        });

        let (out, _model, _stream) =
            convert_claude_request_to_openai(&serde_json::to_vec(&input).unwrap());
        let v: Value = serde_json::from_slice(&out).unwrap();

        assert!(v["messages"][0]["tool_calls"].is_array());
        assert_eq!(v["messages"][0]["tool_calls"][0]["id"], "c1");
        assert_eq!(v["messages"][0]["tool_calls"][0]["function"]["name"], "f");
        assert_eq!(v["messages"][0]["content"], Value::Null);

        // tool_result becomes a separate tool message
        assert_eq!(v["messages"][1]["role"], "tool");
        assert_eq!(v["messages"][1]["tool_call_id"], "c1");
        assert_eq!(v["messages"][1]["content"], "ok");
    }

    #[test]
    fn translate_claude_request_tools_and_tool_choice() {
        let input = json!({
            "model":"claude-3.5",
            "tools": [
                {"name":"t1","description":"d","input_schema":{"type":"object","properties":{}}}
            ],
            "tool_choice": {"type":"tool","name":"t1"},
            "messages": [{"role":"user","content":"hi"}]
        });

        let (out, _model, _stream) =
            convert_claude_request_to_openai(&serde_json::to_vec(&input).unwrap());
        let v: Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "t1");
        assert_eq!(v["tools"][0]["function"]["description"], "d");
        assert!(v["tools"][0]["function"]["parameters"].is_object());

        assert_eq!(v["tool_choice"]["type"], "function");
        assert_eq!(v["tool_choice"]["function"]["name"], "t1");
    }

    #[test]
    fn translate_claude_stream_events_basic_text() {
        let mut state = ClaudeStreamState::new("gpt-5.4");

        let created = br#"data: {"type":"response.created","response":{"model":"gpt-5.4","usage":{"input_tokens":1}}}"#;
        let events = convert_codex_stream_to_claude_events(created, &mut state);
        assert!(events[0].starts_with("event: message_start\n"));
        assert!(events[0].contains("\"type\":\"message_start\""));

        let delta = br#"data: {"type":"response.output_text.delta","delta":"hi"}"#;
        let events = convert_codex_stream_to_claude_events(delta, &mut state);
        assert_eq!(events.len(), 2);
        assert!(events[0].starts_with("event: content_block_start\n"));
        assert!(events[1].starts_with("event: content_block_delta\n"));

        let completed = br#"data: {"type":"response.completed","response":{"usage":{"output_tokens":2},"output":[{"type":"message","content":[{"type":"output_text","text":"hi"}]}]}}"#;
        let events = convert_codex_stream_to_claude_events(completed, &mut state);
        assert!(
            events
                .iter()
                .any(|e| e.starts_with("event: message_delta\n"))
        );
        assert!(
            events
                .iter()
                .any(|e| e.starts_with("event: message_stop\n"))
        );
    }

    #[test]
    fn translate_claude_non_stream_extracts_completed_and_builds_json() {
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-5.4\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5.4\",\"usage\":{\"input_tokens\":1,\"output_tokens\":2},\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi\"}]}]}}\n\n",
        );

        let got = convert_codex_full_sse_to_claude_response_with_meta(sse.as_bytes(), "gpt-5.4");
        assert!(got.found_completed);
        assert!(got.has_text);
        assert!(!got.has_tool_use);

        let v: Value = serde_json::from_str(&got.json).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "hi");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["input_tokens"], 1);
        assert_eq!(v["usage"]["output_tokens"], 2);
    }
}
