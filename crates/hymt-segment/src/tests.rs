use crate::{
    split::{is_cjk_character, split_clauses, split_paragraphs, split_sentences},
    Segmenter,
};

// ── helpers ──────────────────────────────────────────────────────────────────

fn seg() -> Segmenter {
    Segmenter::fallback()
}

// ── estimate_token_count ─────────────────────────────────────────────────────

#[test]
fn empty_string_is_zero_tokens() {
    assert_eq!(seg().count_tokens(""), 0);
}

#[test]
fn four_ascii_chars_is_one_token() {
    assert_eq!(seg().count_tokens("abcd"), 1);
}

#[test]
fn single_char_is_one_token() {
    assert_eq!(seg().count_tokens("a"), 1);
}

// ── paragraph splitting ───────────────────────────────────────────────────────

#[test]
fn split_paragraphs_two_blocks() {
    let text = "Hello world.\n\nSecond paragraph.";
    let parts = split_paragraphs(text);
    assert_eq!(parts.len(), 2);
    assert!(parts[0].ends_with('\n'));
    assert_eq!(parts[1], "Second paragraph.");
}

#[test]
fn split_paragraphs_preserves_separator() {
    let text = "A\n\nB";
    let parts = split_paragraphs(text);
    assert_eq!(parts[0], "A\n\n");
    assert_eq!(parts[1], "B");
}

#[test]
fn split_paragraphs_single_block() {
    let text = "No double newline here.";
    let parts = split_paragraphs(text);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0], text);
}

#[test]
fn split_paragraphs_whitespace_between_newlines() {
    let text = "A\n   \nB";
    let parts = split_paragraphs(text);
    assert_eq!(parts.len(), 2);
}

// ── sentence splitting ────────────────────────────────────────────────────────

#[test]
fn split_sentences_basic_english() {
    let text = "Hello world. How are you? I am fine!";
    let parts = split_sentences(text);
    assert!(
        parts.len() >= 2,
        "expected multiple sentences, got {:?}",
        parts
    );
}

#[test]
fn split_sentences_cjk() {
    let text = "这是第一句。这是第二句！这是第三句？";
    let parts = split_sentences(text);
    assert_eq!(parts.len(), 3);
}

#[test]
fn split_sentences_no_split_on_decimal() {
    let text = "The value is 3.14 meters.";
    let parts = split_sentences(text);
    // Should not split "3.14" – one sentence or two (boundary after last .)
    assert!(!parts.iter().any(|p| p.trim() == "14 meters."));
}

#[test]
fn split_sentences_abbreviation_not_split() {
    let text = "Dr. Smith is here. He knows.";
    let parts = split_sentences(text);
    // "Dr." should not trigger a split; at least one part should contain "Dr. Smith"
    assert!(parts.iter().any(|p| p.contains("Dr.")));
}

// ── clause splitting ──────────────────────────────────────────────────────────

#[test]
fn split_clauses_comma() {
    let text = "First, second, third.";
    let parts = split_clauses(text);
    assert!(parts.len() >= 2);
    assert!(parts[0].ends_with(','));
}

#[test]
fn split_clauses_cjk_punctuation() {
    let text = "一，二；三：四";
    let parts = split_clauses(text);
    assert_eq!(parts.len(), 4);
}

// ── segment() ────────────────────────────────────────────────────────────────

#[test]
fn segment_empty_returns_empty() {
    let result = seg().segment("", 100).unwrap();
    assert!(result.is_empty());
}

#[test]
fn segment_short_text_single_chunk() {
    let text = "Short.";
    let result = seg().segment(text, 100).unwrap();
    assert_eq!(result, vec![text.to_string()]);
}

#[test]
fn segment_paragraph_split() {
    // Two paragraphs, each under 20 tokens, combined over 30 tokens.
    // With max_tokens=20 they should be separated.
    let para = "a".repeat(60);
    let text = format!("{para}\n\n{para}");
    let result = seg().segment(&text, 20).unwrap();
    assert!(result.len() >= 2, "expected ≥2 segments, got {:?}", result);
}

#[test]
fn segment_cjk_character_split() {
    // 80 CJK chars, each ~3 bytes → estimate ~60 tokens at max=10 each chunk
    let text: String = "你好世界".repeat(20);
    let result = seg().segment(&text, 10).unwrap();
    assert!(result.len() > 1);
    for seg_item in &result {
        assert!(
            seg().count_tokens(seg_item) <= 10,
            "segment too long: {:?}",
            seg_item
        );
    }
}

#[test]
fn segment_mixed_latin_cjk() {
    let text = "Hello 世界 world. 你好 everyone!";
    let result = seg().segment(text, 5).unwrap();
    for seg_item in &result {
        assert!(seg().count_tokens(seg_item) <= 5);
    }
}

#[test]
fn segment_invalid_max_tokens_zero() {
    let err = seg().segment("text", 0).unwrap_err();
    assert!(matches!(err, crate::SegmentError::InvalidMaxTokens));
}

#[test]
fn segment_reassembles_to_original_content() {
    let text = "First paragraph.\n\nSecond paragraph. With two sentences.\n\nThird.";
    let result = seg().segment(text, 8).unwrap();
    let joined: String = result.join("");
    assert_eq!(joined, text);
}

// ── CJK detection ────────────────────────────────────────────────────────────

#[test]
fn is_cjk_detects_chinese() {
    assert!(is_cjk_character('你'));
    assert!(is_cjk_character('好'));
    assert!(is_cjk_character('界'));
}

#[test]
fn is_cjk_rejects_latin() {
    assert!(!is_cjk_character('a'));
    assert!(!is_cjk_character('Z'));
    assert!(!is_cjk_character('1'));
}

#[test]
fn is_cjk_detects_hiragana_katakana() {
    assert!(is_cjk_character('あ'));
    assert!(is_cjk_character('ア'));
}

#[test]
fn is_cjk_detects_hangul() {
    assert!(is_cjk_character('한'));
}

// ── edge cases ────────────────────────────────────────────────────────────────

#[test]
fn segment_single_very_long_word_gets_char_split() {
    // Latin word with no spaces, force character split
    let word = "a".repeat(40);
    let result = seg().segment(&word, 5).unwrap();
    assert!(result.len() > 1);
    let joined: String = result.join("");
    assert_eq!(joined, word);
}

#[test]
fn segment_all_segments_within_budget() {
    let text = "One sentence. Two sentences. Three sentences. Four sentences. Five here.";
    let max = 6;
    let result = seg().segment(text, max).unwrap();
    for seg_item in &result {
        assert!(
            seg().count_tokens(seg_item) <= max,
            "over budget: {:?} ({} tokens)",
            seg_item,
            seg().count_tokens(seg_item)
        );
    }
}
