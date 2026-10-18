//! Transport completion and source-only token recognition.
//!
//! Translation wording, length and paragraph layout belong to the model. Source
//! partitioning and protected Markdown reconstruction belong to the harness.
use serde::{Deserialize, Serialize};

mod json_content;
pub use json_content::json_string_values;
mod placeholders;
pub use placeholders::{preserved_placeholder_ranges, preserved_placeholders};
mod urls;
pub use urls::preserved_url_ranges;
pub use urls::urls as preserved_urls;

/// Bump app-cache keys when mechanical output admission changes.
pub const OUTPUT_ADMISSION_VERSION: &[u8] = b"input-only-v3";

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

/// Check transport termination without inspecting model-authored text.
pub fn validate_completion_termination(termination: CompletionTermination) -> CompletenessResult {
    let mut checks_failed = Vec::new();
    match termination {
        CompletionTermination::Length => checks_failed.push("finish_reason_length".to_owned()),
        CompletionTermination::Timeout => checks_failed.push("timeout".to_owned()),
        CompletionTermination::MissingTerminal => checks_failed.push("missing_terminal".to_owned()),
        CompletionTermination::Unknown | CompletionTermination::Stop => {}
    }
    CompletenessResult {
        is_complete: checks_failed.is_empty(),
        checks_failed,
    }
}

/// Recognize a complete JSON object or array for source planning.
pub fn parse_json_document(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    (trimmed.starts_with('{') || trimmed.starts_with('['))
        .then(|| serde_json::from_str::<serde_json::Value>(trimmed).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_completion_is_independent_of_model_content() {
        for termination in [CompletionTermination::Stop, CompletionTermination::Unknown] {
            assert!(validate_completion_termination(termination).is_complete);
        }
        for (termination, failure) in [
            (CompletionTermination::Length, "finish_reason_length"),
            (CompletionTermination::Timeout, "timeout"),
            (CompletionTermination::MissingTerminal, "missing_terminal"),
        ] {
            let result = validate_completion_termination(termination);
            assert!(!result.is_complete);
            assert_eq!(result.checks_failed, [failure]);
        }
    }
}
