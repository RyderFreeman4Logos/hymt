//! Source JSON string extraction for token accounting.
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
