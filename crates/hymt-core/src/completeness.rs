//! Three-layer completeness validation for translated segments.
//!
//! Prevents silent content loss caused by LLM early-stop: a translation that
//! passes all three checks is considered complete enough to cache and use.

use std::collections::HashSet;

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
    /// Advisory warnings that do not trigger a retry (e.g. paragraph_count mismatch when token_ratio passes).
    pub advisory_warnings: Vec<String>,
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
    let mut advisory_warnings: Vec<String> = Vec::new();

    // Check 1: character ratio.
    let mut char_ratio_passed = None; // None means check was not applicable
    if let Some(min_ratio) = min_char_ratio(target_lang, &thresholds) {
        if input_stats.char_count > 0 {
            let actual_ratio = output_stats.char_count as f64 / input_stats.char_count as f64;
            if actual_ratio < min_ratio {
                checks_failed.push("token_ratio".to_owned());
                char_ratio_passed = Some(false);
            } else {
                char_ratio_passed = Some(true);
            }
        }
    }

    // Check 2: paragraph count ratio.
    if input_stats.paragraph_count > 0 {
        let ratio = output_stats.paragraph_count as f64 / input_stats.paragraph_count as f64;
        if ratio < thresholds.min_paragraph_ratio {
            // Demote to advisory if char_ratio was applicable AND passed.
            // If char_ratio was NOT applicable (non en/zh) or FAILED, it's a hard failure.
            if char_ratio_passed == Some(true) {
                advisory_warnings.push("paragraph_count".to_owned());
            } else {
                checks_failed.push("paragraph_count".to_owned());
            }
        }
    }

    // Check 3: heading preservation.
    if input_stats.heading_count > 0 && output_stats.heading_count < input_stats.heading_count {
        checks_failed.push("heading_preservation".to_owned());
    }

    if cli_help_translation_is_complete(input_text, output_text, target_lang, &checks_failed) {
        let before = checks_failed.len();
        checks_failed.retain(|check| check != "token_ratio" && check != "paragraph_count");
        if checks_failed.len() != before {
            advisory_warnings.push("cli_help_density".to_owned());
        }
    }

    CompletenessResult {
        is_complete: checks_failed.is_empty(),
        checks_failed,
        advisory_warnings,
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
    let normalized = target_lang.to_lowercase().replace('_', "-");
    if normalized == "zh" || normalized.starts_with("zh-") {
        return output_text.chars().any(is_cjk_char);
    }
    if normalized == "en" || normalized.starts_with("en-") {
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
}
