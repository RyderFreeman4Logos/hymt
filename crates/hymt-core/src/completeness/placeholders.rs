//! Source placeholder recognition for document ownership.
use std::{collections::HashSet, ops::Range};

/// Complete placeholder identities, with the same grammar as source ownership.
pub fn preserved_placeholders(text: &str) -> HashSet<String> {
    preserved_placeholder_ranges(text)
        .into_iter()
        .map(|range| text[range].to_owned())
        .collect()
}

/// Byte ranges of every recognized occurrence, including repeated identities.
/// Two forward passes retain the existing double-brace and identifier grammars;
/// no distinct identity causes another search of the document.
pub fn preserved_placeholder_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut remaining = text;
    while let Some(start) = remaining.find("{{") {
        let after_start = &remaining[start + 2..];
        let Some(end) = after_start.find("}}") else {
            break;
        };
        if !after_start[..end].trim().is_empty() {
            let offset = text.len() - remaining.len() + start;
            ranges.push(offset..offset + end + 4);
        }
        remaining = &after_start[end + 2..];
    }
    let mut remaining = text;
    while let Some(start) = remaining.find('{') {
        let after_start = &remaining[start + 1..];
        let Some(end) = after_start.find('}') else {
            break;
        };
        let candidate = &after_start[..end];
        if !candidate.is_empty()
            && candidate.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
            })
        {
            let offset = text.len() - remaining.len() + start;
            ranges.push(offset..offset + end + 2);
        }
        remaining = &after_start[end + 1..];
    }
    ranges
}
