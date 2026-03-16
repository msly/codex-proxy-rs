#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseResult {
    pub model_name: String,
    pub thinking_suffix: Option<String>,
    pub is_fast: bool,
}

const VALID_THINKING_SUFFIXES: &[&str] = &[
    "minimal", "low", "medium", "high", "xhigh", "max", "none", "auto",
];

// 模型名尾段与思考后缀冲突时，避免误剥离（对齐 Go 实现）。
const KNOWN_AMBIGUOUS_MODELS: &[&str] = &["gpt-5.1-codex-max"];

pub fn parse_model_suffix(model: &str) -> ParseResult {
    let mut model = model.trim().to_string();
    if model.is_empty() {
        return ParseResult {
            model_name: model,
            thinking_suffix: None,
            is_fast: false,
        };
    }

    let mut is_fast = false;
    let lower = model.to_lowercase();
    if lower.ends_with("-fast") && model.len() > 5 {
        is_fast = true;
        model.truncate(model.len() - 5);
    }

    let mut thinking_suffix: Option<String> = None;
    if let Some(last_dash) = model.rfind('-') {
        if last_dash > 0 && last_dash + 1 < model.len() {
            let tail = &model[last_dash + 1..];
            let tail_lower = tail.trim().to_lowercase();
            let is_ambiguous = KNOWN_AMBIGUOUS_MODELS
                .iter()
                .any(|m| m.eq_ignore_ascii_case(model.as_str()));

            if !is_ambiguous && is_thinking_or_budget_suffix(&tail_lower) {
                thinking_suffix = Some(tail_lower);
                model.truncate(last_dash);
            }
        }
    }

    ParseResult {
        model_name: model,
        thinking_suffix,
        is_fast,
    }
}

fn is_thinking_or_budget_suffix(tail_lower: &str) -> bool {
    if VALID_THINKING_SUFFIXES.iter().any(|s| s == &tail_lower) {
        return true;
    }
    if let Ok(v) = tail_lower.parse::<i64>() {
        // 对齐 Go：必须 > 100，避免 gpt-5 里的 “5” 被误判为预算
        return v > 100;
    }
    false
}
