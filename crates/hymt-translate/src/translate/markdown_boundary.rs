//! Preserve parser-resolved delimiter identity when translated punctuation changes flanking.
use super::{reconstructed_section_parts, TranslationPlan};
use pulldown_cmark::{Event, Parser, Tag};
use std::collections::{BTreeSet, HashMap, HashSet};

type Owner = (usize, usize);
type Span = (u8, Owner, Owner);

#[cfg(test)]
thread_local! {
    static PARSE_WORK: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

#[cfg(test)]
pub(super) fn take_parse_work() -> (usize, usize) {
    PARSE_WORK.with(|work| work.replace((0, 0)))
}

pub(super) fn requires_validated_rendering(plan: &TranslationPlan) -> bool {
    plan.document_plan.as_ref().is_some_and(|document| {
        document.sections.iter().any(|section| {
            section.kind == super::SectionKind::Paragraph
                && !section.should_translate
                && section.text.contains(['*', '_'])
        })
    })
}

pub(super) fn block_structure(text: &str) -> Vec<String> {
    use pulldown_cmark::TagEnd;
    #[cfg(test)]
    PARSE_WORK.with(|work| {
        let (calls, bytes) = work.get();
        work.set((calls + 1, bytes + text.len()));
    });
    Parser::new(text)
        .filter_map(|event| match event {
            Event::Start(tag)
                if !matches!(
                    tag,
                    Tag::Emphasis
                        | Tag::Strong
                        | Tag::Strikethrough
                        | Tag::Superscript
                        | Tag::Subscript
                        | Tag::Link { .. }
                        | Tag::Image { .. }
                ) =>
            {
                Some(format!("start:{tag:?}"))
            }
            Event::End(tag)
                if !matches!(
                    tag,
                    TagEnd::Emphasis
                        | TagEnd::Strong
                        | TagEnd::Strikethrough
                        | TagEnd::Superscript
                        | TagEnd::Subscript
                        | TagEnd::Link
                        | TagEnd::Image
                ) =>
            {
                Some(format!("end:{tag:?}"))
            }
            Event::Rule => Some("rule".to_owned()),
            _ => None,
        })
        .collect()
}

fn spans_in_parts(parts: &[String], first_section: usize) -> BTreeSet<Span> {
    let mut offset = 0;
    let offsets: Vec<_> = parts
        .iter()
        .map(|part| {
            let start = offset;
            offset += part.len();
            start
        })
        .collect();
    let owner = |position| {
        let mut index = offsets
            .partition_point(|start| *start <= position)
            .saturating_sub(1);
        while index > 0 && parts[index].is_empty() {
            index -= 1;
        }
        (first_section + index, position - offsets[index])
    };
    let text = parts.join("");
    #[cfg(test)]
    PARSE_WORK.with(|work| {
        let (calls, bytes) = work.get();
        work.set((calls + 1, bytes + text.len()));
    });
    Parser::new(&text)
        .into_offset_iter()
        .filter_map(|(event, range)| {
            let kind = match event {
                Event::Start(Tag::Emphasis) => 0,
                Event::Start(Tag::Strong) => 1,
                Event::Start(Tag::Strikethrough) => 2,
                _ => return None,
            };
            Some((kind, owner(range.start), owner(range.end - 1)))
        })
        .collect()
}

fn spans(plan: &TranslationPlan, translations: &[String]) -> BTreeSet<Span> {
    let Some(document) = &plan.document_plan else {
        return BTreeSet::new();
    };
    let parts = reconstructed_section_parts(
        document,
        &plan.segment_section_groups,
        &plan.segments,
        translations,
    );
    spans_in_parts(&parts, 0)
}

fn section_part(
    plan: &TranslationPlan,
    translations: &[String],
    section_segments: &HashMap<usize, (usize, usize)>,
    section_index: usize,
) -> String {
    let document = plan
        .document_plan
        .as_ref()
        .expect("section groups require document");
    let Some(&(first, last)) = section_segments.get(&section_index) else {
        return document.sections[section_index].text.clone();
    };
    let group = &plan.segment_section_groups[first];
    if group.first() != Some(&section_index) {
        return String::new();
    }
    let mut part = String::new();
    for (offset, translation) in translations[first..=last].iter().enumerate() {
        part.push_str(translation);
        let index = first + offset;
        if index < last {
            super::restore_trailing_newlines(&mut part, &plan.segments[index]);
        }
    }
    let group_source: String = group
        .iter()
        .map(|&index| document.sections[index].text.as_str())
        .collect();
    super::restore_trailing_newlines(&mut part, &group_source);
    part
}

/// Insert spaces at model-owned edges, retaining all source and
/// translated bytes. Each insertion must reduce exact parser-span mismatches;
/// matching total emphasis counts is insufficient. ASCII spaces are preferred; a
/// nonbreaking space is a visible gap fallback when ASCII would change block
/// semantics (for example, create a list). Unrepairable input fails closed.
pub(super) fn render_translations(plan: &TranslationPlan, translations: &mut [String]) -> bool {
    if plan.segment_section_groups.is_empty() {
        return true;
    }
    let source = plan
        .document_plan
        .as_ref()
        .map(|document| {
            document
                .sections
                .iter()
                .map(|section| section.text.as_str())
                .collect::<String>()
        })
        .unwrap_or_default();
    let expected = spans(plan, &plan.segments);
    let mut actual = spans(plan, translations);
    if actual == expected {
        return true;
    }
    let expected_blocks = block_structure(&source);
    if block_structure(&plan.reconstruct(translations)) != expected_blocks {
        return false;
    }
    let document = plan
        .document_plan
        .as_ref()
        .expect("section groups require document");
    let mut section_segments: HashMap<usize, (usize, usize)> = HashMap::new();
    for (index, group) in plan.segment_section_groups.iter().enumerate() {
        for section in group {
            section_segments
                .entry(*section)
                .and_modify(|bounds| bounds.1 = index)
                .or_insert((index, index));
        }
    }
    let adjacent = |owner: Owner, before: bool| -> Option<(usize, bool)> {
        let mut index = owner.0;
        loop {
            index = if before {
                index.checked_sub(1)?
            } else {
                index.checked_add(1)?
            };
            let section = document.sections.get(index)?;
            if let Some((first, last)) = section_segments.get(&index) {
                return Some((if before { *last } else { *first }, !before));
            }
            if !section.text.trim().is_empty() {
                return None;
            }
        }
    };
    let source_parts = reconstructed_section_parts(
        document,
        &plan.segment_section_groups,
        &plan.segments,
        &plan.segments,
    );
    let mut parts = reconstructed_section_parts(
        document,
        &plan.segment_section_groups,
        &plan.segments,
        translations,
    );
    // Probe each mismatched span in its own delimiter-and-leaf window.
    // Independent spans can share a translated leaf without making their
    // parser windows include each other's delimiters.
    for (span @ (_, open, close), missing) in expected
        .difference(&actual)
        .map(|span| (span, true))
        .chain(actual.difference(&expected).map(|span| (span, false)))
    {
        let left = adjacent(*open, missing);
        let right = adjacent(*close, !missing);
        let mut start = open.0.min(close.0);
        let mut end = open.0.max(close.0) + 1;
        for owner in [*open, *close] {
            for before in [true, false] {
                if let Some((segment, _)) = adjacent(owner, before) {
                    let section = plan.segment_section_groups[segment][0];
                    start = start.min(section);
                    end = end.max(section + 1);
                }
            }
        }
        let local_expected = spans_in_parts(&source_parts[start..end], start);
        let local_actual = spans_in_parts(&parts[start..end], start);
        if missing && !local_expected.contains(span) {
            return false;
        }
        if local_actual.contains(span) == missing {
            continue;
        }
        let old_score = local_expected.symmetric_difference(&local_actual).count();
        let local_blocks = block_structure(&parts[start..end].join(""));
        let candidates = [
            left.map(|edge| vec![edge]),
            right.map(|edge| vec![edge]),
            left.zip(right).map(|(first, last)| vec![first, last]),
        ];
        let mut repaired = false;
        'candidates: for separator in [' ', '\u{a0}'] {
            for edits in candidates.iter().flatten() {
                let mut originals = HashMap::new();
                let mut changed_sections = HashSet::new();
                for &(index, leading) in edits {
                    let section = plan.segment_section_groups[index][0];
                    let text = &mut translations[index];
                    if (leading && text.starts_with(char::is_whitespace))
                        || (!leading && text.ends_with(char::is_whitespace))
                    {
                        continue;
                    }
                    originals.entry(index).or_insert_with(|| text.clone());
                    text.insert(if leading { 0 } else { text.len() }, separator);
                    changed_sections.insert(section);
                }
                if originals.is_empty() {
                    continue;
                }
                for &section in &changed_sections {
                    parts[section] = section_part(plan, translations, &section_segments, section);
                }
                let next = spans_in_parts(&parts[start..end], start);
                if next.contains(span) == missing
                    && local_expected.symmetric_difference(&next).count() < old_score
                    && block_structure(&parts[start..end].join("")) == local_blocks
                {
                    repaired = true;
                    break 'candidates;
                }
                for (index, original) in originals {
                    translations[index] = original;
                }
                for section in changed_sections {
                    parts[section] = section_part(plan, translations, &section_segments, section);
                }
            }
        }
        if !repaired {
            return false;
        }
    }
    actual = spans(plan, translations);
    actual == expected && block_structure(&plan.reconstruct(translations)) == expected_blocks
}
