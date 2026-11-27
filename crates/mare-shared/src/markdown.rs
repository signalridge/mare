//! Source identities for generic fenced-code presenters. The Markdown parser owns boundaries;
//! a presenter receives normalized code and never rewrites the document.
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fence {
    /// UTF-8 byte offsets, zero based; occurrence identity is independent of code contents.
    pub start: usize,
    pub end: usize,
    pub info: String,
    pub language: String,
    pub source: String,
    pub closed: bool,
}

pub fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_MATH
}

/// Whether Markdown contains a rendered image (code text containing image syntax is ignored).
pub fn has_images(text: &str) -> bool {
    Parser::new_ext(text, options()).any(|event| matches!(event, Event::Start(Tag::Image { .. })))
}

pub fn fences(text: &str) -> Vec<Fence> {
    let mut out = Vec::new();
    let mut current: Option<Fence> = None;
    let mut content_end = None;
    for (event, range) in Parser::new_ext(text, options()).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                let raw = &text[range.clone()];
                let opening = raw.chars().next().unwrap_or('`');
                let length = raw.chars().take_while(|c| *c == opening).count();
                let last = raw.trim_end().lines().last().unwrap_or("").trim_start_matches([' ', '\t', '>']);
                let closing_length = last.chars().take_while(|c| *c == opening).count();
                let closed =
                    raw.lines().count() > 1 && closing_length >= length && last[closing_length..].trim().is_empty();
                content_end = None;
                current = Some(Fence {
                    start: range.start,
                    end: range.end,
                    language: info.split_whitespace().next().unwrap_or("").to_ascii_lowercase(),
                    info: info.into_string(),
                    source: String::new(),
                    closed,
                });
            }
            Event::Text(code) if current.is_some() => {
                content_end = Some(range.end);
                current.as_mut().unwrap().source.push_str(&code);
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(mut fence) = current.take() {
                    if content_end == Some(fence.end) {
                        fence.closed = false;
                    }
                    out.push(fence);
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identities_are_occurrences_and_language_is_normalized() {
        let text = "é\n```MERMAID title\na-->b\n```\n\n```mermaid\na-->b\n```\n";
        let blocks = fences(text);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].source, blocks[1].source);
        assert_ne!(blocks[0].start, blocks[1].start);
        assert_eq!(blocks[0].language, "mermaid");
        assert_eq!(blocks[0].info, "MERMAID title");
        assert!(blocks.iter().all(|b| b.closed));
        assert!(text.is_char_boundary(blocks[0].start));
    }
    #[test]
    fn nested_tilde_and_open_fences_keep_parser_source() {
        let blocks = fences("> ~~~diagram\n> a\n> ~~~\n\n- item\n\n  ```other\n  b\n  ```\n\n```open\nc");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].source, "a\n");
        assert!(blocks[0].closed);
        assert_eq!(blocks[1].source, "b\n");
        assert!(blocks[1].closed);
        assert!(!blocks[2].closed);
        assert!(fences("    indented code\n\n`inline`").is_empty());
        assert!(!fences("> ~~~diagram\n> >~~~")[0].closed);
        assert!(fences("```empty\n```")[0].closed);
        assert!(!fences("```empty\n")[0].closed);
    }
}
