//! Markdown document translation.
//!
//! Output naming convention: `source.md` → `source.zh-cn.md`
//! (language suffix inserted between the stem and the extension).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use hymt_cache::history::HistoryDB;
use hymt_client::TranslationClient;
use hymt_core::config::HotConfig;
use hymt_core::language::resolve_target_language;
use hymt_core::templates::{PromptOpts, TemplateType};
use hymt_segment::Segmenter;

use crate::translate::{translate_text, TranslationCtx};

// ── Output path helpers ───────────────────────────────────────────────────────

/// Maps a target language code to its canonical path suffix.
/// `zh` → `zh-cn`; all others pass through unchanged.
pub fn target_lang_path_suffix(target_lang: &str) -> &str {
    match target_lang {
        "zh" => "zh-cn",
        other => other,
    }
}

/// Build the output path for a translated markdown file.
///
/// If `output_path` is specified, it is returned as-is.
/// Otherwise the suffix is inserted before the extension:
/// `notes.md` + `"zh-cn"` → `notes.zh-cn.md`.
pub fn build_output_path(
    source: &Path,
    base_dir: &Path,
    output_path: Option<&Path>,
    output_dir: Option<&Path>,
    target_suffix: &str,
) -> PathBuf {
    if let Some(out) = output_path {
        return out.to_owned();
    }
    let stem = source.file_stem().unwrap_or_default().to_string_lossy();
    let ext = source
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let new_name = format!("{stem}.{target_suffix}{ext}");

    if let Some(dir) = output_dir {
        // Preserve relative path under output_dir
        if let Ok(rel) = source.strip_prefix(base_dir) {
            return dir
                .join(rel.parent().unwrap_or(Path::new("")))
                .join(&new_name);
        }
        return dir.join(&new_name);
    }
    source.parent().unwrap_or(Path::new("")).join(&new_name)
}

/// Validate that a language suffix is safe to embed in a file path.
///
/// Only ASCII alphanumeric characters, hyphens, and underscores are allowed.
/// This blocks path traversal (`..`), path separators (`/`, `\`), null bytes,
/// and any other character that could be misinterpreted by the filesystem.
fn validate_lang_suffix(suffix: &str) -> Result<()> {
    if suffix.is_empty() {
        anyhow::bail!("target language suffix must not be empty");
    }
    if suffix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(())
    } else {
        anyhow::bail!(
            "invalid target language suffix {:?}: only ASCII alphanumeric, hyphens, and underscores are allowed",
            suffix
        )
    }
}

// ── DocTranslationTarget ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct DocTranslationTarget {
    pub source_path: PathBuf,
    pub output_path: PathBuf,
    pub target_lang: String,
}

// ── build_doc_translation_targets ────────────────────────────────────────────

/// Resolve all (source, output, target_lang) triples for the given source path.
///
/// If `source` is a directory, all `.md` files are discovered (recursively if
/// `recursive` is set).  The language suffix suffix files (e.g. `foo.zh-cn.md`)
/// are skipped to avoid translating already-translated output.
pub(crate) fn build_doc_translation_targets(
    source: &Path,
    target_lang: &str,
    config: Option<&HotConfig>,
    output_path: Option<&Path>,
    output_dir: Option<&Path>,
    recursive: bool,
    explicit_target: bool,
) -> Result<Vec<DocTranslationTarget>> {
    let resolved = source
        .canonicalize()
        .with_context(|| format!("resolving {}", source.display()))?;

    if resolved.is_file() {
        validate_markdown(&resolved)?;
        let effective = resolve_file_target_lang(&resolved, target_lang, config, explicit_target);
        let suffix = target_lang_path_suffix(&effective);
        validate_lang_suffix(suffix)?;
        let out = build_output_path(
            &resolved,
            resolved.parent().unwrap_or(&resolved),
            output_path,
            output_dir,
            suffix,
        );
        return Ok(vec![DocTranslationTarget {
            source_path: resolved,
            output_path: out,
            target_lang: effective,
        }]);
    }

    if !resolved.is_dir() {
        anyhow::bail!("unsupported translate-doc source: {}", resolved.display());
    }

    let files = scan_markdown_files(&resolved, recursive);
    let mut targets = Vec::new();
    for path in files {
        let effective = resolve_file_target_lang(&path, target_lang, config, explicit_target);
        let suffix = target_lang_path_suffix(&effective);
        validate_lang_suffix(suffix)?;
        // Skip files whose stem already ends with the target suffix
        if path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.ends_with(&format!(".{suffix}")))
            .unwrap_or(false)
        {
            continue;
        }
        let out = build_output_path(&path, &resolved, None, output_dir, suffix);
        targets.push(DocTranslationTarget {
            source_path: path,
            output_path: out,
            target_lang: effective,
        });
    }
    Ok(targets)
}

