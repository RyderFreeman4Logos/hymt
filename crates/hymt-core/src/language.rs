//! Document language analysis and section-level translation planning.
//!
//! Language detection uses CJK character ratio counting, which is reliable for
//! Chinese-family targets and returns `None` for all other targets.

use serde::{Deserialize, Serialize};

use crate::language_spec::{language_spec_or_none, normalize_language_code, LanguageFamily};

/// CJK Unified Ideographs block (U+4E00–U+9FFF).
fn is_cjk_char(c: char) -> bool {
    ('\u{4E00}'..='\u{9FFF}').contains(&c)
}

/// Minimum paragraph ratio required to declare a section "already in target language".
pub const TARGET_PARAGRAPH_RATIO: f64 = 0.60;

/// Minimum analyzed characters required before paragraph-level target detection can skip text.
pub const MIN_TARGET_PARAGRAPH_ANALYZED_CHARS: usize = 4;

/// How the document planner should treat already-target-language paragraphs.
///
/// Code and frontmatter are preserved under every policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentTranslationPolicy {
    /// Submit every non-code paragraph to the translation model.
    TranslateAll,
    /// Preserve only paragraphs detected as confidently already in the target language.
    SkipHighConfidenceTargetParagraphs,
}

// ── Result types ─────────────────────────────────────────────────────────────

/// Result of running language detection on a text chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LanguageDetectionResult {
    /// Fraction of analyzed characters that belong to the target language.
    pub target_ratio: f64,
    /// Most-frequently detected language tag, if any.
    pub detected_lang: Option<String>,
    /// Number of non-whitespace characters analyzed.
    pub analyzed_chars: usize,
}

/// Semantic role of a document section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SectionKind {
    Paragraph,
    Separator,
    Code,
}

/// One logical section of a document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentSection {
    pub text: String,
    pub kind: SectionKind,
    pub paragraph_index: Option<usize>,
    pub detected_lang: Option<String>,
    pub target_ratio: Option<f64>,
    pub analyzed_chars: usize,
    pub is_target_language: bool,
    pub should_translate: bool,
}

impl DocumentSection {
    fn raw(text: impl Into<String>, kind: SectionKind, paragraph_index: Option<usize>) -> Self {
        Self {
            text: text.into(),
            kind,
            paragraph_index,
            detected_lang: None,
            target_ratio: None,
            analyzed_chars: 0,
            is_target_language: false,
            should_translate: false,
        }
    }
}

/// Translation plan for an entire document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentLanguagePlan {
    pub sections: Vec<DocumentSection>,
    pub target_lang: String,
}

