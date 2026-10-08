use super::{DocumentLanguagePlan, SectionKind, Segmenter};
use pulldown_cmark::{Event, LinkType, Parser, Tag};
use std::ops::Range;

// Keep parser-resolved identity separate from the translated display label.
// Parser IDs retain source escapes (only whitespace is normalized); do not
// escape them again or normalize them independently of the definition lookup.
// The existing explicit-reference ownership/reconstruction path handles both.
fn make_short_references_explicit(plan: &mut DocumentLanguagePlan) {
    let source: String = plan
        .sections
        .iter()
        .map(|section| section.text.as_str())
        .collect();
    let mut edits = Vec::new();
    for (event, node) in Parser::new(&source).into_offset_iter() {
        if let Event::Start(Tag::Link { link_type, id, .. } | Tag::Image { link_type, id, .. }) =
            event
        {
            if matches!(link_type, LinkType::Shortcut | LinkType::Collapsed) {
                // pulldown's collapsed node ends before its trailing empty ID.
                let end = node.end
                    + if link_type == LinkType::Collapsed && source[node.end..].starts_with("[]") {
                        2
                    } else {
                        0
                    };
                edits.push((node.clone(), node.end..end, format!("[{id}]")));
            }
        }
    }
    edits.sort_by_key(|(_, range, _)| range.start);
    let mut index = 0;
    let mut offset = 0;
    for section in &mut plan.sections {
        let end = offset + section.text.len();
        let first = index;
        while index < edits.len() && edits[index].1.start <= end {
            index += 1;
        }
        if section.should_translate && section.kind == SectionKind::Paragraph {
            let mut normalized = String::with_capacity(section.text.len());
            let mut cursor = 0;
            for (node, range, id) in &edits[first..index] {
                if node.start >= offset && range.end <= end {
                    normalized.push_str(&section.text[cursor..range.start - offset]);
                    normalized.push_str(id);
                    cursor = range.end - offset;
                }
            }
            if cursor > 0 {
                normalized.push_str(&section.text[cursor..]);
                section.text = normalized;
            }
        }
        offset = end;
    }
}

