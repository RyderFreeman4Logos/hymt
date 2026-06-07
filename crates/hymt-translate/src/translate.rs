//! Translation orchestration: segment → cache → parallel translate → completeness → reassemble.
//!
//! Pipeline:
//!   1. `plan_translation` — compute token budget, segment text, build section groups.
//!   2. `translate_text` — check cache per segment, translate missing ones in parallel
//!      (bounded by the concurrency semaphore inside `TranslationClient`), validate
//!      completeness with retry, reassemble, record history.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use chrono::{SecondsFormat, Utc};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_stream::StreamExt as _;

use hymt_cache::history::{format_duration, HistoryDB, TaskRecord};
use hymt_client::TranslationClient;
use hymt_core::completeness::{validate_completeness, CompletenessResult, CompletenessThresholds};
use hymt_core::config::HotConfig;
use hymt_core::language::{build_document_translation_plan, DocumentLanguagePlan, SectionKind};
use hymt_core::templates::{build_prompt, PromptOpts, TemplateType};
use hymt_segment::Segmenter;

// ── TranslationCtx ────────────────────────────────────────────────────────────

/// Shared translation service dependencies threaded through the pipeline.
pub struct TranslationCtx<'a> {
    pub config: &'a HotConfig,
    pub client: &'a TranslationClient,
    pub segmenter: &'a Segmenter,
    pub history: &'a HistoryDB,
}

/// Incremental translation output emitted by [`translate_text_stream`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    /// A streamed output chunk from the start of the reconstructed document.
    Token(String),
    /// A segment has completed and is available for reconstruction.
    ///
    /// Segment indexes are zero-based and match [`TranslationPlan::segments`].
    SegmentDone(usize),
    /// The complete reconstructed translation.
    AllDone(String),
}

/// Controls when segment 0 output is emitted while first-chunk priority is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamOutputMode {
    /// Buffer segment 0 until it passes completeness validation.
    Validated,
    /// Emit segment 0 tokens as soon as the streaming backend returns them.
    Optimistic,
}

// ── Token budget constants (matches translate.py) ─────────────────────────────

const OUTPUT_SAFETY_FACTOR: f64 = 1.5;
const MIN_EXPANSION_FOR_BUDGET: f64 = 1.0;

fn expansion_ratio(target_lang: &str) -> f64 {
    match target_lang.to_lowercase().trim() {
        "en" => 1.8,
        "zh" | "zh-cn" | "zh-tw" => 0.7,
        "ja" => 1.0,
        "ko" => 0.9,
        "de" | "fr" | "es" => 1.3,
        "ru" => 1.2,
        _ => 1.2,
    }
}

// ── TranslationPlan ───────────────────────────────────────────────────────────

/// Segmentation plan for a single translation task.
pub struct TranslationPlan {
    /// Total token count of the original source text.
    pub source_tokens: usize,
    /// Segments to be translated (may span multiple document sections).
    pub segments: Vec<String>,
    /// Maximum tokens that can be submitted per segment.
    pub available_source_tokens: usize,
    /// Document language plan used to build the segments.
    pub document_plan: Option<DocumentLanguagePlan>,
    /// For each segment: the first section index of its group.
    pub segment_section_indexes: Vec<usize>,
    /// For each segment: all section indexes in its group (for correct reconstruction).
    pub segment_section_groups: Vec<Vec<usize>>,
}

impl TranslationPlan {
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Reassemble translated segments into the final document, restoring untranslated sections.
    pub fn reconstruct(&self, translations: &[String]) -> String {
        debug_assert_eq!(translations.len(), self.segments.len());
        match &self.document_plan {
            None => translations.join(""),
            Some(plan) if !self.segment_section_groups.is_empty() => {
                reconstruct_section_groups(plan, &self.segment_section_groups, translations)
            }
            Some(plan) => reconstruct_sections(plan, &self.segment_section_indexes, translations),
        }
    }

    /// Return untranslated source text that reconstructs before priority segment 0.
    fn reconstruct_prefix_before_segment(&self, segment_index: usize) -> String {
        debug_assert_eq!(
            segment_index, 0,
            "reconstruct_prefix_before_segment is only valid for segment 0"
        );
        let Some(plan) = &self.document_plan else {
            return String::new();
        };
        let first_section = self
            .segment_section_groups
            .get(segment_index)
            .and_then(|group| group.first().copied())
            .or_else(|| self.segment_section_indexes.get(segment_index).copied());
        let Some(first_section) = first_section else {
            return String::new();
        };
        plan.sections
            .iter()
            .take(first_section)
            .map(|section| section.text.as_str())
            .collect()
    }
}

fn untranslated_text_before_segment(
    plan: &TranslationPlan,
    segment_index: usize,
    next_section_index: &mut usize,
) -> String {
    let Some(doc_plan) = &plan.document_plan else {
        return String::new();
    };

    if let Some(group) = plan
        .segment_section_groups
        .get(segment_index)
        .filter(|group| !group.is_empty())
    {
        let start = group[0];
        if start < *next_section_index {
            return String::new();
        }
        let text = doc_plan.sections[*next_section_index..start]
            .iter()
            .map(|section| section.text.as_str())
            .collect();
        if let Some(end) = group.last() {
            *next_section_index = (*end + 1).max(*next_section_index);
        }
        return text;
    }

    let Some(start) = plan.segment_section_indexes.get(segment_index).copied() else {
        return String::new();
    };
    if start < *next_section_index {
        return String::new();
    }
    let text = doc_plan.sections[*next_section_index..start]
        .iter()
        .map(|section| section.text.as_str())
        .collect();
    *next_section_index = (start + 1).max(*next_section_index);
    text
}

fn untranslated_text_after_segments(plan: &TranslationPlan, next_section_index: usize) -> String {
    let Some(doc_plan) = &plan.document_plan else {
        return String::new();
    };
    doc_plan
        .sections
        .get(next_section_index..)
        .unwrap_or_default()
        .iter()
        .map(|section| section.text.as_str())
        .collect()
}

fn reconstruction_newline_after_segment(
    plan: &TranslationPlan,
    translations: &[Option<String>],
    segment_index: usize,
) -> Option<String> {
    let doc_plan = plan.document_plan.as_ref()?;

    if let Some(group) = plan
        .segment_section_groups
        .get(segment_index)
        .filter(|group| !group.is_empty())
    {
        if plan
            .segment_section_groups
            .get(segment_index + 1)
            .is_some_and(|next| next == group)
        {
            return None;
        }

        let source_text: String = group
            .iter()
            .map(|&i| doc_plan.sections[i].text.as_str())
            .collect();
        if !source_text.ends_with('\n') {
            return None;
        }

        let mut output = String::new();
        for (idx, candidate) in plan.segment_section_groups.iter().enumerate() {
            if candidate == group {
                output.push_str(translations.get(idx)?.as_deref()?);
            }
        }
        return (!output.ends_with('\n')).then(|| "\n".to_owned());
    }

    let section_index = *plan.segment_section_indexes.get(segment_index)?;
    if plan
        .segment_section_indexes
        .get(segment_index + 1)
        .is_some_and(|next| *next == section_index)
    {
        return None;
    }
    let section = doc_plan.sections.get(section_index)?;
    if !section.text.ends_with('\n') {
        return None;
    }

    let mut output = String::new();
    for (idx, candidate) in plan.segment_section_indexes.iter().enumerate() {
        if *candidate == section_index {
            output.push_str(translations.get(idx)?.as_deref()?);
        }
    }
    (!output.ends_with('\n')).then(|| "\n".to_owned())
}

fn reconstruct_sections(
    plan: &DocumentLanguagePlan,
    segment_section_indexes: &[usize],
    translations: &[String],
) -> String {
    let mut section_trans: HashMap<usize, Vec<&str>> = HashMap::new();
    for (sec_idx, trans) in segment_section_indexes.iter().zip(translations.iter()) {
        section_trans
            .entry(*sec_idx)
            .or_default()
            .push(trans.as_str());
    }
    let mut parts = Vec::with_capacity(plan.sections.len());
    for (i, section) in plan.sections.iter().enumerate() {
        if section.should_translate {
            let mut out = section_trans
                .get(&i)
                .map(|ts| ts.join(""))
                .unwrap_or_default();
            if section.text.ends_with('\n') && !out.ends_with('\n') {
                out.push('\n');
            }
            parts.push(out);
        } else {
            parts.push(section.text.clone());
        }
    }
    parts.join("")
}

