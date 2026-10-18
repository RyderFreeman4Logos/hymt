//! Validate decoded JSON string emptiness and identities without judging wording.
use super::validate_completeness;
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

pub(super) fn string_failures(input: &Value, output: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    visit_pairs(input, output, &mut failures);
    failures
}

fn visit_pairs(input: &Value, output: &Value, failures: &mut Vec<String>) {
    match (input, output) {
        (Value::String(source), Value::String(translated)) if !source.trim().is_empty() => {
            failures.extend(validate_completeness(source, translated).checks_failed);
        }
        (Value::Array(source), Value::Array(translated)) => {
            for (source, translated) in source.iter().zip(translated) {
                visit_pairs(source, translated, failures);
            }
        }
        (Value::Object(source), Value::Object(translated)) => {
            for (key, source) in source {
                if let Some(translated) = translated.get(key) {
                    visit_pairs(source, translated, failures);
                }
            }
        }
        _ => {} // The shared identity validator owns missing keys and type changes.
    }
}
