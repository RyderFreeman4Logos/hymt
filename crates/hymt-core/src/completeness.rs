//! Mechanical admission for provider output: transport, emptiness and identities.
//!
//! Translation wording, length and paragraph layout belong to the model. Source
//! partitioning and protected Markdown reconstruction belong to the harness.
use serde::{Deserialize, Serialize};

mod identity;
pub use identity::{identity_preservation_failures, parse_json_document, preserved_urls};
mod json_content;
pub use json_content::json_string_values;
mod placeholders;
pub use placeholders::{preserved_placeholder_ranges, preserved_placeholders};
mod urls;
pub use urls::preserved_url_ranges;
use urls::urls;

/// Bump app-cache keys when mechanical output admission changes.
pub const OUTPUT_ADMISSION_VERSION: &[u8] = b"terminal-required-v2";

/// Final document state; this is not a translation quality assessment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletenessStatus {
    Valid,
    DegradedBestEffort,
}

/// Transport termination signal supplied when a caller has it available.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionTermination {
    #[default]
    Unknown,
    Stop,
    Length,
    Timeout,
    MissingTerminal,
}

/// Mechanical failures that require repair or original-source retention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletenessResult {
    pub is_complete: bool,
    pub checks_failed: Vec<String>,
}

/// Check empty output and source identities without transport information.
pub fn validate_completeness(input_text: &str, output_text: &str) -> CompletenessResult {
    validate_completeness_with_termination(input_text, output_text, CompletionTermination::Unknown)
}