fn reconstruct_section_groups(
    plan: &DocumentLanguagePlan,
    segment_section_groups: &[Vec<usize>],
    translations: &[String],
) -> String {
    let mut group_trans: HashMap<usize, Vec<&str>> = HashMap::new();
    let mut group_texts: HashMap<usize, String> = HashMap::new();
    let mut covered: HashSet<usize> = HashSet::new();

    for (group, trans) in segment_section_groups.iter().zip(translations.iter()) {
        if group.is_empty() {
            continue;
        }
        let first = group[0];
        group_trans.entry(first).or_default().push(trans.as_str());
        group_texts.entry(first).or_insert_with(|| {
            group
                .iter()
                .map(|&i| plan.sections[i].text.as_str())
                .collect()
        });
        covered.extend(group.iter().copied());
    }

    let mut parts = Vec::with_capacity(plan.sections.len());
    for (i, section) in plan.sections.iter().enumerate() {
        if covered.contains(&i) {
            if let Some(ts) = group_trans.get(&i) {
                let mut out = ts.join("");
                if let Some(gt) = group_texts.get(&i) {
                    if gt.ends_with('\n') && !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                parts.push(out);
            }
        } else {
            parts.push(section.text.clone());
        }
    }
    parts.join("")
}

// ── plan_translation ──────────────────────────────────────────────────────────

/// Compute token budget and segment `text` into a [`TranslationPlan`].
///
/// The available token budget per segment accounts for:
/// - prompt template overhead (measured via the segmenter)
/// - max output tokens reservation
/// - per-language expansion ratio with a safety factor
pub fn plan_translation(
    text: &str,
    target_lang: &str,
    config: &HotConfig,
    segmenter: &Segmenter,
    template: &TemplateType,
    opts: &PromptOpts,
) -> Result<TranslationPlan> {
    let overhead_prompt = build_prompt("", target_lang, template, opts)?;
    let overhead_tokens = segmenter.count_tokens(&overhead_prompt);
    let context_window = config.context_window() as usize;
    let max_output = config.max_output_tokens() as usize;

    let base_budget = context_window.saturating_sub(overhead_tokens + max_output);
    if base_budget == 0 {
        anyhow::bail!(
            "context_window ({context_window}) too small for template overhead \
             ({overhead_tokens}) + max_output_tokens ({max_output})"
        );
    }

    let ratio = expansion_ratio(target_lang).max(MIN_EXPANSION_FOR_BUDGET);
    let max_safe = ((max_output as f64) / (ratio * OUTPUT_SAFETY_FACTOR)) as usize;
    let available = base_budget.min(max_safe).max(1);

    if text.is_empty() {
        return Ok(TranslationPlan {
            source_tokens: 0,
            segments: Vec::new(),
            available_source_tokens: available,
            document_plan: None,
            segment_section_indexes: Vec::new(),
            segment_section_groups: Vec::new(),
        });
    }

    let doc_plan = build_document_translation_plan(text, target_lang);
    let source_tokens = segmenter.count_tokens(text);
    let (segments, indexes, groups) = segment_document_plan(&doc_plan, segmenter, available)?;

    Ok(TranslationPlan {
        source_tokens,
        segments,
        available_source_tokens: available,
        document_plan: Some(doc_plan),
        segment_section_indexes: indexes,
        segment_section_groups: groups,
    })
}

type SegmentPlanResult = (Vec<String>, Vec<usize>, Vec<Vec<usize>>);

fn segment_document_plan(
    doc_plan: &DocumentLanguagePlan,
    segmenter: &Segmenter,
    max_tokens: usize,
) -> Result<SegmentPlanResult> {
    let mut segments: Vec<String> = Vec::new();
    let mut indexes: Vec<usize> = Vec::new();
    let mut groups: Vec<Vec<usize>> = Vec::new();

    for group in translation_section_groups(doc_plan) {
        let text: String = group
            .iter()
            .map(|&i| doc_plan.sections[i].text.as_str())
            .collect();
        let segs = segmenter
            .segment(&text, max_tokens)
            .map_err(|e| anyhow!("segmentation error: {e}"))?;
        let group_clone = group.clone();
        for _ in 0..segs.len() {
            indexes.push(group[0]);
            groups.push(group_clone.clone());
        }
        segments.extend(segs);
    }
    Ok((segments, indexes, groups))
}

/// Groups consecutive translatable sections, absorbing intervening separators
/// when the next content section is also translatable.
fn translation_section_groups(plan: &DocumentLanguagePlan) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let sections = &plan.sections;

    for (i, section) in sections.iter().enumerate() {
        if section.should_translate {
            current.push(i);
            continue;
        }
        if section.kind == SectionKind::Separator && !current.is_empty() {
            let next_translatable = sections[i + 1..]
                .iter()
                .find(|s| s.kind != SectionKind::Separator)
                .is_some_and(|s| s.should_translate);
            if next_translatable {
                current.push(i);
                continue;
            }
        }
        if !current.is_empty() {
            groups.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

// ── Hashing ───────────────────────────────────────────────────────────────────

pub(crate) fn segment_cache_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    hex::encode(h.finalize())
}

pub(crate) fn template_options_hash(opts: &PromptOpts) -> String {
    let mut entries: Vec<(&str, serde_json::Value)> = Vec::new();
    if let Some(terms) = &opts.terms {
        let v: Vec<_> = terms.iter().map(|(a, b)| [a, b]).collect();
        entries.push(("terms", serde_json::json!(v)));
    }
    if let Some(s) = &opts.style {
        entries.push(("style", serde_json::json!(s)));
    }
    if let Some(inst) = &opts.instructions {
        entries.push(("instructions", serde_json::json!(inst)));
    }
    if let Some(ft) = &opts.format_type {
        entries.push(("format_type", serde_json::json!(ft)));
    }
    if let Some(ctx) = &opts.context {
        entries.push(("context", serde_json::json!(ctx)));
    }
    if entries.is_empty() {
        return String::new();
    }
    entries.sort_by_key(|(k, _)| *k);
    let map: serde_json::Map<String, serde_json::Value> = entries
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
    let json = serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_default();
    let mut h = Sha256::new();
    h.update(json.as_bytes());
    hex::encode(h.finalize())
}

// ── Completeness helpers ──────────────────────────────────────────────────────

fn completeness_thresholds(config: &HotConfig) -> CompletenessThresholds {
    CompletenessThresholds {
        zh_to_en_min_ratio: config.completeness_zh_to_en_min_ratio(),
        en_to_zh_min_ratio: config.completeness_en_to_zh_min_ratio(),
        min_paragraph_ratio: config.completeness_min_paragraph_ratio(),
    }
}

fn check_completeness(
    segment: &str,
    translated: &str,
    target_lang: &str,
    config: &HotConfig,
) -> CompletenessResult {
    let thresholds = completeness_thresholds(config);
    validate_completeness(segment, translated, target_lang, Some(&thresholds))
}

fn cached_segment_is_complete(
    index: usize,
    segment: &str,
    cached: &str,
    target_lang: &str,
    config: &HotConfig,
) -> bool {
    let result = check_completeness(segment, cached, target_lang, config);
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
        "Warning: cached segment {} failed completeness, retranslating: {:?}",
        index + 1,
        result.checks_failed
    );
    false
}

// ── Single-segment translation with completeness retry ────────────────────────

struct SegmentTranslateRequest<'a> {
    index: usize,
    client: &'a TranslationClient,
    segment: &'a str,
    target_lang: &'a str,
    template: &'a TemplateType,
    opts: &'a PromptOpts,
    config: &'a HotConfig,
}

async fn translate_segment_with_completeness(
    index: usize,
    client: &TranslationClient,
    segment: &str,
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    config: &HotConfig,
) -> Result<(String, f64)> {
    let max_retries = config.completeness_max_retries() as usize;
    let started = Instant::now();
    let mut best = String::new();

    for attempt in 0..=max_retries {
        let mut prompt = build_prompt(segment, target_lang, template, opts)?;
        if attempt > 0 {
            prompt.push_str("\n\nTranslate the COMPLETE input. Do not stop early.");
        }

        let translated = client
            .translate(&prompt)
            .await
            .map_err(|e| anyhow!("HTTP translation failed: {e}"))?;

        let result = check_completeness(segment, &translated, target_lang, config);
        best = translated;

        if !result.advisory_warnings.is_empty() {
            eprintln!(
                "Note: segment {} has advisory warnings: {:?}",
                index + 1,
                result.advisory_warnings
            );
        }

        if result.is_complete {
            return Ok((best, started.elapsed().as_secs_f64()));
        }

        let action = if attempt < max_retries {
            "retrying"
        } else {
            "retries exhausted"
        };
        eprintln!(
            "Warning: segment {} failed completeness (attempt {}/{}, {}): {:?}",
            index + 1,
            attempt + 1,
            max_retries + 1,
            action,
            result.checks_failed
        );
    }

    eprintln!(
        "Warning: segment {} exceeded {} retries, using best attempt",
        index + 1,
        max_retries
    );
    Ok((best, started.elapsed().as_secs_f64()))
}

