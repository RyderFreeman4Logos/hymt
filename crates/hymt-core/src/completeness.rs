//! Layered truncation and structural validation for translated segments.
//!
//! This module detects cheap, observable signs of truncated or structurally lost
//! output. It deliberately does **not** estimate translation quality or semantic
//! equivalence. A passing result is only suitable for retry/cache decisions.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::language_spec::{language_spec_or_none, LanguageFamily};

const MAX_CALIBRATED_DENSITY_RATIO: f64 = 8.0;

/// Threshold configuration for completeness checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletenessThresholds {
    /// Minimum Unicode-scalar density ratio for zh→en translations (source is zh).
    pub zh_to_en_min_ratio: f64,
    /// Minimum Unicode-scalar density ratio for en→zh translations (source is en).
    pub en_to_zh_min_ratio: f64,
    /// Minimum paragraph-count ratio (output / input).
    pub min_paragraph_ratio: f64,
}

impl Default for CompletenessThresholds {
    fn default() -> Self {
        Self {
            zh_to_en_min_ratio: 0.3,
            en_to_zh_min_ratio: 0.3,
            min_paragraph_ratio: 0.5,
        }
    }
}

/// Machine-readable state for a validation attempt.
///
/// `DegradedBestEffort` is assigned by retry orchestration after every retry has
/// failed; the fast validator itself never claims semantic translation quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletenessStatus {
    Valid,
    ValidWithAdvisories,
    Unverified,
    RetryableIncomplete,
    StructurallyInvalid,
    DegradedBestEffort,
}

impl CompletenessStatus {
    /// Whether this attempt can be used without a completeness retry.
    pub fn accepts_output(self) -> bool {
        matches!(
            self,
            Self::Valid | Self::ValidWithAdvisories | Self::Unverified
        )
    }
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
}

/// Optional transport context for layered validation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletenessContext {
    pub termination: CompletionTermination,
}

/// Indicates whether density bounds are calibrated for the selected target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DensityStatus {
    Calibrated,
    Unverified,
}

/// Unicode-scalar density measurement and the bounds that applied to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DensityCheck {
    pub status: DensityStatus,
    pub ratio: Option<f64>,
    pub minimum_ratio: Option<f64>,
    pub maximum_ratio: Option<f64>,
}

/// Raw structural and Unicode-scalar counts extracted from a text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletenessStats {
    /// Count of Unicode scalar values. This is intentionally not UTF-8 byte length.
    pub unicode_scalar_count: usize,
    pub paragraph_count: usize,
    pub heading_count: usize,
    pub fenced_code_block_count: usize,
    pub url_count: usize,
    pub placeholder_count: usize,
}

/// Outcome of a truncation/structure validation attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletenessResult {
    /// Compatibility boolean for callers that only need the retry decision.
    pub is_complete: bool,
    pub status: CompletenessStatus,
    /// Higher is better only for selecting a best failed attempt; it is not a QE score.
    pub score: i32,
    pub density: DensityCheck,
    /// Stable failure codes such as `token_ratio`, `empty_output`, and `json_validity`.
    pub checks_failed: Vec<String>,
    /// Advisory codes that do not trigger a retry, including `unverified_density`.
    pub advisory_warnings: Vec<String>,
    pub input_stats: CompletenessStats,
    pub output_stats: CompletenessStats,
}

impl CompletenessResult {
    /// Marks a selected failed attempt after retry exhaustion.
    pub fn into_degraded_best_effort(mut self) -> Self {
        self.is_complete = false;
        self.status = CompletenessStatus::DegradedBestEffort;
        self
    }
}

/// Validates whether `output_text` has observable signs of being a complete enough
/// translation of `input_text`, without making a semantic-quality claim.
pub fn validate_completeness(
    input_text: &str,
    output_text: &str,
    target_lang: &str,
    thresholds: Option<&CompletenessThresholds>,
) -> CompletenessResult {
    validate_completeness_with_context(
        input_text,
        output_text,
        target_lang,
        thresholds,
        &CompletenessContext::default(),
    )
}