// Only structural unions may be coalesced; owner and opaque containment must
// continue to refer to a single original interval.
fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = merged.last_mut() {
            if range.start <= previous.end {
                previous.end = previous.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    merged
}

fn clipped_ranges(
    ranges: &[Range<usize>],
    cursor: &mut usize,
    section: Range<usize>,
) -> Vec<Range<usize>> {
    while *cursor < ranges.len() && ranges[*cursor].end <= section.start {
        #[cfg(test)]
        super::tests::ownership_interval_tests::visit(2);
        *cursor += 1;
    }
    ranges[*cursor..]
        .iter()
        .take_while(|range| {
            #[cfg(test)]
            super::tests::ownership_interval_tests::visit(2);
            range.start < section.end
        })
        .filter_map(|range| {
            let left = range.start.max(section.start);
            let right = range.end.min(section.end);
            (left < right).then(|| left - section.start..right - section.start)
        })
        .collect()
}

/// Keep oversized atomic Markdown blocks out of model input while preserving
/// translatable text around them as independently segmentable sections.
pub(super) fn split_oversized_protected_blocks(
    doc_plan: &mut DocumentLanguagePlan,
    segmenter: &Segmenter,
    max_tokens: usize,
) {
    make_short_references_explicit(doc_plan);
    // Reference definitions and Setext markers can live in another section.
    // Parse once in complete-document context, then intersect source byte ranges.
    let source: String = doc_plan
        .sections
        .iter()
        .map(|section| section.text.as_str())
        .collect();
    let structural_ranges = merge_ranges(protected_markdown_structure_ranges(&source));
    let mut structural_cursor = 0;
    let mut offset = 0;
    let mut split_sections = Vec::with_capacity(doc_plan.sections.len());
    for section in std::mem::take(&mut doc_plan.sections) {
        let start = offset;
        offset += section.text.len();
        let structural_ranges =
            clipped_ranges(&structural_ranges, &mut structural_cursor, start..offset);
        if section.kind != SectionKind::Paragraph || !section.should_translate {
            split_sections.push(section);
            continue;
        }

        let oversized_ranges: Vec<_> = protected_markdown_block_ranges(&section.text)
            .into_iter()
            .filter(|range| {
                let tokens = segmenter.count_tokens(&section.text[range.clone()]);
                if tokens <= max_tokens {
                    return false;
                }
                eprintln!(
                    "Warning: preserved protected block untranslated ({tokens} tokens exceeds segment limit {max_tokens})"
                );
                true
            })
            .collect();
        if oversized_ranges.is_empty() && structural_ranges.is_empty() {
            split_sections.push(section);
            continue;
        }

        let mut pieces: Vec<Range<usize>> = oversized_ranges;
        pieces.extend(structural_ranges);
        let merged = merge_ranges(pieces);
        let mut cursor = 0;
        for range in merged {
            if range.start < cursor {
                continue;
            }
            if cursor < range.start {
                let mut before = section.clone();
                before.text = section.text[cursor..range.start].to_owned();
                split_sections.push(before);
            }
            let mut protected = section.clone();
            protected.text = section.text[range.clone()].to_owned();
            protected.should_translate = false;
            split_sections.push(protected);
            cursor = range.end;
        }
        if cursor < section.text.len() {
            let mut after = section;
            after.text = after.text[cursor..].to_owned();
            split_sections.push(after);
        }
    }
    doc_plan.sections = split_sections;
}

pub(super) fn protected_markdown_block_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut offset = 0;
    let lines: Vec<_> = text
        .split_inclusive('\n')
        .map(|line| {
            let start = offset;
            offset += line.len();
            (start, offset, line.trim_end_matches(['\r', '\n']))
        })
        .collect();
    // Reverse skyline: a nearer, wider closer dominates every later narrower
    // closer. Descending widths permit a binary lookup without suffix rescans.
    let mut closers: Vec<(usize, usize)> = Vec::new();
    let mut next_closer = vec![None; lines.len()];
    for index in (0..lines.len()).rev() {
        #[cfg(test)]
        super::tests::ownership_interval_tests::visit(3);
        let line = lines[index].2.trim_start();
        if let Some(width) = opening_fence_width(line) {
            let count = closers.partition_point(|(candidate, _)| {
                #[cfg(test)]
                super::tests::ownership_interval_tests::visit(3);
                *candidate >= width
            });
            if count > 0 {
                next_closer[index] = Some(closers[count - 1].1);
            }
            if is_closing_fence(line, width) {
                while closers
                    .last()
                    .is_some_and(|(candidate, _)| *candidate <= width)
                {
                    closers.pop();
                }
                closers.push((width, index));
            }
        }
    }
    let mut ranges = Vec::new();
    let mut line_index = 0;
    while line_index < lines.len() {
        #[cfg(test)]
        super::tests::ownership_interval_tests::visit(3);
        if let Some(closing_index) = next_closer[line_index] {
            ranges.push(lines[line_index].0..lines[closing_index].1);
            line_index = closing_index + 1;
            continue;
        }

        if line_index + 2 < lines.len()
            && is_markdown_table_line(lines[line_index].2)
            && is_markdown_table_separator(lines[line_index + 1].2)
            && is_markdown_table_line(lines[line_index + 2].2)
        {
            let mut end_index = line_index + 3;
            while end_index < lines.len() && is_markdown_table_line(lines[end_index].2) {
                end_index += 1;
            }
            ranges.push(lines[line_index].0..lines[end_index - 1].1);
            line_index = end_index;
            continue;
        }

        line_index += 1;
    }
    ranges
}

