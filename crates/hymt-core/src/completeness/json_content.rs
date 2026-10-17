//! JSON layout and keys do not measure translated string content.
use super::{compute_stats, validate_completeness, CompletenessStats, CompletenessThresholds};
use serde_json::Value;

/// Returns string values without treating object keys as translated content.
pub fn json_string_values(value: &Value) -> Vec<&str> {
    let mut strings = Vec::new();
    collect_strings(value, &mut strings);
    strings
}

fn collect_strings<'a>(value: &'a Value, strings: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => strings.push(text),
        Value::Array(items) => items.iter().for_each(|item| collect_strings(item, strings)),
        Value::Object(items) => items
            .values()
            .for_each(|item| collect_strings(item, strings)),
        _ => {}
    }
}

pub(super) fn stats(value: &Value) -> CompletenessStats {
    let mut total = compute_stats("");
    for text in json_string_values(value) {
        let item = compute_stats(text);
        total.unicode_scalar_count += item.unicode_scalar_count;
        total.estimated_tokens += item.estimated_tokens;
        total.paragraph_count += item.paragraph_count;
        total.heading_count += item.heading_count;
        total.fenced_code_block_count += item.fenced_code_block_count;
        total.url_count += item.url_count;
        total.placeholder_count += item.placeholder_count;
    }
    total
}

pub(super) fn string_failures(
    input: &Value,
    output: &Value,
    target: &str,
    thresholds: &CompletenessThresholds,
) -> Vec<String> {
    let mut failures = Vec::new();
    visit_pairs(input, output, target, thresholds, &mut failures);
    failures
}

fn visit_pairs(
    input: &Value,
    output: &Value,
    target: &str,
    thresholds: &CompletenessThresholds,
    failures: &mut Vec<String>,
) {
    match (input, output) {
        (Value::String(source), Value::String(translated)) if !source.trim().is_empty() => {
            failures.extend(
                validate_completeness(source, translated, target, Some(thresholds)).checks_failed,
            );
        }
        (Value::Array(source), Value::Array(translated)) => {
            for (source, translated) in source.iter().zip(translated) {
                visit_pairs(source, translated, target, thresholds, failures);
            }
        }
        (Value::Object(source), Value::Object(translated)) => {
            for (key, source) in source {
                if let Some(translated) = translated.get(key) {
                    visit_pairs(source, translated, target, thresholds, failures);
                }
            }
        }
        _ => {} // The shared identity validator owns missing keys and type changes.
    }
}
