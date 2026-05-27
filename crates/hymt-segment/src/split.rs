// Splitting logic ported from segment.py: paragraph → sentence → clause → word/char hierarchy.

const CJK_SENTENCE_ENDERS: &[char] = &['。', '！', '？'];
const EN_SENTENCE_ENDERS: &[char] = &['.', '!', '?'];
const CLAUSE_ENDERS: &[char] = &['，', ',', '、', '；', ';', '：', ':'];
const TRAILING_CLOSERS: &[char] = &[
    '"', '\'', '\u{2019}', '\u{201D}', '「', '」', '』', '）', ')', ']', '】', '》', '〉',
];

pub fn split_paragraphs(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut paragraphs = Vec::new();
    let mut start = 0;
    let mut i = 0;

    while i < n {
        if chars[i] == '\n' {
            let mut j = i + 1;
            while j < n && chars[j] != '\n' && chars[j].is_whitespace() {
                j += 1;
            }
            if j < n && chars[j] == '\n' {
                let seg: String = chars[start..=j].iter().collect();
                if !seg.is_empty() {
                    paragraphs.push(seg);
                }
                start = j + 1;
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }

    if start < n {
        let remaining: String = chars[start..].iter().collect();
        if !remaining.is_empty() {
            paragraphs.push(remaining);
        }
    }

    paragraphs
}

pub fn split_sentences(text: &str) -> Vec<String> {
    split_on_boundaries(text, sentence_boundary_end)
}

pub fn split_clauses(text: &str) -> Vec<String> {
    split_on_boundaries(text, clause_boundary_end)
}

/// Split text on whitespace boundaries, preserving whitespace tokens (matches Python \s+ capture split).
pub fn split_on_whitespace(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut prev_ws = false;

    for ch in text.chars() {
        let is_ws = ch.is_whitespace();
        if !current.is_empty() && is_ws != prev_ws {
            parts.push(std::mem::take(&mut current));
        }
        current.push(ch);
        prev_ws = is_ws;
    }

    if !current.is_empty() {
        parts.push(current);
    }

    parts.into_iter().filter(|p| !p.is_empty()).collect()
}

fn split_on_boundaries<F>(text: &str, boundary_end_at: F) -> Vec<String>
where
    F: Fn(&[char], usize) -> Option<usize>,
{
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut parts = Vec::new();
    let mut start = 0;
    let mut i = 0;

    while i < n {
        if let Some(boundary_end) = boundary_end_at(&chars, i) {
            let split_at = consume_trailing_closers(&chars, boundary_end);
            let part: String = chars[start..split_at].iter().collect();
            parts.push(part);
            start = split_at;
            i = split_at;
        } else {
            i += 1;
        }
    }

    if start < n {
        let remaining: String = chars[start..].iter().collect();
        if !remaining.is_empty() {
            parts.push(remaining);
        }
    }

    parts.into_iter().filter(|p| !p.is_empty()).collect()
}

fn sentence_boundary_end(chars: &[char], index: usize) -> Option<usize> {
    let ch = chars[index];

    if ch == '…' {
        return Some(consume_consecutive(chars, index, '…'));
    }

    if CJK_SENTENCE_ENDERS.contains(&ch) {
        return Some(consume_sentence_enders(chars, index));
    }

    if !EN_SENTENCE_ENDERS.contains(&ch) {
        return None;
    }

    if ch == '.' && is_decimal_point(chars, index) {
        return None;
    }

    let boundary_end = consume_sentence_enders(chars, index);
    let lookahead = consume_trailing_closers(chars, boundary_end);

    if lookahead < chars.len()
        && boundary_end > 0
        && chars[boundary_end - 1] == '.'
        && looks_like_abbreviation(chars, boundary_end - 1)
    {
        return None;
    }

    if starts_sentence_after_boundary(chars, lookahead) {
        Some(boundary_end)
    } else {
        None
    }
}

fn clause_boundary_end(chars: &[char], index: usize) -> Option<usize> {
    if CLAUSE_ENDERS.contains(&chars[index]) {
        Some(index + 1)
    } else {
        None
    }
}

fn consume_sentence_enders(chars: &[char], index: usize) -> usize {
    let mut end = index;
    while end < chars.len() {
        let ch = chars[end];
        if ch == '…' {
            end = consume_consecutive(chars, end, '…');
        } else if CJK_SENTENCE_ENDERS.contains(&ch) || EN_SENTENCE_ENDERS.contains(&ch) {
            end += 1;
        } else {
            break;
        }
    }
    end
}

fn consume_consecutive(chars: &[char], index: usize, target: char) -> usize {
    let mut end = index;
    while end < chars.len() && chars[end] == target {
        end += 1;
    }
    end
}

fn consume_trailing_closers(chars: &[char], index: usize) -> usize {
    let mut end = index;
    while end < chars.len() && TRAILING_CLOSERS.contains(&chars[end]) {
        end += 1;
    }
    end
}

fn starts_sentence_after_boundary(chars: &[char], index: usize) -> bool {
    if index >= chars.len() {
        return true;
    }

    let mut lookahead = index;
    while lookahead < chars.len() && chars[lookahead].is_whitespace() {
        lookahead += 1;
    }

    if lookahead == index {
        return false;
    }

    if lookahead >= chars.len() {
        return true;
    }

    let next = chars[lookahead];
    next.is_uppercase() || is_cjk_character(next)
}

fn is_decimal_point(chars: &[char], index: usize) -> bool {
    index > 0
        && index + 1 < chars.len()
        && chars[index - 1].is_ascii_digit()
        && chars[index + 1].is_ascii_digit()
}

fn looks_like_abbreviation(chars: &[char], index: usize) -> bool {
    let mut start = index;
    while start > 0 && (chars[start - 1].is_alphabetic() || chars[start - 1] == '.') {
        start -= 1;
    }

    let candidate: String = chars[start..index].iter().collect();
    if candidate.is_empty() || !candidate.chars().any(|c| c.is_alphabetic()) {
        return false;
    }

    let lower = candidate.to_lowercase();
    if is_common_abbreviation(&lower) {
        return true;
    }

    let parts: Vec<&str> = candidate.split('.').filter(|p| !p.is_empty()).collect();
    parts.len() > 1
        && parts
            .iter()
            .all(|p| p.len() == 1 && p.chars().next().map(|c| c.is_alphabetic()).unwrap_or(false))
}

fn is_common_abbreviation(s: &str) -> bool {
    matches!(
        s,
        "dr" | "mr"
            | "mrs"
            | "ms"
            | "prof"
            | "sr"
            | "jr"
            | "st"
            | "vs"
            | "etc"
            | "e.g"
            | "i.e"
            | "fig"
            | "no"
    )
}

pub fn is_cjk_character(ch: char) -> bool {
    let cp = ch as u32;
    (0x3400..=0x4DBF).contains(&cp)
        || (0x4E00..=0x9FFF).contains(&cp)
        || (0x3040..=0x30FF).contains(&cp)
        || (0xAC00..=0xD7AF).contains(&cp)
}
