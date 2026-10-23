//! Publish only paragraphs whose complete source owner can no longer be replaced.
use super::*;

pub(super) struct ParagraphStream<'a> {
    source: &'a str,
    plan: &'a TranslationPlan,
    output: StreamEventSink,
    pub(super) next_segment: usize,
    next_section: usize,
    published_bytes: usize,
    pending_end: usize,
    pending_ready: usize,
    degraded_owners: HashSet<usize>,
    #[cfg(test)]
    pub(super) owner_reads: usize,
    #[cfg(test)]
    pub(super) readiness_reads: usize,
}

impl<'a> ParagraphStream<'a> {
    pub(super) fn new(
        source: &'a str,
        plan: &'a TranslationPlan,
        output: mpsc::Sender<StreamEvent>,
    ) -> Self {
        Self {
            source,
            plan,
            output: StreamEventSink::Immediate(output),
            next_segment: 0,
            next_section: 0,
            published_bytes: 0,
            pending_end: 0,
            pending_ready: 0,
            degraded_owners: HashSet::new(),
            #[cfg(test)]
            owner_reads: 0,
            #[cfg(test)]
            readiness_reads: 0,
        }
    }

    pub(super) async fn flush(
        &mut self,
        translations: &[Option<String>],
        completed: Option<(usize, bool)>,
    ) -> Result<()> {
        if let Some((index, degraded)) = completed {
            if degraded {
                let owner = retained_original::owner(self.plan, index)
                    .context("failed segment has no source owner")?;
                self.degraded_owners.insert(owner);
            }
            if (self.next_segment..self.pending_end).contains(&index) {
                self.pending_ready += 1;
            }
        }
        while self.next_segment < translations.len() {
            let owner = retained_original::owner(self.plan, self.next_segment)
                .context("streamed paragraph has no source owner")?;
            if self.pending_end == self.next_segment {
                let mut owner_reads = 0;
                self.pending_end = (self.next_segment..translations.len())
                    .take_while(|&index| {
                        owner_reads += 1;
                        retained_original::owner(self.plan, index) == Some(owner)
                    })
                    .last()
                    .context("streamed paragraph has no segments")?
                    + 1;
                #[cfg(test)]
                {
                    self.owner_reads += owner_reads;
                }
                #[cfg(not(test))]
                let _ = owner_reads;
                self.pending_ready = translations[self.next_segment..self.pending_end]
                    .iter()
                    .filter(|text| text.is_some())
                    .count();
                #[cfg(test)]
                {
                    self.readiness_reads += self.pending_end - self.next_segment;
                }
            }
            if self.pending_ready < self.pending_end - self.next_segment {
                break;
            }
            let document = self
                .plan
                .document_plan
                .as_ref()
                .context("missing document")?;
            let start = self.next_section
                + document.sections[self.next_section..]
                    .iter()
                    .position(|section| section.paragraph_index == Some(owner))
                    .context("missing source paragraph start")?;
            let after = start
                + document.sections[start..]
                    .iter()
                    .position(|section| section.paragraph_index != Some(owner))
                    .unwrap_or(document.sections.len() - start);
            if self.degraded_owners.contains(&owner) {
                let mut text: String = document.sections[self.next_section..start]
                    .iter()
                    .map(|section| section.text.as_str())
                    .collect();
                let original = self
                    .plan
                    .original_paragraphs
                    .iter()
                    .find(|paragraph| paragraph.index == owner)
                    .context("missing original paragraph")?;
                text.push_str(
                    self.source
                        .get(original.bytes.clone())
                        .context("invalid original paragraph range")?,
                );
                self.published_bytes += text.len();
                send_stream_event(&self.output, StreamEvent::Token(text)).await?;
                for index in self.next_segment..self.pending_end {
                    send_stream_event(&self.output, StreamEvent::SegmentDone(index)).await?;
                }
                self.next_segment = self.pending_end;
                self.next_section = after;
            } else {
                self.published_bytes += flush_ready_stream_prefix(
                    self.plan,
                    &translations[..self.pending_end],
                    &mut self.next_section,
                    &mut self.next_segment,
                    &self.output,
                )
                .await?;
                let tail: String = document.sections[self.next_section..after]
                    .iter()
                    .map(|section| section.text.as_str())
                    .collect();
                if !tail.is_empty() {
                    self.published_bytes += tail.len();
                    send_stream_event(&self.output, StreamEvent::Token(tail)).await?;
                }
                self.next_section = after;
            }
            self.pending_ready = 0;
        }
        Ok(())
    }

    pub(super) async fn finish(self, translated: &str) -> Result<()> {
        let suffix = translated
            .get(self.published_bytes..)
            .context("streamed paragraph prefix exceeds final output")?;
        if !suffix.is_empty() {
            send_stream_event(&self.output, StreamEvent::Token(suffix.to_owned())).await?;
        }
        Ok(())
    }
}
