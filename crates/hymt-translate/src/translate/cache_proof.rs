//! Prove cached reconstruction against source-owned bytes, then replay the renderer.
use super::{markdown_boundary, translation_section_groups, TranslationPlan};
use hymt_core::completeness::{preserved_placeholder_ranges, preserved_url_ranges};
use pulldown_cmark::{Event, Parser, Tag};
use std::ops::Range;

type Owner = (usize, usize);
pub(super) type Role = (u8, Owner, Owner);

// KMP consumes each search window once. Reverse matching uses the same byte
// algorithm without allocating a reversed document or exploring bindings.
fn find_literal(haystack: &[u8], needle: &[u8], reverse: bool) -> Option<usize> {
    let byte = |bytes: &[u8], i| bytes[if reverse { bytes.len() - 1 - i } else { i }];
    let mut prefix = vec![0; needle.len()];
    let mut matched = 0;
    for i in 1..needle.len() {
        while matched > 0 && byte(needle, i) != byte(needle, matched) {
            matched = prefix[matched - 1];
        }
        if byte(needle, i) == byte(needle, matched) {
            matched += 1;
        }
        prefix[i] = matched;
    }
    matched = 0;
    for i in 0..haystack.len() {
        while matched > 0 && byte(haystack, i) != byte(needle, matched) {
            matched = prefix[matched - 1];
        }
        if byte(haystack, i) == byte(needle, matched) {
            matched += 1;
        }
        if matched == needle.len() {
            return Some(if reverse {
                haystack.len() - i - 1
            } else {
                i + 1 - matched
            });
        }
    }
    None
}

fn bind_literals(
    source: &str,
    candidate: &str,
    fixed: &[Range<usize>],
    reverse: bool,
) -> Option<Vec<Range<usize>>> {
    let mut bindings = vec![0..0; fixed.len()];
    let mut cursor = if reverse { candidate.len() } else { 0 };
    for step in 0..fixed.len() {
        let i = if reverse {
            fixed.len() - 1 - step
        } else {
            step
        };
        let original = &fixed[i];
        let literal = &source[original.clone()];
        let window = if reverse {
            0..cursor
        } else {
            cursor..candidate.len()
        };
        let start = if original.start == 0 {
            candidate.starts_with(literal).then_some(0)?
        } else if original.end == source.len() {
            candidate
                .ends_with(literal)
                .then(|| candidate.len() - literal.len())?
        } else {
            window.start
                + find_literal(
                    &candidate.as_bytes()[window.clone()],
                    literal.as_bytes(),
                    reverse,
                )?
        };
        let end = start + literal.len();
        if start < window.start || end > window.end {
            return None;
        }
        bindings[i] = start..end;
        cursor = if reverse { start } else { end };
    }
    Some(bindings)
}

// Positions map to a specific fixed source atom, not merely a matching token.
fn owner(position: usize, fixed: &[Range<usize>]) -> Option<Owner> {
    let index = fixed.partition_point(|range| range.end <= position);
    let range = fixed.get(index)?;
    (range.start <= position).then(|| (index, position - range.start))
}

pub(super) fn roles(
    text: &str,
    fixed: &[Range<usize>],
    coherent: &[Range<usize>],
) -> Option<Vec<Role>> {
    let mut result = Vec::new();
    let is_coherent = |range: &Range<usize>| {
        let index = coherent.partition_point(|slot| slot.end <= range.start);
        coherent
            .get(index)
            .is_some_and(|slot| slot.start <= range.start && range.end <= slot.end)
    };
    for (event, range) in Parser::new(text).into_offset_iter() {
        let kind = match event {
            Event::Code(_) => 0,
            Event::Html(_) | Event::InlineHtml(_) => 1,
            Event::Start(Tag::CodeBlock(_)) => 2,
            Event::Start(Tag::Link { .. }) => 3,
            Event::Start(Tag::Image { .. }) => 4,
            _ => continue,
        };
        if !is_coherent(&range) {
            result.push((
                kind,
                owner(range.start, fixed)?,
                owner(range.end.checked_sub(1)?, fixed)?,
            ));
        }
    }
    for (kind, ranges) in [
        (5, preserved_url_ranges(text)),
        (6, preserved_placeholder_ranges(text)),
    ] {
        for range in ranges {
            if !is_coherent(&range) {
                let start = owner(range.start, fixed)?;
                let end = owner(range.end.checked_sub(1)?, fixed)?;
                if start.0 != end.0 {
                    return None;
                }
                result.push((kind, start, end));
            }
        }
    }
    Some(result)
}

