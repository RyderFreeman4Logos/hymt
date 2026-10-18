use super::*;
mod table_shape;

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct MarkdownStructure {
    pub(super) blocks: Vec<String>,
    pub(super) headings: Vec<pulldown_cmark::HeadingLevel>,
    pub(super) links: Vec<(String, String)>,
    pub(super) images: Vec<(String, String)>,
    pub(super) inline: Vec<TagEnd>,
    pub(super) code: Vec<String>,
    pub(super) tables: Vec<(usize, Vec<usize>)>,
}

pub(super) fn markdown_structure(text: &str) -> MarkdownStructure {
    let mut structure = MarkdownStructure {
        blocks: block_structure(text),
        tables: table_shape::table_shapes(text),
        ..MarkdownStructure::default()
    };
    for event in Parser::new(text) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => structure.headings.push(level),
            Event::Start(Tag::Link {
                dest_url, title, ..
            }) => structure
                .links
                .push((dest_url.to_string(), title.to_string())),
            Event::Start(Tag::Image {
                dest_url, title, ..
            }) => structure
                .images
                .push((dest_url.to_string(), title.to_string())),
            Event::Start(tag @ (Tag::Emphasis | Tag::Strong | Tag::Strikethrough)) => {
                structure.inline.push(tag.to_end())
            }
            Event::End(end @ (TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough)) => {
                structure.inline.push(end)
            }
            Event::Code(code) => structure.code.push(code.to_string()),
            _ => {}
        }
    }
    structure
}

pub(super) fn block_structure(text: &str) -> Vec<String> {
    use pulldown_cmark::TagEnd;
    // Paragraph reflow is model-owned; syntax and container roles still guard assembly.
    Parser::new(text)
        .filter_map(|event| match event {
            Event::Start(tag)
                if !matches!(
                    tag,
                    Tag::Paragraph
                        | Tag::Emphasis
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
                    TagEnd::Paragraph
                        | TagEnd::Emphasis
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