/// Like [`validate_completeness`], with optional transport termination evidence.
pub fn validate_completeness_with_context(
    input_text: &str,
    output_text: &str,
    target_lang: &str,
    thresholds: Option<&CompletenessThresholds>,
    context: &CompletenessContext,
) -> CompletenessResult {
    let thresholds = thresholds.cloned().unwrap_or_default();
    let input_stats = compute_stats(input_text);
    let output_stats = compute_stats(output_text);
    let density = density_check(&input_stats, &output_stats, target_lang, &thresholds);
    let mut checks_failed = Vec::new();
    let mut advisory_warnings = Vec::new();

    // Layer 1: transport/termination and empty response.
    if output_text.trim().is_empty() {
        checks_failed.push("empty_output".to_owned());
    }
    match context.termination {
        CompletionTermination::Length => checks_failed.push("finish_reason_length".to_owned()),
        CompletionTermination::Timeout => checks_failed.push("timeout".to_owned()),
        CompletionTermination::Unknown | CompletionTermination::Stop => {}
    }

    // Layer 2: calibrated Unicode-scalar density. Other targets are explicit,
    // actionable advisories rather than implicit passes.
    let mut density_passed = None;
    match density.status {
        DensityStatus::Calibrated => {
            if let Some(ratio) = density.ratio {
                if ratio < density.minimum_ratio.unwrap_or_default() {
                    checks_failed.push("token_ratio".to_owned());
                    density_passed = Some(false);
                } else if ratio > density.maximum_ratio.unwrap_or(f64::INFINITY) {
                    checks_failed.push("density_upper_bound".to_owned());
                    density_passed = Some(false);
                } else {
                    density_passed = Some(true);
                }
            }
        }
        DensityStatus::Unverified => advisory_warnings.push("unverified_density".to_owned()),
    }

    // Layer 3: cheap structural retention checks.
    if input_stats.paragraph_count > 0 {
        let ratio = output_stats.paragraph_count as f64 / input_stats.paragraph_count as f64;
        if ratio < thresholds.min_paragraph_ratio {
            if density_passed == Some(true) {
                advisory_warnings.push("paragraph_count".to_owned());
            } else {
                checks_failed.push("paragraph_count".to_owned());
            }
        }
    }

    if input_stats.heading_count > 0 && output_stats.heading_count < input_stats.heading_count {
        checks_failed.push("heading_preservation".to_owned());
    }
    if input_stats.fenced_code_block_count > output_stats.fenced_code_block_count {
        checks_failed.push("fenced_code_preservation".to_owned());
    }
    if !preserves_all(
        &placeholder_tokens(input_text),
        &placeholder_tokens(output_text),
    ) {
        checks_failed.push("placeholder_preservation".to_owned());
    }
    if !preserves_all(&urls(input_text), &urls(output_text)) {
        checks_failed.push("url_preservation".to_owned());
    }
    if let Some(input_json) = parse_json_document(input_text) {
        match serde_json::from_str::<serde_json::Value>(output_text.trim()) {
            Ok(output_json) if preserves_json_object_keys(&input_json, &output_json) => {}
            Ok(_) => checks_failed.push("json_key_preservation".to_owned()),
            Err(_) => checks_failed.push("json_validity".to_owned()),
        }
    }
    if generic_refusal(output_text) {
        checks_failed.push("generic_refusal".to_owned());
    }

    if cli_help_translation_is_complete(input_text, output_text, target_lang, &checks_failed) {
        let before = checks_failed.len();
        checks_failed.retain(|check| check != "token_ratio" && check != "paragraph_count");
        if checks_failed.len() != before {
            advisory_warnings.push("cli_help_density".to_owned());
        }
    }

    let status = classify_status(&checks_failed, &advisory_warnings);
    let score = validation_score(status, &checks_failed, &density);
    CompletenessResult {
        is_complete: status.accepts_output(),
        status,
        score,
        density,
        checks_failed,
        advisory_warnings,
        input_stats,
        output_stats,
    }
}

// ── Internals ─────────────────────────────────────────────────────────────────

fn compute_stats(text: &str) -> CompletenessStats {
    CompletenessStats {
        unicode_scalar_count: text.chars().count(),
        paragraph_count: count_paragraphs(text),
        heading_count: count_markdown_headings(text),
        fenced_code_block_count: count_fenced_code_blocks(text),
        url_count: urls(text).len(),
        placeholder_count: placeholder_tokens(text).len(),
    }
}

fn density_check(
    input: &CompletenessStats,
    output: &CompletenessStats,
    target_lang: &str,
    thresholds: &CompletenessThresholds,
) -> DensityCheck {
    let ratio = (input.unicode_scalar_count > 0)
        .then(|| output.unicode_scalar_count as f64 / input.unicode_scalar_count as f64);
    let Some(minimum_ratio) = min_unicode_scalar_ratio(target_lang, thresholds) else {
        return DensityCheck {
            status: DensityStatus::Unverified,
            ratio,
            minimum_ratio: None,
            maximum_ratio: None,
        };
    };
    DensityCheck {
        status: DensityStatus::Calibrated,
        ratio,
        minimum_ratio: Some(minimum_ratio),
        maximum_ratio: Some(MAX_CALIBRATED_DENSITY_RATIO),
    }
}