/// Admit only unambiguous source-template reconstructions. Candidate punctuation
/// cannot enlarge owners. Arbitrary provider edge whitespace can conservatively
/// miss this cache; the cold translation contract is unchanged.
pub(super) fn cached_owned_payloads_match(plan: &TranslationPlan, candidate: &str) -> bool {
    let Some(document) = &plan.document_plan else {
        return candidate.is_empty();
    };
    let source: String = document.sections.iter().map(|s| s.text.as_str()).collect();
    let groups = translation_section_groups(document);
    if groups.is_empty() {
        return source == candidate;
    }
    let mut offset = 0;
    let offsets: Vec<_> = document
        .sections
        .iter()
        .map(|s| {
            let start = offset;
            offset += s.text.len();
            start..offset
        })
        .collect();
    let mut fixed = Vec::new();
    let mut slots = Vec::new();
    let mut cursor = 0;
    for group in &groups {
        let start = offsets[group[0]].start;
        let end = offsets[*group.last().unwrap()].end;
        // Reconstruction restores these source newlines after the translated leaf.
        let editable_end = start + source[start..end].trim_end_matches('\n').len();
        if cursor < start {
            fixed.push(cursor..start);
        }
        slots.push(start..editable_end);
        cursor = editable_end;
    }
    if cursor < source.len() {
        fixed.push(cursor..source.len());
    }
    let Some(bindings) = bind_literals(&source, candidate, &fixed, false) else {
        return false;
    };
    if bind_literals(&source, candidate, &fixed, true).as_ref() != Some(&bindings) {
        return false;
    }
    let mut coherent_source = Vec::new();
    let mut coherent_candidate = Vec::new();
    let mut translations = Vec::with_capacity(groups.len());
    let mut fixed_cursor = 0;
    for (slot, group) in slots.iter().zip(&groups) {
        while fixed_cursor < fixed.len() && fixed[fixed_cursor].end <= slot.start {
            fixed_cursor += 1;
        }
        let start = if fixed_cursor == 0 {
            0
        } else {
            bindings[fixed_cursor - 1].end
        };
        let end = bindings
            .get(fixed_cursor)
            .map_or(candidate.len(), |r| r.start);
        if document.sections[group[0]]
            .paragraph_index
            .is_some_and(|owner| plan.coherent_paragraphs.contains(&owner))
        {
            coherent_source.push(slot.clone());
            coherent_candidate.push(start..end);
        }
        let Some(mut text) = candidate.get(start..end) else {
            return false;
        };
        // Only one renderer separator per edge is eligible. Replay below proves
        // the actual source delimiter permits and requires that exact insertion.
        if text.starts_with([' ', '\u{a0}']) {
            text = &text[text.chars().next().unwrap().len_utf8()..];
        }
        if text.ends_with([' ', '\u{a0}']) {
            text = &text[..text.len() - text.chars().next_back().unwrap().len_utf8()];
        }
        if text.is_empty()
            || text.starts_with(char::is_whitespace)
            || text.ends_with(char::is_whitespace)
        {
            return false;
        }
        translations.push(text.to_owned());
    }
    let source_roles = roles(&source, &fixed, &coherent_source);
    if source_roles.is_none() || source_roles != roles(candidate, &bindings, &coherent_candidate) {
        return false;
    }
    // One scratch segment per existing editable group retains owner section IDs
    // and outer edges while avoiding guesses about provider segment boundaries.
    let scratch = TranslationPlan {
        source_tokens: plan.source_tokens,
        segments: groups
            .iter()
            .map(|group| {
                group
                    .iter()
                    .map(|&i| document.sections[i].text.as_str())
                    .collect()
            })
            .collect(),
        available_source_tokens: plan.available_source_tokens,
        token_budget: plan.token_budget.clone(),
        document_plan: Some(document.clone()),
        segment_section_indexes: groups.iter().map(|group| group[0]).collect(),
        segment_section_groups: groups,
        original_paragraphs: plan.original_paragraphs.clone(),
        coherent_paragraphs: plan.coherent_paragraphs.clone(),
    };
    let Ok(contexts) = super::segment_contexts(&scratch) else {
        return false;
    };
    // Other editable slots and opaque source bytes cannot subsidize a missing
    // payload. Reuse cold/segment-cache admission in the original source context.
    if !scratch
        .segments
        .iter()
        .zip(&translations)
        .zip(contexts)
        .enumerate()
        .all(|(index, ((source, translated), context))| {
            super::cached_segment_is_complete(
                index,
                super::SegmentSource::new(source, context),
                translated,
            )
        })
    {
        return false;
    }
    markdown_boundary::render_translations(&scratch, &mut translations)
        && scratch.reconstruct(&translations) == candidate
}
