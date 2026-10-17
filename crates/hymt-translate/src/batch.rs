//! Multi-file batch translation.
//!
//! `build_batch_plan` analyses a directory of text files, checks which segments
//! are already cached, and returns a `BatchPlan` summarising the work ahead.
//! `run_batch_translation` executes the plan, writing each output file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use hymt_cache::history::{EstimateScope, HistoryDB, SegmentCacheScope};
use hymt_client::TranslationClient;
use hymt_core::config::HotConfig;
use hymt_core::language::resolve_target_language;
use hymt_core::language_spec::normalize_language_code;
use hymt_core::templates::{PromptOpts, TemplateType, PROMPT_SCHEMA_ID};
use hymt_segment::Segmenter;

use crate::doc_translate::{build_output_path, target_lang_path_suffix};
use crate::translate::{
    effective_document_translation_policy, plan_translation, segment_cache_hash,
    template_options_hash, translate_text, write_translation_output, TranslationCtx,
};

// Supported source file extensions
const TEXT_SUFFIXES: &[&str] = &["md", "txt"];

// ── Data types ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct BatchSkippedFile {
    pub path: PathBuf,
    pub relative_path: PathBuf,
    pub reason: String,
}

#[derive(Debug)]
pub struct BatchFilePlan {
    pub source_path: PathBuf,
    pub relative_path: PathBuf,
    pub output_path: PathBuf,
    pub target_lang: String,
    pub text: String,
    pub source_tokens: usize,
    pub segment_count: usize,
    pub cached_segments: usize,
    pub estimated_seconds: Option<f64>,
}

impl BatchFilePlan {
    pub fn missing_segments(&self) -> usize {
        self.segment_count.saturating_sub(self.cached_segments)
    }

    pub fn cache_status(&self) -> &'static str {
        if self.segment_count == 0 || self.cached_segments >= self.segment_count {
            "full"
        } else if self.cached_segments == 0 {
            "none"
        } else {
            "partial"
        }
    }
}

#[derive(Debug)]
pub struct BatchPlan {
    pub root: PathBuf,
    pub files: Vec<BatchFilePlan>,
    pub skipped: Vec<BatchSkippedFile>,
    /// Invocation cache policy, shared by preview and execution.
    pub cache_enabled: bool,
}

impl BatchPlan {
    pub fn total_segments(&self) -> usize {
        self.files.iter().map(|f| f.segment_count).sum()
    }

    pub fn total_cached_segments(&self) -> usize {
        self.files.iter().map(|f| f.cached_segments).sum()
    }

    pub fn total_missing_segments(&self) -> usize {
        self.files.iter().map(|f| f.missing_segments()).sum()
    }

    pub fn total_estimated_seconds(&self) -> Option<f64> {
        let mut total = 0.0f64;
        for f in &self.files {
            if f.estimated_seconds.is_none() && f.missing_segments() > 0 {
                return None;
            }
            total += f.estimated_seconds.unwrap_or(0.0);
        }
        Some(total)
    }
}

// ── BatchPlanOpts ─────────────────────────────────────────────────────────────

/// Configuration options for [`build_batch_plan`].
pub struct BatchPlanOpts<'a> {
    pub output_dir: Option<&'a Path>,
    pub target_lang: &'a str,
    pub template: &'a TemplateType,
    pub prompt_opts: &'a PromptOpts,
    pub recursive: bool,
    pub explicit_target: bool,
    /// CLI override shared with the translation client; otherwise use config.
    pub concurrency_override: Option<u32>,
    /// Bypass application cache reads and writes even when config enables them.
    pub no_cache: bool,
}

// ── build_batch_plan ──────────────────────────────────────────────────────────

