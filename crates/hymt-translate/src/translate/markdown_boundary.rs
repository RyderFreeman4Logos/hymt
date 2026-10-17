//! Preserve parser-resolved delimiter identity when translated punctuation changes flanking.
use super::{reconstructed_section_parts, TranslationPlan};
use pulldown_cmark::{Event, Parser, Tag};
use std::collections::{BTreeSet, HashMap, HashSet};

type Owner = (usize, usize);
type Span = (u8, Owner, Owner);

#[derive(Clone, Copy)]
struct Mismatch {
    span: Span,
    missing: bool,
    left: Option<(usize, bool)>,
    right: Option<(usize, bool)>,
    start: usize,
    end: usize,
    group: usize,
}

struct SharedOpening {
    start: usize,
    end: usize,
    members: Vec<usize>,
    settled_at: Option<usize>,
}

#[cfg(test)]
thread_local! {
    static PARSE_WORK: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    static RENDER_WORK: std::cell::Cell<RenderWork> = const { std::cell::Cell::new(RenderWork::new()) };
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(super) struct RenderWork {
    pub joined_bytes: usize,
    pub copied_bytes: usize,
    pub member_visits: usize,
    pub invalidation_queries: usize,
}

#[cfg(test)]
impl RenderWork {
    const fn new() -> Self {
        Self {
            joined_bytes: 0,
            copied_bytes: 0,
            member_visits: 0,
            invalidation_queries: 0,
        }
    }
}

#[cfg(test)]
pub(super) fn take_render_work() -> RenderWork {
    RENDER_WORK.with(|work| work.replace(RenderWork::new()))
}

#[cfg(test)]
pub(super) fn take_parse_work() -> (usize, usize) {
    PARSE_WORK.with(|work| work.replace((0, 0)))
}

fn join_parts(parts: &[String]) -> String {
    let joined = parts.join("");
    #[cfg(test)]
    RENDER_WORK.with(|work| {
        let mut current = work.get();
        current.joined_bytes += joined.len();
        work.set(current);
    });
    joined
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
            Event::Html(html) => Some(format!("html:{html}")),
            Event::InlineHtml(html) => Some(format!("inline_html:{html}")),
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
    let text = join_parts(parts);
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
    #[cfg(test)]
    RENDER_WORK.with(|work| {
        let mut current = work.get();
        current.copied_bytes += parts.iter().map(String::len).sum::<usize>();
        work.set(current);
    });
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
    #[cfg(test)]
    RENDER_WORK.with(|work| {
        let mut current = work.get();
        current.copied_bytes += part.len() + group_source.len();
        work.set(current);
    });
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
    let reconstructed = plan.reconstruct(translations);
    if hymt_core::completeness::parse_json_document(&source).is_some() {
        return super::ensure_document_identities_preserved(&source, &reconstructed).is_ok();
    }
    let expected_blocks = block_structure(&source);
    if block_structure(&reconstructed) != expected_blocks {
        return false;
    }
    let expected = spans(plan, &plan.segments);
    let mut actual = spans(plan, translations);
    if actual == expected {
        return true;
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
    #[cfg(test)]
    RENDER_WORK.with(|work| {
        let mut current = work.get();
        current.copied_bytes += source_parts.iter().map(String::len).sum::<usize>();
        current.copied_bytes += parts.iter().map(String::len).sum::<usize>();
        work.set(current);
    });
    // Keep the original missing-then-extra order. Spans with one opening
    // section can have different closing windows, yet one edge edit may
    // settle all of them. Their union is checked only after an accepted edit.
    let mut mismatches = Vec::new();
    let mut groups: Vec<SharedOpening> = Vec::new();
    let mut opening_groups = HashMap::new();
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
        let group = *opening_groups.entry((missing, open.0)).or_insert_with(|| {
            groups.push(SharedOpening {
                start,
                end,
                members: Vec::new(),
                settled_at: None,
            });
            groups.len() - 1
        });
        groups[group].start = groups[group].start.min(start);
        groups[group].end = groups[group].end.max(end);
        groups[group].members.push(mismatches.len());
        mismatches.push(Mismatch {
            span: *span,
            missing,
            left,
            right,
            start,
            end,
            group,
        });
    }
    let mut revision = 0;
    // Independent spans still use their own delimiter-and-leaf windows.
    for mismatch in &mismatches {
        let Mismatch {
            span,
            missing,
            left,
            right,
            start,
            end,
            group,
        } = *mismatch;
        #[cfg(test)]
        RENDER_WORK.with(|work| {
            let mut current = work.get();
            current.member_visits += 1;
            current.invalidation_queries += 1;
            work.set(current);
        });
        if groups[group].settled_at == Some(revision) {
            continue;
        }
        let local_expected = spans_in_parts(&source_parts[start..end], start);
        let local_actual = spans_in_parts(&parts[start..end], start);
        if missing && !local_expected.contains(&span) {
            return false;
        }
        if local_actual.contains(&span) == missing {
            continue;
        }
        let old_score = local_expected.symmetric_difference(&local_actual).count();
        let local_blocks = block_structure(&join_parts(&parts[start..end]));
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
                    originals.entry(index).or_insert_with(|| {
                        #[cfg(test)]
                        RENDER_WORK.with(|work| {
                            let mut current = work.get();
                            current.copied_bytes += text.len();
                            work.set(current);
                        });
                        text.clone()
                    });
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
                if next.contains(&span) == missing
                    && local_expected.symmetric_difference(&next).count() < old_score
                    && block_structure(&join_parts(&parts[start..end])) == local_blocks
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
        revision += 1;
        let shared = &mut groups[group];
        if shared.members.len() > 1 {
            let group_expected =
                spans_in_parts(&source_parts[shared.start..shared.end], shared.start);
            let group_actual = spans_in_parts(&parts[shared.start..shared.end], shared.start);
            #[cfg(test)]
            RENDER_WORK.with(|work| {
                let mut current = work.get();
                current.member_visits += shared.members.len();
                work.set(current);
            });
            if group_expected == group_actual
                && shared.members.iter().all(|&index| {
                    group_expected.contains(&mismatches[index].span) == mismatches[index].missing
                })
                && block_structure(&join_parts(&source_parts[shared.start..shared.end]))
                    == block_structure(&join_parts(&parts[shared.start..shared.end]))
            {
                shared.settled_at = Some(revision);
            }
        }
    }
    actual = spans(plan, translations);
    actual == expected && block_structure(&plan.reconstruct(translations)) == expected_blocks
}
