//! Shared terminal and cached completeness admission; no publication happens here.
use super::{
    cache_proof, check_completeness, ensure_markdown_structure_preserved,
    markdown_context::SegmentSource, plan_translation, recover_generated_inline_code,
    CompletenessResult, CompletionTermination,
};
use anyhow::Result;
use hymt_core::{
    completeness::CompletenessContext,
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
        || !check_completeness(
            source,
            cached,
            target_lang,
            config,
            &CompletenessContext::default(),
        )
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
    .is_ok_and(|plan| cache_proof::cached_owned_payloads_match(&plan, cached, target_lang, config))
}

pub(super) fn cached_segment_is_complete<'a>(
    index: usize,
    segment: impl Into<SegmentSource<'a>>,
    cached: &str,
    target_lang: &str,
    config: &HotConfig,
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
    let result = check_completeness(
        segment,
        cached,
        target_lang,
        config,
        &source.completeness_context(CompletionTermination::Unknown),
    );
    if !result.advisory_warnings.is_empty() {
        eprintln!(
            "Note: cached segment {} has advisory warnings: {:?}",
            index + 1,
            result.advisory_warnings
        );
    }
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

/// A best-effort fallback must retain enough source material to remain usable.
/// Explicit warn-only mode still rejects empty responses and outputs below this
/// floor before any document can be published.
const MIN_BEST_EFFORT_OUTPUT_RATIO_DENOMINATOR: usize = 3;
const MIN_BEST_EFFORT_OUTPUT_RATIO_PERCENT: usize = 100 / MIN_BEST_EFFORT_OUTPUT_RATIO_DENOMINATOR;

pub(super) fn reject_unrecoverably_incomplete_best_attempt(
    index: usize,
    source: &str,
    translated: &str,
) -> Result<()> {
    if let (Some(source), Some(translated)) = (
        hymt_core::completeness::parse_json_document(source),
        hymt_core::completeness::parse_json_document(translated),
    ) {
        for (source, translated) in hymt_core::completeness::json_string_values(&source)
            .into_iter()
            .zip(hymt_core::completeness::json_string_values(&translated))
        {
            if !source.trim().is_empty() {
                reject_content_floor(index, source, translated)?;
            }
        }
        return Ok(());
    }
    reject_content_floor(index, source, translated)
}

fn reject_content_floor(index: usize, source: &str, translated: &str) -> Result<()> {
    let source_tokens = approx_source_tokens(source);
    let output_tokens = approx_source_tokens(translated);
    let empty = translated.trim().is_empty();
    let severely_truncated =
        output_tokens.saturating_mul(MIN_BEST_EFFORT_OUTPUT_RATIO_DENOMINATOR) < source_tokens;
    if empty || severely_truncated {
        anyhow::bail!(
            "segment {} best attempt is unrecoverably incomplete \
             (output_tokens={}, source_tokens={}, minimum_output_ratio={MIN_BEST_EFFORT_OUTPUT_RATIO_PERCENT}%)",
            index + 1,
            output_tokens,
            source_tokens,
        );
    }
    Ok(())
}

pub(super) fn validate_exhausted_attempt(
    index: usize,
    source: SegmentSource<'_>,
    best: &ScoredAttempt,
    opts: &PromptOpts,
    config: &HotConfig,
) -> Result<()> {
    // Empty output has no Markdown events; preserve the missing-output diagnostic.
    if best.text.trim().is_empty() {
        return reject_unrecoverably_incomplete_best_attempt(index, source.text, &best.text);
    }
    source.structure_preserved(&best.text)?;
    ensure_document_identities_preserved(source.text, &best.text)?;
    reject_unrecoverably_incomplete_best_attempt(index, source.text, &best.text)?;
    if !opts
        .warn_only_completeness
        .unwrap_or_else(|| config.completeness_warn_only())
    {
        anyhow::bail!(
            "segment {} failed completeness after retries: {:?}",
            index + 1,
            best.validation.checks_failed,
        );
    }
    Ok(())
}

/// One failed candidate retained only long enough to select the safest available
/// fallback. Its score ranks observable validation signals, not translation QE.
#[derive(Debug)]
pub(super) struct ScoredAttempt {
    pub(super) attempt: usize,
    pub(super) text: String,
    pub(super) validation: CompletenessResult,
    pub(super) structure_preserved: bool,
}

impl ScoredAttempt {
    pub(super) fn new<'a>(
        attempt: usize,
        source: impl Into<SegmentSource<'a>>,
        text: impl Into<String>,
        validation: CompletenessResult,
    ) -> Self {
        let source = source.into();
        let text = text.into();
        Self {
            attempt,
            structure_preserved: source.structure_preserved(&text).is_ok(),
            text,
            validation,
        }
    }

    pub(super) fn selection_reason(&self) -> &'static str {
        "structural_validity_then_validation_score"
    }
}

pub(super) fn score_provider_attempt(
    attempt: usize,
    source: SegmentSource<'_>,
    text: String,
    target_lang: &str,
    config: &HotConfig,
    termination: CompletionTermination,
    unpublished: bool,
) -> ScoredAttempt {
    let text = if unpublished {
        recover_generated_inline_code(source, &text).unwrap_or(text)
    } else {
        text
    };
    let validation = check_completeness(
        source.text,
        &text,
        target_lang,
        config,
        &source.completeness_context(termination),
    );
    ScoredAttempt::new(attempt, source, text, validation)
}

/// Prefer structurally valid attempts, then the earliest highest-scoring one.
pub(super) fn select_best_attempt(
    current: Option<ScoredAttempt>,
    candidate: ScoredAttempt,
) -> ScoredAttempt {
    match current {
        Some(current) if current.structure_preserved != candidate.structure_preserved => {
            if current.structure_preserved {
                current
            } else {
                candidate
            }
        }
        Some(current) if current.validation.score >= candidate.validation.score => current,
        _ => candidate,
    }
}