/// Scan `directory`, plan translations for all supported text files, and check
/// the cache to determine which segments still need translating.
/// A client sharing `config` performs deferred live preflight on the first replay
/// miss, before planning or segment-cache lookup. Replay needs no live backend.
pub async fn build_batch_plan(
    directory: &Path,
    config: &HotConfig,
    client: &TranslationClient,
    segmenter: &Segmenter,
    history: &HistoryDB,
    opts: &BatchPlanOpts<'_>,
) -> Result<BatchPlan> {
    config.maybe_reload()?;
    let mut snapshot = config.detached_snapshot();
    let concurrency = opts
        .concurrency_override
        .unwrap_or_else(|| snapshot.concurrency())
        .max(1);
    let (output_dir, target_lang, template, prompt_opts, recursive, explicit_target) = (
        opts.output_dir,
        opts.target_lang,
        opts.template,
        opts.prompt_opts,
        opts.recursive,
        opts.explicit_target,
    );
    let root = directory
        .canonicalize()
        .with_context(|| format!("resolving {}", directory.display()))?;

    let source_paths = scan_text_files(&root, recursive);
    eprintln!("Scanned {} files in {}", source_paths.len(), root.display());

    let template_name = template.as_str();
    let application_cache_enabled = snapshot.application_cache_enabled() && !opts.no_cache;
    let mut files = Vec::new();
    let mut skipped = Vec::new();

    for path in source_paths {
        let relative = path.strip_prefix(&root).unwrap_or(&path).to_owned();

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                skipped.push(BatchSkippedFile {
                    path: path.clone(),
                    relative_path: relative,
                    reason: format!("read error: {e}"),
                });
                continue;
            }
        };

        if text.is_empty() {
            skipped.push(BatchSkippedFile {
                path: path.clone(),
                relative_path: relative,
                reason: "empty file".to_owned(),
            });
            continue;
        }

        let effective_lang = if explicit_target {
            normalize_language_code(target_lang)?.to_owned()
        } else {
            resolve_target_language(
                &text,
                target_lang,
                &snapshot.primary_lang(),
                &snapshot.secondary_lang(),
                false,
            )
        };

        let suffix = target_lang_path_suffix(&effective_lang)?;

        // Skip files whose stem already ends with the target suffix
        if path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.ends_with(&format!(".{suffix}")))
            .unwrap_or(false)
        {
            skipped.push(BatchSkippedFile {
                path: path.clone(),
                relative_path: relative,
                reason: format!("already translated ({suffix})"),
            });
            continue;
        }

        let output_path = build_output_path(&path, &root, None, output_dir, suffix);

        if application_cache_enabled {
            let replay_ctx = TranslationCtx {
                config: &snapshot,
                client,
                segmenter,
                history,
                cache_enabled: true,
            };
            if let Ok(Some(replay)) = crate::translate::offline_replay::find_replay(
                &text,
                &effective_lang,
                template,
                prompt_opts,
                &replay_ctx,
                false,
            ) {
                files.push(BatchFilePlan {
                    source_path: path,
                    relative_path: relative,
                    output_path,
                    target_lang: effective_lang,
                    text,
                    source_tokens: replay.plan.source_tokens,
                    segment_count: replay.plan.segment_count(),
                    cached_segments: replay.plan.segment_count(),
                    estimated_seconds: Some(0.0),
                });
                continue;
            }
        }

        if (snapshot.strict_backend_preflight() || client.has_deferred_preflight())
            && snapshot.backend_runtime_info().is_none()
        {
            client.preflight_backend().await?.report_warnings();
            snapshot = config.detached_snapshot();
        }
        let config = &snapshot;
        let options_hash = template_options_hash(
            prompt_opts,
            effective_document_translation_policy(prompt_opts, config),
        );
        let profile_id = config.model_profile()?.id();
        let inference_fingerprint = config.inference_fingerprint(template_name, &options_hash)?;
        let cache_enabled = application_cache_enabled && inference_fingerprint.is_cache_verified();
        let cache_scope = SegmentCacheScope {
            target_lang: &effective_lang,
            template_type: template_name,
            options_hash: &options_hash,
            profile_id,
            inference_fingerprint: inference_fingerprint.hash(),
        };
        let plan = plan_translation(
            &text,
            &effective_lang,
            config,
            segmenter,
            template,
            prompt_opts,
        )
        .with_context(|| format!("planning {}", path.display()))?;

        // Count how many segments are already cached
        let seg_hashes: Vec<String> = plan
            .segments
            .iter()
            .map(|s| segment_cache_hash(s))
            .collect();
        let hash_refs: Vec<&str> = seg_hashes.iter().map(|s| s.as_str()).collect();
        let cached_set = if cache_enabled {
            history
                .find_cached_segment_hashes(&hash_refs, cache_scope)
                .unwrap_or_default()
        } else {
            Default::default()
        };
        let cached_segments = seg_hashes
            .iter()
            .filter(|h| cached_set.contains(*h))
            .count();

        let missing = plan.segment_count().saturating_sub(cached_segments);
        let estimated_seconds = if missing == 0 {
            Some(0.0)
        } else {
            history
                .estimate(
                    plan.segments
                        .iter()
                        .zip(&seg_hashes)
                        .filter(|(_, hash)| !cached_set.contains(*hash))
                        .map(|(segment, _)| segmenter.count_tokens(segment) as i64)
                        .sum(),
                    missing as i64,
                    concurrency as i64,
                    EstimateScope {
                        target_lang: &effective_lang,
                        template_type: template_name,
                        profile_id,
                        inference_fingerprint: inference_fingerprint.hash(),
                        prompt_schema: PROMPT_SCHEMA_ID,
                        config_version: config.config_version() as i64,
                        counting_source: if segmenter.has_exact_tokenizer() {
                            "local"
                        } else {
                            "approximate"
                        },
                    },
                )
                .ok()
                .flatten()
                .map(|e| e.seconds)
        };

        files.push(BatchFilePlan {
            source_path: path,
            relative_path: relative,
            output_path,
            target_lang: effective_lang,
            text,
            source_tokens: plan.source_tokens,
            segment_count: plan.segment_count(),
            cached_segments,
            estimated_seconds,
        });
    }

    Ok(BatchPlan {
        root,
        files,
        skipped,
        cache_enabled: application_cache_enabled,
    })
}