fn classify_status(checks_failed: &[String], advisory_warnings: &[String]) -> CompletenessStatus {
    if checks_failed.is_empty() {
        if advisory_warnings
            .iter()
            .any(|warning| warning == "unverified_density")
        {
            CompletenessStatus::Unverified
        } else if advisory_warnings.is_empty() {
            CompletenessStatus::Valid
        } else {
            CompletenessStatus::ValidWithAdvisories
        }
    } else if checks_failed.iter().any(|check| {
        matches!(
            check.as_str(),
            "heading_preservation"
                | "fenced_code_preservation"
                | "placeholder_preservation"
                | "url_preservation"
                | "json_validity"
                | "json_key_preservation"
        )
    }) {
        CompletenessStatus::StructurallyInvalid
    } else {
        CompletenessStatus::RetryableIncomplete
    }
}

fn validation_score(
    status: CompletenessStatus,
    checks_failed: &[String],
    density: &DensityCheck,
) -> i32 {
    let mut score = match status {
        CompletenessStatus::Valid => 1_000,
        CompletenessStatus::ValidWithAdvisories => 950,
        CompletenessStatus::Unverified => 700,
        CompletenessStatus::RetryableIncomplete => 500,
        CompletenessStatus::StructurallyInvalid => 300,
        CompletenessStatus::DegradedBestEffort => 0,
    };
    score -= (checks_failed.len() as i32) * 75;
    if density.status == DensityStatus::Calibrated {
        if let (Some(ratio), Some(minimum)) = (density.ratio, density.minimum_ratio) {
            score += ((ratio / minimum).min(1.0) * 50.0) as i32;
        }
    }
    score.max(0)
}

fn count_fenced_code_blocks(text: &str) -> usize {
    text.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("```") || trimmed.starts_with("~~~")
        })
        .count()
        .div_ceil(2)
}

fn placeholder_tokens(text: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let mut remaining = text;
    while let Some(start) = remaining.find("{{") {
        let after_start = &remaining[start + 2..];
        let Some(end) = after_start.find("}}") else {
            break;
        };
        let candidate = &after_start[..end];
        if !candidate.trim().is_empty() {
            tokens.insert(format!("{{{{{candidate}}}}}"));
        }
        remaining = &after_start[end + 2..];
    }

    let mut remaining = text;
    while let Some(start) = remaining.find('{') {
        let after_start = &remaining[start + 1..];
        let Some(end) = after_start.find('}') else {
            break;
        };
        let candidate = &after_start[..end];
        if !candidate.is_empty()
            && candidate.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
            })
        {
            tokens.insert(format!("{{{candidate}}}"));
        }
        remaining = &after_start[end + 1..];
    }
    tokens
}

fn urls(text: &str) -> HashSet<String> {
    text.split_whitespace()
        .filter_map(|token| {
            let start = token.find("https://").or_else(|| token.find("http://"))?;
            let url = &token[start..];
            let url = url.trim_matches(|character: char| {
                matches!(
                    character,
                    '<' | '>'
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | ','
                        | '.'
                        | ';'
                        | ':'
                        | '!'
                        | '?'
                )
            });
            (!url.is_empty()).then(|| url.to_owned())
        })
        .collect()
}

fn preserves_all(input: &HashSet<String>, output: &HashSet<String>) -> bool {
    input.iter().all(|token| output.contains(token))
}

fn parse_json_document(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();
    (trimmed.starts_with('{') || trimmed.starts_with('['))
        .then(|| serde_json::from_str::<serde_json::Value>(trimmed).ok())
        .flatten()
}

fn preserves_json_object_keys(input: &serde_json::Value, output: &serde_json::Value) -> bool {
    match input {
        serde_json::Value::Object(input) => {
            let serde_json::Value::Object(output) = output else {
                return false;
            };
            input.iter().all(|(key, input_value)| {
                output.get(key).is_some_and(|output_value| {
                    preserves_json_object_keys(input_value, output_value)
                })
            })
        }
        serde_json::Value::Array(input) => {
            let serde_json::Value::Array(output) = output else {
                return false;
            };
            input.len() <= output.len()
                && input.iter().zip(output).all(|(input_value, output_value)| {
                    preserves_json_object_keys(input_value, output_value)
                })
        }
        _ => true,
    }
}

fn count_paragraphs(text: &str) -> usize {
    text.split("\n\n").filter(|b| !b.trim().is_empty()).count()
}

fn count_markdown_headings(text: &str) -> usize {
    text.lines().filter(|line| line.starts_with('#')).count()
}

fn cli_help_translation_is_complete(
    input_text: &str,
    output_text: &str,
    target_lang: &str,
    checks_failed: &[String],
) -> bool {
    if checks_failed.is_empty() || has_non_density_failure(checks_failed) {
        return false;
    }
    if !looks_like_cli_help(input_text)
        || !looks_like_translated_cli_help(output_text)
        || generic_refusal(output_text)
    {
        return false;
    }
    output_has_target_language_signal(output_text, target_lang)
        && preserves_cli_options(input_text, output_text)
}

