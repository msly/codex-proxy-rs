use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

pub fn convert_openai_request_to_codex(model_name: &str, raw_json: &[u8], stream: bool) -> Vec<u8> {
    let v: Value = serde_json::from_slice(raw_json).unwrap_or_else(|_| Value::Object(Map::new()));
    let out = convert_openai_value_to_codex_value(model_name, v, stream);
    serde_json::to_vec(&out).unwrap_or_else(|_| b"{}".to_vec())
}

pub fn build_reverse_tool_name_map(raw_json: &[u8]) -> HashMap<String, String> {
    let v: Value = serde_json::from_slice(raw_json).unwrap_or_else(|_| Value::Object(Map::new()));
    build_reverse_tool_name_map_from_value(&v)
}

pub fn convert_openai_value_to_codex_value(model_name: &str, v: Value, stream: bool) -> Value {
    let original_tool_name_map = build_tool_name_map(&v);
    if v.get("input").is_some() {
        convert_existing_input(model_name, v, stream)
    } else {
        convert_chat_completions(model_name, v, stream, &original_tool_name_map)
    }
}

pub fn build_reverse_tool_name_map_from_value(v: &Value) -> HashMap<String, String> {
    build_tool_name_map(v)
        .into_iter()
        .map(|(orig, short)| (short, orig))
        .collect()
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

    fix_tools_array_schema(&mut v);
    convert_system_role_to_developer(&mut v);
    ensure_input_contains_json(&mut v);
    v
}

