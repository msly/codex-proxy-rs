pub mod request;

pub use request::convert_openai_request_to_codex;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn translate_existing_input_string_to_array_and_map_reasoning_effort() {
        let input = json!({
            "input": "hi",
            "reasoning_effort": "high",
            "service_tier": "fast"
        });
        let out =
            convert_openai_request_to_codex("gpt-5.4", &serde_json::to_vec(&input).unwrap(), true);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["model"], "gpt-5.4");
        assert_eq!(v["stream"], true);
        assert_eq!(v["store"], false);
        assert_eq!(v["reasoning"]["effort"], "high");
        assert!(v.get("reasoning_effort").is_none());
        assert!(v.get("service_tier").is_none()); // fast 不透传
        assert!(v["input"].is_array());
        assert_eq!(v["input"][0]["role"], "user");
        assert_eq!(v["input"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn translate_chat_completions_messages_to_input_array() {
        let input = json!({
            "messages": [
                {"role":"system","content":"s"},
                {"role":"user","content":"u"},
                {"role":"tool","tool_call_id":"c1","content":"out"}
            ]
        });
        let out =
            convert_openai_request_to_codex("gpt-5.4", &serde_json::to_vec(&input).unwrap(), false);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["model"], "gpt-5.4");
        assert_eq!(v["stream"], false);
        assert!(v["input"].is_array());
        assert_eq!(v["input"][0]["type"], "message");
        assert_eq!(v["input"][0]["role"], "developer"); // system -> developer
        assert_eq!(v["input"][1]["role"], "user");
        assert_eq!(v["input"][2]["type"], "function_call_output");
        assert_eq!(v["input"][2]["call_id"], "c1");
    }
}