/// Translate literal inline text leaves; keep their complete syntax source-owned.
/// Parser byte ranges preserve nested delimiters without inventing Markdown grammar.
fn protected_markdown_structure_ranges(text: &str) -> Vec<Range<usize>> {
    let parser = Parser::new(text);
    let mut owned: Vec<_> = parser
        .reference_definitions()
        .iter()
        .map(|(_, definition)| definition.span.clone())
        .collect();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b'\\' && bytes[index + 1].is_ascii_punctuation() {
            owned.push(index..index + 2);
            index += 2;
        } else {
            index += 1;
        }
    }
    let mut opaque = owned.clone();
    let mut editable = Vec::new();
    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(
                Tag::Heading { .. }
                | Tag::Emphasis
                | Tag::Strong
                | Tag::Strikethrough
                | Tag::BlockQuote(_),
            ) => owned.push(range),
            Event::Start(Tag::CodeBlock(_))
            | Event::Code(_)
            | Event::Html(_)
            | Event::InlineHtml(_) => {
                opaque.push(range.clone());
                owned.push(range);
            }
            Event::Start(Tag::Link { link_type, .. } | Tag::Image { link_type, .. }) => {
                if matches!(
                    link_type,
                    pulldown_cmark::LinkType::Autolink
                        | pulldown_cmark::LinkType::Email
                        | pulldown_cmark::LinkType::Shortcut
                        | pulldown_cmark::LinkType::Collapsed
                ) {
                    opaque.push(range.clone());
                }
                owned.push(range);
            }
            Event::Text(content) if text[range.clone()] == *content => {
                // Flanking whitespace belongs to syntax, not trimmed model text.
                let literal = &text[range.clone()];
                let start = range.start + literal.len() - literal.trim_start().len();
                let end = range.start + literal.trim_end().len();
                if start < end {
                    editable.push(start..end);
                }
            }
            _ => {}
        }
    }
    exclude_opaque(&mut editable, opaque);
    owned_gaps(text, owned, &editable)
}

fn exclude_opaque(editable: &mut Vec<Range<usize>>, mut opaque: Vec<Range<usize>>) {
    editable.sort_by_key(|range| range.start);
    opaque.sort_by_key(|range| range.start);
    let mut opaque_cursor = 0;
    let mut opaque_end = 0;
    editable.retain(|range| {
        while opaque_cursor < opaque.len() && opaque[opaque_cursor].start <= range.start {
            #[cfg(test)]
            super::tests::ownership_interval_tests::visit(1);
            opaque_end = opaque_end.max(opaque[opaque_cursor].end);
            opaque_cursor += 1;
        }
        range.end > opaque_end
    });
}

fn owned_gaps(
    text: &str,
    mut owned: Vec<Range<usize>>,
    editable: &[Range<usize>],
) -> Vec<Range<usize>> {
    for node in &mut owned {
        // Syntax owns adjacent whitespace too: losing a reference definition's
        // final newline or delimiter flanking space changes the parse.
        node.start = text[..node.start].trim_end().len();
        node.end = text.len() - text[node.end..].trim_start().len();
    }
    owned.sort_by_key(|node| node.start);
    let mut protected = Vec::new();
    let mut first = 0;
    for node in owned {
        while first < editable.len() && editable[first].start < node.start {
            #[cfg(test)]
            super::tests::ownership_interval_tests::visit(0);
            first += 1;
        }
        let mut cursor = node.start;
        for content in editable[first..].iter().take_while(|content| {
            #[cfg(test)]
            super::tests::ownership_interval_tests::visit(0);
            content.start < node.end
        }) {
            if content.start < node.start || content.end > node.end {
                continue;
            }
            if cursor < content.start {
                protected.push(cursor..content.start);
            }
            cursor = content.end;
        }
        if cursor < node.end {
            protected.push(cursor..node.end);
        }
    }
    protected
}

#[cfg(test)]
mod tests;

fn opening_fence_width(line: &str) -> Option<usize> {
    let width = line
        .chars()
        .take_while(|character| *character == '`')
        .count();
    (width >= 3).then_some(width)
}

fn is_closing_fence(line: &str, opening_width: usize) -> bool {
    let line = line.trim_start();
    let width = line
        .chars()
        .take_while(|character| *character == '`')
        .count();
    width >= opening_width && line[width..].trim().is_empty()
}

fn is_markdown_table_line(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with('|') && line[1..].contains('|')
}

fn is_markdown_table_separator(line: &str) -> bool {
    let line = line.trim();
    if !line.starts_with('|') || !line.ends_with('|') {
        return false;
    }
    let cells: Vec<_> = line[1..line.len() - 1].split('|').collect();
    cells.len() >= 2
        && cells.iter().all(|cell| {
            let marker = cell.trim().trim_matches(':');
            marker.len() >= 3 && marker.bytes().all(|byte| byte == b'-')
        })
}
