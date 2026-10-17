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
    if let Some(input_json) = parse_json_document(input_text) {
        match serde_json::from_str::<serde_json::Value>(output_text.trim()) {
            Ok(output_json) => {
                if !unique_json_keys(input_text)
                    || !unique_json_keys(output_text)
                    || !preserves_json_contract(&input_json, &output_json, &mut checks_failed)
                {
                    checks_failed.push("json_key_preservation".to_owned());
                }
            }
            Err(_) => checks_failed.push("json_validity".to_owned()),
        }
        checks_failed.sort();
        checks_failed.dedup();
        return checks_failed;
    }
    text_identity_failures(input_text, output_text, &mut checks_failed);
    checks_failed
}

fn text_identity_failures(input_text: &str, output_text: &str, checks_failed: &mut Vec<String>) {
    if !preserves_occurrences(input_text, output_text, preserved_placeholder_ranges) {
        checks_failed.push("placeholder_preservation".to_owned());
    }
    if !preserves_occurrences(input_text, output_text, preserved_url_ranges) {
        checks_failed.push("url_preservation".to_owned());
    }
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

fn preserves_json_contract(
    input: &serde_json::Value,
    output: &serde_json::Value,
    failures: &mut Vec<String>,
) -> bool {
    match input {
        serde_json::Value::Object(input) => {
            let serde_json::Value::Object(output) = output else {
                return false;
            };
            input.len() == output.len()
                && input.iter().all(|(key, input_value)| {
                    output.get(key).is_some_and(|output_value| {
                        preserves_json_contract(input_value, output_value, failures)
                    })
                })
        }
        serde_json::Value::Array(input) => {
            let serde_json::Value::Array(output) = output else {
                return false;
            };
            input.len() == output.len()
                && input.iter().zip(output).all(|(input_value, output_value)| {
                    preserves_json_contract(input_value, output_value, failures)
                })
        }
        serde_json::Value::String(input) => {
            let serde_json::Value::String(output) = output else {
                return false;
            };
            text_identity_failures(input, output, failures);
            true
        }
        _ => input == output,
    }
}

// Value intentionally normalizes JSON strings and numbers, but its map loses
// duplicate keys. Reject that ambiguity before trusting the normalized value.
fn unique_json_keys(text: &str) -> bool {
    use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
    struct Keys;
    impl<'de> Deserialize<'de> for Keys {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_any(Self)
        }
    }
    impl<'de> Visitor<'de> for Keys {
        type Value = Self;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("JSON with unique object keys")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self, M::Error> {
            let mut keys = HashSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !keys.insert(key) {
                    return Err(serde::de::Error::custom("duplicate JSON key"));
                }
                map.next_value::<Keys>()?;
            }
            Ok(self)
        }
        fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self, S::Error> {
            while seq.next_element::<Keys>()?.is_some() {}
            Ok(self)
        }
        fn visit_bool<E>(self, _: bool) -> Result<Self, E> {
            Ok(self)
        }
        fn visit_i64<E>(self, _: i64) -> Result<Self, E> {
            Ok(self)
        }
        fn visit_u64<E>(self, _: u64) -> Result<Self, E> {
            Ok(self)
        }
        fn visit_f64<E>(self, _: f64) -> Result<Self, E> {
            Ok(self)
        }
        fn visit_str<E>(self, _: &str) -> Result<Self, E> {
            Ok(self)
        }
        fn visit_unit<E>(self) -> Result<Self, E> {
            Ok(self)
        }
    }
    serde_json::from_str::<Keys>(text).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_identity_compares_decoded_corresponding_values() {
        let source = r#"{"message":"Read {{field}} and {{field}} at https://example.test/a and https://example.test/a.","other":"Keep {name}."}"#;
        let literal = r#"{"message":"请阅读 {{field}} 和 {{field}}，参见 https://example.test/a 和 https://example.test/a.","other":"保留 {name}。"}"#;
        let escaped = literal.replace('{', "\\u007b").replace('}', "\\u007d");
        // Escape string contents, leaving the JSON object delimiters intact.
        let escaped = format!("{{{}}}", &escaped[6..escaped.len() - 6])
            .replace("https://", "https:\\/\\/")
            .replace("message", "\\u006dessage");
        for (input, output) in [
            (source, literal),
            (source, escaped.as_str()),
            (escaped.as_str(), literal),
        ] {
            assert!(
                identity_preservation_failures(input, output).is_empty(),
                "{output}"
            );
        }
        for output in [
            literal.replacen("{{field}}", "字段", 1),
            literal.replacen("https://example.test/a", "地址", 1),
            literal
                .replace("Keep", "保留")
                .replace("{{field}} 和 {{field}}", "字段"),
            literal.replace("{name}", r#"\\u007bname\\u007d"#),
            literal
                .replace("保留 {name}", "保留 {{field}} 和 {{field}} {name}")
                .replace("请阅读 {{field}} 和 {{field}}", "请阅读字段"),
        ] {
            assert!(
                !identity_preservation_failures(source, &output).is_empty(),
                "{output}"
            );
        }
    }

    #[test]
    fn json_identity_rejects_duplicate_keys_at_every_depth() {
        for (source, duplicate) in [
            (
                r#"{"message":"Read"}"#,
                r#"{"message":"lost","message":"阅读"}"#,
            ),
            (
                r#"{"items":[{"message":"Read"}]}"#,
                r#"{"items":[{"message":"lost","message":"阅读"}]}"#,
            ),
            (
                r#"{"message":"Read"}"#,
                r#"{"message":"lost","\u006dessage":"阅读"}"#,
            ),
        ] {
            assert!(!identity_preservation_failures(source, duplicate).is_empty());
            assert!(!identity_preservation_failures(duplicate, source).is_empty());
        }
    }

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
