//! Source position carried into every local structural admission.
use super::TranslationPlan;
use anyhow::{ensure, Result};

#[derive(Clone, Copy, Default)]
pub(super) struct MarkdownContext {
    inline_start: bool,
}

#[derive(Clone, Copy)]
pub(super) struct SegmentSource<'a> {
    pub(super) text: &'a str,
    context: MarkdownContext,
}

impl<'a> SegmentSource<'a> {
    pub(super) fn new(text: &'a str, context: MarkdownContext) -> Self {
        Self { text, context }
    }

    pub(super) fn completeness_context(
        self,
        termination: super::CompletionTermination,
    ) -> hymt_core::completeness::CompletenessContext {
        hymt_core::completeness::CompletenessContext {
            termination,
            markdown_inline_start: self.context.inline_start,
        }
    }

    pub(super) fn structure_preserved(self, translated: &str) -> Result<()> {
        if hymt_core::completeness::parse_json_document(self.text).is_some() {
            return super::ensure_document_identities_preserved(self.text, translated);
        }
        if self.context.inline_start {
            // A mid-line leaf cannot acquire start-of-line block grammar merely
            // because source-owned syntax separated it from earlier prose.
            return super::ensure_markdown_structure_preserved(
                &format!("x {}", self.text),
                &format!("x {translated}"),
            );
        }
        super::ensure_markdown_structure_preserved(self.text, translated)
    }
}

impl<'a> From<&'a str> for SegmentSource<'a> {
    fn from(text: &'a str) -> Self {
        Self::new(text, MarkdownContext::default())
    }
}

impl<'a> From<&'a String> for SegmentSource<'a> {
    fn from(text: &'a String) -> Self {
        Self::from(text.as_str())
    }
}

pub(super) fn segment_contexts(plan: &TranslationPlan) -> Result<Vec<MarkdownContext>> {
    let Some(document) = &plan.document_plan else {
        return Ok(vec![MarkdownContext::default(); plan.segment_count()]);
    };
    let mut source = String::new();
    let offsets: Vec<_> = document
        .sections
        .iter()
        .map(|section| {
            let offset = source.len();
            source.push_str(&section.text);
            offset
        })
        .collect();
    let mut contexts = Vec::with_capacity(plan.segment_count());
    let mut previous = None;
    let mut consumed = 0;
    let mut scanned = 0;
    let mut inline_start = false;
    for (segment, group) in plan.segments.iter().zip(&plan.segment_section_groups) {
        let first = group[0];
        if previous != Some(first) {
            consumed = 0;
            previous = Some(first);
        }
        let start = offsets[first] + consumed;
        ensure!(
            source[start..].starts_with(segment),
            "segment source position changed"
        );
        // Monotone source cursor: no per-leaf document-prefix rescans.
        for character in source[scanned..start].chars() {
            inline_start = if character == '\n' {
                false
            } else {
                inline_start || !character.is_whitespace()
            };
        }
        #[cfg(test)]
        CONTEXT_WORK.with(|work| work.set(work.get() + start - scanned));
        scanned = start;
        contexts.push(MarkdownContext { inline_start });
        consumed += segment.len();
    }
    Ok(contexts)
}

#[cfg(test)]
thread_local! {
    static CONTEXT_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn take_context_work() -> usize {
    CONTEXT_WORK.with(|work| work.replace(0))
}
