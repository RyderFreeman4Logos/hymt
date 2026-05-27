//! Three-layer completeness validation for translated segments.
//!
//! Prevents silent content loss caused by LLM early-stop: a translation that
//! passes all three checks is considered complete enough to cache and use.

use serde::{Deserialize, Serialize};

/// Threshold configuration for completeness checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletenessThresholds {
    /// Minimum character-count ratio for zh→en translations (source is zh).
    pub zh_to_en_min_ratio: f64,
    /// Minimum character-count ratio for en→zh translations (source is en).
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

/// Raw counts extracted from a text for completeness comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletenessStats {
    pub char_count: usize,
    pub paragraph_count: usize,
    pub heading_count: usize,
}

/// Outcome of a completeness check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletenessResult {
    pub is_complete: bool,
    /// Names of checks that failed: `"token_ratio"`, `"paragraph_count"`, `"heading_preservation"`.
    pub checks_failed: Vec<String>,
    pub input_stats: CompletenessStats,
    pub output_stats: CompletenessStats,
}

/// Validates whether `output_text` is a sufficiently complete translation of `input_text`.
///
/// `target_lang` determines which character-ratio threshold to apply:
/// - `"en"` / `"en-*"` → `zh_to_en_min_ratio` (source was Chinese)
/// - `"zh"` / `"zh-*"` → `en_to_zh_min_ratio` (source was English)
/// - anything else → no char-ratio check
pub fn validate_completeness(
    input_text: &str,
    output_text: &str,
    target_lang: &str,
    thresholds: Option<&CompletenessThresholds>,
) -> CompletenessResult {
    let thresholds = thresholds.cloned().unwrap_or_default();
    let input_stats = compute_stats(input_text);
    let output_stats = compute_stats(output_text);
    let mut checks_failed: Vec<String> = Vec::new();

    // Check 1: character ratio.
    if let Some(min_ratio) = min_char_ratio(target_lang, &thresholds) {
        if input_stats.char_count > 0 {
            let actual_ratio = output_stats.char_count as f64 / input_stats.char_count as f64;
            if actual_ratio < min_ratio {
                checks_failed.push("token_ratio".to_owned());
            }
        }
    }

    // Check 2: paragraph count ratio.
    if input_stats.paragraph_count > 0 {
        let ratio = output_stats.paragraph_count as f64 / input_stats.paragraph_count as f64;
        if ratio < thresholds.min_paragraph_ratio {
            checks_failed.push("paragraph_count".to_owned());
        }
    }

    // Check 3: heading preservation.
    if input_stats.heading_count > 0 && output_stats.heading_count < input_stats.heading_count {
        checks_failed.push("heading_preservation".to_owned());
    }

    CompletenessResult {
        is_complete: checks_failed.is_empty(),
        checks_failed,
        input_stats,
        output_stats,
    }
}

// ── Internals ─────────────────────────────────────────────────────────────────

fn compute_stats(text: &str) -> CompletenessStats {
    CompletenessStats {
        char_count: text.len(),
        paragraph_count: count_paragraphs(text),
        heading_count: count_markdown_headings(text),
    }
}

fn count_paragraphs(text: &str) -> usize {
    text.split("\n\n").filter(|b| !b.trim().is_empty()).count()
}

fn count_markdown_headings(text: &str) -> usize {
    text.lines().filter(|line| line.starts_with('#')).count()
}

/// Returns the applicable minimum char ratio for `target_lang`, or `None`.
fn min_char_ratio(target_lang: &str, thresholds: &CompletenessThresholds) -> Option<f64> {
    let normalized = target_lang.to_lowercase().replace('_', "-");
    if normalized == "en" || normalized.starts_with("en-") {
        return Some(thresholds.zh_to_en_min_ratio);
    }
    if normalized == "zh" || normalized.starts_with("zh-") {
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
}