async fn send_stream_event(tx: &mpsc::Sender<StreamEvent>, event: StreamEvent) -> Result<()> {
    tx.send(event)
        .await
        .map_err(|_| anyhow!("stream event receiver dropped"))
}

fn joined_segment(
    res: std::result::Result<Result<(usize, String)>, tokio::task::JoinError>,
) -> Result<(usize, String)> {
    match res {
        Ok(Ok(segment)) => Ok(segment),
        Ok(Err(e)) => Err(e.context("segment translation failed")),
        Err(e) => Err(anyhow!("segment task panicked: {e}")),
    }
}

async fn translate_segment_with_completeness_streaming(
    request: SegmentTranslateRequest<'_>,
    event_tx: &mpsc::Sender<StreamEvent>,
    first_token_tx: Option<mpsc::Sender<()>>,
    output_mode: StreamOutputMode,
) -> Result<(String, f64)> {
    let max_retries = request.config.completeness_max_retries() as usize;
    let started = Instant::now();
    let mut prompt = build_prompt(
        request.segment,
        request.target_lang,
        request.template,
        request.opts,
    )?;
    let mut stream = request
        .client
        .translate_stream(&prompt)
        .await
        .map_err(|e| anyhow!("HTTP streaming translation failed: {e}"))?;
    let mut translated = String::new();
    let mut streamed_tokens: Vec<String> = Vec::new();
    let mut first_token_tx = first_token_tx;
    let mut emitted_optimistically = false;

    while let Some(item) = stream.next().await {
        let token = item.map_err(|e| anyhow!("HTTP streaming translation failed: {e}"))?;
        if token.is_empty() {
            continue;
        }
        if let Some(tx) = first_token_tx.take() {
            let _ = tx.try_send(());
        }
        translated.push_str(&token);
        match output_mode {
            StreamOutputMode::Validated => streamed_tokens.push(token),
            StreamOutputMode::Optimistic => {
                emitted_optimistically = true;
                send_stream_event(event_tx, StreamEvent::Token(token)).await?;
            }
        }
    }

    let mut best = translated;
    let result = check_completeness(request.segment, &best, request.target_lang, request.config);
    if !result.advisory_warnings.is_empty() {
        eprintln!(
            "Note: segment {} has advisory warnings: {:?}",
            request.index + 1,
            result.advisory_warnings
        );
    }
    if result.is_complete {
        // Non-tty output replays buffered tokens only after completeness
        // validation. TTY output has already emitted them optimistically; in
        // both modes the parallel chunks are released by the genuine first SSE
        // token above, not by this validation point.
        if output_mode == StreamOutputMode::Validated {
            for token in streamed_tokens {
                send_stream_event(event_tx, StreamEvent::Token(token)).await?;
            }
        }
        send_stream_event(event_tx, StreamEvent::SegmentDone(request.index)).await?;
        return Ok((best, started.elapsed().as_secs_f64()));
    }

    let action = if max_retries > 0 {
        "retrying"
    } else {
        "retries exhausted"
    };
    eprintln!(
        "Warning: segment {} failed completeness (attempt 1/{}, {}): {:?}",
        request.index + 1,
        max_retries + 1,
        action,
        result.checks_failed
    );

    for attempt in 1..=max_retries {
        prompt = build_prompt(
            request.segment,
            request.target_lang,
            request.template,
            request.opts,
        )?;
        prompt.push_str("\n\nTranslate the COMPLETE input. Do not stop early.");

        let translated = request
            .client
            .translate(&prompt)
            .await
            .map_err(|e| anyhow!("HTTP translation failed: {e}"))?;

        let result = check_completeness(
            request.segment,
            &translated,
            request.target_lang,
            request.config,
        );
        best = translated;

        if !result.advisory_warnings.is_empty() {
            eprintln!(
                "Note: segment {} has advisory warnings: {:?}",
                request.index + 1,
                result.advisory_warnings
            );
        }

        if result.is_complete {
            if !best.is_empty()
                && (output_mode == StreamOutputMode::Validated || !emitted_optimistically)
            {
                send_stream_event(event_tx, StreamEvent::Token(best.clone())).await?;
                if let Some(tx) = first_token_tx.take() {
                    let _ = tx.try_send(());
                }
            }
            send_stream_event(event_tx, StreamEvent::SegmentDone(request.index)).await?;
            return Ok((best, started.elapsed().as_secs_f64()));
        }

        let action = if attempt < max_retries {
            "retrying"
        } else {
            "retries exhausted"
        };
        eprintln!(
            "Warning: segment {} failed completeness (attempt {}/{}, {}): {:?}",
            request.index + 1,
            attempt + 1,
            max_retries + 1,
            action,
            result.checks_failed
        );
    }

    eprintln!(
        "Warning: segment {} exceeded {} retries, using best attempt",
        request.index + 1,
        max_retries
    );
    if !best.is_empty() && (output_mode == StreamOutputMode::Validated || !emitted_optimistically) {
        send_stream_event(event_tx, StreamEvent::Token(best.clone())).await?;
        if let Some(tx) = first_token_tx.take() {
            let _ = tx.try_send(());
        }
    }
    send_stream_event(event_tx, StreamEvent::SegmentDone(request.index)).await?;
    Ok((best, started.elapsed().as_secs_f64()))
}

// ── Pipeline partition helper ─────────────────────────────────────────────────

/// Split `missing` into a priority chunk and the remaining parallel chunks.
///
/// When `first_chunk_priority` is enabled and chunk 0 is the first uncached
/// segment, it is returned as the exclusive priority chunk so it gets dedicated
/// GPU throughput before the rest are dispatched concurrently.  In all other
/// cases `priority_chunk` is `None` and `parallel` is a copy of `missing`.
fn partition_pipeline(
    missing: &[usize],
    first_chunk_priority: bool,
) -> (Option<usize>, Vec<usize>) {
    if first_chunk_priority && missing.first() == Some(&0) {
        (
            Some(0),
            missing.iter().copied().filter(|&i| i != 0).collect(),
        )
    } else {
        (None, missing.to_vec())
    }
}

// ── translate_text ─────────────────────────────────────────────────────────────

