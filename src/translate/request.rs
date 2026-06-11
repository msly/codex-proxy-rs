use serde_json::{Map, Value, json};

pub fn convert_openai_request_to_codex(model_name: &str, raw_json: &[u8], stream: bool) -> Vec<u8> {
    let v: Value = serde_json::from_slice(raw_json).unwrap_or_else(|_| Value::Object(Map::new()));
    let out = convert_openai_value_to_codex_value(model_name, v, stream);
    serde_json::to_vec(&out).unwrap_or_else(|_| b"{}".to_vec())
}

pub fn convert_openai_value_to_codex_value(model_name: &str, v: Value, stream: bool) -> Value {
    convert_existing_input(model_name, v, stream)
}

pub(crate) fn normalize_codex_instructions(v: &mut Value) {
    let Some(obj) = v.as_object_mut() else {
        return;
    };

    let needs_default = match obj.get("instructions") {
        None => true,
        Some(Value::Null) => true,
        _ => false,
    };

    if needs_default {
        obj.insert("instructions".to_string(), Value::String(String::new()));
    }
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

    normalize_codex_instructions(&mut v);

    let keep_previous_response_id = input_has_tool_call_output(&v);

    // 删除上游不支持的参数（对齐 Go：这里只做最小子集）
    for key in [
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
    if !keep_previous_response_id {
        delete(&mut v, &["previous_response_id"]);
    }

    fix_tools_array_schema(&mut v);
    convert_system_role_to_developer(&mut v);
    ensure_input_contains_json(&mut v);
    v
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

fn ensure_input_contains_json(v: &mut Value) {
    let format_type = v
        .get("text")
        .and_then(|t| t.get("format"))
        .and_then(|f| f.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    if format_type != "json_object" && format_type != "json_schema" {
        return;
    }

    let has_json = |s: &str| s.to_lowercase().contains("json");

    if let Some(instructions) = v.get("instructions").and_then(Value::as_str) {
        if has_json(instructions) {
            return;
        }
    }

    if let Some(input) = v.get("input").and_then(Value::as_array) {
        for item in input {
            if item.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if has_json(text) {
                            return;
                        }
                    }
                }
            }
        }
    }

    let existing = v
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let next = if existing.is_empty() {
        "Respond in JSON format.".to_string()
    } else {
        format!("Respond in JSON format.\n\n{existing}")
    };
    set(v, &["instructions"], Value::String(next));
}

fn input_has_tool_call_output(v: &Value) -> bool {
    v.get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(is_codex_tool_output_item_type)
            })
        })
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

fn fix_tools_array_schema(v: &mut Value) {
    let Some(tools) = v.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for t in tools {
        let mut fixed = false;
        if t.get("type").and_then(Value::as_str) == Some("function") {
            if let Some(params) = t.get_mut("function").and_then(|f| f.get_mut("parameters")) {
                fixed |= fix_schema_node(params);
            } else if let Some(params) = t.get_mut("parameters") {
                fixed |= fix_schema_node(params);
            }
        } else if let Some(params) = t.get_mut("parameters") {
            fixed |= fix_schema_node(params);
        }
        let _ = fixed;
    }
}

fn fix_schema_node(node: &mut Value) -> bool {
    let mut changed = false;
    let Some(obj) = node.as_object_mut() else {
        return false;
    };

    if obj
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t == "array")
    {
        if !obj.contains_key("items") {
            obj.insert("items".to_string(), Value::Object(Map::new()));
            changed = true;
        }
    }

    if let Some(props) = obj.get_mut("properties").and_then(Value::as_object_mut) {
        for (_, v) in props.iter_mut() {
            changed |= fix_schema_node(v);
        }
    }

    if let Some(items) = obj.get_mut("items") {
        changed |= fix_schema_node(items);
    }

    for key in ["oneOf", "anyOf", "allOf", "prefixItems"] {
        if let Some(arr) = obj.get_mut(key).and_then(Value::as_array_mut) {
            for elem in arr {
                changed |= fix_schema_node(elem);
            }
        }
    }

    if let Some(ap) = obj.get_mut("additionalProperties") {
        changed |= fix_schema_node(ap);
    }

    changed
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
