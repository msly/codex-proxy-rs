pub mod claude;
pub mod request;
pub mod response;

pub use claude::{
    ClaudeNonStreamResult, ClaudeStreamState, convert_claude_request_to_openai,
    convert_codex_full_sse_to_claude_response_with_meta, convert_codex_stream_to_claude_events,
};
pub use request::{build_reverse_tool_name_map, convert_openai_request_to_codex};
pub use response::{
    StreamState, convert_non_stream_response, convert_stream_chunk,
    extract_completed_response_payload,
};

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
        assert_eq!(v["service_tier"], "fast");
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

    #[test]
    fn translate_json_object_adds_format_and_json_instruction() {
        let input = json!({
            "messages": [
                {"role":"user","content":"return object"}
            ],
            "response_format": {
                "type": "json_object"
            }
        });
        let out =
            convert_openai_request_to_codex("gpt-5.4", &serde_json::to_vec(&input).unwrap(), false);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["text"]["format"]["type"], "json_object");
        assert_eq!(v["instructions"], "Respond in JSON format.");
    }

    #[test]
    fn translate_existing_input_null_instructions_normalizes_to_empty_string() {
        let input = json!({
            "input": "hi",
            "instructions": null
        });
        let out =
            convert_openai_request_to_codex("gpt-5.4", &serde_json::to_vec(&input).unwrap(), true);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["instructions"], "");
    }

    #[test]
    fn translate_existing_input_keeps_previous_response_id_for_tool_outputs() {
        let input = json!({
            "previous_response_id": "resp_prev",
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "tool output"
                }
            ]
        });
        let out =
            convert_openai_request_to_codex("gpt-5.4", &serde_json::to_vec(&input).unwrap(), true);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert_eq!(v["previous_response_id"], "resp_prev");
    }

    #[test]
    fn translate_existing_input_drops_previous_response_id_without_tool_outputs() {
        let input = json!({
            "previous_response_id": "resp_prev",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hi"}]
                }
            ]
        });
        let out =
            convert_openai_request_to_codex("gpt-5.4", &serde_json::to_vec(&input).unwrap(), true);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();

        assert!(v.get("previous_response_id").is_none());
    }
}
