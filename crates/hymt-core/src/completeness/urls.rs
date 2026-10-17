//! Source URL identity bounded by parser-owned Markdown spans.

use std::collections::HashSet;

pub(super) fn urls(text: &str) -> HashSet<String> {
    preserved_url_ranges(text)
        .into_iter()
        .map(|range| text[range].to_owned())
        .collect()
}

/// Source byte ranges of immutable URLs, using the same grammar as completeness
/// validation. Markdown boundaries belong to formatting or link labels, not URLs.
pub fn preserved_url_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    preserved_url_ranges_with_scan(text, |_, _| {})
}

#[derive(Clone, Copy)]
enum UrlScanStep {
    Scheme,
    Lexical,
    Boundary,
    Delimiter,
}

fn preserved_url_ranges_with_scan(
    text: &str,
    mut scanned: impl FnMut(UrlScanStep, usize),
) -> Vec<std::ops::Range<usize>> {
    let boundaries = markdown_boundaries(text, &mut scanned);
    scan_url_ranges(text, &boundaries, &mut scanned, |_| {})
}

fn scan_url_ranges(
    text: &str,
    boundaries: &[(usize, Option<usize>)],
    scanned: &mut impl FnMut(UrlScanStep, usize),
    mut candidate_end: impl FnMut(usize),
) -> Vec<std::ops::Range<usize>> {
    let mut urls = Vec::new();
    let mut offset = 0;
    let mut boundary = 0;
    while let Some(start) = earliest_url_scheme_start(&text[offset..], scanned) {
        let start = offset + start;
        while boundary < boundaries.len() && boundaries[boundary].0 <= start {
            scanned(UrlScanStep::Boundary, 1);
            boundary += 1;
        }
        let mut end = text.len();
        let mut retained_end = start;
        // Each kind has its own balance; accepted URLs need not be properly nested.
        let mut balances = [0usize; 4];
        'candidate: for (relative, character) in text[start..].char_indices() {
            let index = start + relative;
            while boundary < boundaries.len() && boundaries[boundary].0 <= index {
                scanned(UrlScanStep::Boundary, 1);
                let (owner_end, opening) = boundaries[boundary];
                if owner_end > start && opening.is_none_or(|opening| opening < start) {
                    end = owner_end;
                    break 'candidate;
                }
                boundary += 1;
            }
            scanned(UrlScanStep::Lexical, 1);
            if character.is_whitespace() || is_fullwidth_url_punctuation(character) {
                end = index;
                break;
            }
            let delimiter = match character {
                '(' | ')' => Some((0, character == ')')),
                '[' | ']' => Some((1, character == ']')),
                '{' | '}' => Some((2, character == '}')),
                '<' | '>' => Some((3, character == '>')),
                _ => None,
            };
            if let Some((kind, closing)) = delimiter {
                scanned(UrlScanStep::Delimiter, 1);
                if closing {
                    if balances[kind] == 0 {
                        end = index;
                        break;
                    }
                    balances[kind] -= 1;
                } else {
                    balances[kind] += 1;
                }
            }
            // Trailing punctuation is consumed; later URL bytes can retain it.
            if !matches!(character, ',' | '.' | ';' | ':' | '!' | '?') {
                retained_end = index + character.len_utf8();
            }
        }
        if retained_end > start {
            urls.push(start..retained_end);
        }
        candidate_end(end);
        offset = end;
    }
    urls
}

