use serde_json::{Map, Value, json};

pub fn convert_openai_request_to_codex(model_name: &str, raw_json: &[u8], stream: bool) -> Vec<u8> {
    let v: Value = serde_json::from_slice(raw_json).unwrap_or_else(|_| Value::Object(Map::new()));
    let out = if v.get("input").is_some() {
        convert_existing_input(model_name, v, stream)
    } else {
        convert_chat_completions(model_name, v, stream)
    };
    serde_json::to_vec(&out).unwrap_or_else(|_| b"{}".to_vec())
}

fn convert_existing_input(model_name: &str, mut v: Value, stream: bool) -> Value {
    // input 为字符串时，转换为标准消息数组
    if let Some(input) = v.get("input").and_then(Value::as_str) {
        let msg = json!([{
            "type": "message",
            "role": "user",
            "content": [{"type":"input_text","text": input}]
        }]);
        set(&mut v, &["input"], msg);
    }

    set(&mut v, &["model"], Value::String(model_name.to_string()));
    set(&mut v, &["stream"], Value::Bool(stream));
    set(&mut v, &["store"], Value::Bool(false));
    set(&mut v, &["parallel_tool_calls"], Value::Bool(true));
    set(
        &mut v,
        &["include"],
        Value::Array(vec![Value::String(
            "reasoning.encrypted_content".to_string(),
        )]),
    );

    if v.get("reasoning_effort").is_some() {
        if let Some(val) = v.get("reasoning_effort").cloned() {
            set(&mut v, &["reasoning", "effort"], val);
        }
        delete(&mut v, &["reasoning_effort"]);
    } else if v.get("variant").is_some() {
        if let Some(val) = v.get("variant").cloned() {
            set(&mut v, &["reasoning", "effort"], val);
        }
    }

    // instructions 必须存在
    if v.get("instructions").is_none() {
        set(&mut v, &["instructions"], Value::String(String::new()));
    }

    // 删除上游不支持的参数（对齐 Go：这里只做最小子集）
    for key in [
        "previous_response_id",
        "stream_options",
        "prompt_cache_retention",
        "safety_identifier",
        "generate",
        "max_output_tokens",
        "max_completion_tokens",
        "temperature",
        "top_p",
        "truncation",
        "context_management",
        "user",
        "variant",
    ] {
        delete(&mut v, &[key]);
    }

    // service_tier：仅保留 "priority"
    if let Some(tier) = v.get("service_tier").and_then(Value::as_str) {
        if tier != "priority" {
            delete(&mut v, &["service_tier"]);
        }
    }

    convert_system_role_to_developer(&mut v);
    v
}

fn convert_chat_completions(model_name: &str, v: Value, stream: bool) -> Value {
    let mut out = json!({
        "instructions": "",
        "stream": stream,
        "parallel_tool_calls": true,
        "reasoning": {"summary":"auto"},
        "include": ["reasoning.encrypted_content"],
        "model": model_name,
        "store": false,
        "input": []
    });

    // reasoning_effort / reasoning.effort / variant → reasoning.effort（最小映射）
    if let Some(val) = v.get("reasoning_effort").cloned() {
        set(&mut out, &["reasoning", "effort"], val);
    } else if let Some(val) = v.get("reasoning").and_then(|r| r.get("effort")).cloned() {
        set(&mut out, &["reasoning", "effort"], val);
    } else if let Some(val) = v.get("variant").cloned() {
        set(&mut out, &["reasoning", "effort"], val);
    }

    if let Some(messages) = v.get("messages").and_then(Value::as_array) {
        for m in messages {
            let role = m.get("role").and_then(Value::as_str).unwrap_or_default();
            if role == "tool" {
                let item = json!({
                    "type": "function_call_output",
                    "call_id": m.get("tool_call_id").and_then(Value::as_str).unwrap_or_default(),
                    "output": m.get("content").and_then(Value::as_str).unwrap_or_default(),
                });
                push_input(&mut out, item);
                continue;
            }

            let mapped_role = if role == "system" { "developer" } else { role };
            let mut msg = json!({
                "type": "message",
                "role": mapped_role,
                "content": []
            });

            if let Some(content) = m.get("content") {
                match content {
                    Value::String(s) if !s.is_empty() => {
                        let part_type = if role == "assistant" {
                            "output_text"
                        } else {
                            "input_text"
                        };
                        push_content(&mut msg, json!({"type": part_type, "text": s}));
                    }
                    Value::Array(items) => {
                        for it in items {
                            let t = it.get("type").and_then(Value::as_str).unwrap_or_default();
                            if t == "text" {
                                let text =
                                    it.get("text").and_then(Value::as_str).unwrap_or_default();
                                let part_type = if role == "assistant" {
                                    "output_text"
                                } else {
                                    "input_text"
                                };
                                push_content(&mut msg, json!({"type": part_type, "text": text}));
                            }
                        }
                    }
                    _ => {}
                }
            }

            push_input(&mut out, msg);
        }
    }

    out
}

fn convert_system_role_to_developer(v: &mut Value) {
    let input = match v.get_mut("input") {
        Some(Value::Array(arr)) => arr,
        _ => return,
    };
    for item in input {
        let role = item.get("role").and_then(Value::as_str).unwrap_or_default();
        if role == "system" {
            if let Some(obj) = item.as_object_mut() {
                obj.insert("role".to_string(), Value::String("developer".to_string()));
            }
        }
    }
}

fn push_input(out: &mut Value, item: Value) {
    if let Some(arr) = out.get_mut("input").and_then(Value::as_array_mut) {
        arr.push(item);
    }
}

fn push_content(msg: &mut Value, part: Value) {
    if let Some(arr) = msg.get_mut("content").and_then(Value::as_array_mut) {
        arr.push(part);
    }
}

fn set(v: &mut Value, path: &[&str], value: Value) {
    if path.is_empty() {
        *v = value;
        return;
    }

    let mut cur = v;
    for (i, key) in path.iter().enumerate() {
        let is_last = i + 1 == path.len();
        if is_last {
            if let Some(obj) = cur.as_object_mut() {
                obj.insert((*key).to_string(), value);
            } else {
                *cur = json!({ *key: value });
            }
            return;
        }

        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        let obj = cur.as_object_mut().expect("object ensured");
        cur = obj.entry((*key).to_string()).or_insert_with(|| json!({}));
    }
}

fn delete(v: &mut Value, path: &[&str]) {
    if path.is_empty() {
        return;
    }
    if path.len() == 1 {
        if let Some(obj) = v.as_object_mut() {
            obj.remove(path[0]);
        }
        return;
    }

    let mut cur = v;
    for key in &path[..path.len() - 1] {
        cur = match cur.get_mut(*key) {
            Some(next) => next,
            None => return,
        };
    }

    if let Some(obj) = cur.as_object_mut() {
        obj.remove(path[path.len() - 1]);
    }
}
