//! Occurrence ownership is captured before reference normalization or segmentation.
use super::*;
use std::collections::BTreeSet;
use std::ops::Range;

#[derive(Clone, Debug)]
pub(super) struct SourceParagraph {
    index: usize,
    bytes: Range<usize>,
    oversized: bool,
}

/// An untranslated original paragraph, indexed by its occurrence in the source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedRegion {
    pub paragraph_index: usize,
    pub source_bytes: Range<usize>,
    pub output_bytes: Range<usize>,
}

pub(super) fn source_paragraphs(
    document: &DocumentLanguagePlan,
    segmenter: &Segmenter,
    budget: usize,
) -> Vec<SourceParagraph> {
    let mut offset = 0;
    document
        .sections
        .iter()
        .filter_map(|section| {
            let start = offset;
            offset += section.text.len();
            section.paragraph_index.map(|index| SourceParagraph {
                index,
                bytes: start..offset,
                oversized: section.should_translate
                    && markdown_ownership::protected_markdown_block_ranges(&section.text)
                        .iter()
                        .any(|range| segmenter.count_tokens(&section.text[range.clone()]) > budget),
            })
        })
        .collect()
}

fn owner(plan: &TranslationPlan, segment: usize) -> Option<usize> {
    let section = *plan.segment_section_groups.get(segment)?.first()?;
    plan.document_plan.as_ref()?.sections[section].paragraph_index
}

fn reset_owners(plan: &TranslationPlan, completed: &mut [String], retained: &BTreeSet<usize>) {
    for (index, text) in completed.iter_mut().enumerate() {
        if owner(plan, index).is_some_and(|owner| retained.contains(&owner)) {
            text.clone_from(&plan.segments[index]);
        }
    }
}

fn reconstruct(
    source: &str,
    plan: &TranslationPlan,
    completed: &[String],
    retained: &BTreeSet<usize>,
) -> Result<(String, Vec<RetainedRegion>)> {
    let Some(document) = &plan.document_plan else {
        return Ok((plan.reconstruct(completed), Vec::new()));
    };
    let parts = reconstructed_section_parts(
        document,
        &plan.segment_section_groups,
        &plan.segments,
        completed,
    );
    let mut output = String::new();
    let mut emitted = BTreeSet::new();
    let mut regions = Vec::new();
    for (section, part) in document.sections.iter().zip(parts) {
        if let Some(index) = section
            .paragraph_index
            .filter(|index| retained.contains(index))
        {
            if emitted.insert(index) {
                let paragraph = plan
                    .original_paragraphs
                    .iter()
                    .find(|paragraph| paragraph.index == index)
                    .context("retained paragraph has no original source owner")?;
                let start = output.len();
                output.push_str(
                    source
                        .get(paragraph.bytes.clone())
                        .context("invalid original paragraph byte range")?,
                );
                regions.push(RetainedRegion {
                    paragraph_index: index,
                    source_bytes: paragraph.bytes.clone(),
                    output_bytes: start..output.len(),
                });
            }
        } else {
            output.push_str(&part);
        }
    }
    Ok((output, regions))
}

fn validate(source: &str, output: &str) -> Result<()> {
    ensure_markdown_structure_preserved(source, output)?;
    ensure_document_identities_preserved(source, output)
}

/// Render accepted leaves, restore every failed owner's original bytes once, then
/// validate the undecorated body. A root notice never enters a Markdown container.
pub(super) fn finalize(
    source: &str,
    plan: &TranslationPlan,
    completed: &mut [String],
    degraded_segments: &mut Vec<usize>,
    retain_original: bool,
) -> Result<(String, Vec<RetainedRegion>)> {
    let mut retained: BTreeSet<_> = degraded_segments
        .iter()
        .map(|index| {
            owner(plan, index - 1).context("failed segment has no original paragraph owner")
        })
        .collect::<Result<_>>()?;
    if retain_original {
        retained.extend(
            plan.original_paragraphs
                .iter()
                .filter(|paragraph| paragraph.oversized)
                .map(|paragraph| paragraph.index),
        );
    }
    reset_owners(plan, completed, &retained);
    let rendered = markdown_boundary::render_translations(plan, completed);
    let (mut body, mut regions) = reconstruct(source, plan, completed, &retained)?;
    if !rendered || validate(source, &body).is_err() {
        if !retain_original || hymt_core::completeness::parse_json_document(source).is_some() {
            if !rendered {
                anyhow::bail!("translated Markdown inline boundaries changed");
            }
            validate(source, &body)?;
        }
        // Failure-only isolation: one original paragraph's candidates in source
        // context. Never blame a transport error or replace unrelated good owners.
        let mut found = false;
        for paragraph in &plan.original_paragraphs {
            if retained.contains(&paragraph.index) {
                continue;
            }
            let mut isolated = plan.segments.clone();
            for (index, text) in isolated.iter_mut().enumerate() {
                if owner(plan, index) == Some(paragraph.index) {
                    text.clone_from(&completed[index]);
                }
            }
            let valid = markdown_boundary::render_translations(plan, &mut isolated)
                && validate(source, &plan.reconstruct(&isolated)).is_ok();
            if !valid {
                retained.insert(paragraph.index);
                found = true;
            }
        }
        if !found {
            anyhow::bail!(
                "translated document validation failed without an attributable paragraph"
            );
        }
        reset_owners(plan, completed, &retained);
        if !markdown_boundary::render_translations(plan, completed) {
            anyhow::bail!(
                "translated Markdown inline boundaries changed after original restoration"
            );
        }
        (body, regions) = reconstruct(source, plan, completed, &retained)?;
        validate(source, &body)?;
    }
    for index in 0..plan.segment_count() {
        if owner(plan, index).is_some_and(|owner| retained.contains(&owner)) {
            degraded_segments.push(index + 1);
        }
    }
    degraded_segments.sort_unstable();
    degraded_segments.dedup();
    if regions.is_empty() {
        return Ok((body, regions));
    }
    let mut notice = String::from("Translation incomplete. Untranslated original paragraphs were retained unchanged. Body line numbers below exclude this notice.\n\n");
    for region in &regions {
        let first = body[..region.output_bytes.start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1;
        let last = first
            + body[region.output_bytes.clone()]
                .trim_end_matches(['\r', '\n'])
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count();
        notice.push_str(&format!(
            "Untranslated original: paragraph {} (source bytes {}..{}; body lines {}..{}).\n\n",
            region.paragraph_index, region.source_bytes.start, region.source_bytes.end, first, last
        ));
    }
    // Keep existing YAML frontmatter at byte zero. All other notices precede the
    // complete body, including lists, tables, block quotes, and open fences.
    let insertion = plan
        .document_plan
        .as_ref()
        .and_then(|document| document.sections.first())
        .filter(|section| {
            section.kind == SectionKind::Code
                && section
                    .text
                    .lines()
                    .next()
                    .is_some_and(|line| line.trim() == "---")
        })
        .map_or(0, |section| section.text.len());
    if insertion > 0 && !body[..insertion].ends_with("\n\n") {
        notice.insert(0, '\n');
    }
    for region in &mut regions {
        region.output_bytes.start += notice.len();
        region.output_bytes.end += notice.len();
    }
    body.insert_str(insertion, &notice);
    Ok((body, regions))
}