fn has_non_density_failure(checks_failed: &[String]) -> bool {
    checks_failed
        .iter()
        .any(|check| check != "token_ratio" && check != "paragraph_count")
}

fn looks_like_cli_help(text: &str) -> bool {
    let lower = text.to_lowercase();
    let has_usage = lower.contains("usage:") || lower.contains("用法");
    let has_help_section = lower.contains("options:")
        || lower.contains("arguments:")
        || lower.contains("commands:")
        || lower.contains("examples:")
        || lower.contains("选项")
        || lower.contains("参数")
        || lower.contains("命令")
        || lower.contains("示例");
    has_usage && has_help_section && long_options(text).len() >= 2
}

fn looks_like_translated_cli_help(text: &str) -> bool {
    let lower = text.to_lowercase();
    let has_usage = lower.contains("usage:") || lower.contains("用法");
    let has_options = lower.contains("options:") || lower.contains("选项");
    has_usage && has_options
}

fn preserves_cli_options(input_text: &str, output_text: &str) -> bool {
    let input_options = long_options(input_text);
    if input_options.len() < 2 {
        return false;
    }
    let preserved = input_options
        .iter()
        .filter(|option| output_text.contains(option.as_str()))
        .count();
    preserved == input_options.len()
}

fn long_options(text: &str) -> HashSet<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .filter(|token| {
            token
                .strip_prefix("--")
                .and_then(|name| name.chars().next())
                .is_some_and(|c| c.is_ascii_alphanumeric())
        })
        .map(ToOwned::to_owned)
        .collect()
}

fn output_has_target_language_signal(output_text: &str, target_lang: &str) -> bool {
    let Some(spec) = language_spec_or_none(target_lang) else {
        return !output_text.trim().is_empty();
    };
    if spec.family == LanguageFamily::Chinese {
        return output_text.chars().any(is_cjk_char);
    }
    if spec.canonical_code == "en" {
        return output_text.chars().any(|c| c.is_ascii_alphabetic());
    }
    !output_text.trim().is_empty()
}

fn is_cjk_char(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
}