impl DocumentLanguagePlan {
    /// Returns a new plan with every paragraph marked for translation.
    pub fn translate_all_paragraphs(mut self) -> Self {
        for section in &mut self.sections {
            if section.kind == SectionKind::Paragraph {
                section.should_translate = true;
            }
        }
        self
    }
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Detects how much of `text` is in `target_lang`.
///
/// Returns `None` when detection is unsupported for the given target.
/// Only CJK (zh/yue) detection is available.
pub fn detect_target_language(text: &str, target_lang: &str) -> Option<LanguageDetectionResult> {
    detect_without_detector(text, target_lang)
}

/// Determines the effective target language for a document.
///
/// When `explicit_target` is `true`, `requested_target_lang` is returned
/// unchanged. Otherwise the document is analyzed: if it appears to already be
/// predominantly in `primary_lang`, the `secondary_lang` is chosen.
pub fn resolve_target_language(
    text: &str,
    requested_target_lang: &str,
    primary_lang: &str,
    secondary_lang: &str,
    explicit_target: bool,
) -> String {
    if explicit_target {
        return canonical_or_requested(requested_target_lang);
    }
    let detection = detect_target_language(text, primary_lang);
    if let Some(d) = detection {
        if d.target_ratio > TARGET_PARAGRAPH_RATIO {
            return canonical_or_requested(secondary_lang);
        }
    }
    canonical_or_requested(primary_lang)
}

fn is_short_code_like_paragraph(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.chars().count() <= 32
        && trimmed
            .chars()
            .any(|c| matches!(c, ';' | '=' | '{' | '}' | '(' | ')' | '[' | ']' | '`'))
}

/// Splits `text` into sections and annotates each paragraph with language
/// detection results. Paragraphs already in `target_lang` are flagged
/// `is_target_language = true` and `should_translate = false`.
pub fn analyze_document_language(text: &str, target_lang: &str) -> DocumentLanguagePlan {
    let detects_chinese = is_chinese_family(target_lang);
    let raw_sections = split_document_sections(text);
    let sections = raw_sections
        .into_iter()
        .map(|mut section| {
            if section.kind != SectionKind::Paragraph {
                return section;
            }
            let detection = detect_chunk_sequence(&[section.text.clone()], detects_chinese);
            let is_target = !is_short_code_like_paragraph(&section.text)
                && detection
                    .as_ref()
                    .map(|d| {
                        d.analyzed_chars >= MIN_TARGET_PARAGRAPH_ANALYZED_CHARS
                            && d.target_ratio > TARGET_PARAGRAPH_RATIO
                    })
                    .unwrap_or(false);
            section.detected_lang = detection.as_ref().and_then(|d| d.detected_lang.clone());
            section.target_ratio = detection.as_ref().map(|d| d.target_ratio);
            section.analyzed_chars = detection.as_ref().map(|d| d.analyzed_chars).unwrap_or(0);
            section.is_target_language = is_target;
            section.should_translate = !is_target;
            section
        })
        .collect();

    DocumentLanguagePlan {
        sections,
        target_lang: canonical_or_requested(target_lang),
    }
}

/// Builds a document translation plan according to `policy`.
///
/// [`DocumentTranslationPolicy::SkipHighConfidenceTargetParagraphs`] reuses the
/// CJK-only analyzer; unsupported target languages conservatively translate all
/// paragraphs. [`DocumentTranslationPolicy::TranslateAll`] bypasses detection.
pub fn plan_document_translation(
    text: &str,
    target_lang: &str,
    policy: DocumentTranslationPolicy,
) -> DocumentLanguagePlan {
    match policy {
        DocumentTranslationPolicy::TranslateAll => {
            build_document_translation_plan(text, target_lang)
        }
        DocumentTranslationPolicy::SkipHighConfidenceTargetParagraphs => {
            analyze_document_language(text, target_lang)
        }
    }
}

/// Builds a plan that marks every paragraph for translation without running
/// any language detection.
pub fn build_document_translation_plan(text: &str, target_lang: &str) -> DocumentLanguagePlan {
    let sections = split_document_sections(text)
        .into_iter()
        .map(|mut s| {
            if s.kind == SectionKind::Paragraph {
                s.should_translate = true;
            }
            s
        })
        .collect();
    DocumentLanguagePlan {
        sections,
        target_lang: canonical_or_requested(target_lang),
    }
}

// ── Internals ─────────────────────────────────────────────────────────────────

fn canonical_or_requested(code: &str) -> String {
    normalize_language_code(code)
        .map(str::to_owned)
        .unwrap_or_else(|_| code.trim().to_owned())
}

fn is_chinese_family(target_lang: &str) -> bool {
    language_spec_or_none(target_lang).is_some_and(|spec| spec.family == LanguageFamily::Chinese)
}

fn detect_without_detector(text: &str, target_lang: &str) -> Option<LanguageDetectionResult> {
    if !is_chinese_family(target_lang) {
        return None;
    }
    let chunks = detection_chunks(text);
    if chunks.is_empty() {
        return None;
    }
    detect_chunk_sequence(&chunks, true)
}

/// Runs CJK-based detection over a slice of text chunks.
fn detect_chunk_sequence(
    chunks: &[String],
    detects_chinese: bool,
) -> Option<LanguageDetectionResult> {
    if chunks.is_empty() || !detects_chinese {
        return None;
    }

    let mut analyzed_chars = 0usize;
    let mut target_chars = 0usize;
    let mut has_cjk = false;

    for chunk in chunks {
        for c in chunk.chars() {
            if c.is_whitespace() {
                continue;
            }
            analyzed_chars += 1;
            if is_cjk_char(c) {
                target_chars += 1;
                has_cjk = true;
            }
        }
    }

    if analyzed_chars == 0 {
        return None;
    }

    let target_ratio = target_chars as f64 / analyzed_chars as f64;
    let detected_lang = if has_cjk { Some("zh".to_owned()) } else { None };

    Some(LanguageDetectionResult {
        target_ratio,
        detected_lang,
        analyzed_chars,
    })
}

/// Splits `text` into paragraph-sized chunks on double-newlines.
fn detection_chunks(text: &str) -> Vec<String> {
    let mut chunks: Vec<String> = text
        .split("\n\n")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    if chunks.is_empty() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            chunks.push(trimmed.to_owned());
        }
    }
    chunks
}

