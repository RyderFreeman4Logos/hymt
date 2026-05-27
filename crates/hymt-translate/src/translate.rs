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
use tokio::task::JoinSet;

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
    use hymt_core::templates::TemplateType;
    use hymt_segment::Segmenter;

    fn fallback_segmenter() -> Segmenter {
        Segmenter::fallback()
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
}
