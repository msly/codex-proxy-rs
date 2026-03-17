use std::fs;
use std::path::PathBuf;

use codex_proxy_rs::{thinking, translate};

fn fixture_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("fixtures/translate");
    p
}

#[test]
fn golden_translate_fixtures() {
    let dir = fixture_dir();
    let mut entries: Vec<_> = fs::read_dir(&dir)
        .expect("read fixtures/translate")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort();

    let mut ran = 0usize;
    for input_path in entries {
        let Some(name) = input_path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.ends_with(".input.json") {
            continue;
        }
        let expected_path =
            input_path.with_file_name(name.replace(".input.json", ".expected.json"));
        if !expected_path.exists() {
            panic!("missing expected fixture: {}", expected_path.display());
        }

        let input_bytes = fs::read(&input_path).expect("read input fixture");
        let input_value: serde_json::Value =
            serde_json::from_slice(&input_bytes).expect("parse input fixture json");

        let model = input_value
            .get("model")
            .and_then(|v| v.as_str())
            .expect("fixture must contain model");
        let stream = input_value
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let (body_after_thinking, base_model) = thinking::apply_thinking(&input_bytes, model);
        let out_bytes =
            translate::convert_openai_request_to_codex(&base_model, &body_after_thinking, stream);

        let out_value: serde_json::Value =
            serde_json::from_slice(&out_bytes).expect("parse output json");
        let expected_bytes = fs::read(&expected_path).expect("read expected fixture");
        let expected_value: serde_json::Value =
            serde_json::from_slice(&expected_bytes).expect("parse expected json");

        assert_eq!(
            out_value,
            expected_value,
            "fixture mismatch: {}",
            input_path.display()
        );
        ran += 1;
    }

    assert!(
        ran > 0,
        "no .input.json fixtures found in {}",
        dir.display()
    );
}
