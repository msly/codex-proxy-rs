use serde_json::{Map, Value, json};

pub fn convert_completions_request_to_chat_value(v: Value) -> Value {
    let model = v
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let prompt = prompt_to_text(v.get("prompt"));

    let mut out = json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": prompt
            }
        ]
    });

    for key in [
        "stream",
        "max_tokens",
        "temperature",
        "top_p",
        "stop",
        "presence_penalty",
        "frequency_penalty",
        "logit_bias",
        "user",
        "reasoning_effort",
        "reasoning",
        "variant",
        "service_tier",
    ] {
        if let Some(value) = v.get(key) {
            set(&mut out, &[key], value.clone());
        }
    }

    out
}

pub fn convert_chat_completion_to_completion(chat_json: &str) -> String {
    let chat: Value = serde_json::from_str(chat_json).unwrap_or_else(|_| Value::Object(Map::new()));
    let mut out = json!({
        "id": chat.get("id").and_then(Value::as_str).unwrap_or_default(),
        "object": "text_completion",
        "created": chat.get("created").and_then(Value::as_i64).unwrap_or(0),
        "model": chat.get("model").and_then(Value::as_str).unwrap_or_default(),
        "choices": []
    });

    if let Some(usage) = chat.get("usage") {
        out["usage"] = usage.clone();
    }

    let mut choices = Vec::new();
    if let Some(chat_choices) = chat.get("choices").and_then(Value::as_array) {
        for (idx, choice) in chat_choices.iter().enumerate() {
            let text = choice
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            choices.push(json!({
                "text": text,
                "index": choice.get("index").and_then(Value::as_i64).unwrap_or(idx as i64),
                "logprobs": Value::Null,
                "finish_reason": choice.get("finish_reason").cloned().unwrap_or(Value::Null),
            }));
        }
    }
    out["choices"] = Value::Array(choices);
    out.to_string()
}

pub fn convert_chat_completion_chunk_to_completion_chunk(chat_json: &str) -> String {
    let chat: Value = serde_json::from_str(chat_json).unwrap_or_else(|_| Value::Object(Map::new()));
    let mut out = json!({
        "id": chat.get("id").and_then(Value::as_str).unwrap_or_default(),
        "object": "text_completion",
        "created": chat.get("created").and_then(Value::as_i64).unwrap_or(0),
        "model": chat.get("model").and_then(Value::as_str).unwrap_or_default(),
        "choices": []
    });

    let mut choices = Vec::new();
    if let Some(chat_choices) = chat.get("choices").and_then(Value::as_array) {
        for (idx, choice) in chat_choices.iter().enumerate() {
            let text = choice
                .get("delta")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            choices.push(json!({
                "text": text,
                "index": choice.get("index").and_then(Value::as_i64).unwrap_or(idx as i64),
                "logprobs": Value::Null,
                "finish_reason": choice.get("finish_reason").cloned().unwrap_or(Value::Null),
            }));
        }
    }
    out["choices"] = Value::Array(choices);
    out.to_string()
}

fn prompt_to_text(prompt: Option<&Value>) -> String {
    match prompt {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => "Complete this:".to_string(),
    }
}

fn set(v: &mut Value, path: &[&str], value: Value) {
    if path.is_empty() {
        *v = value;
        return;
    }
    let mut cur = v;
    for key in &path[..path.len() - 1] {
        if !cur.get(*key).is_some_and(Value::is_object) {
            cur[*key] = Value::Object(Map::new());
        }
        cur = cur.get_mut(*key).unwrap();
    }
    cur[path[path.len() - 1]] = value;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn completions_request_to_chat_joins_prompt_array() {
        let out = convert_completions_request_to_chat_value(json!({
            "model": "gpt-5.4",
            "prompt": ["a", "b"],
            "stream": true,
            "temperature": 0.2
        }));

        assert_eq!(out["model"], "gpt-5.4");
        assert_eq!(out["messages"][0]["content"], "a\nb");
        assert_eq!(out["stream"], true);
        assert_eq!(out["temperature"], 0.2);
    }

    #[test]
    fn chat_completion_to_completion_extracts_text() {
        let out = convert_chat_completion_to_completion(
            r#"{"id":"chat1","created":1,"model":"m","choices":[{"index":0,"message":{"content":"hi"},"finish_reason":"stop"}],"usage":{"total_tokens":2}}"#,
        );
        let v: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(v["object"], "text_completion");
        assert_eq!(v["choices"][0]["text"], "hi");
        assert_eq!(v["usage"]["total_tokens"], 2);
    }
}
