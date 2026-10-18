//! Shared terminal and cached completeness admission; no publication happens here.
use super::{
    cache_proof, ensure_markdown_structure_preserved, markdown_context::SegmentSource,
    plan_translation, recover_generated_inline_code, validate_completeness_with_termination,
    CompletenessResult, CompletionTermination,
};
use anyhow::Result;
use hymt_core::{
    config::HotConfig,
    language::is_cjk_char,
    templates::{PromptOpts, TemplateType},
};
use hymt_segment::Segmenter;

pub(super) fn ensure_document_identities_preserved(source: &str, translated: &str) -> Result<()> {
    let failures = hymt_core::completeness::identity_preservation_failures(source, translated);
    if !failures.is_empty() {
        anyhow::bail!("translated document identities changed: {failures:?}");
    }
    Ok(())
}

pub(crate) fn cached_document_is_complete(
    source: &str,
    cached: &str,
    target_lang: &str,
    config: &HotConfig,
    opts: &PromptOpts,
    segmenter: &Segmenter,
) -> bool {
    if ensure_markdown_structure_preserved(source, cached).is_err()
        || !validate_completeness_with_termination(source, cached, CompletionTermination::Unknown)
            .is_complete
    {
        return false;
    }
    // Atomic JSON owns its keys through the identity validator, while values
    // and JSON whitespace remain model-owned rather than Markdown syntax.
    if hymt_core::completeness::parse_json_document(source).is_some() {
        return true;
    }
    plan_translation(
        source,
        target_lang,
        config,
        segmenter,
        &TemplateType::Default,
        opts,
    )
    .is_ok_and(|plan| cache_proof::cached_owned_payloads_match(&plan, cached))
}

pub(super) fn cached_segment_is_complete<'a>(
    index: usize,
    segment: impl Into<SegmentSource<'a>>,
    cached: &str,
) -> bool {
    let source = segment.into();
    let segment = source.text;
    if let Err(error) = source.structure_preserved(cached) {
        eprintln!(
            "Warning: cached segment {} changed Markdown structure, retranslating: {error}",
            index + 1
        );
        return false;
    }
    let result =
        validate_completeness_with_termination(segment, cached, CompletionTermination::Unknown);
    if result.is_complete {
        return true;
    }
    eprintln!(
        "Warning: cached segment {} did not pass validation, retranslating: {:?}",
        index + 1,
        result.checks_failed
    );
    false
}

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
    source: SegmentSource<'_>,
    attempt: &CheckedAttempt,
    opts: &PromptOpts,
    config: &HotConfig,
) -> Result<()> {
    if hymt_core::completeness::parse_json_document(source.text).is_none()
        && opts
            .warn_only_completeness
            .unwrap_or_else(|| config.completeness_warn_only())
    {
        return Ok(());
    }
    // Atomic JSON and explicit strict mode never publish a failed candidate.
    if !attempt.text.trim().is_empty() {
        source.structure_preserved(&attempt.text)?;
        ensure_document_identities_preserved(source.text, &attempt.text)?;
    }
    anyhow::bail!(
        "segment {} failed completeness after retries: {:?}",
        index + 1,
        attempt.validation.checks_failed,
    );
}

/// A provider response checked only for protocol and source-owned structure.
#[derive(Debug)]
pub(super) struct CheckedAttempt {
    pub(super) text: String,
    pub(super) validation: CompletenessResult,
    pub(super) structure_preserved: bool,
}

pub(super) fn validate_provider_attempt(
    source: SegmentSource<'_>,
    text: String,
    termination: CompletionTermination,
    unpublished: bool,
) -> CheckedAttempt {
    let text = if unpublished {
        recover_generated_inline_code(source, &text).unwrap_or(text)
    } else {
        text
    };
    CheckedAttempt {
        structure_preserved: source.structure_preserved(&text).is_ok(),
        validation: validate_completeness_with_termination(source.text, &text, termination),
        text,
    }
}
