//! Multi-file batch translation.
//!
//! `build_batch_plan` analyses a directory of text files, checks which segments
//! are already cached, and returns a `BatchPlan` summarising the work ahead.
//! `run_batch_translation` executes the plan, writing each output file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use hymt_cache::history::{HistoryDB, SegmentCacheScope};
use hymt_client::TranslationClient;
use hymt_core::config::HotConfig;
use hymt_core::language::resolve_target_language;
use hymt_core::language_spec::normalize_language_code;
use hymt_core::templates::{PromptOpts, TemplateType};
use hymt_segment::Segmenter;

use crate::doc_translate::{build_output_path, target_lang_path_suffix};
use crate::translate::{
    effective_document_translation_policy, plan_translation, segment_cache_hash,
    template_options_hash, translate_text, TranslationCtx,
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
}

// ── build_batch_plan ──────────────────────────────────────────────────────────

/// Scan `directory`, plan translations for all supported text files, and check
/// the cache to determine which segments still need translating.
pub fn build_batch_plan(
    directory: &Path,
    config: &HotConfig,
    segmenter: &Segmenter,
    history: &HistoryDB,
    opts: &BatchPlanOpts<'_>,
) -> Result<BatchPlan> {
    config.maybe_reload()?;
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
    let options_hash = template_options_hash(
        prompt_opts,
        effective_document_translation_policy(prompt_opts, config),
    );
    let profile_id = config.model_profile()?.id();
    let inference_fingerprint = config.inference_fingerprint(template_name, &options_hash)?;
    // Incomplete Generic/server-default identities can change across a server
    // restart, so they are never eligible for segment-cache reuse.
    let cache_enabled = inference_fingerprint.is_cache_verified();
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
                &config.primary_lang(),
                &config.secondary_lang(),
                false,
            )
        };

        let cache_scope = SegmentCacheScope {
            target_lang: &effective_lang,
            template_type: template_name,
            options_hash: &options_hash,
            profile_id,
            inference_fingerprint: inference_fingerprint.hash(),
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

        let plan_result = plan_translation(
            &text,
            &effective_lang,
            config,
            segmenter,
            template,
            prompt_opts,
        );
        let plan = match plan_result {
            Ok(p) => p,
            Err(e) => {
                skipped.push(BatchSkippedFile {
                    path: path.clone(),
                    relative_path: relative,
                    reason: format!("plan error: {e}"),
                });
                continue;
            }
        };

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
                    missing as i64,
                    config.concurrency() as i64,
                    Some(&effective_lang),
                    Some(template_name),
                    Some(config.config_version() as i64),
                    None,
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
        };
        let outcome = translate_text(&file.text, &file.target_lang, template, opts, &tctx)
            .await
            .with_context(|| format!("translating {}", file.source_path.display()))?;
        outcome.report_completeness_degraded();

        if let Some(parent) = file.output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&file.output_path, &outcome.text)
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
profile = "hy_mt2_7b"
model = "{model}"

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

    #[test]
    fn batch_plan_reloads_config_before_cache_lookup() {
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
        };
        let batch = build_batch_plan(&source_dir, &config, &segmenter, &history, &opts).unwrap();

        assert_eq!(batch.files.len(), 1);
        assert_eq!(
            batch.files[0].cached_segments, 0,
            "the changed model identity must miss the old cache entry"
        );
    }
}