fn scan_text_files(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    WalkDir::new(dir)
        .max_depth(if recursive { usize::MAX } else { 1 })
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| TEXT_SUFFIXES.iter().any(|&ext| s.eq_ignore_ascii_case(ext)))
                .unwrap_or(false)
        })
        .map(|e| e.path().to_owned())
        .collect()
}

// ── show_batch_preview ────────────────────────────────────────────────────────

/// Print a human-readable summary of the batch plan to stderr.
pub fn show_batch_preview(plan: &BatchPlan) {
    eprintln!(
        "Batch plan: {} files, {} segments ({} cached, {} missing)",
        plan.files.len(),
        plan.total_segments(),
        plan.total_cached_segments(),
        plan.total_missing_segments(),
    );
    if let Some(secs) = plan.total_estimated_seconds() {
        eprintln!("Estimated time: ~{:.0}s", secs);
    }
    for f in &plan.files {
        eprintln!(
            "  {} → {} [{}] {}/{} cached",
            f.relative_path.display(),
            f.output_path.display(),
            f.cache_status(),
            f.cached_segments,
            f.segment_count,
        );
    }
    if !plan.skipped.is_empty() {
        eprintln!("Skipped {} files:", plan.skipped.len());
        for s in &plan.skipped {
            eprintln!("  {} — {}", s.relative_path.display(), s.reason);
        }
    }
}

// ── run_batch_translation ─────────────────────────────────────────────────────