/// Splits a document into paragraphs, separators, and code blocks.
fn split_document_sections(text: &str) -> Vec<DocumentSection> {
    let mut sections: Vec<DocumentSection> = Vec::new();
    let mut paragraph_lines: Vec<&str> = Vec::new();
    let mut code_lines: Vec<&str> = Vec::new();
    let mut fence_char = '\0';
    let mut fence_len = 0usize;
    let mut paragraph_index = 0usize;

    let flush_paragraph = |sections: &mut Vec<DocumentSection>,
                           paragraph_lines: &mut Vec<&str>,
                           paragraph_index: &mut usize| {
        if paragraph_lines.is_empty() {
            return;
        }
        *paragraph_index += 1;
        let text = paragraph_lines.join("");
        sections.push(DocumentSection::raw(
            text,
            SectionKind::Paragraph,
            Some(*paragraph_index),
        ));
        paragraph_lines.clear();
    };

    let body = if let Some((frontmatter, rest)) = split_yaml_frontmatter(text) {
        sections.push(DocumentSection::raw(frontmatter, SectionKind::Code, None));
        rest
    } else {
        text
    };

    for line in lines_with_endings(body) {
        if !code_lines.is_empty() {
            code_lines.push(line);
            if is_closing_fence(line, fence_char, fence_len) {
                sections.push(DocumentSection::raw(
                    code_lines.join(""),
                    SectionKind::Code,
                    None,
                ));
                code_lines.clear();
                fence_char = '\0';
                fence_len = 0;
            }
            continue;
        }

        if let Some((fc, fl)) = opening_fence(line) {
            flush_paragraph(&mut sections, &mut paragraph_lines, &mut paragraph_index);
            fence_char = fc;
            fence_len = fl;
            code_lines.push(line);
            continue;
        }

        if line.trim().is_empty() {
            flush_paragraph(&mut sections, &mut paragraph_lines, &mut paragraph_index);
            sections.push(DocumentSection::raw(line, SectionKind::Separator, None));
            continue;
        }

        paragraph_lines.push(line);
    }

    if !code_lines.is_empty() {
        sections.push(DocumentSection::raw(
            code_lines.join(""),
            SectionKind::Code,
            None,
        ));
    }
    flush_paragraph(&mut sections, &mut paragraph_lines, &mut paragraph_index);
    sections
}

fn split_yaml_frontmatter(text: &str) -> Option<(&str, &str)> {
    let first_line_end = text.find('\n')? + 1;
    let first_line = &text[..first_line_end];
    if !is_yaml_frontmatter_marker(first_line) {
        return None;
    }

    let mut offset = first_line_end;
    for line in lines_with_endings(&text[first_line_end..]) {
        offset += line.len();
        if is_yaml_frontmatter_marker(line) {
            return Some((&text[..offset], &text[offset..]));
        }
    }

    None
}

fn is_yaml_frontmatter_marker(line: &str) -> bool {
    line.trim_end() == "---"
}

/// Iterates over lines of `text`, preserving line endings.
fn lines_with_endings(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
        let (line, remaining) = rest.split_at(end);
        rest = remaining;
        Some(line)
    })
}

/// Returns `(fence_char, fence_length)` when `line` starts a code fence.
fn opening_fence(line: &str) -> Option<(char, usize)> {
    let stripped = line.trim_start_matches([' ', '\t']);
    let fc = stripped.chars().next()?;
    if fc != '`' && fc != '~' {
        return None;
    }
    let count = stripped.chars().take_while(|&c| c == fc).count();
    if count < 3 {
        return None;
    }
    Some((fc, count))
}

