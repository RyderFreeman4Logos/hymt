//! Mechanical identity contract shared by retry, document, and cache admission.
use super::{preserved_placeholder_ranges, preserved_url_ranges, urls};
use std::{
    collections::{HashMap, HashSet},
    ops::Range,
};

/// Identity failures independent of translation density and transport termination.
/// Suitable for validating a reconstructed document with source-owned syntax.
pub fn identity_preservation_failures(input_text: &str, output_text: &str) -> Vec<String> {
    let mut checks_failed = Vec::new();
    if !preserves_occurrences(input_text, output_text, preserved_placeholder_ranges) {
        checks_failed.push("placeholder_preservation".to_owned());
    }
    if !preserves_occurrences(input_text, output_text, preserved_url_ranges) {
        checks_failed.push("url_preservation".to_owned());
    }
    if let Some(input_json) = parse_json_document(input_text) {
        match serde_json::from_str::<serde_json::Value>(output_text.trim()) {
            Ok(output_json) if preserves_json_contract(&input_json, &output_json) => {}
            Ok(_) => checks_failed.push("json_key_preservation".to_owned()),
            Err(_) => checks_failed.push("json_validity".to_owned()),
        }
    }
    checks_failed
}

pub fn preserved_urls(text: &str) -> HashSet<String> {
    urls(text)
}

// Each grammar scans each document once (placeholders retain their two passes).
// Counting ranges preserves multiplicity without one full-text search per identity.
fn preserves_occurrences(input: &str, output: &str, scan: fn(&str) -> Vec<Range<usize>>) -> bool {
    let counts = |text: &str| {
        let mut counts = HashMap::<String, usize>::new();
        for range in scan(text) {
            *counts.entry(text[range].to_owned()).or_default() += 1;
        }
        counts
    };
    let output = counts(output);
    counts(input)
        .iter()
        .all(|(token, count)| output.get(token).is_some_and(|actual| actual >= count))
}

/// Parse a complete JSON object or array using the completeness contract.
pub fn parse_json_document(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    (trimmed.starts_with('{') || trimmed.starts_with('['))
        .then(|| serde_json::from_str::<serde_json::Value>(trimmed).ok())
        .flatten()
}

fn preserves_json_contract(input: &serde_json::Value, output: &serde_json::Value) -> bool {
    match input {
        serde_json::Value::Object(input) => {
            let serde_json::Value::Object(output) = output else {
                return false;
            };
            input.len() == output.len()
                && input.iter().all(|(key, input_value)| {
                    output.get(key).is_some_and(|output_value| {
                        preserves_json_contract(input_value, output_value)
                    })
                })
        }
        serde_json::Value::Array(input) => {
            let serde_json::Value::Array(output) = output else {
                return false;
            };
            input.len() == output.len()
                && input.iter().zip(output).all(|(input_value, output_value)| {
                    preserves_json_contract(input_value, output_value)
                })
        }
        serde_json::Value::String(_) => output.is_string(),
        _ => input == output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_contract_preserves_every_repeated_occurrence() {
        let mut admitted = Vec::new();
        for (source, output, failure) in [
            (
                "Read https://example.test/a then https://example.test/a again.",
                "请阅读 https://example.test/a 内容。",
                "url_preservation",
            ),
            (
                "Read {{field}} then {{field}} again.",
                "请阅读 {{field}} 内容。",
                "placeholder_preservation",
            ),
            (
                "Read {field} then {field} again.",
                "请阅读 {field} 内容。",
                "placeholder_preservation",
            ),
        ] {
            if !identity_preservation_failures(source, output)
                .iter()
                .any(|item| item == failure)
            {
                admitted.push(source);
            }
            assert!(
                identity_preservation_failures(source, &source.replace("Read", "阅读")).is_empty()
            );
        }
        assert!(
            admitted.is_empty(),
            "lost occurrences admitted: {admitted:?}"
        );
    }

    #[test]
    fn identity_contract_preserves_json_shape_types_and_scalars() {
        let source = r#"{"message":"Read this instruction.","count":2,"enabled":true,"empty":null,"items":[{"name":"First"}]}"#;
        let original: serde_json::Value = serde_json::from_str(source).unwrap();
        let mut valid = original.clone();
        valid["message"] = serde_json::json!("请阅读操作说明。");
        valid["items"][0]["name"] = serde_json::json!("第一项");
        assert!(identity_preservation_failures(source, &valid.to_string()).is_empty());
        let mut admitted = Vec::new();
        for (label, key, value) in [
            ("number value", "count", serde_json::json!(3)),
            (
                "number container",
                "count",
                serde_json::json!({"invented": 2}),
            ),
            ("boolean", "enabled", serde_json::json!(false)),
            ("null", "empty", serde_json::json!("null")),
            ("string type", "message", serde_json::json!(["Text"])),
            ("extra key", "added", serde_json::json!("Text")),
            (
                "array appended",
                "items",
                serde_json::json!([{"name":"第一项"},{"name":"新增"}]),
            ),
            ("array shortened", "items", serde_json::json!([])),
            (
                "nested key moved",
                "items",
                serde_json::json!([{"nested":{"name":"第一项"}}]),
            ),
        ] {
            let mut candidate = valid.clone();
            candidate[key] = value;
            if identity_preservation_failures(source, &candidate.to_string()).is_empty() {
                admitted.push(label);
            }
        }
        assert!(
            admitted.is_empty(),
            "changed JSON contract admitted: {admitted:?}"
        );
    }
}
