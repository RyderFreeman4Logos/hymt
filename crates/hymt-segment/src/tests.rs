use crate::{
    split::{
        is_cjk_character, split_clauses, split_list_items, split_markdown_blocks, split_paragraphs,
        split_sentences, MarkdownBlock,
    },
    Segmenter,
};
use hymt_core::model_profile::ModelProfile;

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

#[test]
fn tokenizer_cache_is_scoped_to_the_selected_model_profile() {
    let small = crate::tokenizer_path(ModelProfile::HyMt2_1_8b).unwrap();
    let medium = crate::tokenizer_path(ModelProfile::HyMt2_7b).unwrap();
    let large = crate::tokenizer_path(ModelProfile::HyMt2_30bA3b).unwrap();

    assert!(small.ends_with("hy_mt2_1_8b/tokenizer.json"));
    assert!(medium.ends_with("hy_mt2_7b/tokenizer.json"));
    assert!(large.ends_with("hy_mt2_30b_a3b/tokenizer.json"));
    assert_ne!(small, medium);
    assert_ne!(medium, large);
    assert_eq!(crate::tokenizer_path(ModelProfile::Generic), None);
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

// ── markdown-aware block splitting ───────────────────────────────────────────

#[test]
fn blockquote_stays_together() {
    let text = "> Line one.\n> Line two.\n> Line three.";
    let blocks = split_markdown_blocks(text);
    assert_eq!(blocks.len(), 1);
    assert!(matches!(&blocks[0], MarkdownBlock::Blockquote(_)));
    assert_eq!(blocks[0].as_str(), text);
}

#[test]
fn nested_blockquote_stays_together() {
    let text = "> Outer.\n>> Nested deeper.\n> Back to outer.";
    let blocks = split_markdown_blocks(text);
    assert_eq!(blocks.len(), 1);
    assert!(matches!(&blocks[0], MarkdownBlock::Blockquote(_)));
}

#[test]
fn fenced_code_block_stays_together() {
    let text = "```rust\nfn hello() {\n    println!(\"hi\");\n}\n```";
    let blocks = split_markdown_blocks(text);
    assert_eq!(blocks.len(), 1);
    assert!(matches!(&blocks[0], MarkdownBlock::FencedCode(_)));
    assert_eq!(blocks[0].as_str(), text);
}

#[test]
fn table_stays_together() {
    let text = "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
    let blocks = split_markdown_blocks(text);
    assert_eq!(blocks.len(), 1);
    assert!(matches!(&blocks[0], MarkdownBlock::Table(_)));
    assert_eq!(blocks[0].as_str(), text);
}

#[test]
fn mixed_content_splits_correctly() {
    let text = "Intro paragraph.\n\n> Blockquote line.\n\nOutro paragraph.";
    let blocks = split_markdown_blocks(text);
    assert_eq!(blocks.len(), 3, "blocks: {blocks:?}");
    assert!(matches!(&blocks[0], MarkdownBlock::Normal(_)));
    assert!(matches!(&blocks[1], MarkdownBlock::Blockquote(_)));
    assert!(matches!(&blocks[2], MarkdownBlock::Normal(_)));
}

#[test]
fn split_markdown_blocks_reassembles_to_original() {
    let text = "Intro.\n\n> Quote block.\n\nCode:\n\n```\nlet x = 1;\n```\n\nOutro.";
    let blocks = split_markdown_blocks(text);
    let joined: String = blocks.into_iter().map(|b| b.into_string()).collect();
    assert_eq!(joined, text);
}

#[test]
fn split_markdown_blocks_plain_text_unchanged() {
    let text = "First paragraph.\n\nSecond paragraph.";
    let blocks = split_markdown_blocks(text);
    assert!(blocks.iter().all(|b| matches!(b, MarkdownBlock::Normal(_))));
    let joined: String = blocks.into_iter().map(|b| b.into_string()).collect();
    assert_eq!(joined, text);
}

#[test]
fn list_block_detected() {
    let text = "- item one\n- item two\n- item three";
    let blocks = split_markdown_blocks(text);
    assert_eq!(blocks.len(), 1);
    assert!(matches!(&blocks[0], MarkdownBlock::List(_)));
}

#[test]
fn split_list_items_basic() {
    let text = "- item one\n- item two\n- item three";
    let items = split_list_items(text);
    assert_eq!(items.len(), 3);
    assert!(items[0].contains("item one"));
    assert!(items[1].contains("item two"));
    assert!(items[2].contains("item three"));
}

// ── segment() with markdown blocks ───────────────────────────────────────────

#[test]
fn segment_fenced_code_fails_closed_when_oversized() {
    // A code block with many lines exceeding a tiny token budget must never be
    // emitted as an over-budget atomic request.
    let code = "```\n".to_owned() + &"x = 1;\n".repeat(20) + "```\n";
    let error = seg().segment(&code, 5).unwrap_err();
    assert!(matches!(
        error,
        crate::SegmentError::ProtectedBlockTooLarge { .. }
    ));
}

#[test]
fn segment_table_fails_closed_when_oversized() {
    let table = "| A | B |\n|---|---|\n".to_owned() + &"| x | y |\n".repeat(10);
    let error = seg().segment(&table, 5).unwrap_err();
    assert!(matches!(
        error,
        crate::SegmentError::ProtectedBlockTooLarge { .. }
    ));
}

#[test]
fn segment_blockquote_stays_together_within_budget() {
    let text = "> Short quote.";
    let result = seg().segment(text, 100).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], text);
}

#[test]
fn segment_oversized_blockquote_preserves_prefix() {
    // Build a large blockquote that must be split.
    let bq: String = (1..=30).map(|i| format!("> Line {i}.\n")).collect();
    let result = seg().segment(&bq, 5).unwrap();
    assert!(
        result.len() > 1,
        "oversized blockquote should produce multiple segments"
    );
    for seg_item in &result {
        let first = seg_item.lines().next().unwrap_or("");
        assert!(
            first.trim_start().starts_with('>'),
            "every blockquote segment must start with '>': {seg_item:?}"
        );
    }
}

#[test]
fn segment_markdown_reassembles_to_original() {
    let text = "Intro.\n\n> Quote block.\n\nCode:\n\n```\nlet x = 1;\n```\n\nOutro.";
    let result = seg().segment(text, 5).unwrap();
    let joined: String = result.join("");
    assert_eq!(joined, text);
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
