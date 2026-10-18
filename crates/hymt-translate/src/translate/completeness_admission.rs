//! Transport completion checks; model-authored content is returned unchanged.
use super::{CompletenessResult, CompletionTermination};
use anyhow::Result;
use hymt_core::{config::HotConfig, language::is_cjk_char, templates::PromptOpts};

pub(super) fn approx_source_tokens(segment: &str) -> usize {
    if let Some(value) = hymt_core::completeness::parse_json_document(segment) {
        return hymt_core::completeness::json_string_values(&value)
            .into_iter()
            .map(approx_plain_tokens)
            .sum();
    }
    approx_plain_tokens(segment)
}

fn approx_plain_tokens(segment: &str) -> usize {
    if segment.is_empty() {
        return 0;
    }

    let mut cjk = 0usize;
    let mut other = 0usize;
    for character in segment.chars() {
        if is_cjk_char(character) {
            cjk += 1;
        } else {
            other += 1;
        }
    }

    cjk + other.div_ceil(4)
}

pub(super) fn validate_exhausted_attempt(
    index: usize,
    source: &str,
    attempt: &CheckedAttempt,
    opts: &PromptOpts,
    config: &HotConfig,
) -> Result<()> {
    if hymt_core::completeness::parse_json_document(source).is_none()
        && opts
            .warn_only_completeness
            .unwrap_or_else(|| config.completeness_warn_only())
    {
        return Ok(());
    }
    anyhow::bail!(
        "segment {} failed completeness after retries: {:?}",
        index + 1,
        attempt.validation.checks_failed,
    );
}

#[derive(Debug)]
pub(super) struct CheckedAttempt {
    pub(super) text: String,
    pub(super) validation: CompletenessResult,
}

pub(super) fn validate_provider_attempt(
    text: String,
    termination: CompletionTermination,
) -> CheckedAttempt {
    CheckedAttempt {
        validation: hymt_core::completeness::validate_completion_termination(termination),
        text,
    }
}