/// Translate `text` to `target_lang`, caching and translating segments in parallel.
///
/// All segments are checked against the cache first; only missing or incomplete
/// cached segments are translated.  Results are stored back to the cache and a
/// task record is written to the history DB.
pub async fn translate_text(
    text: &str,
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    ctx: &TranslationCtx<'_>,
) -> Result<String> {
    if text.is_empty() {
        return Ok(String::new());
    }

    let template_name = template.as_str();
    let plan = plan_translation(text, target_lang, ctx.config, ctx.segmenter, template, opts)?;

    eprintln!(
        "Source tokens: {}; segments: {}",
        plan.source_tokens,
        plan.segment_count()
    );

    let options_hash = template_options_hash(opts);
    let seg_hashes: Vec<String> = plan
        .segments
        .iter()
        .map(|s| segment_cache_hash(s))
        .collect();
    let _seg_tokens: Vec<usize> = plan
        .segments
        .iter()
        .map(|s| ctx.segmenter.count_tokens(s))
        .collect();

    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let wall_start = Instant::now();

    // ── Phase 1: cache lookup ─────────────────────────────────────────────────

    let mut translations: Vec<Option<String>> = vec![None; plan.segment_count()];
    let mut missing: Vec<usize> = Vec::new();

    for (i, hash) in seg_hashes.iter().enumerate() {
        match ctx
            .history
            .find_segment_cached(hash, target_lang, template_name, &options_hash)
        {
            Ok(Some(cached))
                if cached_segment_is_complete(
                    i,
                    &plan.segments[i],
                    &cached,
                    target_lang,
                    ctx.config,
                ) =>
            {
                translations[i] = Some(cached);
            }
            Ok(_) => missing.push(i),
            Err(e) => {
                eprintln!("Warning: cache lookup error: {e}");
                missing.push(i);
            }
        }
    }

    // ── Phase 2: parallel translate missing segments ───────────────────────────

    if !missing.is_empty() {
        // Pipeline mode: translate chunk 0 exclusively first so it gets full GPU
        // throughput and can be displayed while the remaining chunks are translating.
        let (priority_chunk, remaining) =
            partition_pipeline(&missing, ctx.config.first_chunk_priority());
        missing = remaining;
        if let Some(chunk_idx) = priority_chunk {
            let (translated, _) = translate_segment_with_completeness(
                chunk_idx,
                ctx.client,
                &plan.segments[chunk_idx],
                target_lang,
                template,
                opts,
                ctx.config,
            )
            .await?;
            let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
            if let Err(e) = ctx.history.store_segment_cache(
                &seg_hashes[chunk_idx],
                target_lang,
                template_name,
                &translated,
                &now,
                &options_hash,
            ) {
                eprintln!("Warning: cache store error: {e}");
            }
            translations[chunk_idx] = Some(translated);
        }

        if !missing.is_empty() {
            let mut join_set: JoinSet<Result<(usize, String)>> = JoinSet::new();

            for &idx in &missing {
                let client = ctx.client.clone();
                let segment = plan.segments[idx].clone();
                let target = target_lang.to_owned();
                let tmpl = template.clone();
                let cloned_opts = opts.clone();
                let cfg = ctx.config.clone();

                join_set.spawn(async move {
                    let (translated, _elapsed) = translate_segment_with_completeness(
                        idx,
                        &client,
                        &segment,
                        &target,
                        &tmpl,
                        &cloned_opts,
                        &cfg,
                    )
                    .await?;
                    Ok((idx, translated))
                });
            }

            while let Some(res) = join_set.join_next().await {
                match res {
                    Ok(Ok((idx, translated))) => {
                        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
                        if let Err(e) = ctx.history.store_segment_cache(
                            &seg_hashes[idx],
                            target_lang,
                            template_name,
                            &translated,
                            &now,
                            &options_hash,
                        ) {
                            eprintln!("Warning: cache store error: {e}");
                        }
                        translations[idx] = Some(translated);
                    }
                    Ok(Err(e)) => return Err(e.context("segment translation failed")),
                    Err(e) => return Err(anyhow!("segment task panicked: {e}")),
                }
            }
        }
    }

    // ── Phase 3: reconstruct + record ─────────────────────────────────────────

    let completed: Vec<String> = translations
        .into_iter()
        .enumerate()
        .map(|(i, t)| t.ok_or_else(|| anyhow!("missing translated segment {i}")))
        .collect::<Result<_>>()?;

    let translated = plan.reconstruct(&completed);
    let duration = wall_start.elapsed().as_secs_f64();
    let output_tokens = ctx.segmenter.count_tokens(&translated);
    let tps = if duration > 0.0 {
        output_tokens as f64 / duration
    } else {
        0.0
    };

    let record = TaskRecord {
        id: None,
        started_at,
        finished_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        duration_seconds: duration,
        input_tokens: plan.source_tokens as i64,
        output_tokens: output_tokens as i64,
        segments: plan.segment_count() as i64,
        concurrency: ctx.config.concurrency() as i64,
        source_lang: None,
        target_lang: target_lang.to_owned(),
        template_type: template_name.to_owned(),
        model: {
            let m = ctx.config.model();
            if m.is_empty() {
                None
            } else {
                Some(m)
            }
        },
        tokens_per_second: tps,
        input_chars: text.len() as i64,
        output_chars: translated.len() as i64,
        output_text: Some(translated.clone()),
        input_hash: None,
        config_version: ctx.config.config_version() as i64,
    };

    if let Err(e) = ctx.history.insert_task(&record) {
        eprintln!("Warning: failed to record timing history: {e}");
    } else {
        eprintln!(
            "Completed in {} | avg {:.1} tok/s | timing recorded",
            format_duration(duration),
            tps
        );
    }

    // Check for timing divergence
    let seg_count: i64 = plan.segment_count() as i64;
    let concurrency = ctx.config.concurrency() as i64;
    let cfg_ver = ctx.config.config_version() as i64;
    if let Ok(Some(estimate)) = ctx.history.estimate(
        seg_count,
        concurrency,
        Some(target_lang),
        Some(template_name),
        Some(cfg_ver),
        None,
    ) {
        let threshold = ctx.config.timing_divergence_threshold();
        let data = hymt_cache::TimingIssueData {
            input_tokens: plan.source_tokens as i64,
            output_tokens: output_tokens as i64,
            segments: seg_count,
            actual_seconds: duration,
            estimated_seconds: estimate.seconds,
            config_version: cfg_ver,
            target_lang: target_lang.to_owned(),
            template_type: template_name.to_owned(),
            concurrency,
            model: record.model.clone(),
        };
        if hymt_cache::is_divergent(&data, threshold) {
            eprintln!(
                "Warning: timing divergence detected (actual {:.1}s vs estimated {:.1}s)",
                duration, estimate.seconds
            );
        }
    }

    Ok(translated)
}

/// Translate `text` and emit incremental output events for the pipeline path.
///
/// Segment 0 output is buffered until completeness validation passes. When
/// first-chunk priority is disabled or segment 0 is already cached, the final
/// translation is emitted as [`StreamEvent::AllDone`] after the normal
/// translation completes.
pub async fn translate_text_stream(
    text: &str,
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    ctx: &TranslationCtx<'_>,
    event_tx: mpsc::Sender<StreamEvent>,
) -> Result<String> {
    translate_text_stream_with_mode(
        text,
        target_lang,
        template,
        opts,
        ctx,
        StreamOutputMode::Validated,
        event_tx,
    )
    .await
}