fn markdown_boundaries(
    text: &str,
    scanned: &mut impl FnMut(UrlScanStep, usize),
) -> Vec<(usize, Option<usize>)> {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let mut boundaries = Vec::new();
    let mut inline = Vec::new();
    let mut paragraph = false;
    let mut link_depth = 0;
    for (event, range) in Parser::new(text).into_offset_iter() {
        match event {
            Event::Start(Tag::Paragraph) => paragraph = true,
            Event::End(TagEnd::Paragraph) => paragraph = false,
            Event::Start(Tag::Link { .. } | Tag::Image { .. }) => {
                boundaries.push((range.start, None));
                link_depth += 1;
            }
            Event::End(TagEnd::Link | TagEnd::Image) => link_depth -= 1,
            Event::Code(_) => {
                let ticks = text[range.clone()]
                    .bytes()
                    .take_while(|b| *b == b'`')
                    .count();
                boundaries.extend([(range.start, None), (range.end - ticks, None)]);
            }
            Event::Html(_) | Event::InlineHtml(_) => boundaries.push((range.start, None)),
            // Match the translator's source-owned table-like paragraph layout.
            // Only literal Text owns separators; link destinations and code do not.
            Event::Text(content)
                if paragraph && link_depth == 0 && text[range.clone()] == *content =>
            {
                boundaries.extend(
                    content
                        .match_indices('|')
                        .map(|(offset, _)| (range.start + offset, None)),
                );
            }
            Event::Start(Tag::Emphasis) => {
                boundaries.push((range.end - 1, Some(range.start)));
                inline.push(range);
            }
            Event::Start(Tag::Strong | Tag::Strikethrough) => {
                boundaries.push((range.end - 2, Some(range.start)));
                inline.push(range);
            }
            _ => {}
        }
    }
    // Preserve internal a*b*c, but a parsed suffix before the actual URL
    // endpoint belongs to Markdown. Reuse the lexer (including delimiter
    // balance), so neither later owners nor unmatched closers hide that suffix.
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut stops: Vec<_> = boundaries
        .iter()
        .filter_map(|&(offset, opening)| opening.is_none().then_some(offset))
        .collect();
    scan_url_ranges(text, &boundaries, scanned, |end| stops.push(end));
    stops.sort_unstable();
    stops.dedup();
    let mut stop = stops.len();
    inline.sort_unstable_by_key(|range| std::cmp::Reverse(range.end));
    let mut suffix = text.char_indices().rev().peekable();
    let mut has_text = false;
    for range in inline {
        while let Some(&(offset, character)) = suffix.peek() {
            if offset < range.end {
                break;
            }
            suffix.next();
            while stop > 0 && stops[stop - 1] > offset {
                has_text = false;
                stop -= 1;
            }
            if character.is_whitespace() || is_fullwidth_url_punctuation(character) {
                has_text = false;
            } else if !character.is_ascii_punctuation() {
                has_text = true;
            }
            if stop > 0 && stops[stop - 1] == offset {
                has_text = false;
                stop -= 1;
            }
        }
        if !has_text {
            boundaries.push((range.start, None));
        }
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    boundaries
}

fn earliest_url_scheme_start(
    text: &str,
    scanned: &mut impl FnMut(UrlScanStep, usize),
) -> Option<usize> {
    let bytes = text.as_bytes();
    for index in 0..bytes.len() {
        scanned(UrlScanStep::Scheme, 1);
        if bytes[index] == b'h'
            && (bytes[index..].starts_with(b"http://") || bytes[index..].starts_with(b"https://"))
        {
            return Some(index);
        }
    }
    None
}

fn is_fullwidth_url_punctuation(character: char) -> bool {
    matches!(
        character,
        '，' | '。'
            | '；'
            | '：'
            | '！'
            | '？'
            | '）'
            | '］'
            | '｝'
            | '＞'
            | '》'
            | '〉'
            | '」'
            | '』'
            | '】'
            | '、'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completeness::preserved_urls;

    #[test]
    fn parser_owners_bound_urls_without_clipping_internal_punctuation() {
        for (source, expected) in [
            (
                "|https://example.test/x|Visible|\n|---|---|\n",
                "https://example.test/x",
            ),
            (
                "https://example.test/x**Visible**.",
                "https://example.test/x",
            ),
            (
                "https://example.test/x*Visible*，",
                "https://example.test/x",
            ),
            (
                "https://example.test/x`kept`Visible",
                "https://example.test/x",
            ),
            (
                "https://example.test/x<span>Visible</span>",
                "https://example.test/x",
            ),
            ("https://example.test/x|Visible", "https://example.test/x"),
            (
                "|https://example.test/x**Visible**|Other|\n|---|---|\n",
                "https://example.test/x",
            ),
            (
                "https://example.test/x**Visible**[Other](#other)",
                "https://example.test/x",
            ),
            (
                "https://example.test/x**Visible**)Other",
                "https://example.test/x",
            ),
            ("https://example.test/a*b*c", "https://example.test/a*b*c"),
            (
                "[Visible](https://example.test/a*b*c|d)",
                "https://example.test/a*b*c|d",
            ),
            (
                "|[Visible](https://example.test/a*b*c|d)|Tail|\n|---|---|\n",
                "https://example.test/a*b*c|d",
            ),
        ] {
            assert_eq!(
                preserved_urls(source),
                HashSet::from([expected.to_owned()]),
                "{source}"
            );
        }
    }

    #[test]
    fn urls_stop_at_markdown_boundaries_without_swallowing_link_labels() {
        for text in [
            "__https://example.com/path__[contain code](https://example.net/next)",
            "**https://example.com/path**[Figure A](https://example.net/next)",
            "https://example.com/path[visible label](https://example.net/next)",
        ] {
            assert_eq!(
                preserved_urls(text),
                HashSet::from([
                    "https://example.com/path".to_owned(),
                    "https://example.net/next".to_owned(),
                ]),
                "{text}"
            );
        }
        for url in [
            "https://[::1]/path",
            "https://example.com/?items[]=one",
            "https://example.com/a_b_(c)",
            "https://example.com/a*b*c",
            "https://example.com/a\\_b",
        ] {
            assert_eq!(preserved_urls(url), HashSet::from([url.to_owned()]));
        }
    }

    #[test]
    fn url_ranges_keep_later_formatting_boundaries() {
        let text = "https://first.test/x **https://second.test/y**";
        let urls: Vec<_> = preserved_url_ranges(text)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(urls, ["https://first.test/x", "https://second.test/y"]);
    }

    #[test]
    fn url_boundary_scans_stay_bounded_before_future_formatting() {
        for size in [128, 256] {
            let text = format!(
                "{}\n{}",
                "http://a.test/x https://b.test/y\n".repeat(size),
                "**word**\n".repeat(size)
            );
            let mut scans = 0;
            let ranges = preserved_url_ranges_with_scan(&text, |step, count| {
                if matches!(step, UrlScanStep::Boundary) {
                    scans += count;
                }
            });
            assert_eq!(ranges.len(), 2 * size);
            assert!(
                scans <= 2 * size,
                "{size} lines made {scans} boundary scans for {} URLs",
                ranges.len()
            );
        }
    }

    #[test]
    fn url_scan_work_stays_bounded_across_owner_and_lexer_shapes() {
        let mut results = Vec::new();
        for size in [8, 16] {
            let cases = [
                (
                    "strong-plain",
                    "**http://a.test/x**.**https://b.test/y**.".repeat(size),
                    ["http://a.test/x", "https://b.test/y"]
                        .repeat(size)
                        .into_iter()
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                ),
                (
                    "strong",
                    "**http://a.test/x()**.**https://b.test/y()**.".repeat(size),
                    ["http://a.test/x()", "https://b.test/y()"]
                        .repeat(size)
                        .into_iter()
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                ),
                (
                    "link",
                    "http://a.test/x[L](#x).https://b.test/y[L](#x).".repeat(size),
                    ["http://a.test/x", "https://b.test/y"]
                        .repeat(size)
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                ),
                (
                    "image",
                    "http://a.test/x![L](#x).https://b.test/y![L](#x).".repeat(size),
                    ["http://a.test/x", "https://b.test/y"]
                        .repeat(size)
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                ),
                (
                    "balanced",
                    format!("https://e.test/{}", "()".repeat(size * 16)),
                    vec![format!("https://e.test/{}", "()".repeat(size * 16))],
                ),
            ];
            for (shape, text, expected) in cases {
                let mut work = [0; 4];
                let ranges = preserved_url_ranges_with_scan(&text, |step, count| {
                    let index = match step {
                        UrlScanStep::Scheme => 0,
                        UrlScanStep::Lexical => 1,
                        UrlScanStep::Boundary => 2,
                        UrlScanStep::Delimiter => 3,
                    };
                    work[index] += count;
                });
                let actual = ranges
                    .into_iter()
                    .map(|range| text[range].to_owned())
                    .collect::<Vec<_>>();
                assert_eq!(actual, expected, "{shape} size={size}");
                results.push((shape, size, text.len(), work));
            }
        }
        for size in [128, 256] {
            let future_text = format!(
                "{}\n{}",
                "http://a.test/x https://b.test/y\n".repeat(size),
                "**word**\n".repeat(size)
            );
            let future_expected = ["http://a.test/x", "https://b.test/y"]
                .repeat(size)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let mut future_work = [0; 4];
            let future_actual = preserved_url_ranges_with_scan(&future_text, |step, count| {
                let index = match step {
                    UrlScanStep::Scheme => 0,
                    UrlScanStep::Lexical => 1,
                    UrlScanStep::Boundary => 2,
                    UrlScanStep::Delimiter => 3,
                };
                future_work[index] += count;
            })
            .into_iter()
            .map(|range| future_text[range].to_owned())
            .collect::<Vec<_>>();
            assert_eq!(future_actual, future_expected, "future size={size}");
            results.push(("future", size, future_text.len(), future_work));
            for (shape, source, expected) in [
                ("http", "http://a.test/x ", "http://a.test/x"),
                ("https", "https://b.test/y ", "https://b.test/y"),
                (
                    "invalid-prefix",
                    "httpx httpy http://a.test/x ",
                    "http://a.test/x",
                ),
            ] {
                let text = source.repeat(size);
                let mut work = [0; 4];
                let ranges = preserved_url_ranges_with_scan(&text, |step, count| {
                    let index = match step {
                        UrlScanStep::Scheme => 0,
                        UrlScanStep::Lexical => 1,
                        UrlScanStep::Boundary => 2,
                        UrlScanStep::Delimiter => 3,
                    };
                    work[index] += count;
                });
                let actual = ranges
                    .into_iter()
                    .map(|range| text[range].to_owned())
                    .collect::<Vec<_>>();
                assert_eq!(
                    actual,
                    vec![expected.to_owned(); size],
                    "{shape} size={size}"
                );
                results.push((shape, size, text.len(), work));
            }
        }
        let measured = results
            .iter()
            .map(|(shape, size, bytes, work)| format!("{shape}/{size}/{bytes}: {work:?}"))
            .collect::<Vec<_>>()
            .join("; ");
        for (shape, size, bytes, work) in results {
            let total: usize = work.iter().sum();
            assert!(
                total <= 12 * bytes + 16,
                "{shape} size={size} bytes={bytes} scheme={} lexical={} boundary={} delimiter={}; all: {measured}",
                work[0], work[1], work[2], work[3],
            );
        }
    }

    #[test]
    fn url_scan_preserves_balanced_and_unicode_source_bytes() {
        for (text, expected) in [
            ("https://[::1]/path", "https://[::1]/path"),
            (
                "https://example.com/?items[]=one",
                "https://example.com/?items[]=one",
            ),
            ("https://example.com/a\\_b", "https://example.com/a\\_b"),
            (
                "https://example.com/path([)]",
                "https://example.com/path([)]",
            ),
            (
                "前 https://example.com/路径_(甲)）后",
                "https://example.com/路径_(甲)",
            ),
            (
                "https://example.com/a_(c),;:!?",
                "https://example.com/a_(c)",
            ),
        ] {
            let urls = preserved_url_ranges(text)
                .into_iter()
                .map(|range| &text[range])
                .collect::<Vec<_>>();
            assert_eq!(urls, [expected], "{text}");
        }
    }
}