/// Execute a batch plan, translating each file and writing the output.
pub async fn run_batch_translation(
    plan: &BatchPlan,
    config: &HotConfig,
    client: &TranslationClient,
    segmenter: &Segmenter,
    history: &HistoryDB,
    template: &TemplateType,
    opts: &PromptOpts,
) -> Result<()> {
    for (i, file) in plan.files.iter().enumerate() {
        eprintln!(
            "[{}/{}] {} → {}",
            i + 1,
            plan.files.len(),
            file.relative_path.display(),
            file.output_path.display()
        );

        let tctx = TranslationCtx {
            config,
            client,
            segmenter,
            history,
            cache_enabled: plan.cache_enabled,
        };
        let outcome = translate_text(&file.text, &file.target_lang, template, opts, &tctx)
            .await
            .with_context(|| format!("translating {}", file.source_path.display()))?;
        outcome.report_completeness_degraded();

        write_translation_output(&file.output_path, &outcome.text)
            .await
            .with_context(|| format!("writing {}", file.output_path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use hymt_cache::history::SegmentCacheScope;
    use tempfile::TempDir;

    fn cache_config(model: &str) -> String {
        format!(
            r#"[endpoint]
url = "http://127.0.0.1:8401/v1"
backend = "llama_cpp"
profile = "hy_mt2_7b"
model = "{model}"

[inference.override]
temperature = 0.7
top_p = 0.6
top_k = 20
repetition_penalty = 1.05
min_p = 0.0
repeat_last_n = 64

[cache]
enabled = true

[translation]
context_window = 512
max_output_tokens = 40
concurrency = 1
timeout = 5

[completeness]
zh_to_en_min_ratio = 0.3
en_to_zh_min_ratio = 0.3
min_paragraph_ratio = 0.5
max_retries = 1
"#,
        )
    }

    #[tokio::test]
    async fn batch_preview_ignores_warm_cache_by_default_and_honors_opt_in_and_no_cache() {
        for (setting, no_cache, expected_cached) in [
            (None, false, 0),
            (Some(true), false, 1),
            (Some(true), true, 0),
        ] {
            let tmp = TempDir::new().unwrap();
            let source_dir = tmp.path().join("source");
            std::fs::create_dir(&source_dir).unwrap();
            let source =
                "A source paragraph long enough to produce one cached translation segment.";
            std::fs::write(source_dir.join("input.md"), source).unwrap();
            let contents = cache_config("same-model");
            let contents = if setting.is_none() {
                contents.replace("[cache]\nenabled = true\n", "")
            } else {
                contents
            };
            let config_path = tmp.path().join("config.toml");
            std::fs::write(&config_path, contents).unwrap();
            let config = HotConfig::from_path(&config_path).unwrap();
            let segmenter = Segmenter::fallback();
            let template = TemplateType::Default;
            let prompt_opts = PromptOpts::default();
            let plan = plan_translation(source, "zh", &config, &segmenter, &template, &prompt_opts)
                .unwrap();
            assert_eq!(plan.segment_count(), 1);
            let options_hash = template_options_hash(
                &prompt_opts,
                effective_document_translation_policy(&prompt_opts, &config),
            );
            let fingerprint = config
                .inference_fingerprint(template.as_str(), &options_hash)
                .unwrap();
            assert!(
                fingerprint.is_cache_verified(),
                "fixture must exercise the cache gate, not an unverified-identity miss"
            );
            let scope = SegmentCacheScope {
                target_lang: "zh",
                template_type: template.as_str(),
                options_hash: &options_hash,
                profile_id: config.model_profile().unwrap().id(),
                inference_fingerprint: fingerprint.hash(),
            };
            let history = HistoryDB::new(tmp.path().join("history.db"));
            history
                .store_segment_cache(
                    &segment_cache_hash(&plan.segments[0]),
                    scope,
                    "existing translation",
                    "2024-01-01T00:00:00Z",
                )
                .unwrap();
            let opts = BatchPlanOpts {
                output_dir: None,
                target_lang: "zh",
                template: &template,
                prompt_opts: &prompt_opts,
                recursive: false,
                explicit_target: true,
                concurrency_override: None,
                no_cache,
            };
            let client = TranslationClient::new(config.clone()).unwrap();
            let batch =
                build_batch_plan(&source_dir, &config, &client, &segmenter, &history, &opts)
                    .await
                    .unwrap();
            assert_eq!(batch.total_cached_segments(), expected_cached);
            assert_eq!(batch.cache_enabled, expected_cached == 1);
            assert!(
                history
                    .find_segment_cached(&segment_cache_hash(&plan.segments[0]), scope)
                    .unwrap()
                    .is_some(),
                "disabled use must not delete old cache entries"
            );
        }
    }

    #[tokio::test]
    async fn batch_plan_reloads_config_before_cache_lookup() {
        let tmp = TempDir::new().unwrap();
        let source_dir = tmp.path().join("source");
        std::fs::create_dir(&source_dir).unwrap();
        let source = "A source paragraph long enough to produce one cached translation segment.";
        std::fs::write(source_dir.join("input.md"), source).unwrap();

        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, cache_config("old-model")).unwrap();
        let config = HotConfig::from_path(&config_path).unwrap();
        let segmenter = Segmenter::fallback();
        let template = TemplateType::Default;
        let prompt_opts = PromptOpts::default();
        let plan =
            plan_translation(source, "zh", &config, &segmenter, &template, &prompt_opts).unwrap();
        assert_eq!(plan.segment_count(), 1);
        let options_hash = template_options_hash(
            &prompt_opts,
            effective_document_translation_policy(&prompt_opts, &config),
        );
        let old_fingerprint = config
            .inference_fingerprint(template.as_str(), &options_hash)
            .unwrap();
        let old_scope = SegmentCacheScope {
            target_lang: "zh",
            template_type: template.as_str(),
            options_hash: &options_hash,
            profile_id: config.model_profile().unwrap().id(),
            inference_fingerprint: old_fingerprint.hash(),
        };
        let history = HistoryDB::new(tmp.path().join("history.db"));
        history
            .store_segment_cache(
                &segment_cache_hash(&plan.segments[0]),
                old_scope,
                "stale cached translation",
                "2024-01-01T00:00:00Z",
            )
            .unwrap();

        std::fs::write(&config_path, cache_config("new-model")).unwrap();
        let opts = BatchPlanOpts {
            output_dir: None,
            target_lang: "zh",
            template: &template,
            prompt_opts: &prompt_opts,
            recursive: false,
            explicit_target: true,
            concurrency_override: None,
            no_cache: false,
        };
        let client = TranslationClient::new(config.clone()).unwrap();
        let batch = build_batch_plan(&source_dir, &config, &client, &segmenter, &history, &opts)
            .await
            .unwrap();

        assert_eq!(batch.files.len(), 1);
        assert_eq!(
            batch.files[0].cached_segments, 0,
            "the changed model identity must miss the old cache entry"
        );
    }
}
