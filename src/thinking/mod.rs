pub mod apply;
pub mod suffix;

pub use apply::apply_thinking;
pub use suffix::{ParseResult, parse_model_suffix};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn thinking_parse_suffix_basic() {
        assert_eq!(
            parse_model_suffix("gpt-5.4"),
            ParseResult {
                model_name: "gpt-5.4".to_string(),
                thinking_suffix: None,
                is_fast: false,
            }
        );
        assert_eq!(
            parse_model_suffix("gpt-5.4-high"),
            ParseResult {
                model_name: "gpt-5.4".to_string(),
                thinking_suffix: Some("high".to_string()),
                is_fast: false,
            }
        );
    }

    #[test]
    fn thinking_parse_suffix_fast_is_stripped() {
        assert_eq!(
            parse_model_suffix("gpt-5.4-fast"),
            ParseResult {
                model_name: "gpt-5.4".to_string(),
                thinking_suffix: None,
                is_fast: true,
            }
        );
        assert_eq!(
            parse_model_suffix("gpt-5.4-high-fast"),
            ParseResult {
                model_name: "gpt-5.4".to_string(),
                thinking_suffix: Some("high".to_string()),
                is_fast: true,
            }
        );
    }

    #[test]
    fn thinking_parse_suffix_budget_and_ambiguous_model() {
        assert_eq!(
            parse_model_suffix("any-new-model-24577-fast"),
            ParseResult {
                model_name: "any-new-model".to_string(),
                thinking_suffix: Some("24577".to_string()),
                is_fast: true,
            }
        );
        assert_eq!(
            parse_model_suffix("gpt-5.1-codex-max"),
            ParseResult {
                model_name: "gpt-5.1-codex-max".to_string(),
                thinking_suffix: None,
                is_fast: false,
            }
        );
    }

    #[test]
    fn thinking_apply_suffix_overrides_body() {
        let body = json!({
            "reasoning_effort": "low"
        });
        let (out, base) = apply_thinking(&serde_json::to_vec(&body).unwrap(), "gpt-5.4-high");
        assert_eq!(base, "gpt-5.4");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning"]["effort"], "high");
    }

    #[test]
    fn thinking_apply_extracts_from_body_when_no_suffix() {
        let body = json!({
            "reasoning_effort": "high"
        });
        let (out, base) = apply_thinking(&serde_json::to_vec(&body).unwrap(), "gpt-5.4");
        assert_eq!(base, "gpt-5.4");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning"]["effort"], "high");
    }

    #[test]
    fn thinking_apply_fast_sets_priority_service_tier() {
        let body = json!({});
        let (out, base) = apply_thinking(&serde_json::to_vec(&body).unwrap(), "gpt-5.4-fast");
        assert_eq!(base, "gpt-5.4");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["service_tier"], "priority");
    }

    #[test]
    fn thinking_apply_small_budget_maps_to_auto() {
        let body = json!({});
        let (out, base) = apply_thinking(&serde_json::to_vec(&body).unwrap(), "gpt-5.4-512");
        assert_eq!(base, "gpt-5.4");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["reasoning"]["effort"], "auto");
    }
}
