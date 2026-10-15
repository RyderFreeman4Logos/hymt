//! Preserve parser-resolved delimiter identity when translated punctuation changes flanking.
use super::{reconstructed_section_parts, TranslationPlan};
use pulldown_cmark::{Event, Parser, Tag};
use std::collections::{BTreeSet, HashMap, HashSet};

type Owner = (usize, usize);
type Span = (u8, Owner, Owner);

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
        (index, position - offsets[index])
    };
    let text = parts.join("");
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
    while actual != expected {
        let old_score = expected.symmetric_difference(&actual).count();
        let boundaries: Vec<_> = expected
            .difference(&actual)
            .map(|(_, open, close)| [adjacent(*open, true), adjacent(*close, false)])
            .chain(
                actual
                    .difference(&expected)
                    .map(|(_, open, close)| [adjacent(*open, false), adjacent(*close, true)]),
            )
            .collect();
        // Prefer one inserted space. A span whose opening and closing both
        // changed flanking can require both outside separators simultaneously.
        let candidates: Vec<_> = boundaries
            .iter()
            .flat_map(|pair| {
                [
                    pair[0].map(|edge| vec![edge]),
                    pair[1].map(|edge| vec![edge]),
                    pair[0].zip(pair[1]).map(|(first, last)| vec![first, last]),
                ]
                .into_iter()
                .flatten()
            })
            .collect();
        let mut improved = false;
        'separators: for separator in [' ', '\u{a0}'] {
            let mut visited = HashSet::new();
            for edits in &candidates {
                if !visited.insert(edits.clone()) {
                    continue;
                }
                let mut originals = HashMap::new();
                for &(index, leading) in edits {
                    let text = &mut translations[index];
                    if (leading && text.starts_with(char::is_whitespace))
                        || (!leading && text.ends_with(char::is_whitespace))
                    {
                        continue;
                    }
                    originals.entry(index).or_insert_with(|| text.clone());
                    text.insert(if leading { 0 } else { text.len() }, separator);
                }
                if originals.is_empty() {
                    continue;
                }
                let next = spans(plan, translations);
                if expected.symmetric_difference(&next).count() < old_score
                    && block_structure(&plan.reconstruct(translations)) == expected_blocks
                {
                    actual = next;
                    improved = true;
                    break 'separators;
                }
                for (index, original) in originals {
                    translations[index] = original;
                }
            }
        }
        if !improved {
            return false;
        }
    }
    true
}
