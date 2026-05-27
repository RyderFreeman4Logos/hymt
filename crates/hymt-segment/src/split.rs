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

// ── Markdown-aware block splitting ───────────────────────────────────────────

/// A typed markdown block returned by [`split_markdown_blocks`].
#[derive(Debug, Clone, PartialEq)]
pub enum MarkdownBlock {
    /// Regular paragraph text.
    Normal(String),
    /// Content between ``` or ~~~ fences. Never split, even when oversized.
    FencedCode(String),
    /// Lines beginning with `>`. Split at line boundaries if oversized.
    Blockquote(String),
    /// Lines beginning with `|`. Never split, even when oversized.
    Table(String),
    /// Consecutive list items. May be split at top-level item boundaries.
    List(String),
}

#[cfg(test)]
impl MarkdownBlock {
    pub fn into_string(self) -> String {
        match self {
            Self::Normal(s)
            | Self::FencedCode(s)
            | Self::Blockquote(s)
            | Self::Table(s)
            | Self::List(s) => s,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Normal(s)
            | Self::FencedCode(s)
            | Self::Blockquote(s)
            | Self::Table(s)
            | Self::List(s) => s,
        }
    }
}

/// End-of-line position: index after the `\n`, or `text.len()` if no newline.
fn next_line_end(text: &str, pos: usize) -> usize {
    text[pos..]
        .find('\n')
        .map(|i| pos + i + 1)
        .unwrap_or(text.len())
}

/// The content of the line at `pos`, with the trailing `\r\n` / `\n` stripped.
fn line_at(text: &str, pos: usize) -> &str {
    let end = next_line_end(text, pos);
    let s = &text[pos..end];
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.strip_suffix('\r').unwrap_or(s)
}

fn is_list_item_line(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
        return true;
    }
    if matches!(t, "-" | "*" | "+") {
        return true;
    }
    // Ordered list: one or more digits followed by `.` or `)` then space or end.
    let rest = t.trim_start_matches(|c: char| c.is_ascii_digit());
    if rest.len() < t.len() {
        if let Some(after) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')')) {
            return after.is_empty() || after.starts_with(' ');
        }
    }
    false
}

fn is_list_continuation_line(line: &str) -> bool {
    !line.trim().is_empty() && (line.starts_with("  ") || line.starts_with('\t'))
}

/// Split `text` into markdown-aware atomic blocks, preserving every byte of the
/// original text so that `blocks.map(into_string).join("") == text`.
pub fn split_markdown_blocks(text: &str) -> Vec<MarkdownBlock> {
    let mut blocks: Vec<MarkdownBlock> = Vec::new();
    let mut pos = 0usize;
    let mut normal_start = 0usize;

    // Emit any accumulated normal text as one or more Normal blocks.
    let flush_normal = |blocks: &mut Vec<MarkdownBlock>, slice: &str| {
        for para in split_paragraphs(slice) {
            if !para.is_empty() {
                blocks.push(MarkdownBlock::Normal(para));
            }
        }
    };

    while pos < text.len() {
        let line = line_at(text, pos);
        let line_end = next_line_end(text, pos);
        let trimmed = line.trim_start();

        // ── Fenced code block ─────────────────────────────────────────────
        let fence_opt = if trimmed.starts_with("```") {
            Some("```")
        } else if trimmed.starts_with("~~~") {
            Some("~~~")
        } else {
            None
        };

        if let Some(fence) = fence_opt {
            flush_normal(&mut blocks, &text[normal_start..pos]);
            let block_start = pos;
            let mut search = line_end;
            let closed = loop {
                if search >= text.len() {
                    break false;
                }
                let cl = line_at(text, search);
                let cl_end = next_line_end(text, search);
                if cl.trim_start().starts_with(fence) {
                    search = cl_end;
                    break true;
                }
                search = cl_end;
            };
            let block_end = if closed { search } else { text.len() };
            blocks.push(MarkdownBlock::FencedCode(
                text[block_start..block_end].to_owned(),
            ));
            pos = block_end;
            normal_start = pos;
            continue;
        }

        // ── Blockquote ────────────────────────────────────────────────────
        if trimmed.starts_with('>') {
            flush_normal(&mut blocks, &text[normal_start..pos]);
            let block_start = pos;
            let mut search = line_end;
            loop {
                if search >= text.len() {
                    break;
                }
                let nl = line_at(text, search);
                let nl_end = next_line_end(text, search);
                if nl.trim_start().starts_with('>') {
                    search = nl_end;
                } else if nl.trim().is_empty() {
                    // Include blank line only if the next content line is also `>`.
                    let peek = nl_end;
                    if peek < text.len() && line_at(text, peek).trim_start().starts_with('>') {
                        search = next_line_end(text, peek);
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            blocks.push(MarkdownBlock::Blockquote(
                text[block_start..search].to_owned(),
            ));
            pos = search;
            normal_start = pos;
            continue;
        }

        // ── Table ─────────────────────────────────────────────────────────
        if trimmed.starts_with('|') {
            flush_normal(&mut blocks, &text[normal_start..pos]);
            let block_start = pos;
            let mut search = line_end;
            loop {
                if search >= text.len() {
                    break;
                }
                let nl = line_at(text, search);
                let nl_end = next_line_end(text, search);
                if nl.trim_start().starts_with('|') {
                    search = nl_end;
                } else {
                    break;
                }
            }
            blocks.push(MarkdownBlock::Table(text[block_start..search].to_owned()));
            pos = search;
            normal_start = pos;
            continue;
        }

        // ── List ──────────────────────────────────────────────────────────
        if is_list_item_line(trimmed) {
            flush_normal(&mut blocks, &text[normal_start..pos]);
            let block_start = pos;
            let mut search = line_end;
            let mut last_content_end = line_end;
            loop {
                if search >= text.len() {
                    break;
                }
                let nl = line_at(text, search);
                let nl_end = next_line_end(text, search);
                if is_list_item_line(nl) || is_list_continuation_line(nl) {
                    last_content_end = nl_end;
                    search = nl_end;
                } else if nl.trim().is_empty() {
                    let peek = nl_end;
                    if peek < text.len() {
                        let pl = line_at(text, peek);
                        if is_list_item_line(pl) || is_list_continuation_line(pl) {
                            let pe = next_line_end(text, peek);
                            last_content_end = pe;
                            search = pe;
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            blocks.push(MarkdownBlock::List(
                text[block_start..last_content_end].to_owned(),
            ));
            pos = last_content_end;
            normal_start = pos;
            continue;
        }

        pos = line_end;
    }

    // Flush any trailing normal text.
    if normal_start < text.len() {
        flush_normal(&mut blocks, &text[normal_start..]);
    }

    blocks
}

/// Split a list block into individual top-level items.
pub fn split_list_items(text: &str) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    let mut pos = 0usize;
    let mut item_start = 0usize;

    while pos < text.len() {
        let line = line_at(text, pos);
        let line_end = next_line_end(text, pos);
        // New top-level item starts only after we've accumulated at least one line.
        if is_list_item_line(line) && pos > item_start {
            let item = text[item_start..pos].to_owned();
            if !item.trim().is_empty() {
                items.push(item);
            }
            item_start = pos;
        }
        pos = line_end;
    }

    if item_start < text.len() {
        let item = text[item_start..].to_owned();
        if !item.trim().is_empty() {
            items.push(item);
        }
    }

    items
}
