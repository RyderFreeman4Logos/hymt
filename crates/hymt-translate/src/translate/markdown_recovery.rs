//! Conservative recovery of unpublished provider-added inline code.

use pulldown_cmark::{Event, Parser};

use super::{markdown_context::SegmentSource, markdown_structure};

/// Remove only provider-added inline-code delimiters when the Markdown parse
/// proves that no other syntax or interpreted text changes.
pub(super) fn recover_generated_inline_code<'a>(
    source: impl Into<SegmentSource<'a>>,
    reply: &str,
) -> Option<String> {
    let source = source.into();
    if source.text.contains('`') || !reply.contains('`') {
        return None;
    }
    let original_reply = reply;
    let reply = source.frame(reply);
    let source_structure = markdown_structure(&source.frame(source.text));
    let mut reply_structure = markdown_structure(&reply);
    if !source_structure.code.is_empty() || reply_structure.code.is_empty() {
        return None;
    }
    reply_structure.code.clear();
    if reply_structure != source_structure {
        return None;
    }

    let mut recovered = String::with_capacity(reply.len());
    let mut cursor = 0;
    for (event, range) in Parser::new(&reply).into_offset_iter() {
        let Event::Code(parsed) = event else {
            continue;
        };
        let before = reply.get(cursor..range.start)?;
        if before.contains('`') {
            return None;
        }
        recovered.push_str(before);
        let raw = reply.get(range.clone())?;
        let delimiters = raw.bytes().take_while(|byte| *byte == b'`').count();
        if delimiters == 0 || raw.len() < delimiters * 2 {
            return None;
        }
        let closing = raw.bytes().rev().take_while(|byte| *byte == b'`').count();
        if closing != delimiters {
            return None;
        }
        let payload = raw.get(delimiters..raw.len() - delimiters)?;
        if payload != parsed.as_ref() {
            return None;
        }
        recovered.push_str(payload);
        cursor = range.end;
    }
    let after = reply.get(cursor..)?;
    if after.contains('`') {
        return None;
    }
    recovered.push_str(after);
    let recovered = recovered
        .get(reply.len() - original_reply.len()..)?
        .to_owned();
    if recovered == original_reply
        || semantic_markdown_events(&reply) != semantic_markdown_events(&source.frame(&recovered))
        || source.structure_preserved(&recovered).is_err()
    {
        return None;
    }
    Some(recovered)
}

fn semantic_markdown_events(text: &str) -> Vec<Event<'_>> {
    let mut events = Vec::new();
    let mut run = None::<String>;
    for event in Parser::new(text) {
        match event {
            Event::Text(value) | Event::Code(value) => {
                // Keep the growing run owned until its boundary; converting it
                // back to an event after each leaf would recopy the prefix.
                #[cfg(test)]
                super::tests::record_semantic_bytes(value.len());
                run.get_or_insert_with(String::new).push_str(&value);
            }
            other => {
                if let Some(text) = run.take() {
                    events.push(Event::Text(text.into()));
                }
                events.push(other);
            }
        }
    }
    if let Some(text) = run {
        events.push(Event::Text(text.into()));
    }
    events
}
