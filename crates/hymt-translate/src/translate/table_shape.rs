//! Ordered GFM table dimensions, including raw widths that the parser pads or trims.

use pulldown_cmark::{Event, Options, Parser, Tag};

/// The parser supplies table and row boundaries (excluding block containers).
/// Keep the existing non-table inline contract separate: enabling tables there
/// would change code/link parsing and Paragraph ownership for existing callers.
/// Alignment and cell wording are not shape; originally ragged widths are valid.
pub(super) fn table_shapes(text: &str) -> Vec<(usize, Vec<usize>)> {
    let mut tables = Vec::new();
    for (event, span) in Parser::new_ext(text, Options::ENABLE_TABLES).into_offset_iter() {
        match event {
            Event::Start(Tag::Table(columns)) => tables.push((columns.len(), Vec::new())),
            Event::Start(Tag::TableRow) => {
                if let Some((_, rows)) = tables.last_mut() {
                    rows.push(raw_row_width(&text[span]));
                }
            }
            _ => {}
        }
    }
    tables
}

/// GFM separates cells at unescaped literal pipes, even inside code/HTML.
/// Entities are not separators. Optional edge pipes are not empty cells.
/// Scan each parser-owned row once; do not rescan the document per table.
fn raw_row_width(row: &str) -> usize {
    let row = row.trim_matches(|c: char| c.is_ascii_whitespace());
    let mut separators = 0;
    let mut escaped = false;
    let mut trailing_pipe = false;
    for byte in row.bytes() {
        trailing_pipe = byte == b'|' && !escaped;
        if trailing_pipe {
            separators += 1;
        }
        escaped = byte == b'\\' && !escaped;
    }
    separators + 1 - usize::from(row.starts_with('|')) - usize::from(trailing_pipe)
}