fn convert_chat_completions(
    model_name: &str,
    v: Value,
    stream: bool,
    original_tool_name_map: &HashMap<String, String>,
) -> Value {
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
                            match t {
                                "text" => {
                                    let text =
                                        it.get("text").and_then(Value::as_str).unwrap_or_default();
                                    let part_type = if role == "assistant" {
                                        "output_text"
                                    } else {
                                        "input_text"
                                    };
                                    push_content(
                                        &mut msg,
                                        json!({"type": part_type, "text": text}),
                                    );
                                }
                                "image_url" => {
                                    if role == "user" {
                                        let url = it
                                            .get("image_url")
                                            .and_then(|v| v.get("url"))
                                            .and_then(Value::as_str)
                                            .unwrap_or_default();
                                        let mut part = json!({"type":"input_image"});
                                        if !url.is_empty() {
                                            set(
                                                &mut part,
                                                &["image_url"],
                                                Value::String(url.to_string()),
                                            );
                                        }
                                        push_content(&mut msg, part);
                                    }
                                }
                                "file" => {
                                    if role == "user" {
                                        let file_data = it
                                            .get("file")
                                            .and_then(|v| v.get("file_data"))
                                            .and_then(Value::as_str)
                                            .unwrap_or_default();
                                        let filename = it
                                            .get("file")
                                            .and_then(|v| v.get("filename"))
                                            .and_then(Value::as_str)
                                            .unwrap_or_default();
                                        if !file_data.is_empty() {
                                            let mut part =
                                                json!({"type":"input_file","file_data":file_data});
                                            if !filename.is_empty() {
                                                set(
                                                    &mut part,
                                                    &["filename"],
                                                    Value::String(filename.to_string()),
                                                );
                                            }
                                            push_content(&mut msg, part);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }

            push_input(&mut out, msg);

            if role == "assistant" {
                if let Some(tool_calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        if tc.get("type").and_then(Value::as_str) != Some("function") {
                            continue;
                        }
                        let call_id = tc.get("id").and_then(Value::as_str).unwrap_or_default();
                        let name = tc
                            .get("function")
                            .and_then(|v| v.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let arguments = tc
                            .get("function")
                            .and_then(|v| v.get("arguments"))
                            .and_then(Value::as_str)
                            .unwrap_or_default();

                        let mapped = map_tool_name(name, original_tool_name_map);
                        let item = json!({
                            "type": "function_call",
                            "call_id": call_id,
                            "name": mapped,
                            "arguments": arguments,
                        });
                        push_input(&mut out, item);
                    }
                }
            }
        }
    }

    // response_format -> text.format
    if let Some(rf) = v.get("response_format") {
        if let Some(rft) = rf.get("type").and_then(Value::as_str) {
            match rft {
                "text" => {
                    set(
                        &mut out,
                        &["text", "format", "type"],
                        Value::String("text".to_string()),
                    );
                }
                "json_object" => {
                    set(
                        &mut out,
                        &["text", "format", "type"],
                        Value::String("json_object".to_string()),
                    );
                }
                "json_schema" => {
                    if let Some(js) = rf.get("json_schema") {
                        set(
                            &mut out,
                            &["text", "format", "type"],
                            Value::String("json_schema".to_string()),
                        );
                        if let Some(name) = js.get("name") {
                            set(&mut out, &["text", "format", "name"], name.clone());
                        }
                        if let Some(strict) = js.get("strict") {
                            set(&mut out, &["text", "format", "strict"], strict.clone());
                        }
                        if let Some(schema) = js.get("schema") {
                            set(&mut out, &["text", "format", "schema"], schema.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // tools mapping
    if let Some(tools) = v.get("tools").and_then(Value::as_array) {
        if !tools.is_empty() {
            set(&mut out, &["tools"], Value::Array(Vec::new()));
            for t in tools {
                let tool_type = t.get("type").and_then(Value::as_str).unwrap_or_default();

                if tool_type == "custom" {
                    let mut item = json!({"type":"function"});
                    if let Some(name) = t.get("name").and_then(Value::as_str) {
                        if !name.is_empty() {
                            set(
                                &mut item,
                                &["name"],
                                Value::String(map_tool_name(name, original_tool_name_map)),
                            );
                        }
                    }
                    if let Some(desc) = t.get("description") {
                        set(&mut item, &["description"], desc.clone());
                    }
                    if t.get("format").is_some() {
                        set(
                            &mut item,
                            &["parameters"],
                            json!({"type":"object","properties":{"patch":{"type":"string","description":"The patch content"}}}),
                        );
                    } else {
                        set(
                            &mut item,
                            &["parameters"],
                            json!({"type":"object","properties":{}}),
                        );
                    }
                    push_tool(&mut out, item);
                    continue;
                }

                if !tool_type.is_empty() && tool_type != "function" {
                    if t.is_object() {
                        push_tool(&mut out, t.clone());
                    }
                    continue;
                }

                if tool_type == "function" {
                    let mut item = json!({"type":"function"});
                    if let Some(fn_obj) = t.get("function") {
                        if let Some(name) = fn_obj.get("name").and_then(Value::as_str) {
                            if !name.is_empty() {
                                set(
                                    &mut item,
                                    &["name"],
                                    Value::String(map_tool_name(name, original_tool_name_map)),
                                );
                            }
                        }
                        if let Some(desc) = fn_obj.get("description") {
                            set(&mut item, &["description"], desc.clone());
                        }
                        if let Some(params) = fn_obj.get("parameters") {
                            let mut fixed = params.clone();
                            let _ = fix_schema_node(&mut fixed);
                            set(&mut item, &["parameters"], fixed);
                        }
                        if let Some(strict) = fn_obj.get("strict") {
                            set(&mut item, &["strict"], strict.clone());
                        }
                    }
                    push_tool(&mut out, item);
                }
            }
        }
    }

    // tool_choice mapping
    if let Some(tc) = v.get("tool_choice") {
        match tc {
            Value::String(s) => {
                set(&mut out, &["tool_choice"], Value::String(s.clone()));
            }
            Value::Object(obj) => {
                let tc_type = obj.get("type").and_then(Value::as_str).unwrap_or_default();
                if tc_type == "function" {
                    let name = tc
                        .get("function")
                        .and_then(|v| v.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let mut choice = json!({"type":"function"});
                    if !name.is_empty() {
                        set(
                            &mut choice,
                            &["name"],
                            Value::String(map_tool_name(name, original_tool_name_map)),
                        );
                    }
                    set(&mut out, &["tool_choice"], choice);
                } else if !tc_type.is_empty() {
                    set(&mut out, &["tool_choice"], tc.clone());
                }
            }
            _ => {}
        }
    }

    if let Some(service_tier) = v.get("service_tier").and_then(Value::as_str) {
        let service_tier = service_tier.trim();
        if !service_tier.is_empty() {
            set(
                &mut out,
                &["service_tier"],
                Value::String(service_tier.to_string()),
            );
        }
    }

    ensure_input_contains_json(&mut out);
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

fn push_tool(out: &mut Value, item: Value) {
    if let Some(arr) = out.get_mut("tools").and_then(Value::as_array_mut) {
        arr.push(item);
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

fn build_tool_name_map(raw: &Value) -> HashMap<String, String> {
    let mut names: Vec<String> = Vec::new();
    if let Some(tools) = raw.get("tools").and_then(Value::as_array) {
        for t in tools {
            if t.get("type").and_then(Value::as_str) != Some("function") {
                continue;
            }
            if let Some(name) = t
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                names.push(name.to_string());
            }
        }
    }
    if names.is_empty() {
        return HashMap::new();
    }
    build_short_name_map(&names)
}

fn shorten_name_if_needed(name: &str) -> String {
    const LIMIT: usize = 64;
    if name.len() <= LIMIT {
        return name.to_string();
    }
    if name.starts_with("mcp__") {
        if let Some(idx) = name.rfind("__") {
            if idx > 0 {
                let mut candidate = format!("mcp__{}", &name[idx + 2..]);
                if candidate.len() > LIMIT {
                    candidate.truncate(LIMIT);
                }
                return candidate;
            }
        }
    }
    name.chars().take(LIMIT).collect()
}

fn build_short_name_map(names: &[String]) -> HashMap<String, String> {
    let mut used: HashSet<String> = HashSet::new();
    let mut m: HashMap<String, String> = HashMap::new();

    for orig in names {
        let cand = base_candidate(orig);
        let uniq = make_unique(&cand, &mut used);
        used.insert(uniq.clone());
        m.insert(orig.clone(), uniq);
    }

    fn base_candidate(name: &str) -> String {
        const LIMIT: usize = 64;
        if name.len() <= LIMIT {
            return name.to_string();
        }
        if name.starts_with("mcp__") {
            if let Some(idx) = name.rfind("__") {
                if idx > 0 {
                    let mut cand = format!("mcp__{}", &name[idx + 2..]);
                    if cand.len() > LIMIT {
                        cand.truncate(LIMIT);
                    }
                    return cand;
                }
            }
        }
        name.chars().take(LIMIT).collect()
    }

    fn make_unique(cand: &str, used: &mut HashSet<String>) -> String {
        const LIMIT: usize = 64;
        if !used.contains(cand) {
            return cand.to_string();
        }
        let base = cand.to_string();
        for i in 1.. {
            let suffix = format!("_{}", i);
            let allowed = LIMIT.saturating_sub(suffix.len());
            let mut tmp = base.clone();
            if tmp.len() > allowed {
                tmp.truncate(allowed);
            }
            tmp.push_str(&suffix);
            if !used.contains(&tmp) {
                return tmp;
            }
        }
        unreachable!()
    }

    m
}

fn map_tool_name(name: &str, tool_name_map: &HashMap<String, String>) -> String {
    if let Some(short) = tool_name_map.get(name) {
        return short.clone();
    }
    shorten_name_if_needed(name)
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