fn resolve_file_target_lang(
    path: &Path,
    requested: &str,
    config: Option<&HotConfig>,
    explicit_target: bool,
) -> String {
    if explicit_target {
        return requested.to_owned();
    }
    if let Some(cfg) = config {
        if let Ok(text) = std::fs::read_to_string(path) {
            return resolve_target_language(
                &text,
                requested,
                &cfg.primary_lang(),
                &cfg.secondary_lang(),
                explicit_target,
            );
        }
    }
    requested.to_owned()
}

fn validate_markdown(path: &Path) -> Result<()> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if ext != "md" {
        anyhow::bail!(
            "translate-doc only supports .md files, got: {}",
            path.display()
        );
    }
    Ok(())
}

fn scan_markdown_files(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    let walker = WalkDir::new(dir)
        .max_depth(if recursive { usize::MAX } else { 1 })
        .follow_links(false);
    walker
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.eq_ignore_ascii_case("md"))
                .unwrap_or(false)
        })
        .map(|e| e.path().to_owned())
        .collect()
}

// ── DocTranslationOpts ────────────────────────────────────────────────────────

/// Options for [`run_doc_translation`].
pub struct DocTranslationOpts<'a> {
    pub target_lang: &'a str,
    pub config: &'a HotConfig,
    pub client: &'a TranslationClient,
    pub segmenter: &'a Segmenter,
    pub history: &'a HistoryDB,
    pub output_path: Option<&'a Path>,
    pub output_dir: Option<&'a Path>,
    pub recursive: bool,
    pub template: &'a TemplateType,
    pub prompt_opts: &'a PromptOpts,
    /// Whether the caller explicitly specified the target language.
    pub explicit_target: bool,
}

// ── run_doc_translation ───────────────────────────────────────────────────────