/// Translate `text` and emit incremental output events with an explicit mode.
pub async fn translate_text_stream_with_mode(
    text: &str,
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    ctx: &TranslationCtx<'_>,
    output_mode: StreamOutputMode,
    event_tx: mpsc::Sender<StreamEvent>,
) -> Result<String> {
    if text.is_empty() {
        send_stream_event(&event_tx, StreamEvent::AllDone(String::new())).await?;
        return Ok(String::new());
    }

    let template_name = template.as_str();
    let plan = plan_translation(text, target_lang, ctx.config, ctx.segmenter, template, opts)?;

    eprintln!(
        "Source tokens: {}; segments: {}",
        plan.source_tokens,
        plan.segment_count()
    );

    let options_hash = template_options_hash(opts);
    let seg_hashes: Vec<String> = plan
        .segments
        .iter()
        .map(|s| segment_cache_hash(s))
        .collect();
    let _seg_tokens: Vec<usize> = plan
        .segments
        .iter()
        .map(|s| ctx.segmenter.count_tokens(s))
        .collect();

    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let wall_start = Instant::now();

    let mut translations: Vec<Option<String>> = vec![None; plan.segment_count()];
    let mut missing: Vec<usize> = Vec::new();

    for (i, hash) in seg_hashes.iter().enumerate() {
        match ctx
            .history
            .find_segment_cached(hash, target_lang, template_name, &options_hash)
        {
            Ok(Some(cached))
                if cached_segment_is_complete(
                    i,
                    &plan.segments[i],
                    &cached,
                    target_lang,
                    ctx.config,
                ) =>
            {
                translations[i] = Some(cached);
            }
            Ok(_) => missing.push(i),
            Err(e) => {
                eprintln!("Warning: cache lookup error: {e}");
                missing.push(i);
            }
        }
    }

    if !ctx.config.first_chunk_priority() {
        let mut next_section_index = 0;
        for idx in 0..plan.segment_count() {
            let prefix = untranslated_text_before_segment(&plan, idx, &mut next_section_index);
            if !prefix.is_empty() {
                send_stream_event(&event_tx, StreamEvent::Token(prefix)).await?;
            }

            if let Some(cached) = translations[idx].as_ref() {
                if !cached.is_empty() {
                    send_stream_event(&event_tx, StreamEvent::Token(cached.clone())).await?;
                }
                send_stream_event(&event_tx, StreamEvent::SegmentDone(idx)).await?;
            } else {
                let (translated, _elapsed) = translate_segment_with_completeness_streaming(
                    SegmentTranslateRequest {
                        index: idx,
                        client: ctx.client,
                        segment: &plan.segments[idx],
                        target_lang,
                        template,
                        opts,
                        config: ctx.config,
                    },
                    &event_tx,
                    None,
                    output_mode,
                )
                .await?;
                let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
                if let Err(e) = ctx.history.store_segment_cache(
                    &seg_hashes[idx],
                    target_lang,
                    template_name,
                    &translated,
                    &now,
                    &options_hash,
                ) {
                    eprintln!("Warning: cache store error: {e}");
                }
                translations[idx] = Some(translated);
            }

            if let Some(newline) = reconstruction_newline_after_segment(&plan, &translations, idx) {
                send_stream_event(&event_tx, StreamEvent::Token(newline)).await?;
            }
        }

        let suffix = untranslated_text_after_segments(&plan, next_section_index);
        if !suffix.is_empty() {
            send_stream_event(&event_tx, StreamEvent::Token(suffix)).await?;
        }
    } else if !missing.is_empty() {
        let (priority_chunk, remaining) = partition_pipeline(&missing, true);
        missing = remaining;

        if let Some(chunk_idx) = priority_chunk {
            let client = ctx.client.clone();
            let segment = plan.segments[chunk_idx].clone();
            let target = target_lang.to_owned();
            let tmpl = template.clone();
            let cloned_opts = opts.clone();
            let cfg = ctx.config.clone();
            let event_tx_clone = event_tx.clone();
            let (first_token_tx, mut first_token_rx) = mpsc::channel(1);

            let leading_prefix = plan.reconstruct_prefix_before_segment(chunk_idx);
            if !leading_prefix.is_empty() {
                send_stream_event(&event_tx, StreamEvent::Token(leading_prefix)).await?;
            }

            let mut priority_task = tokio::spawn(async move {
                let (translated, _elapsed) = translate_segment_with_completeness_streaming(
                    SegmentTranslateRequest {
                        index: chunk_idx,
                        client: &client,
                        segment: &segment,
                        target_lang: &target,
                        template: &tmpl,
                        opts: &cloned_opts,
                        config: &cfg,
                    },
                    &event_tx_clone,
                    Some(first_token_tx),
                    output_mode,
                )
                .await?;
                Ok((chunk_idx, translated))
            });

            let mut priority_done: Option<(usize, String)> = None;
            if !missing.is_empty() {
                tokio::select! {
                    _ = first_token_rx.recv() => {}
                    res = &mut priority_task => {
                        priority_done = Some(joined_segment(res)?);
                    }
                }

                let mut join_set: JoinSet<Result<(usize, String)>> = JoinSet::new();
                for &idx in &missing {
                    let client = ctx.client.clone();
                    let segment = plan.segments[idx].clone();
                    let target = target_lang.to_owned();
                    let tmpl = template.clone();
                    let cloned_opts = opts.clone();
                    let cfg = ctx.config.clone();

                    join_set.spawn(async move {
                        let (translated, _elapsed) = translate_segment_with_completeness(
                            idx,
                            &client,
                            &segment,
                            &target,
                            &tmpl,
                            &cloned_opts,
                            &cfg,
                        )
                        .await?;
                        Ok((idx, translated))
                    });
                }

                let (idx, translated) = if let Some(done) = priority_done {
                    done
                } else {
                    joined_segment(priority_task.await)?
                };
                let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
                if let Err(e) = ctx.history.store_segment_cache(
                    &seg_hashes[idx],
                    target_lang,
                    template_name,
                    &translated,
                    &now,
                    &options_hash,
                ) {
                    eprintln!("Warning: cache store error: {e}");
                }
                translations[idx] = Some(translated);

                while let Some(res) = join_set.join_next().await {
                    let (idx, translated) = joined_segment(res)?;
                    let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
                    if let Err(e) = ctx.history.store_segment_cache(
                        &seg_hashes[idx],
                        target_lang,
                        template_name,
                        &translated,
                        &now,
                        &options_hash,
                    ) {
                        eprintln!("Warning: cache store error: {e}");
                    }
                    send_stream_event(&event_tx, StreamEvent::SegmentDone(idx)).await?;
                    translations[idx] = Some(translated);
                }
            } else {
                let (idx, translated) = joined_segment(priority_task.await)?;
                let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
                if let Err(e) = ctx.history.store_segment_cache(
                    &seg_hashes[idx],
                    target_lang,
                    template_name,
                    &translated,
                    &now,
                    &options_hash,
                ) {
                    eprintln!("Warning: cache store error: {e}");
                }
                translations[idx] = Some(translated);
            }
        } else {
            let mut join_set: JoinSet<Result<(usize, String)>> = JoinSet::new();

            for &idx in &missing {
                let client = ctx.client.clone();
                let segment = plan.segments[idx].clone();
                let target = target_lang.to_owned();
                let tmpl = template.clone();
                let cloned_opts = opts.clone();
                let cfg = ctx.config.clone();

                join_set.spawn(async move {
                    let (translated, _elapsed) = translate_segment_with_completeness(
                        idx,
                        &client,
                        &segment,
                        &target,
                        &tmpl,
                        &cloned_opts,
                        &cfg,
                    )
                    .await?;
                    Ok((idx, translated))
                });
            }

            while let Some(res) = join_set.join_next().await {
                let (idx, translated) = joined_segment(res)?;
                let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
                if let Err(e) = ctx.history.store_segment_cache(
                    &seg_hashes[idx],
                    target_lang,
                    template_name,
                    &translated,
                    &now,
                    &options_hash,
                ) {
                    eprintln!("Warning: cache store error: {e}");
                }
                send_stream_event(&event_tx, StreamEvent::SegmentDone(idx)).await?;
                translations[idx] = Some(translated);
            }
        }
    }

    let completed: Vec<String> = translations
        .into_iter()
        .enumerate()
        .map(|(i, t)| t.ok_or_else(|| anyhow!("missing translated segment {i}")))
        .collect::<Result<_>>()?;

    let translated = plan.reconstruct(&completed);
    let duration = wall_start.elapsed().as_secs_f64();
    let output_tokens = ctx.segmenter.count_tokens(&translated);
    let tps = if duration > 0.0 {
        output_tokens as f64 / duration
    } else {
        0.0
    };

    let record = TaskRecord {
        id: None,
        started_at,
        finished_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        duration_seconds: duration,
        input_tokens: plan.source_tokens as i64,
        output_tokens: output_tokens as i64,
        segments: plan.segment_count() as i64,
        concurrency: ctx.config.concurrency() as i64,
        source_lang: None,
        target_lang: target_lang.to_owned(),
        template_type: template_name.to_owned(),
        model: {
            let m = ctx.config.model();
            if m.is_empty() {
                None
            } else {
                Some(m)
            }
        },
        tokens_per_second: tps,
        input_chars: text.len() as i64,
        output_chars: translated.len() as i64,
        output_text: Some(translated.clone()),
        input_hash: None,
        config_version: ctx.config.config_version() as i64,
    };

    if let Err(e) = ctx.history.insert_task(&record) {
        eprintln!("Warning: failed to record timing history: {e}");
    } else {
        eprintln!(
            "Completed in {} | avg {:.1} tok/s | timing recorded",
            format_duration(duration),
            tps
        );
    }

    let seg_count: i64 = plan.segment_count() as i64;
    let concurrency = ctx.config.concurrency() as i64;
    let cfg_ver = ctx.config.config_version() as i64;
    if let Ok(Some(estimate)) = ctx.history.estimate(
        seg_count,
        concurrency,
        Some(target_lang),
        Some(template_name),
        Some(cfg_ver),
        None,
    ) {
        let threshold = ctx.config.timing_divergence_threshold();
        let data = hymt_cache::TimingIssueData {
            input_tokens: plan.source_tokens as i64,
            output_tokens: output_tokens as i64,
            segments: seg_count,
            actual_seconds: duration,
            estimated_seconds: estimate.seconds,
            config_version: cfg_ver,
            target_lang: target_lang.to_owned(),
            template_type: template_name.to_owned(),
            concurrency,
            model: record.model.clone(),
        };
        if hymt_cache::is_divergent(&data, threshold) {
            eprintln!(
                "Warning: timing divergence detected (actual {:.1}s vs estimated {:.1}s)",
                duration, estimate.seconds
            );
        }
    }

    send_stream_event(&event_tx, StreamEvent::AllDone(translated.clone())).await?;
    Ok(translated)
}

// ── translate_file ─────────────────────────────────────────────────────────────