/// Returns `true` when `line` closes a code fence opened by `fence_char` × `fence_len`.
fn is_closing_fence(line: &str, fence_char: char, fence_len: usize) -> bool {
    let stripped = line.trim();
    if stripped.is_empty() {
        return false;
    }
    let count = stripped.chars().take_while(|&c| c == fence_char).count();
    if count < fence_len {
        return false;
    }
    stripped[count..].trim().is_empty()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── CJK detection ────────────────────────────────────────────────────────

    #[test]
    fn detects_chinese_text() {
        let text = "这是一段中文文本，包含大量汉字，占据了主要字符。";
        let result = detect_target_language(text, "zh").unwrap();
        assert!(result.target_ratio > 0.5, "ratio={}", result.target_ratio);
        assert_eq!(result.detected_lang.as_deref(), Some("zh"));
    }

    #[test]
    fn english_text_has_low_cjk_ratio() {
        let text = "This is English text with no Chinese characters at all.";
        let result = detect_target_language(text, "zh").unwrap();
        assert!(result.target_ratio < 0.05, "ratio={}", result.target_ratio);
    }

    #[test]
    fn non_zh_target_returns_none() {
        // Only CJK detection is implemented.
        let result = detect_target_language("hello world", "fr");
        assert!(result.is_none());
    }

    #[test]
    fn empty_text_returns_none() {
        assert!(detect_target_language("   \n  ", "zh").is_none());
    }

    #[test]
    fn chinese_family_aliases_support_detection() {
        let text = "这是一段中文文本，包含大量汉字，占据了主要字符。";
        for target in ["zh-Hant", "ZH_tw", "YUE"] {
            let result = detect_target_language(text, target)
                .unwrap_or_else(|| panic!("expected detection support for {target}"));
            assert!(
                result.target_ratio > 0.5,
                "target={target}, ratio={}",
                result.target_ratio
            );
        }
    }

    // ── resolve_target_language ───────────────────────────────────────────────

    #[test]
    fn explicit_target_passes_through() {
        let lang = resolve_target_language("text", "fr", "zh", "en", true);
        assert_eq!(lang, "fr");
    }

    #[test]
    fn auto_routing_zh_source_returns_secondary() {
        let zh_text = "这段文字主要是中文，用于测试语言路由功能。中文内容非常多。";
        let lang = resolve_target_language(zh_text, "en", "zh", "en", false);
        assert_eq!(lang, "en");
    }

    #[test]
    fn auto_routing_english_source_returns_primary() {
        let en_text = "This paragraph is written in English and contains no Chinese characters.";
        let lang = resolve_target_language(en_text, "en", "zh", "en", false);
        // English text has low CJK ratio → target_ratio < 0.60 → return primary (zh)
        assert_eq!(lang, "zh");
    }

    // ── Document section splitting ────────────────────────────────────────────

    #[test]
    fn splits_paragraphs_and_separators() {
        let text = "First paragraph.\n\nSecond paragraph.\n";
        let sections = split_document_sections(text);
        let kinds: Vec<&SectionKind> = sections.iter().map(|s| &s.kind).collect();
        assert!(kinds.contains(&&SectionKind::Paragraph));
        assert!(kinds.contains(&&SectionKind::Separator));
        let paras: Vec<&DocumentSection> = sections
            .iter()
            .filter(|s| s.kind == SectionKind::Paragraph)
            .collect();
        assert_eq!(paras.len(), 2);
    }

    #[test]
    fn isolates_code_blocks() {
        let text = "intro\n\n```rust\nfn main() {}\n```\n\nconclusion\n";
        let sections = split_document_sections(text);
        let kinds: Vec<&SectionKind> = sections.iter().map(|s| &s.kind).collect();
        assert!(kinds.contains(&&SectionKind::Code));
        let code: Vec<_> = sections
            .iter()
            .filter(|s| s.kind == SectionKind::Code)
            .collect();
        assert_eq!(code.len(), 1);
        assert!(code[0].text.contains("fn main()"));
    }

    #[test]
    fn tilde_fence_detected() {
        let text = "~~~python\nprint('hi')\n~~~\n";
        let sections = split_document_sections(text);
        assert!(sections.iter().any(|s| s.kind == SectionKind::Code));
    }

    #[test]
    fn yaml_frontmatter_is_untranslated_code_section() {
        let text = "---\ntitle: Test\n---\n\nBody paragraph.\n";
        let sections = split_document_sections(text);
        assert_eq!(sections[0].kind, SectionKind::Code);
        assert_eq!(sections[0].text, "---\ntitle: Test\n---\n");
        assert!(!sections[0].should_translate);
        assert!(sections
            .iter()
            .any(|s| s.kind == SectionKind::Paragraph && s.text == "Body paragraph.\n"));
    }

    #[test]
    fn indented_yaml_marker_remains_paragraph() {
        let text = " ---\ntitle: Test\n---\n\nBody paragraph.\n";
        let sections = split_document_sections(text);
        assert_eq!(sections[0].kind, SectionKind::Paragraph);
        assert_eq!(sections[0].text, " ---\ntitle: Test\n---\n");
    }

    #[test]
    fn unclosed_document_start_marker_remains_paragraph() {
        let text = "---\nBody paragraph.\n";
        let sections = split_document_sections(text);
        assert_eq!(sections[0].kind, SectionKind::Paragraph);
    }

    #[test]
    fn paragraph_indices_increment() {
        let text = "A\n\nB\n\nC\n";
        let paras: Vec<_> = split_document_sections(text)
            .into_iter()
            .filter(|s| s.kind == SectionKind::Paragraph)
            .collect();
        assert_eq!(paras.len(), 3);
        assert_eq!(paras[0].paragraph_index, Some(1));
        assert_eq!(paras[1].paragraph_index, Some(2));
        assert_eq!(paras[2].paragraph_index, Some(3));
    }

    // ── analyze_document_language ─────────────────────────────────────────────

    #[test]
    fn chinese_paragraphs_marked_as_target() {
        let text = "这是中文段落，含有大量的汉字，比例很高，超过阈值。\n\n这又是另一段中文。\n";
        let plan = analyze_document_language(text, "zh");
        for section in plan
            .sections
            .iter()
            .filter(|s| s.kind == SectionKind::Paragraph)
        {
            assert!(
                section.is_target_language,
                "Expected Chinese paragraph to be flagged as target: {}",
                &section.text[..section.text.len().min(30)]
            );
            assert!(!section.should_translate);
        }
    }

    #[test]
    fn short_code_like_paragraph_is_not_confidently_classified_as_target_language() {
        let plan = analyze_document_language("变量名;", "zh");
        let paragraph = plan
            .sections
            .iter()
            .find(|section| section.kind == SectionKind::Paragraph)
            .unwrap();
        assert!(paragraph.target_ratio.unwrap() > TARGET_PARAGRAPH_RATIO);
        assert!(!paragraph.is_target_language);
        assert!(paragraph.should_translate);
    }

    #[test]
    fn english_paragraphs_not_target_for_zh() {
        let text = "This is plain English text with no Chinese content.\n";
        let plan = analyze_document_language(text, "zh");
        for section in plan
            .sections
            .iter()
            .filter(|s| s.kind == SectionKind::Paragraph)
        {
            assert!(!section.is_target_language);
            assert!(section.should_translate);
        }
    }

    // ── build_document_translation_plan ──────────────────────────────────────

    #[test]
    fn all_paragraphs_marked_for_translation() {
        let text = "Paragraph one.\n\nParagraph two.\n";
        let plan = build_document_translation_plan(text, "zh");
        assert!(plan
            .sections
            .iter()
            .filter(|s| s.kind == SectionKind::Paragraph)
            .all(|s| s.should_translate));
    }

    #[test]
    fn translate_all_paragraphs_method() {
        let text = "Foo.\n\nBar.\n";
        let plan = analyze_document_language(text, "zh").translate_all_paragraphs();
        assert!(plan
            .sections
            .iter()
            .filter(|s| s.kind == SectionKind::Paragraph)
            .all(|s| s.should_translate));
    }

    #[test]
    fn document_plans_preserve_canonical_target_codes() {
        assert_eq!(
            analyze_document_language("text", "zh_tw").target_lang,
            "zh-Hant"
        );
    }
}