/// Translate `source` (file or directory of `.md` files) to `target_lang`.
///
/// Writes each translated file atomically via a uniquely-named temp file in
/// the same directory, preventing races when multiple translations run in
/// parallel.
pub async fn run_doc_translation(source: &Path, opts: &DocTranslationOpts<'_>) -> Result<()> {
    let targets = build_doc_translation_targets(
        source,
        opts.target_lang,
        Some(opts.config),
        opts.output_path,
        opts.output_dir,
        opts.recursive,
        opts.explicit_target,
    )?;

    if targets.is_empty() {
        eprintln!("No Markdown files selected.");
        return Ok(());
    }

    for (i, target) in targets.iter().enumerate() {
        eprintln!(
            "Document {}/{}: {} -> {}",
            i + 1,
            targets.len(),
            target.source_path.display(),
            target.output_path.display()
        );

        let text = tokio::fs::read_to_string(&target.source_path)
            .await
            .with_context(|| format!("reading {}", target.source_path.display()))?;

        let tctx = TranslationCtx {
            config: opts.config,
            client: opts.client,
            segmenter: opts.segmenter,
            history: opts.history,
        };
        let outcome = translate_text(
            &text,
            &target.target_lang,
            opts.template,
            opts.prompt_opts,
            &tctx,
        )
        .await?;
        outcome.report_completeness_degraded();
        let translated = outcome.text;

        // Atomic write: PID + epoch-ns suffix prevents concurrent temp-file collisions.
        if let Some(parent) = target.output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let uid = format!(
            "tmp.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let tmp = target.output_path.with_extension(uid);
        tokio::fs::write(&tmp, &translated)
            .await
            .with_context(|| format!("writing temp file {}", tmp.display()))?;
        tokio::fs::rename(&tmp, &target.output_path)
            .await
            .with_context(|| format!("renaming to {}", target.output_path.display()))?;
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── target_lang_path_suffix ───────────────────────────────────────────────

    #[test]
    fn zh_maps_to_zh_cn() {
        assert_eq!(target_lang_path_suffix("zh"), "zh-cn");
    }

    #[test]
    fn fr_passes_through() {
        assert_eq!(target_lang_path_suffix("fr"), "fr");
    }

    #[test]
    fn en_passes_through() {
        assert_eq!(target_lang_path_suffix("en"), "en");
    }

    #[test]
    fn zh_cn_passes_through() {
        assert_eq!(target_lang_path_suffix("zh-cn"), "zh-cn");
    }

    // ── build_output_path ─────────────────────────────────────────────────────

    #[test]
    fn output_path_inserts_suffix_before_extension() {
        let source = Path::new("/docs/guide.md");
        let base = Path::new("/docs");
        let out = build_output_path(source, base, None, None, "zh-cn");
        assert_eq!(out, PathBuf::from("/docs/guide.zh-cn.md"));
    }

    #[test]
    fn output_path_respects_explicit_output_path() {
        let source = Path::new("/docs/guide.md");
        let base = Path::new("/docs");
        let explicit = Path::new("/out/translated.md");
        let out = build_output_path(source, base, Some(explicit), None, "zh-cn");
        assert_eq!(out, explicit);
    }

    #[test]
    fn output_path_respects_output_dir() {
        let source = Path::new("/docs/subdir/guide.md");
        let base = Path::new("/docs");
        let out_dir = Path::new("/translated");
        let out = build_output_path(source, base, None, Some(out_dir), "fr");
        assert_eq!(out, PathBuf::from("/translated/subdir/guide.fr.md"));
    }

    #[test]
    fn output_path_handles_no_extension() {
        let source = Path::new("/docs/README");
        let base = Path::new("/docs");
        let out = build_output_path(source, base, None, None, "zh-cn");
        assert_eq!(out, PathBuf::from("/docs/README.zh-cn"));
    }

    #[test]
    fn scan_markdown_skips_non_md_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.md"), "").unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("c.rs"), "").unwrap();
        let files = scan_markdown_files(dir.path(), false);
        assert_eq!(files.len(), 1);
        assert!(files[0].file_name().unwrap() == "a.md");
    }

    // ── validate_lang_suffix ──────────────────────────────────────────────────

    #[test]
    fn lang_suffix_accepts_valid_codes() {
        assert!(validate_lang_suffix("zh-cn").is_ok());
        assert!(validate_lang_suffix("en").is_ok());
        assert!(validate_lang_suffix("pt_BR").is_ok());
        assert!(validate_lang_suffix("fr").is_ok());
    }

    #[test]
    fn lang_suffix_rejects_empty() {
        assert!(validate_lang_suffix("").is_err());
    }

    #[test]
    fn lang_suffix_rejects_path_traversal() {
        assert!(validate_lang_suffix("../etc").is_err());
        assert!(validate_lang_suffix("../../evil").is_err());
        assert!(validate_lang_suffix("..").is_err());
    }

    #[test]
    fn lang_suffix_rejects_forward_slash() {
        assert!(validate_lang_suffix("zh/cn").is_err());
        assert!(validate_lang_suffix("/etc/passwd").is_err());
    }

    #[test]
    fn lang_suffix_rejects_backslash() {
        assert!(validate_lang_suffix("zh\\cn").is_err());
    }

    #[test]
    fn lang_suffix_rejects_null_byte() {
        assert!(validate_lang_suffix("zh\x00cn").is_err());
    }

    #[test]
    fn lang_suffix_rejects_spaces_and_special() {
        assert!(validate_lang_suffix("zh cn").is_err());
        assert!(validate_lang_suffix("zh!cn").is_err());
        assert!(validate_lang_suffix("zh.cn").is_err());
    }

    #[test]
    fn scan_markdown_recursive() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.path().join("a.md"), "").unwrap();
        std::fs::write(sub.join("b.md"), "").unwrap();
        let non_recursive = scan_markdown_files(dir.path(), false);
        let recursive = scan_markdown_files(dir.path(), true);
        assert_eq!(non_recursive.len(), 1);
        assert_eq!(recursive.len(), 2);
    }
}