/// Read `path`, translate it, and write the result to `output_path` (or stdout if `None`).
pub async fn translate_file(
    path: &Path,
    output_path: Option<&Path>,
    target_lang: &str,
    template: &TemplateType,
    opts: &PromptOpts,
    ctx: &TranslationCtx<'_>,
) -> Result<String> {
    let text = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;

    let translated = translate_text(&text, target_lang, template, opts, ctx).await?;

    if let Some(out) = output_path {
        if let Some(parent) = out.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(out, &translated)
            .await
            .with_context(|| format!("writing {}", out.display()))?;
    } else {
        print!("{}", translated);
        if !translated.ends_with('\n') {
            println!();
        }
    }

    Ok(translated)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };

    use chrono::{SecondsFormat, Utc};
    use hymt_cache::history::HistoryDB;
    use hymt_core::templates::TemplateType;
    use hymt_segment::Segmenter;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Notify;

    fn fallback_segmenter() -> Segmenter {
        Segmenter::fallback()
    }

    enum MockResponse {
        Json(String),
        Sse(Vec<String>),
    }

    struct MockServer {
        endpoint_url: String,
        handle: tokio::task::JoinHandle<()>,
    }

    struct GatedMockServer {
        endpoint_url: String,
        parallel_before_stream_done: Arc<AtomicBool>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    impl Drop for GatedMockServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    async fn start_mock_server(responses: Vec<MockResponse>) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let server_responses = Arc::clone(&responses);
        let handle = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let responses = Arc::clone(&server_responses);
                tokio::spawn(async move {
                    let _ = serve_mock_connection(socket, responses).await;
                });
            }
        });

        MockServer {
            endpoint_url: format!("http://{addr}/v1"),
            handle,
        }
    }

    async fn serve_mock_connection(
        mut socket: TcpStream,
        responses: Arc<Mutex<VecDeque<MockResponse>>>,
    ) -> std::io::Result<()> {
        read_http_headers(&mut socket).await?;
        let response = {
            responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("mock response queue exhausted")
        };
        write_mock_response(socket, response).await
    }

    async fn start_gated_first_token_server(
        segment0_tokens: Vec<String>,
        parallel_responses: Vec<String>,
    ) -> GatedMockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let segment0_tokens = Arc::new(segment0_tokens);
        let parallel_responses = Arc::new(Mutex::new(VecDeque::from(parallel_responses)));
        let first_request_seen = Arc::new(AtomicBool::new(false));
        let stream_completed = Arc::new(AtomicBool::new(false));
        let parallel_before_stream_done = Arc::new(AtomicBool::new(false));
        let release_stream = Arc::new(Notify::new());

        let handle = tokio::spawn({
            let segment0_tokens = Arc::clone(&segment0_tokens);
            let parallel_responses = Arc::clone(&parallel_responses);
            let first_request_seen = Arc::clone(&first_request_seen);
            let stream_completed = Arc::clone(&stream_completed);
            let parallel_before_stream_done = Arc::clone(&parallel_before_stream_done);
            let release_stream = Arc::clone(&release_stream);
            async move {
                while let Ok((socket, _)) = listener.accept().await {
                    let segment0_tokens = Arc::clone(&segment0_tokens);
                    let parallel_responses = Arc::clone(&parallel_responses);
                    let stream_completed = Arc::clone(&stream_completed);
                    let parallel_before_stream_done = Arc::clone(&parallel_before_stream_done);
                    let release_stream = Arc::clone(&release_stream);
                    let is_first = !first_request_seen.swap(true, Ordering::SeqCst);
                    tokio::spawn(async move {
                        if is_first {
                            let _ = serve_gated_stream_connection(
                                socket,
                                segment0_tokens,
                                release_stream,
                                stream_completed,
                            )
                            .await;
                        } else {
                            if !stream_completed.load(Ordering::SeqCst) {
                                parallel_before_stream_done.store(true, Ordering::SeqCst);
                            }
                            release_stream.notify_one();
                            let response = {
                                parallel_responses
                                    .lock()
                                    .unwrap()
                                    .pop_front()
                                    .expect("parallel response queue exhausted")
                            };
                            let _ = serve_json_connection(socket, response).await;
                        }
                    });
                }
            }
        });

        GatedMockServer {
            endpoint_url: format!("http://{addr}/v1"),
            parallel_before_stream_done,
            handle,
        }
    }

    async fn read_http_headers(socket: &mut TcpStream) -> std::io::Result<()> {
        let mut buf = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let n = socket.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        Ok(())
    }

    async fn write_mock_response(
        mut socket: TcpStream,
        response: MockResponse,
    ) -> std::io::Result<()> {
        let (content_type, body) = mock_response_body(response);
        let headers = format!(
            "HTTP/1.1 200 OK\r\n\
             content-type: {content_type}\r\n\
             content-length: {}\r\n\
             connection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(headers.as_bytes()).await?;
        socket.write_all(body.as_bytes()).await?;
        socket.shutdown().await?;
        Ok(())
    }

    async fn serve_json_connection(mut socket: TcpStream, content: String) -> std::io::Result<()> {
        read_http_headers(&mut socket).await?;
        write_mock_response(socket, MockResponse::Json(content)).await
    }

    async fn serve_gated_stream_connection(
        mut socket: TcpStream,
        tokens: Arc<Vec<String>>,
        release_stream: Arc<Notify>,
        stream_completed: Arc<AtomicBool>,
    ) -> std::io::Result<()> {
        read_http_headers(&mut socket).await?;
        let first = tokens.first().cloned().unwrap_or_default();
        let rest: Vec<String> = tokens.iter().skip(1).cloned().collect();
        let first_body = sse_token_body(&first);
        let mut rest_body = String::new();
        for token in rest {
            rest_body.push_str(&sse_token_body(&token));
        }
        rest_body.push_str("data: [DONE]\n\n");
        let content_len = first_body.len() + rest_body.len();
        let headers = format!(
            "HTTP/1.1 200 OK\r\n\
             content-type: text/event-stream\r\n\
             content-length: {content_len}\r\n\
             connection: close\r\n\r\n"
        );

        socket.write_all(headers.as_bytes()).await?;
        socket.write_all(first_body.as_bytes()).await?;
        socket.flush().await?;
        release_stream.notified().await;
        socket.write_all(rest_body.as_bytes()).await?;
        socket.shutdown().await?;
        stream_completed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn sse_token_body(token: &str) -> String {
        format!(r#"data: {{"choices":[{{"finish_reason":null,"delta":{{"content":"{token}"}}}}]}}"#)
            + "\n\n"
    }

    fn mock_response_body(response: MockResponse) -> (&'static str, String) {
        match response {
            MockResponse::Json(content) => (
                "application/json",
                format!(
                    r#"{{"choices":[{{"finish_reason":"stop","message":{{"content":"{content}"}}}}]}}"#
                ),
            ),
            MockResponse::Sse(tokens) => {
                let mut body = String::new();
                for token in tokens {
                    body.push_str(&format!(
                        r#"data: {{"choices":[{{"finish_reason":null,"delta":{{"content":"{token}"}}}}]}}"#
                    ));
                    body.push_str("\n\n");
                }
                body.push_str("data: [DONE]\n\n");
                ("text/event-stream", body)
            }
        }
    }

    fn temp_path(tag: &str) -> PathBuf {
        let unique = format!(
            "{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(format!("hymt-translate-test-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("test-file")
    }

    fn make_stream_config(endpoint_url: &str) -> hymt_core::config::HotConfig {
        make_stream_config_with_concurrency(endpoint_url, 1)
    }

    fn make_stream_config_with_concurrency(
        endpoint_url: &str,
        concurrency: usize,
    ) -> hymt_core::config::HotConfig {
        make_stream_config_with_concurrency_and_fcp(endpoint_url, concurrency, true)
    }

    fn make_stream_config_with_fcp(
        endpoint_url: &str,
        first_chunk_priority: bool,
    ) -> hymt_core::config::HotConfig {
        make_stream_config_with_concurrency_and_fcp(endpoint_url, 1, first_chunk_priority)
    }

    fn make_stream_config_with_concurrency_and_fcp(
        endpoint_url: &str,
        concurrency: usize,
        first_chunk_priority: bool,
    ) -> hymt_core::config::HotConfig {
        let path = temp_path("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"[endpoint]
url = "{endpoint_url}"

[translation]
context_window = 512
max_output_tokens = 40
concurrency = {concurrency}
first_chunk_priority = {first_chunk_priority}
timeout = 5

[completeness]
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 1
"#
            ),
        )
        .unwrap();
        hymt_core::config::HotConfig::from_path(&path).unwrap()
    }

    fn streaming_regression_source() -> String {
        "Alpha zero text carries enough source material for cache validation and ordering checks. \
         Bravo one text carries enough source material for cache validation and ordering checks."
            .to_owned()
    }

    fn frontmatter_regression_source() -> String {
        "---\ntitle: Streaming Test\n---\n\n".to_owned() + &streaming_regression_source()
    }

    fn complete_translation(label: &str, source_segment: &str) -> String {
        let filler = "x".repeat((source_segment.len() / 2).max(40));
        format!("{label}_{filler} ")
    }

    fn planned_complete_translations(plan: &TranslationPlan) -> Vec<String> {
        plan.segments
            .iter()
            .enumerate()
            .map(|(i, segment)| complete_translation(&format!("SEGMENT_{i}"), segment))
            .collect()
    }

    async fn render_events_as_stdout(
        mut rx: tokio::sync::mpsc::Receiver<StreamEvent>,
    ) -> Result<String> {
        let mut stdout = String::new();
        let mut streamed_prefix = String::new();

        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::Token(token) => {
                    stdout.push_str(&token);
                    streamed_prefix.push_str(&token);
                }
                StreamEvent::SegmentDone(_) => {}
                StreamEvent::AllDone(translated) => {
                    if let Some(rest) = translated.strip_prefix(&streamed_prefix) {
                        stdout.push_str(rest);
                    } else {
                        stdout.push_str(&translated);
                    }
                    if !translated.ends_with('\n') {
                        stdout.push('\n');
                    }
                }
            }
        }

        Ok(stdout)
    }

    async fn translate_and_render_stdout(
        text: &str,
        cfg: &hymt_core::config::HotConfig,
        segmenter: &Segmenter,
        history: &HistoryDB,
    ) -> Result<(String, String)> {
        let client = TranslationClient::new(cfg.clone())?;
        let ctx = TranslationCtx {
            config: cfg,
            client: &client,
            segmenter,
            history,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let opts = PromptOpts::default();
        let translate = translate_text_stream(text, "zh", &TemplateType::Default, &opts, &ctx, tx);
        let render = render_events_as_stdout(rx);
        tokio::try_join!(translate, render)
    }

    // ── Hashing ───────────────────────────────────────────────────────────────

    #[test]
    fn segment_hash_is_hex_sha256() {
        let h = segment_cache_hash("hello world");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn segment_hash_is_deterministic() {
        assert_eq!(segment_cache_hash("test"), segment_cache_hash("test"));
    }

    #[test]
    fn segment_hash_differs_for_different_text() {
        assert_ne!(segment_cache_hash("a"), segment_cache_hash("b"));
    }

    #[test]
    fn template_options_hash_empty_returns_empty() {
        let opts = PromptOpts::default();
        assert_eq!(template_options_hash(&opts), "");
    }

    #[test]
    fn template_options_hash_is_deterministic() {
        let opts = PromptOpts {
            style: Some("formal".to_owned()),
            ..Default::default()
        };
        assert_eq!(template_options_hash(&opts), template_options_hash(&opts));
    }

    #[test]
    fn template_options_hash_differs_for_different_opts() {
        let a = PromptOpts {
            style: Some("formal".to_owned()),
            ..Default::default()
        };
        let b = PromptOpts {
            style: Some("informal".to_owned()),
            ..Default::default()
        };
        assert_ne!(template_options_hash(&a), template_options_hash(&b));
    }

    // ── plan_translation token budget ─────────────────────────────────────────

    #[test]
    fn plan_empty_text_has_no_segments() {
        let seg = fallback_segmenter();
        // Build a config with known values using a temp file
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "[translation]\ncontext_window = 4096\nmax_output_tokens = 512\n",
        )
        .unwrap();
        let cfg = hymt_core::config::HotConfig::from_path(&cfg_path).unwrap();
        let plan = plan_translation(
            "",
            "en",
            &cfg,
            &seg,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert_eq!(plan.segment_count(), 0);
        assert_eq!(plan.source_tokens, 0);
        assert!(plan.document_plan.is_none());
    }

    #[test]
    fn plan_single_short_text_has_one_segment() {
        let seg = fallback_segmenter();
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(
            &cfg_path,
            "[translation]\ncontext_window = 4096\nmax_output_tokens = 512\n",
        )
        .unwrap();
        let cfg = hymt_core::config::HotConfig::from_path(&cfg_path).unwrap();
        let plan = plan_translation(
            "Hello world.",
            "zh",
            &cfg,
            &seg,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert_eq!(plan.segment_count(), 1);
        assert_eq!(plan.segments[0], "Hello world.");
    }

    #[test]
    fn plan_available_tokens_bounded_by_expansion_ratio() {
        let seg = fallback_segmenter();
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        // Small max_output_tokens to force expansion ratio constraint
        std::fs::write(
            &cfg_path,
            "[translation]\ncontext_window = 16384\nmax_output_tokens = 100\n",
        )
        .unwrap();
        let cfg = hymt_core::config::HotConfig::from_path(&cfg_path).unwrap();
        let plan = plan_translation(
            "test",
            "en",
            &cfg,
            &seg,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        // en expansion ratio = 1.8, safety = 1.5 → max_safe = 100 / (1.8 * 1.5) ≈ 37
        assert!(plan.available_source_tokens <= 37);
    }

    // ── reconstruct ───────────────────────────────────────────────────────────

    #[test]
    fn reconstruct_no_doc_plan_joins_all() {
        let plan = TranslationPlan {
            source_tokens: 0,
            segments: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
            available_source_tokens: 100,
            document_plan: None,
            segment_section_indexes: vec![],
            segment_section_groups: vec![],
        };
        let result = plan.reconstruct(&["A".to_owned(), "B".to_owned(), "C".to_owned()]);
        assert_eq!(result, "ABC");
    }

    #[test]
    fn translation_section_groups_all_translatable() {
        use hymt_core::language::build_document_translation_plan;
        let plan = build_document_translation_plan("Paragraph one.\n\nParagraph two.\n", "zh");
        let groups = translation_section_groups(&plan);
        // Two translatable paragraphs separated by a separator that bridges them
        assert!(!groups.is_empty());
    }

    // ── expansion ratios ──────────────────────────────────────────────────────

    #[test]
    fn known_expansion_ratios() {
        assert!((expansion_ratio("en") - 1.8).abs() < f64::EPSILON);
        assert!((expansion_ratio("zh") - 0.7).abs() < f64::EPSILON);
        assert!((expansion_ratio("ja") - 1.0).abs() < f64::EPSILON);
        assert!((expansion_ratio("unknown") - 1.2).abs() < f64::EPSILON);
    }

    // ── first_chunk_priority config ───────────────────────────────────────────

    fn make_config_with_fcp(fcp: bool) -> hymt_core::config::HotConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "[translation]\ncontext_window = 4096\nmax_output_tokens = 512\nfirst_chunk_priority = {fcp}\n"
            ),
        )
        .unwrap();
        // Keep tempdir alive by leaking it for the test scope (acceptable in tests)
        std::mem::forget(dir);
        hymt_core::config::HotConfig::from_path(&path).unwrap()
    }

    #[test]
    fn first_chunk_priority_false_by_default_in_plan() {
        // Config default must be false so existing behaviour is unchanged
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[translation]\ncontext_window = 4096\nmax_output_tokens = 512\n",
        )
        .unwrap();
        let cfg = hymt_core::config::HotConfig::from_path(&path).unwrap();
        assert!(!cfg.first_chunk_priority());
    }

    #[test]
    fn first_chunk_priority_true_reads_from_config() {
        let cfg = make_config_with_fcp(true);
        assert!(cfg.first_chunk_priority());
    }

    #[test]
    fn first_chunk_priority_false_reads_from_config() {
        let cfg = make_config_with_fcp(false);
        assert!(!cfg.first_chunk_priority());
    }

    // Tests that the `missing` vec manipulation logic is correct when chunk 0
    // is present.  These are pure unit tests of the filtering invariant
    // (no I/O or async needed).
    #[test]
    fn pipeline_mode_removes_chunk0_from_missing() {
        let mut missing: Vec<usize> = vec![0, 1, 2];
        // Simulate the retain call used in the pipeline path
        missing.retain(|&i| i != 0);
        assert_eq!(missing, vec![1, 2]);
    }

    #[test]
    fn pipeline_mode_chunk0_not_in_missing_leaves_vec_unchanged() {
        let mut missing: Vec<usize> = vec![1, 2, 3];
        // first_chunk_priority guard: only enters pipeline path when missing[0] == 0
        if missing.first() == Some(&0) {
            missing.retain(|&i| i != 0);
        }
        assert_eq!(missing, vec![1, 2, 3]);
    }

    #[test]
    fn pipeline_mode_only_chunk0_leaves_empty_after_retain() {
        let mut missing: Vec<usize> = vec![0];
        missing.retain(|&i| i != 0);
        assert!(missing.is_empty());
    }

    // ── partition_pipeline ────────────────────────────────────────────────────

    #[test]
    fn partition_pipeline_fcp_true_chunk0_first_splits_correctly() {
        let (priority, parallel) = partition_pipeline(&[0, 1, 2], true);
        assert_eq!(priority, Some(0));
        assert_eq!(parallel, vec![1, 2]);
    }

    #[test]
    fn partition_pipeline_fcp_false_never_prioritizes_even_with_chunk0() {
        let (priority, parallel) = partition_pipeline(&[0, 1, 2], false);
        assert_eq!(priority, None);
        assert_eq!(parallel, vec![0, 1, 2]);
    }

    #[test]
    fn partition_pipeline_fcp_true_chunk0_not_in_missing_returns_none() {
        // Cache hit for chunk 0 means it is absent from `missing`
        let (priority, parallel) = partition_pipeline(&[1, 2, 3], true);
        assert_eq!(priority, None);
        assert_eq!(parallel, vec![1, 2, 3]);
    }

    #[test]
    fn partition_pipeline_single_chunk0_leaves_parallel_empty() {
        let (priority, parallel) = partition_pipeline(&[0], true);
        assert_eq!(priority, Some(0));
        assert!(parallel.is_empty());
    }

    #[test]
    fn partition_pipeline_preserves_order_of_remaining_chunks() {
        let (priority, parallel) = partition_pipeline(&[0, 3, 1, 2], true);
        assert_eq!(priority, Some(0));
        assert_eq!(parallel, vec![3, 1, 2]);
    }

    #[test]
    fn partition_pipeline_empty_missing_returns_none_and_empty() {
        let (priority, parallel) = partition_pipeline(&[], true);
        assert_eq!(priority, None);
        assert!(parallel.is_empty());
    }

    #[test]
    fn partition_pipeline_fcp_false_empty_missing_returns_none_and_empty() {
        let (priority, parallel) = partition_pipeline(&[], false);
        assert_eq!(priority, None);
        assert!(parallel.is_empty());
    }

    #[tokio::test]
    async fn streaming_stdout_is_complete_for_plain_uncached_segment0() {
        let text = streaming_regression_source();
        let planning_cfg = make_stream_config("http://127.0.0.1:1/v1");
        let segmenter = fallback_segmenter();
        let plan = plan_translation(
            &text,
            "zh",
            &planning_cfg,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert!(
            plan.segment_count() >= 2,
            "test source must split into multiple segments"
        );

        let translations = planned_complete_translations(&plan);
        let expected = plan.reconstruct(&translations);
        let mut responses = vec![MockResponse::Sse(vec![translations[0].clone()])];
        responses.extend(translations.iter().skip(1).cloned().map(MockResponse::Json));
        let server = start_mock_server(responses).await;
        let cfg = make_stream_config(&server.endpoint_url);
        let history = HistoryDB::new(temp_path("plain-stream-history.db"));

        let (translated, stdout) = translate_and_render_stdout(&text, &cfg, &segmenter, &history)
            .await
            .unwrap();

        assert_eq!(translated, expected);
        assert_eq!(stdout, format!("{expected}\n"));
    }

    #[tokio::test]
    async fn streaming_emits_tokens_when_fcp_false_and_segment0_cached() {
        let text = streaming_regression_source();
        let planning_cfg = make_stream_config_with_fcp("http://127.0.0.1:1/v1", false);
        let segmenter = fallback_segmenter();
        let plan = plan_translation(
            &text,
            "zh",
            &planning_cfg,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert!(
            plan.segment_count() >= 2,
            "test source must split into multiple segments"
        );

        let translations = planned_complete_translations(&plan);
        let expected = plan.reconstruct(&translations);
        let responses = translations
            .iter()
            .skip(1)
            .cloned()
            .map(|translation| MockResponse::Sse(vec![translation]))
            .collect();
        let server = start_mock_server(responses).await;
        let cfg = make_stream_config_with_fcp(&server.endpoint_url, false);
        let history = HistoryDB::new(temp_path("fcp-false-cached-history.db"));
        history
            .store_segment_cache(
                &segment_cache_hash(&plan.segments[0]),
                "zh",
                TemplateType::Default.as_str(),
                &translations[0],
                &Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                "",
            )
            .unwrap();

        let client = TranslationClient::new(cfg.clone()).unwrap();
        let ctx = TranslationCtx {
            config: &cfg,
            client: &client,
            segmenter: &segmenter,
            history: &history,
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let translated = translate_text_stream(
            &text,
            "zh",
            &TemplateType::Default,
            &PromptOpts::default(),
            &ctx,
            tx,
        )
        .await
        .unwrap();

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }

        let streamed: String = events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::Token(token) => Some(token.as_str()),
                StreamEvent::SegmentDone(_) | StreamEvent::AllDone(_) => None,
            })
            .collect();
        let all_done_index = events
            .iter()
            .position(|event| matches!(event, StreamEvent::AllDone(_)))
            .expect("stream must finish with AllDone");
        let first_token_index = events
            .iter()
            .position(|event| matches!(event, StreamEvent::Token(_)))
            .expect("FCP=false streaming must emit Token events");

        assert_eq!(translated, expected);
        assert_eq!(streamed, expected);
        assert!(first_token_index < all_done_index);
        assert_eq!(events.last(), Some(&StreamEvent::AllDone(expected)));
    }

    #[tokio::test]
    async fn streaming_stdout_preserves_leading_frontmatter_prefix() {
        let text = frontmatter_regression_source();
        let planning_cfg = make_stream_config("http://127.0.0.1:1/v1");
        let segmenter = fallback_segmenter();
        let plan = plan_translation(
            &text,
            "zh",
            &planning_cfg,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert!(
            plan.segment_count() >= 2,
            "test source must split into multiple segments"
        );

        let translations = planned_complete_translations(&plan);
        let expected = plan.reconstruct(&translations);
        let frontmatter = "---\ntitle: Streaming Test\n---\n\n";
        assert!(expected.starts_with(frontmatter));
        let mut responses = vec![MockResponse::Sse(vec![translations[0].clone()])];
        responses.extend(translations.iter().skip(1).cloned().map(MockResponse::Json));
        let server = start_mock_server(responses).await;
        let cfg = make_stream_config(&server.endpoint_url);
        let history = HistoryDB::new(temp_path("frontmatter-stream-history.db"));

        let (translated, stdout) = translate_and_render_stdout(&text, &cfg, &segmenter, &history)
            .await
            .unwrap();

        assert_eq!(translated, expected);
        assert_eq!(stdout, format!("{expected}\n"));
        assert_eq!(stdout.matches(frontmatter).count(), 1);
    }

    #[tokio::test]
    async fn fcp_parallel_chunks_start_after_first_segment0_token() {
        let text = streaming_regression_source();
        let planning_cfg = make_stream_config_with_concurrency("http://127.0.0.1:1/v1", 4);
        let segmenter = fallback_segmenter();
        let plan = plan_translation(
            &text,
            "zh",
            &planning_cfg,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert!(
            plan.segment_count() >= 2,
            "test source must split into multiple segments"
        );

        let translations = planned_complete_translations(&plan);
        let expected = plan.reconstruct(&translations);
        let first = "SEGMENT_0_".to_owned();
        let rest = translations[0]
            .strip_prefix(&first)
            .expect("test translation must start with first token")
            .to_owned();
        let server =
            start_gated_first_token_server(vec![first, rest], translations[1..].to_vec()).await;
        let cfg = make_stream_config_with_concurrency(&server.endpoint_url, 4);
        let history = HistoryDB::new(temp_path("first-token-gate-history.db"));

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            translate_and_render_stdout(&text, &cfg, &segmenter, &history),
        )
        .await
        .expect("parallel chunks did not start after segment 0's first token")
        .unwrap();

        assert_eq!(result.0, expected);
        assert_eq!(result.1, format!("{expected}\n"));
        assert!(
            server.parallel_before_stream_done.load(Ordering::SeqCst),
            "parallel segment request arrived only after segment 0 stream completed"
        );
    }

    #[tokio::test]
    async fn streaming_stdout_is_complete_when_segment0_is_cached() {
        let text = streaming_regression_source();
        let planning_cfg = make_stream_config("http://127.0.0.1:1/v1");
        let segmenter = fallback_segmenter();
        let plan = plan_translation(
            &text,
            "zh",
            &planning_cfg,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert!(
            plan.segment_count() >= 2,
            "test source must split into multiple segments"
        );

        let translations = planned_complete_translations(&plan);
        let expected = plan.reconstruct(&translations);
        let responses = translations
            .iter()
            .skip(1)
            .cloned()
            .map(MockResponse::Json)
            .collect();
        let server = start_mock_server(responses).await;
        let cfg = make_stream_config(&server.endpoint_url);
        let history = HistoryDB::new(temp_path("cached-history.db"));
        history
            .store_segment_cache(
                &segment_cache_hash(&plan.segments[0]),
                "zh",
                TemplateType::Default.as_str(),
                &translations[0],
                &Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                "",
            )
            .unwrap();

        let (translated, stdout) = translate_and_render_stdout(&text, &cfg, &segmenter, &history)
            .await
            .unwrap();

        assert_eq!(translated, expected);
        assert_eq!(stdout, format!("{expected}\n"));
        assert!(stdout.starts_with(&translations[0]));
    }

    #[tokio::test]
    async fn streaming_stdout_uses_retry_text_when_segment0_completeness_fails() {
        let text = streaming_regression_source();
        let planning_cfg = make_stream_config("http://127.0.0.1:1/v1");
        let segmenter = fallback_segmenter();
        let plan = plan_translation(
            &text,
            "zh",
            &planning_cfg,
            &segmenter,
            &TemplateType::Default,
            &PromptOpts::default(),
        )
        .unwrap();
        assert!(
            plan.segment_count() >= 2,
            "test source must split into multiple segments"
        );
        assert_eq!(
            plan.segment_count(),
            2,
            "retry ordering test expects one parallel segment"
        );

        let translations = planned_complete_translations(&plan);
        let expected = plan.reconstruct(&translations);
        let server = start_gated_first_token_server(
            vec!["short".to_owned()],
            vec![translations[1].clone(), translations[0].clone()],
        )
        .await;
        let cfg = make_stream_config_with_concurrency(&server.endpoint_url, 2);
        let history = HistoryDB::new(temp_path("retry-history.db"));

        let (translated, stdout) = translate_and_render_stdout(&text, &cfg, &segmenter, &history)
            .await
            .unwrap();

        assert_eq!(translated, expected);
        assert_eq!(stdout, format!("{expected}\n"));
        assert!(!stdout.contains("short"));
        for segment in translations.iter().skip(1) {
            assert!(
                stdout.contains(segment),
                "stdout dropped translated segment: {segment}"
            );
        }
    }
}
