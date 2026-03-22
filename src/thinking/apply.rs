use serde_json::{Map, Value, json};

use super::suffix::parse_model_suffix;

pub fn apply_thinking(body: &[u8], model: &str) -> (Vec<u8>, String) {
    let parse = parse_model_suffix(model);
    let base_model = parse.model_name.trim().to_string();

    let mut v: Value = serde_json::from_slice(body).unwrap_or_else(|_| Value::Object(Map::new()));
    let effort = match parse.thinking_suffix.as_deref() {
        Some(suffix) => effort_from_suffix(suffix),
        None => extract_effort_from_body(&v),
    };

    if let Some(effort) = effort {
        set_reasoning_effort(&mut v, &effort);
    }

    if parse.is_fast {
        set_service_tier(&mut v, "priority");
    }

    (
        serde_json::to_vec(&v).unwrap_or_else(|_| b"{}".to_vec()),
        base_model,
    )
}

pub fn apply_thinking_to_value(body_value: &mut Value, model: &str) -> String {
    let parse = parse_model_suffix(model);
    let base_model = parse.model_name.trim().to_string();

    let effort = match parse.thinking_suffix.as_deref() {
        Some(suffix) => effort_from_suffix(suffix),
        None => extract_effort_from_body(body_value),
    };

    if let Some(effort) = effort {
        set_reasoning_effort(body_value, &effort);
    }

    if parse.is_fast {
        set_service_tier(body_value, "priority");
    }

    base_model
}

fn extract_effort_from_body(v: &Value) -> Option<String> {
    if let Some(effort) = v
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
    {
        return normalize_effort(effort);
    }
    if let Some(effort) = v.get("reasoning_effort").and_then(Value::as_str) {
        return normalize_effort(effort);
    }
    if let Some(effort) = v.get("variant").and_then(Value::as_str) {
        return normalize_effort(effort);
    }
    None
}

fn normalize_effort(raw: &str) -> Option<String> {
    let value = raw.trim().to_lowercase();
    if value.is_empty() {
        return None;
    }
    Some(value)
}

fn effort_from_suffix(raw_suffix: &str) -> Option<String> {
    let s = raw_suffix.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    match s.as_str() {
        "none" => return Some("none".to_string()),
        "auto" | "-1" => return Some("medium".to_string()), // 对齐 Go：auto 映射到 medium
        "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => return Some(s),
        _ => {}
    }

    if let Ok(v) = s.parse::<i64>() {
        if v == 0 {
            return Some("none".to_string());
        }
        if v > 0 {
            return Some(budget_to_level(v).to_string());
        }
    }

    None
}

fn budget_to_level(budget: i64) -> &'static str {
    match budget {
        b if b <= 0 => "none",
        b if b <= 512 => "auto",
        b if b <= 1024 => "low",
        b if b <= 8192 => "medium",
        b if b <= 24576 => "high",
        _ => "xhigh",
    }
}

fn set_reasoning_effort(v: &mut Value, effort: &str) {
    let obj = match v.as_object_mut() {
        Some(m) => m,
        None => {
            *v = Value::Object(Map::new());
            v.as_object_mut().expect("just set to object")
        }
    };

    let reasoning = obj.entry("reasoning").or_insert_with(|| json!({}));
    if !reasoning.is_object() {
        *reasoning = json!({});
    }

    if let Some(r) = reasoning.as_object_mut() {
        r.insert("effort".to_string(), Value::String(effort.to_string()));
    }
}

fn set_service_tier(v: &mut Value, tier: &str) {
    let obj = match v.as_object_mut() {
        Some(m) => m,
        None => {
            *v = Value::Object(Map::new());
            v.as_object_mut().expect("just set to object")
        }
    };

    obj.insert("service_tier".to_string(), Value::String(tier.to_string()));
}