fn generic_refusal(output_text: &str) -> bool {
    let lower = output_text.to_lowercase();
    [
        "please provide",
        "no text provided",
        "cannot translate",
        "unable to translate",
        "请提供",
        "无法翻译",
        "不能翻译",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// Returns the applicable minimum Unicode-scalar density ratio for `target_lang`, or `None`.
fn min_unicode_scalar_ratio(target_lang: &str, thresholds: &CompletenessThresholds) -> Option<f64> {
    let spec = language_spec_or_none(target_lang)?;
    if spec.canonical_code == "en" {
        return Some(thresholds.zh_to_en_min_ratio);
    }
    if spec.family == LanguageFamily::Chinese {
        return Some(thresholds.en_to_zh_min_ratio);
    }
    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Happy path ───────────────────────────────────────────────────────────

    #[test]
    fn complete_zh_to_en() {
        let input = "这是一段较长的中文文本。它包含多个句子，用于测试翻译完整性验证功能。";
        let output = "This is a longer Chinese passage. It contains multiple sentences to test translation completeness.";
        let result = validate_completeness(input, output, "en", None);
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
        let result = validate_completeness(input, output, "zh", None);
        assert!(
            result.is_complete,
            "checks_failed={:?}",
            result.checks_failed
        );
    }

    // ── Token ratio failures ─────────────────────────────────────────────────

    #[test]
    fn fails_when_output_too_short_zh_to_en() {
        let input = "这是一段很长的中文段落，包含很多内容和细节，翻译后应该有足够的字数。";
        let output = "Short.";
        let result = validate_completeness(input, output, "en", None);
        assert!(!result.is_complete);
        assert!(result.checks_failed.contains(&"token_ratio".to_owned()));
    }

    #[test]
    fn fails_when_output_too_short_en_to_zh() {
        let input = "This is a long English sentence designed to test the character ratio check.";
        let output = "短";
        let result = validate_completeness(input, output, "zh", None);
        assert!(!result.is_complete);
        assert!(result.checks_failed.contains(&"token_ratio".to_owned()));
    }

    #[test]
    fn no_ratio_check_for_other_languages() {
        let input = "Some source text in French.";
        let output = "Z"; // Would fail ratio check for en/zh
        let result = validate_completeness(input, output, "fr", None);
        // No token_ratio check applies for fr
        assert!(!result.checks_failed.contains(&"token_ratio".to_owned()));
    }

    #[test]
    fn chinese_family_targets_use_zh_completeness_rules() {
        let input = "This is a long English sentence designed to test the character ratio check.";
        let output = "短";
        let result = validate_completeness(input, output, "yue", None);
        assert!(result.checks_failed.contains(&"token_ratio".to_owned()));
    }

    // ── Paragraph count failures ─────────────────────────────────────────────

    #[test]
    fn fails_when_paragraphs_lost() {
        let input = "Para one.\n\nPara two.\n\nPara three.\n\nPara four.";
        let output = "Single paragraph.";
        let result = validate_completeness(input, output, "fr", None);
        assert!(result.checks_failed.contains(&"paragraph_count".to_owned()));
    }

    #[test]
    fn passes_when_paragraph_ratio_above_threshold() {
        let input = "A.\n\nB.";
        let output = "Ä.\n\nB̈.";
        let result = validate_completeness(input, output, "fr", None);
        assert!(!result.checks_failed.contains(&"paragraph_count".to_owned()));
    }

    // ── Heading preservation failures ────────────────────────────────────────

    #[test]
    fn fails_when_heading_dropped() {
        let input = "# Title\n\nContent.";
        let output = "Content without heading.";
        let result = validate_completeness(input, output, "en", None);
        assert!(result
            .checks_failed
            .contains(&"heading_preservation".to_owned()));
    }

    #[test]
    fn passes_when_all_headings_preserved() {
        let input = "# Title\n\n## Sub\n\nContent.";
        let output = "# Título\n\n## Subtítulo\n\nContenido.";
        let result = validate_completeness(input, output, "es", None);
        assert!(!result
            .checks_failed
            .contains(&"heading_preservation".to_owned()));
    }

    #[test]
    fn additional_headings_in_output_is_ok() {
        let input = "# H1\n\nBody.";
        let output = "# H1\n\n## Extra\n\nBody.";
        let result = validate_completeness(input, output, "es", None);
        assert!(!result
            .checks_failed
            .contains(&"heading_preservation".to_owned()));
    }

    // ── Custom thresholds ────────────────────────────────────────────────────

    #[test]
    fn custom_thresholds_applied() {
        let tight = CompletenessThresholds {
            zh_to_en_min_ratio: 0.9,
            en_to_zh_min_ratio: 0.9,
            min_paragraph_ratio: 0.9,
        };
        let input = "Source text here.";
        let output = "Shorter.";
        let result = validate_completeness(input, output, "zh", Some(&tight));
        assert!(result.checks_failed.contains(&"token_ratio".to_owned()));
    }

    // ── Stats helpers ────────────────────────────────────────────────────────

    #[test]
    fn count_paragraphs_basic() {
        assert_eq!(count_paragraphs("a\n\nb\n\nc"), 3);
        assert_eq!(count_paragraphs("single"), 1);
        assert_eq!(count_paragraphs(""), 0);
        assert_eq!(count_paragraphs("\n\n   \n\n"), 0); // blank-only blocks ignored
    }

    #[test]
    fn count_headings_basic() {
        assert_eq!(count_markdown_headings("# H1\n## H2\ntext\n### H3"), 3);
        assert_eq!(count_markdown_headings("no headings here"), 0);
    }

    // ── Edge cases ───────────────────────────────────────────────────────────

    #[test]
    fn empty_input_skips_ratio_check() {
        let result = validate_completeness("", "some output", "en", None);
        assert!(!result.checks_failed.contains(&"token_ratio".to_owned()));
    }

    #[test]
    fn multiple_checks_can_fail_simultaneously() {
        let input = "# Heading\n\nPara one.\n\nPara two.\n\nPara three.";
        let output = "z"; // fails ratio, paragraph count, and heading
        let result = validate_completeness(input, output, "en", None);
        assert!(!result.is_complete);
        assert!(result.checks_failed.len() >= 2);
    }

    // ── Advisory warnings ────────────────────────────────────────────────────

    #[test]
    fn zh_to_en_paragraph_merge_is_advisory() {
        // 3 paras in zh, 1 in en. Ratio = 0.33 < 0.5 (threshold)
        let input = "第一段。\n\n第二段。\n\n第三段。";
        let output = "Para one and two and three merged into one single paragraph that is long enough to pass ratio.";
        let result = validate_completeness(input, output, "en", None);
        // Should be complete because char_ratio passed (output is long enough)
        assert!(result.is_complete);
        assert!(result
            .advisory_warnings
            .contains(&"paragraph_count".to_owned()));
        assert!(!result.checks_failed.contains(&"paragraph_count".to_owned()));
    }

    #[test]
    fn en_to_zh_paragraph_merge_is_advisory() {
        let input = "Para one.\n\nPara two.\n\nPara three.";
        let output = "第一段、第二段和第三段被合并成了一个足够长的段落。";
        let result = validate_completeness(input, output, "zh", None);
        assert!(result.is_complete);
        assert!(result
            .advisory_warnings
            .contains(&"paragraph_count".to_owned()));
    }

    #[test]
    fn paragraph_loss_is_hard_failure_when_ratio_also_fails() {
        let input = "Para one.\n\nPara two.\n\nPara three.";
        let output = "Short."; // fails ratio AND paragraph count
        let result = validate_completeness(input, output, "en", None);
        assert!(!result.is_complete);
        assert!(result.checks_failed.contains(&"token_ratio".to_owned()));
        assert!(result.checks_failed.contains(&"paragraph_count".to_owned()));
        assert!(result.advisory_warnings.is_empty());
    }

    #[test]
    fn paragraph_loss_is_hard_failure_for_unsupported_lang() {
        let input = "Para one.\n\nPara two.\n\nPara three.";
        let output = "Short.";
        // "fr" doesn't have char_ratio check, so paragraph_count is the only guard
        let result = validate_completeness(input, output, "fr", None);
        assert!(!result.is_complete);
        assert!(result.checks_failed.contains(&"paragraph_count".to_owned()));
    }

    #[test]
    fn cli_help_translation_can_be_dense_when_options_are_preserved() {
        let input = "Generate a cited answer.\n\n\
Usage: verbatim ask [OPTIONS] <QUESTION>...\n\n\
Arguments:\n  <QUESTION>... Question text\n\n\
Options:\n  --source-id <SOURCE_ID> Limit retrieval\n  --collection <NAME> Limit retrieval\n  --require-fresh Error on stale collection members\n  --embedding-profile <PROFILE> Use an embedding profile\n  --show-retrieval Show retrieval debug info\n  --context-only Return context without generation\n  --no-generate Alias for context-only\n  --format <FORMAT> Output format\n  --background Queue as background task\n  -h, --help Print help\n\n\
Examples:\n  verbatim ask \"What supports this?\"\n";
        let output = "生成带引用的回答。用法：verbatim ask [选项] <问题>...。参数：<问题>...。选项：--source-id、--collection、--require-fresh、--embedding-profile、--show-retrieval、--context-only、--no-generate、--format、--background、--help。示例：verbatim ask \"有哪些证据？\"。";

        let tight = CompletenessThresholds {
            zh_to_en_min_ratio: 10.0,
            en_to_zh_min_ratio: 10.0,
            min_paragraph_ratio: 0.9,
        };
        let result = validate_completeness(input, output, "zh", Some(&tight));

        assert!(result.is_complete, "{:?}", result.checks_failed);
        assert!(result
            .advisory_warnings
            .contains(&"cli_help_density".to_owned()));
    }

    #[test]
    fn cli_help_translation_missing_half_of_options_still_fails() {
        let input = "Generate a cited answer.\n\n\
Usage: verbatim ask [OPTIONS] <QUESTION>...\n\n\
Arguments:\n  <QUESTION>... Question text\n\n\
Options:\n  --source-id <SOURCE_ID> Limit retrieval\n  --collection <NAME> Limit retrieval\n  --require-fresh Error on stale collection members\n  --embedding-profile <PROFILE> Use an embedding profile\n  --show-retrieval Show retrieval debug info\n  --context-only Return context without generation\n  --no-generate Alias for context-only\n  --format <FORMAT> Output format\n  --background Queue as background task\n  -h, --help Print help\n\n\
Examples:\n  verbatim ask \"What supports this?\"\n";
        let output = "生成带引用的回答。用法：verbatim ask [选项] <问题>...。选项：--source-id、--collection、--require-fresh、--embedding-profile、--show-retrieval。示例：verbatim ask \"有哪些证据？\"。";
        let tight = CompletenessThresholds {
            zh_to_en_min_ratio: 10.0,
            en_to_zh_min_ratio: 10.0,
            min_paragraph_ratio: 0.9,
        };

        let result = validate_completeness(input, output, "zh", Some(&tight));

        assert!(!result.is_complete);
        assert!(result.checks_failed.contains(&"token_ratio".to_owned()));
        assert!(!result
            .advisory_warnings
            .contains(&"cli_help_density".to_owned()));
    }

    #[test]
    fn cli_help_real_smoke_translation_preserves_all_options() {
        let input = "Generate a cited answer.\n\n\
Usage: verbatim ask [OPTIONS] <QUESTION>...\n\n\
Arguments:\n  <QUESTION>... Question text\n\n\
Options:\n  -s, --source-id <SOURCE_ID> Limit retrieval\n  --collection <NAME> Limit retrieval\n  --require-fresh Error on stale collection members\n  --embedding-profile <EMBEDDING_PROFILE> Use an embedding profile\n  --show-retrieval Show retrieval debug info\n  --context-only Return context without generation\n  --no-generate Alias for context-only\n  --format <FORMAT> Output format\n  --background Queue as background task\n  -h, --help Print help\n\n\
Examples:\n  verbatim ask \"What supports this?\"\n";
        let output = "用法：verbatim ask [选项] <问题>...\n\n\
参数：\n  <问题>... 问题文本\n\n\
选项：\n  -s, --source-id <SOURCE_ID>\n          仅从指定来源进行检索\n      --collection <NAME>\n          仅从指定的材料化集合中检索。如需合并多个集合的结果，可重复使用此选项\n      --require-fresh\n          若集合中的内容已过时，则直接报错而非返回警告信息\n      --embedding-profile <EMBEDDING_PROFILE>\n          使用指定的嵌入配置进行检索\n      --show-retrieval\n          显示检索来源及排序相关的调试信息\n      --context-only\n          返回检索上下文信息，而不调用聊天生成功能\n      --no-generate\n          与--context-only功能相同；不会调用任何聊天模型\n      --format <FORMAT>\n          仅当使用--context-only或--no-generate时生效的输出格式。JSON格式包含结构化的定位符/来源字段 [可选值：markdown, json]\n      --background\n          将请求作为持久后台任务排队处理，并立即返回\n  -h, --help\n          显示帮助信息（使用--help可查看更多内容）\n\n\
示例：\n  verbatim ask \"报告得出了什么结论？\"\n  verbatim ask --source-id <source-id> --show-retrieval \"有哪些证据支持这一结论？\"\n  verbatim ask --collection articles \"哪些证据是相关的？\"\n  verbatim ask --context-only \"哪些证据是相关的？\"\n  verbatim ask --no-generate --format json \"哪些证据是相关的？\"\n\n\
注意事项：\n  普通询问模式会在检索完成后调用已配置的聊天模型。\n  --context-only和--no-generate选项仅返回检索上下文信息，不进行聊天生成；在此模式下不支持--background选项。\n  --format选项仅适用于--context-only或--no-generate模式。";
        let tight = CompletenessThresholds {
            zh_to_en_min_ratio: 10.0,
            en_to_zh_min_ratio: 10.0,
            min_paragraph_ratio: 0.9,
        };

        let result = validate_completeness(input, output, "zh", Some(&tight));

        assert!(result.is_complete, "{:?}", result.checks_failed);
        assert!(result.checks_failed.is_empty());
        assert!(result
            .advisory_warnings
            .contains(&"cli_help_density".to_owned()));
    }

    #[test]
    fn cli_help_generic_refusal_still_fails() {
        let input = "Generate a cited answer.\n\n\
Usage: verbatim ask [OPTIONS] <QUESTION>...\n\n\
Arguments:\n  <QUESTION>... Question text\n\n\
Options:\n  --source-id <SOURCE_ID> Limit retrieval\n  --collection <NAME> Limit retrieval\n  --require-fresh Error on stale collection members\n  --embedding-profile <PROFILE> Use an embedding profile\n  --show-retrieval Show retrieval debug info\n  --context-only Return context without generation\n  --no-generate Alias for context-only\n  --format <FORMAT> Output format\n  --background Queue as background task\n  -h, --help Print help\n\n\
Examples:\n  verbatim ask \"What supports this?\"\n";
        let output = "请提供需要处理的完整输入文本。";
        let tight = CompletenessThresholds {
            zh_to_en_min_ratio: 10.0,
            en_to_zh_min_ratio: 10.0,
            min_paragraph_ratio: 0.9,
        };

        let result = validate_completeness(input, output, "zh", Some(&tight));

        assert!(!result.is_complete);
        assert!(result.checks_failed.contains(&"token_ratio".to_owned()));
    }

    #[test]
    fn cli_help_examples_only_output_still_fails() {
        let input = "Generate a cited answer.\n\n\
Usage: verbatim ask [OPTIONS] <QUESTION>...\n\n\
Options:\n  --source-id <SOURCE_ID> Limit retrieval\n  --collection <NAME> Limit retrieval\n  --show-retrieval Show retrieval debug info\n  --context-only Return context without generation\n  --no-generate Alias for context-only\n  --format <FORMAT> Output format\n\n\
Examples:\n  verbatim ask \"What supports this?\"\n";
        let output = "verbatim ask \"报告得出了什么结论？\"\n\
verbatim ask --source-id <source-id> --show-retrieval \"有哪些内容支持这一结论？\"\n\
verbatim ask --collection articles \"哪些证据是相关的？\"\n\
verbatim ask --context-only \"哪些证据是相关的？\"\n\
verbatim ask --no-generate --format json \"哪些证据是相关的？\"";
        let tight = CompletenessThresholds {
            zh_to_en_min_ratio: 10.0,
            en_to_zh_min_ratio: 10.0,
            min_paragraph_ratio: 0.9,
        };

        let result = validate_completeness(input, output, "zh", Some(&tight));

        assert!(!result.is_complete);
        assert!(result.checks_failed.contains(&"token_ratio".to_owned()));
    }

    // ── Layered validator regression coverage ────────────────────────────────

    #[test]
    fn density_uses_unicode_scalars_not_utf8_bytes() {
        let stats = compute_stats("é中");
        assert_eq!(stats.unicode_scalar_count, 2);
    }

    #[test]
    fn zh_to_en_density_ratio_is_not_a_utf8_byte_ratio() {
        // Scalar ratio is 1/2 = 0.5, whereas the old byte ratio was 1/6.
        let result = validate_completeness("中文", "a", "en", None);

        assert!(result.is_complete, "{:?}", result.checks_failed);
        assert_eq!(result.density.ratio, Some(0.5));
    }

    #[test]
    fn uncalibrated_target_reports_actionable_unverified_density() {
        let result =
            validate_completeness("A source paragraph with several words.", "Z", "fr", None);

        assert!(result.is_complete, "{:?}", result.checks_failed);
        assert_eq!(result.status, CompletenessStatus::Unverified);
        assert!(result
            .advisory_warnings
            .contains(&"unverified_density".to_owned()));
    }

    #[test]
    fn calibrated_density_rejects_runaway_upper_bound() {
        let result = validate_completeness("Short source.", &"word ".repeat(100), "en", None);

        assert_eq!(result.status, CompletenessStatus::RetryableIncomplete);
        assert!(result
            .checks_failed
            .contains(&"density_upper_bound".to_owned()));
    }

    #[test]
    fn structural_signals_reject_missing_fence_url_and_placeholder() {
        let input = "# Setup\n\n```sh\necho {name}\n```\n\nRead https://example.test/docs.";
        let output = "# Configuration\n\nRead the documentation.";
        let result = validate_completeness(input, output, "fr", None);

        assert_eq!(result.status, CompletenessStatus::StructurallyInvalid);
        assert!(result
            .checks_failed
            .contains(&"fenced_code_preservation".to_owned()));
        assert!(result
            .checks_failed
            .contains(&"placeholder_preservation".to_owned()));
        assert!(result
            .checks_failed
            .contains(&"url_preservation".to_owned()));
    }

    #[test]
    fn malformed_json_is_a_structural_failure() {
        let result = validate_completeness(r#"{"title":"Hello"}"#, "{not-json}", "fr", None);

        assert_eq!(result.status, CompletenessStatus::StructurallyInvalid);
        assert!(result.checks_failed.contains(&"json_validity".to_owned()));
    }

    #[test]
    fn missing_json_object_key_is_a_structural_failure() {
        let result = validate_completeness(
            r#"{"required_key":"value","retained_key":"value"}"#,
            r#"{"retained_key":"valeur"}"#,
            "fr",
            None,
        );

        assert_eq!(result.status, CompletenessStatus::StructurallyInvalid);
        assert!(result
            .checks_failed
            .contains(&"json_key_preservation".to_owned()));
    }

    #[test]
    fn replaced_json_object_key_is_a_structural_failure() {
        let result = validate_completeness(
            r#"{"required_key":{"nested_key":"value"}}"#,
            r#"{"different_key":{"nested_key":"valeur"}}"#,
            "fr",
            None,
        );

        assert_eq!(result.status, CompletenessStatus::StructurallyInvalid);
        assert!(result
            .checks_failed
            .contains(&"json_key_preservation".to_owned()));
    }

    #[test]
    fn missing_nested_json_object_key_is_a_structural_failure() {
        let result = validate_completeness(
            r#"{"required_key":{"nested_key":"value"}}"#,
            r#"{"required_key":{}}"#,
            "fr",
            None,
        );

        assert_eq!(result.status, CompletenessStatus::StructurallyInvalid);
        assert!(result
            .checks_failed
            .contains(&"json_key_preservation".to_owned()));
    }

    #[test]
    fn finish_reason_length_is_retryable_transport_failure() {
        let context = CompletenessContext {
            termination: CompletionTermination::Length,
        };
        let result = validate_completeness_with_context(
            "Source text.",
            "Translated text.",
            "en",
            None,
            &context,
        );

        assert_eq!(result.status, CompletenessStatus::RetryableIncomplete);
        assert!(result
            .checks_failed
            .contains(&"finish_reason_length".to_owned()));
    }
}
