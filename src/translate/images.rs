use serde_json::{Value, json};

pub fn convert_image_request_to_responses_value(v: Value, edit: bool) -> Value {
    let model = v
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gpt-5.4")
        .to_string();
    let prompt = v
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let output_format = v
        .get("output_format")
        .or_else(|| v.get("format"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("png")
        .to_string();

    let mut content = vec![json!({"type":"input_text","text": prompt})];
    if edit {
        for image in collect_input_images(v.get("image")) {
            content.push(image);
        }
        if let Some(mask) = collect_single_input_image(v.get("mask")) {
            content.push(mask);
        }
    }

    let mut out = json!({
        "model": model,
        "stream": true,
        "input": [{
            "type": "message",
            "role": "user",
            "content": content,
        }],
        "tools": [{
            "type": "image_generation",
            "output_format": output_format,
        }],
        "tool_choice": {"type":"image_generation"},
    });

    if let Some(size) = v
        .get("size")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        set_image_tool_field(&mut out, "size", Value::String(size.to_string()));
    }
    if let Some(quality) = v
        .get("quality")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        set_image_tool_field(&mut out, "quality", Value::String(quality.to_string()));
    }

    out
}

pub fn convert_responses_sse_to_images_json(bytes: &[u8], response_format: &str) -> Option<String> {
    let mut images = Vec::<ImageData>::new();
    for payload in sse_json_payloads(bytes) {
        collect_image_events(&payload, &mut images);
    }
    if images.is_empty() {
        return None;
    }

    let use_url = response_format == "url";
    let data = images
        .into_iter()
        .map(|image| {
            let mut item = json!({});
            if use_url {
                item["url"] = Value::String(format!(
                    "data:{};base64,{}",
                    mime_type_from_output_format(&image.output_format),
                    image.b64
                ));
            } else {
                item["b64_json"] = Value::String(image.b64);
            }
            item
        })
        .collect::<Vec<_>>();

    Some(
        json!({
            "created": now_unix_seconds(),
            "data": data,
        })
        .to_string(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageData {
    output_format: String,
    b64: String,
}

fn collect_image_events(event: &Value, images: &mut Vec<ImageData>) {
    match event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "response.image_generation_call.partial_image" => {
            let b64 = event
                .get("partial_image_b64")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !b64.is_empty() {
                push_unique_image(
                    images,
                    ImageData {
                        output_format: event
                            .get("output_format")
                            .and_then(Value::as_str)
                            .unwrap_or("png")
                            .to_string(),
                        b64: b64.to_string(),
                    },
                );
            }
        }
        "response.output_item.done" => {
            collect_output_image(event.get("item").unwrap_or(&Value::Null), images);
        }
        "response.completed" => {
            if let Some(output) = event
                .get("response")
                .and_then(|response| response.get("output"))
                .and_then(Value::as_array)
            {
                for item in output {
                    collect_output_image(item, images);
                }
            }
        }
        _ => {}
    }
}

fn collect_output_image(item: &Value, images: &mut Vec<ImageData>) {
    if item.get("type").and_then(Value::as_str) != Some("image_generation_call") {
        return;
    }
    let b64 = item
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if b64.is_empty() {
        return;
    }
    push_unique_image(
        images,
        ImageData {
            output_format: item
                .get("output_format")
                .and_then(Value::as_str)
                .unwrap_or("png")
                .to_string(),
            b64: b64.to_string(),
        },
    );
}

fn push_unique_image(images: &mut Vec<ImageData>, image: ImageData) {
    if !images.iter().any(|existing| existing.b64 == image.b64) {
        images.push(image);
    }
}

fn collect_input_images(value: Option<&Value>) -> Vec<Value> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| collect_single_input_image(Some(item)))
            .collect(),
        Some(_) => collect_single_input_image(value).into_iter().collect(),
        None => Vec::new(),
    }
}

fn collect_single_input_image(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(json!({
            "type": "input_image",
            "image_url": s,
        })),
        Value::Object(obj) => obj
            .get("url")
            .or_else(|| obj.get("image_url"))
            .or_else(|| obj.get("b64_json"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(|s| json!({"type":"input_image","image_url":s})),
        _ => None,
    }
}

fn sse_json_payloads(bytes: &[u8]) -> Vec<Value> {
    let mut out = Vec::new();
    for line in bytes.split(|b| *b == b'\n') {
        let line = trim_ascii(line);
        if !line.starts_with(b"data:") {
            continue;
        }
        let payload = trim_ascii(&line[5..]);
        if payload.is_empty() || payload == b"[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(payload) {
            out.push(value);
        }
    }
    out
}

fn mime_type_from_output_format(output_format: &str) -> &'static str {
    match output_format.to_ascii_lowercase().as_str() {
        "jpeg" | "jpg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
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

fn set_image_tool_field(v: &mut Value, key: &str, value: Value) {
    if let Some(tool) = v
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .and_then(|tools| tools.first_mut())
        .and_then(Value::as_object_mut)
    {
        tool.insert(key.to_string(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn image_request_to_responses_injects_image_tool() {
        let out = convert_image_request_to_responses_value(
            json!({
                "model":"gpt-5.4",
                "prompt":"draw a cat",
                "output_format":"webp",
                "size":"1024x1024"
            }),
            false,
        );

        assert_eq!(out["model"], "gpt-5.4");
        assert_eq!(out["input"][0]["content"][0]["text"], "draw a cat");
        assert_eq!(out["tools"][0]["type"], "image_generation");
        assert_eq!(out["tools"][0]["output_format"], "webp");
        assert_eq!(out["tools"][0]["size"], "1024x1024");
    }

    #[test]
    fn image_response_extracts_completed_image_as_b64_json() {
        let raw = br#"data: {"type":"response.completed","response":{"output":[{"type":"image_generation_call","output_format":"png","result":"aGVsbG8="}]}}

data: [DONE]

"#;
        let out = convert_responses_sse_to_images_json(raw, "b64_json").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(v["data"][0]["b64_json"], "aGVsbG8=");
    }

    #[test]
    fn image_response_can_return_data_url() {
        let raw = br#"data: {"type":"response.output_item.done","item":{"type":"image_generation_call","output_format":"jpeg","result":"aGVsbG8="}}

"#;
        let out = convert_responses_sse_to_images_json(raw, "url").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(v["data"][0]["url"], "data:image/jpeg;base64,aGVsbG8=");
    }
}