/// Check observable protocol and identity contracts, without judging model text.
pub fn validate_completeness_with_termination(
    input_text: &str,
    output_text: &str,
    termination: CompletionTermination,
) -> CompletenessResult {
    let mut checks_failed = Vec::new();
    if output_text.trim().is_empty() {
        checks_failed.push("empty_output".to_owned());
    }
    match termination {
        CompletionTermination::Length => checks_failed.push("finish_reason_length".to_owned()),
        CompletionTermination::Timeout => checks_failed.push("timeout".to_owned()),
        CompletionTermination::MissingTerminal => checks_failed.push("missing_terminal".to_owned()),
        CompletionTermination::Unknown | CompletionTermination::Stop => {}
    }
    checks_failed.extend(identity_preservation_failures(input_text, output_text));
    if let (Some(input), Some(output)) = (
        parse_json_document(input_text),
        parse_json_document(output_text),
    ) {
        for failure in json_content::string_failures(&input, &output) {
            if !checks_failed.contains(&failure) {
                checks_failed.push(failure);
            }
        }
    }
    CompletenessResult {
        is_complete: checks_failed.is_empty(),
        checks_failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_text_is_not_rejected_by_content_heuristics() {
        let source = "Read the complete source instruction and preserve its detailed meaning.\n\nSecond complete source paragraph.\n\nThird source paragraph.";
        let expanded = "Expanded output. ".repeat(100);
        for output in [
            "译",
            "cannot translate",
            "无法翻译",
            "One merged paragraph.",
            expanded.as_str(),
        ] {
            let result = validate_completeness(source, output);
            assert!(
                result.is_complete,
                "model text must not trigger repair: {result:?}"
            );
        }
        let result = validate_completeness("# A heading\n\n```\ncode\n```", "Model prose.");
        assert!(
            result.is_complete,
            "generic heading/fence counts are not identity guards: {result:?}"
        );
    }

    #[test]
    fn complete_zh_to_en() {
        let input = "这是一段较长的中文文本。它包含多个句子，用于测试翻译完整性验证功能。";
        let output = "This is a longer Chinese passage. It contains multiple sentences to test translation completeness.";
        let result = validate_completeness(input, output);
        assert!(
            result.is_complete,
            "checks_failed={:?}",
            result.checks_failed
        );
    }

    #[test]
    fn complete_en_to_zh() {
        let input = "This is a test sentence used for completeness validation.";
        let output = "这是一个用于完整性验证的测试句子，内容相当充分。";
        let result = validate_completeness(input, output);
        assert!(
            result.is_complete,
            "checks_failed={:?}",
            result.checks_failed
        );
    }

    #[test]
    fn complete_chinese_to_english_with_protected_url_passes() {
        let input = "请在更改生产服务之前阅读 https://example.test/docs 上的安装指南。";
        let output = "Please read the installation guide at https://example.test/docs before changing the production service.";
        let result = validate_completeness(input, output);
        assert!(result.is_complete, "{:?}", result.checks_failed);
        assert!(!result
            .checks_failed
            .contains(&"url_preservation".to_owned()));
    }

    #[test]
    fn dropped_url_in_chinese_to_english_still_fails() {
        let input = "请在更改生产服务之前阅读 https://example.test/docs 上的安装指南。";
        let output = "Please read the installation guide before changing the production service.";
        let result = validate_completeness(input, output);
        assert!(result
            .checks_failed
            .contains(&"url_preservation".to_owned()));
    }

    #[test]
    fn url_preservation_normalizes_bare_urls_before_cjk_prose_after_delimiters() {
        let input = "Open letter: https://example.test/open-letter）中文。";
        let output = "公开信来源：https://example.test/open-letter";

        let result = validate_completeness(input, output);

        assert!(
            !result
                .checks_failed
                .contains(&"url_preservation".to_owned()),
            "a bare URL before a CJK delimiter and prose has the same target: {result:?}"
        );
    }

    #[test]
    fn url_preservation_accepts_bare_url_rewritten_as_markdown_link() {
        let input = "文本[https://example.test/path?query=value]";
        let output = "[翻译后的链接](https://example.test/path?query=value)";

        assert_eq!(urls(input), urls(output));
    }

    #[test]
    fn url_preservation_still_rejects_a_missing_absolute_target() {
        let result = validate_completeness(
            "Reference: https://example.test/required",
            "Reference omitted.",
        );

        assert!(result
            .checks_failed
            .contains(&"url_preservation".to_owned()));
    }

    #[test]
    fn url_preservation_rejects_missing_earliest_http_target() {
        let result = validate_completeness(
            "References: http://first.example/required and https://second.example/retained",
            "References: https://second.example/retained",
        );

        assert!(result
            .checks_failed
            .contains(&"url_preservation".to_owned()));
    }

    #[test]
    fn url_preservation_keeps_cjk_path_characters() {
        let input = "[source](https://example.test/中文)";
        let preserved = validate_completeness(input, input);
        let truncated = validate_completeness(input, "[source](https://example.test/)");

        assert!(
            !preserved
                .checks_failed
                .contains(&"url_preservation".to_owned()),
            "CJK path should be preserved: {preserved:?}"
        );
        assert!(
            truncated
                .checks_failed
                .contains(&"url_preservation".to_owned()),
            "dropping the CJK path should fail preservation: {truncated:?}"
        );
    }

    #[test]
    fn malformed_json_is_a_structural_failure() {
        let result = validate_completeness(r#"{"title":"Hello"}"#, "{not-json}");

        assert!(result.checks_failed.contains(&"json_validity".to_owned()));
    }

    #[test]
    fn missing_json_object_key_is_a_structural_failure() {
        let result = validate_completeness(
            r#"{"required_key":"value","retained_key":"value"}"#,
            r#"{"retained_key":"valeur"}"#,
        );

        assert!(result
            .checks_failed
            .contains(&"json_key_preservation".to_owned()));
    }

    #[test]
    fn replaced_json_object_key_is_a_structural_failure() {
        let result = validate_completeness(
            r#"{"required_key":{"nested_key":"value"}}"#,
            r#"{"different_key":{"nested_key":"valeur"}}"#,
        );

        assert!(result
            .checks_failed
            .contains(&"json_key_preservation".to_owned()));
    }

    #[test]
    fn missing_nested_json_object_key_is_a_structural_failure() {
        let result = validate_completeness(
            r#"{"required_key":{"nested_key":"value"}}"#,
            r#"{"required_key":{}}"#,
        );

        assert!(result
            .checks_failed
            .contains(&"json_key_preservation".to_owned()));
    }

    #[test]
    fn finish_reason_length_is_retryable_transport_failure() {
        let termination = CompletionTermination::Length;
        let result =
            validate_completeness_with_termination("Source text.", "Translated text.", termination);

        assert!(result
            .checks_failed
            .contains(&"finish_reason_length".to_owned()));
    }
    #[test]
    fn empty_responses_and_transport_failures_remain_rejected() {
        for output in ["", " ", "\n\t"] {
            assert!(validate_completeness("source", output)
                .checks_failed
                .iter()
                .any(|code| code == "empty_output"));
        }
        let result = validate_completeness_with_termination(
            "source",
            "text",
            CompletionTermination::Timeout,
        );
        assert!(result.checks_failed.iter().any(|code| code == "timeout"));
    }
}
