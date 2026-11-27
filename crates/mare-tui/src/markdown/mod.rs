//! Markdown → styled lines. Streaming friendly (see [`StreamingMarkdown`]).
//!
//! Every element has a named theme style (`md.h1` … `md.link`) and its symbols come from
//! [`Theme::md`]`.glyphs` (`mare.o.markdown.glyphs`), so both can be changed. Sources per element
//! are in the table: headings without `#`;
//! inline code on `code_bg` with padding (Crush); code blocks as a `code_bg` card with half-block
//! edges (render-markdown), never wrapped; rotating bullets (render-markdown) with hanging
//! indents; quotes with only the bar colored; GitHub alerts (render-markdown);
//! tables without an outer frame, degrading to key/value and then source.
//!
//! This file holds the `Renderer` state machine whole: its helper structs, the event
//! preprocessing, the `event` / `start` / `end` handlers and the emit and inline helpers they
//! share. The open and close ends of one `Tag` touch the same fields (frames, styles, inline
//! buffer, pending blank), so splitting them apart would scatter one state machine over files.
//! The leaves it calls live next to it: `code` (code cards), `table` (tables), `glyphs`
//! (symbols); `stream` holds the incremental renderer.

use pulldown_cmark::{Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use super::highlight::highlight;
use super::image;
use super::style::*;

mod code;
mod glyphs;
mod stream;
mod table;

pub use stream::{StreamingMarkdown, stable_split};

fn options() -> Options {
    mare_shared::markdown::options()
}

/// Longest horizontal rule.
const RULE_MAX: usize = 80;
/// Rows a Markdown image may take.
const IMAGE_ROWS: usize = 16;

/// Render a complete markdown document to lines no wider than `width` (code lines excepted:
/// they're never wrapped — the live area cuts them, scrollback lets the terminal wrap them).
/// Footnote definitions are collected at the end of the document.
pub fn render_markdown(src: &str, width: usize, theme: &Theme) -> Vec<Line> {
    let (mut body, notes) = render_parts(src, width, theme);
    join_blocks(&mut body, notes);
    body
}

type Ev<'a> = (Event<'a>, std::ops::Range<usize>);

/// The body and the footnote definitions of `src`, rendered separately so a streaming message
/// can keep its footnotes after the still-growing tail.
fn render_parts(src: &str, width: usize, theme: &Theme) -> (Vec<Line>, Vec<Line>) {
    render_parts_streaming(src, width, theme, None)
}

fn render_parts_streaming(
    src: &str,
    width: usize,
    theme: &Theme,
    highlight: Option<&mut super::highlight::StreamingHighlight>,
) -> (Vec<Line>, Vec<Line>) {
    render_parts_presented(src, width, theme, highlight, &std::collections::BTreeMap::new(), 0)
}

/// Render cached, pure-node presentations in the original parser context (lists and quotes
/// retain their frames). Source fallback and non-target blocks use exactly the ordinary path.
pub fn render_presented(
    src: &str,
    width: usize,
    theme: &Theme,
    blocks: &std::collections::BTreeMap<usize, mare_shared::ui::Node>,
    frame: usize,
) -> Vec<Line> {
    let (mut body, notes) = render_parts_presented(src, width, theme, None, blocks, frame);
    join_blocks(&mut body, notes);
    body
}

fn render_parts_presented(
    src: &str,
    width: usize,
    theme: &Theme,
    mut highlight: Option<&mut super::highlight::StreamingHighlight>,
    blocks: &std::collections::BTreeMap<usize, mare_shared::ui::Node>,
    frame: usize,
) -> (Vec<Line>, Vec<Line>) {
    let width = width.max(4);
    let events = loose_footnote_refs(Parser::new_ext(src, options()).into_offset_iter().collect());
    let (mut body, mut notes) = (Vec::new(), Vec::new());
    let mut depth = 0usize;
    // the pictures are queued as they're drawn: all of an answer's in one pass, prepared side by
    // side
    for (ev, range) in events {
        let into_notes = depth > 0 || matches!(ev, Event::Start(Tag::FootnoteDefinition(_)));
        match &ev {
            Event::Start(Tag::FootnoteDefinition(_)) => depth += 1,
            Event::End(TagEnd::FootnoteDefinition) => depth = depth.saturating_sub(1),
            _ => {}
        }
        if into_notes { notes.push((ev, range)) } else { body.push((ev, range)) }
    }
    let body = run(&body, width, theme, src, highlight.as_deref_mut(), blocks, frame);
    let _offset = crate::image::row_offset(if body.is_empty() { 0 } else { body.len() + 1 });
    let notes = run(&notes, width, theme, src, highlight, blocks, frame);
    (body, notes)
}

fn run(
    events: &[Ev],
    width: usize,
    theme: &Theme,
    src: &str,
    highlight: Option<&mut super::highlight::StreamingHighlight>,
    blocks: &std::collections::BTreeMap<usize, mare_shared::ui::Node>,
    frame: usize,
) -> Vec<Line> {
    let mut r = Renderer::new(width, theme, src, highlight);
    r.presentations = blocks.clone();
    r.presentation_frame = frame;
    for i in 0..events.len() {
        let (ev, range) = events[i].clone();
        if let Event::Start(Tag::List(Some(_))) = &ev {
            r.next_list_width = ordered_list_width(&events[i..]);
        }
        r.range = range;
        r.event(ev);
    }
    r.finish()
}

/// Append `more` after `out` with one blank line between non-empty parts.
fn join_blocks(out: &mut Vec<Line>, more: Vec<Line>) {
    if !out.is_empty() && !more.is_empty() {
        out.push(Line::empty());
    }
    out.extend(more);
}

/// A `[^label]` whose definition is not in this source (a streamed block rendered on its own, or
/// a definition still to come) is shown like a resolved reference instead of as raw text.
fn loose_footnote_refs(events: Vec<Ev>) -> Vec<Ev> {
    type Pending = Option<(String, std::ops::Range<usize>)>;
    fn flush(pending: &mut Pending, out: &mut Vec<Ev>) {
        let Some((text, range)) = pending.take() else { return };
        let mut rest = text.as_str();
        while let Some(i) = rest.find("[^") {
            let after = &rest[i + 2..];
            let len = after.find(|c: char| c == ']' || c == '[' || c.is_whitespace()).unwrap_or(after.len());
            if len == 0 || !after[len..].starts_with(']') {
                out.push((Event::Text(rest[..i + 2].to_string().into()), range.clone()));
                rest = after;
                continue;
            }
            if i > 0 {
                out.push((Event::Text(rest[..i].to_string().into()), range.clone()));
            }
            out.push((Event::FootnoteReference(after[..len].to_string().into()), range.clone()));
            rest = &after[len + 1..];
        }
        if !rest.is_empty() {
            out.push((Event::Text(rest.to_string().into()), range));
        }
    }
    let mut out: Vec<Ev> = Vec::with_capacity(events.len());
    let mut code = 0usize;
    let mut pending: Pending = None;
    for (ev, range) in events {
        match ev {
            Event::Text(t) if code == 0 => match &mut pending {
                Some((s, r)) => {
                    s.push_str(&t);
                    r.end = range.end;
                }
                None => pending = Some((t.to_string(), range)),
            },
            ev => {
                flush(&mut pending, &mut out);
                match &ev {
                    Event::Start(Tag::CodeBlock(_)) => code += 1,
                    Event::End(TagEnd::CodeBlock) => code = code.saturating_sub(1),
                    _ => {}
                }
                out.push((ev, range));
            }
        }
    }
    flush(&mut pending, &mut out);
    out
}

/// Width of the largest number of the ordered list that starts at `events[0]` (right-aligned
/// numbers).
fn ordered_list_width(events: &[Ev]) -> usize {
    let Some((Event::Start(Tag::List(Some(start))), _)) = events.first() else { return 1 };
    let (mut depth, mut items) = (0usize, 0u64);
    for (ev, _) in events {
        match ev {
            Event::Start(Tag::List(_)) => depth += 1,
            Event::End(TagEnd::List(_)) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Start(Tag::Item) if depth == 1 => items += 1,
            _ => {}
        }
    }
    (start + items.saturating_sub(1)).to_string().len()
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF | 0x2E80..=0x2FFF | 0x3000..=0x303F | 0x3040..=0x30FF | 0x3100..=0x31FF
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xA960..=0xA97F | 0xAC00..=0xD7FF | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F | 0xFF00..=0xFFEF | 0x20000..=0x3FFFF)
}

/// A link target that is a local file path (shown in the inline-code style).
fn is_local_path(url: &str) -> bool {
    !url.is_empty() && !url.contains("://") && !url.starts_with('#') && !url.starts_with("mailto:")
}

struct Frame {
    first: Vec<Span>,
    rest: Vec<Span>,
    used: bool,
    is_item: bool,
}

struct ListState {
    next: Option<u64>,
    width: usize,
}

struct TableState {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Line>>,
    header_rows: usize,
    cur: Vec<Line>,
    source: String,
}

struct Link {
    url: String,
    /// where the link text starts: (inline line index, span count)
    text_start: (usize, usize),
    local: bool,
}

struct Renderer<'t> {
    width: usize,
    theme: &'t Theme,
    src: &'t str,
    range: std::ops::Range<usize>,
    out: Vec<Line>,
    /// rows of `out` that are code lines (never wrapped or cut here)
    code_rows: Vec<bool>,
    frames: Vec<Frame>,
    lists: Vec<ListState>,
    next_list_width: usize,
    styles: Vec<Style>,
    inline: Vec<Line>,
    pending_blank: bool,
    code: Option<(String, String)>, // (lang, text)
    /// the open code block has no closing fence yet (it's streaming)
    code_open: bool,
    code_start: usize,
    presentations: std::collections::BTreeMap<usize, mare_shared::ui::Node>,
    presentation_frame: usize,
    highlight: Option<&'t mut super::highlight::StreamingHighlight>,
    table: Option<TableState>,
    links: Vec<Link>,
    /// a local Markdown image being shown: its path (alt text is not printed)
    image: Option<String>,
}

impl<'t> Renderer<'t> {
    fn new(
        width: usize,
        theme: &'t Theme,
        src: &'t str,
        highlight: Option<&'t mut super::highlight::StreamingHighlight>,
    ) -> Self {
        Renderer {
            width,
            theme,
            src,
            range: 0..0,
            out: Vec::new(),
            code_rows: Vec::new(),
            frames: Vec::new(),
            lists: Vec::new(),
            next_list_width: 1,
            styles: vec![Style::new()],
            inline: Vec::new(),
            pending_blank: false,
            code: None,
            code_open: false,
            code_start: 0,
            presentations: std::collections::BTreeMap::new(),
            presentation_frame: 0,
            highlight,
            table: None,
            links: Vec::new(),
            image: None,
        }
    }

    fn st(&self, name: &str) -> Style {
        self.theme.style(name)
    }

    fn style(&self) -> Style {
        *self.styles.last().unwrap_or(&Style::new())
    }

    fn push_style(&mut self, f: impl FnOnce(Style) -> Style) {
        let s = f(self.style());
        self.styles.push(s);
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn prefix(&mut self, consume: bool) -> Vec<Span> {
        let mut p = Vec::new();
        for f in &mut self.frames {
            if consume && !f.used {
                p.extend(f.first.iter().cloned());
                f.used = true;
            } else {
                p.extend(f.rest.iter().cloned());
            }
        }
        p
    }

    fn prefix_width(&self) -> usize {
        self.frames.iter().map(|f| f.rest.iter().map(Span::width).sum::<usize>()).sum()
    }

    fn avail(&self) -> usize {
        self.width.saturating_sub(self.prefix_width()).max(1)
    }

    fn push_out(&mut self, line: Line, code: bool) {
        self.out.push(line);
        self.code_rows.push(code);
    }

    fn emit(&mut self, line: Line, consume: bool) {
        let p = self.prefix(consume);
        self.push_out(line.prefixed(p), false);
    }

    fn emit_code(&mut self, line: Line) {
        let p = self.prefix(true);
        let fill = line.fill_bg;
        let mut l = line.prefixed(p);
        l.fill_bg = fill;
        self.push_out(l, true);
    }

    fn emit_blank(&mut self) {
        let mut p = self.prefix(false);
        // keep quote gutters, drop pure indentation
        while let Some(last) = p.last_mut() {
            let t = last.text.trim_end().to_string();
            if t.is_empty() {
                p.pop();
            } else {
                last.text = t;
                break;
            }
        }
        self.push_out(Line::new(p), false);
    }

    /// Text with the current style; italics are dropped on CJK runs.
    fn text(&mut self, t: &str) {
        if self.image.is_some() {
            return;
        }
        let st = self.style();
        if self.inline.is_empty() {
            self.inline.push(Line::empty());
        }
        let line = self.inline.last_mut().unwrap();
        if !st.italic || !t.chars().any(is_cjk) {
            line.push(Span::styled(t, st));
            return;
        }
        let mut plain = st;
        plain.italic = false;
        let mut run = String::new();
        let mut run_cjk = None;
        for c in t.chars() {
            let cjk = is_cjk(c);
            if run_cjk.is_some_and(|r| r != cjk) {
                line.push(Span::styled(std::mem::take(&mut run), if run_cjk == Some(true) { plain } else { st }));
            }
            run_cjk = Some(cjk);
            run.push(c);
        }
        if !run.is_empty() {
            line.push(Span::styled(run, if run_cjk == Some(true) { plain } else { st }));
        }
    }

    fn hard_break(&mut self) {
        if self.inline.is_empty() {
            self.inline.push(Line::empty());
        }
        self.inline.push(Line::empty());
    }

    fn flush_inline(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let lines = std::mem::take(&mut self.inline);
        let avail = self.avail();
        for logical in lines {
            for wrapped in wrap_line(&logical, avail) {
                self.emit(wrapped, true);
            }
        }
    }

    fn block_start(&mut self) {
        self.flush_inline();
        if self.pending_blank && !self.out.is_empty() {
            self.emit_blank();
        }
        self.pending_blank = false;
    }

    fn event(&mut self, ev: Event) {
        if let Some((_, buf)) = &mut self.code {
            match ev {
                Event::Text(t) => {
                    buf.push_str(&t);
                    return;
                }
                Event::End(TagEnd::CodeBlock) => {}
                _ => return,
            }
        }
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) | Event::InlineMath(t) => {
                // no backticks; padding on both sides (Crush `styles.go:38-47`), plain spaces;
                // `$…$` math is not rendered either: its source shows in the code style
                let st = self.style().patch(self.st("md.code"));
                if self.image.is_none() {
                    if self.inline.is_empty() {
                        self.inline.push(Line::empty());
                    }
                    self.inline.last_mut().unwrap().push(Span::styled(format!(" {t} "), st));
                }
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.hard_break(),
            Event::Rule => {
                self.block_start();
                let w = self.avail().min(RULE_MAX);
                let g = self.glyph("rule");
                let line = Line::styled(g.repeat(w / str_width(&g).max(1)), self.st("md.rule"));
                self.emit(line, true);
                self.pending_blank = true;
            }
            Event::Html(h) | Event::InlineHtml(h) => {
                let text = strip_html_comments(&h);
                let st = self.st("md.html");
                for (i, part) in text.trim_end_matches('\n').split('\n').enumerate() {
                    if i > 0 {
                        self.hard_break();
                    }
                    self.styles.push(st);
                    self.text(part);
                    self.styles.pop();
                }
            }
            Event::TaskListMarker(checked) => {
                let (glyph, st) = if checked {
                    (format!("{} ", self.glyph("task_done")), self.st("md.task_done"))
                } else {
                    (format!("{} ", self.glyph("task_todo")), self.st("md.task_todo"))
                };
                if let Some(f) = self.frames.iter_mut().rev().find(|f| f.is_item)
                    && !f.used
                {
                    // the checkbox takes the bullet's place (Crush)
                    let w = str_width(&glyph);
                    f.first = vec![Span::styled(glyph.clone(), st)];
                    f.rest = vec![Span::raw(" ".repeat(w))];
                }
            }
            Event::FootnoteReference(r) => {
                let st = self.st("md.footnote");
                self.styles.push(st);
                self.text(&format!("[{r}]"));
                self.styles.pop();
            }
            Event::DisplayMath(m) => {
                // `$$…$$` is not rendered: its source shows as a code card
                self.flush_inline();
                self.render_code("", &m, false);
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.block_start(),
            Tag::Heading { level, .. } => {
                // a blank line before every heading except at the start
                if !self.out.is_empty() {
                    self.pending_blank = true;
                }
                self.block_start();
                let name = match level {
                    HeadingLevel::H1 => "md.h1",
                    HeadingLevel::H2 => "md.h2",
                    HeadingLevel::H3 => "md.h3",
                    HeadingLevel::H4 => "md.h4",
                    HeadingLevel::H5 => "md.h5",
                    HeadingLevel::H6 => "md.h6",
                };
                let st = self.st(name);
                self.styles.push(st);
            }
            Tag::BlockQuote(kind) => {
                self.block_start();
                let (bar_style, alert) = match kind {
                    Some(k) => {
                        let (key, title, style) = match k {
                            BlockQuoteKind::Note => ("note", "Note", "md.alert_note"),
                            BlockQuoteKind::Tip => ("tip", "Tip", "md.alert_tip"),
                            BlockQuoteKind::Important => ("important", "Important", "md.alert_important"),
                            BlockQuoteKind::Warning => ("warning", "Warning", "md.alert_warning"),
                            BlockQuoteKind::Caution => ("caution", "Caution", "md.alert_caution"),
                        };
                        (self.st(style), Some(format!("{} {title}", self.glyph(key))))
                    }
                    None => (self.st("md.quote_bar"), None),
                };
                let g = vec![Span::styled(format!("{} ", self.glyph("quote")), Style { bold: false, ..bar_style })];
                self.frames.push(Frame { first: g.clone(), rest: g, used: false, is_item: false });
                if let Some(title) = alert {
                    self.emit(Line::styled(title, bar_style.bold()), true);
                }
                let q = self.st("md.quote");
                self.styles.push(q);
            }
            Tag::CodeBlock(kind) => {
                self.block_start();
                self.code_start = self.range.start;
                let (lang, fenced) = match kind {
                    CodeBlockKind::Fenced(l) => (l.split_whitespace().next().unwrap_or("").to_string(), true),
                    CodeBlockKind::Indented => (String::new(), false),
                };
                // pulldown-cmark closes an unterminated fence at the end of the document: look
                // at the source for the closing fence (`lang ⋯` while the block streams).
                // Inside a blockquote, lines begin with `>` (plus optional space), so strip those
                // before inspecting opening/closing fences.
                self.code_open = fenced && {
                    fn strip_quote(mut s: &str) -> &str {
                        loop {
                            let t = s.trim_start();
                            if let Some(rest) = t.strip_prefix('>') {
                                s = rest.strip_prefix(' ').unwrap_or(rest);
                            } else {
                                return t;
                            }
                        }
                    }
                    let block = self.src.get(self.range.clone()).unwrap_or("");
                    let mut lines = block.trim_end().lines();
                    let first = lines.next().map(strip_quote).unwrap_or("");
                    let fence: String = first.chars().take_while(|c| *c == '`' || *c == '~').collect();
                    let last = lines.last().map(strip_quote).unwrap_or("");
                    last.is_empty()
                        || !(last.starts_with(&fence)
                            && last.trim_start_matches(fence.chars().next().unwrap_or('`')).is_empty())
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.block_start();
                } else {
                    self.flush_inline();
                }
                let width = if start.is_some() { self.next_list_width } else { 1 };
                self.lists.push(ListState { next: start, width });
            }
            Tag::Item => {
                self.flush_inline();
                if self.pending_blank && !self.out.is_empty() {
                    self.emit_blank();
                }
                self.pending_blank = false;
                let depth = self.lists.len().saturating_sub(1);
                let (marker, st) = match self.lists.last_mut() {
                    Some(ListState { next: Some(n), width }) => {
                        let m = format!("{:>w$}. ", n, w = *width);
                        *n += 1;
                        (m, self.st("md.number"))
                    }
                    _ => {
                        let g = self.glyph(["bullet1", "bullet2", "bullet3"][depth % 3]);
                        (format!("{g} "), self.st("md.bullet"))
                    }
                };
                let w = str_width(&marker);
                self.frames.push(Frame {
                    first: vec![Span::styled(marker, st)],
                    rest: vec![Span::raw(" ".repeat(w))],
                    used: false,
                    is_item: true,
                });
            }
            Tag::Table(aligns) => {
                self.block_start();
                let source = self.src.get(self.range.clone()).unwrap_or("").to_string();
                self.table = Some(TableState { aligns, rows: vec![], header_rows: 0, cur: vec![], source });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.cur.clear();
                }
            }
            Tag::TableCell => self.inline.clear(),
            Tag::Emphasis => self.push_style(|s| s.italic()),
            Tag::Strong => self.push_style(|s| s.bold()),
            Tag::Strikethrough => {
                let st = self.st("md.strike");
                self.push_style(|s| s.patch(st).strike())
            }
            Tag::Link { dest_url, .. } => {
                let pos = (self.inline.len(), self.inline.last().map_or(0, |l| l.spans.len()));
                let local = is_local_path(&dest_url);
                self.links.push(Link { url: dest_url.to_string(), text_start: pos, local });
                let st = if local {
                    self.st("md.path")
                } else {
                    let mut st = self.st("md.link");
                    // OSC 8: the text is clickable where the terminal supports it
                    if self.theme.md.osc8 && !dest_url.starts_with('#') {
                        st = st.link(&dest_url);
                    }
                    st
                };
                self.push_style(|s| s.patch(st));
            }
            Tag::Image { dest_url, .. } => {
                let url = dest_url.to_string();
                if is_local_path(&url) && std::path::Path::new(&url).is_file() {
                    self.image = Some(url);
                } else {
                    let st = self.st("md.image");
                    self.styles.push(st);
                    self.text("[image: ");
                }
            }
            Tag::FootnoteDefinition(label) => {
                self.block_start();
                let st = self.st("md.footnote");
                let first = format!("[{label}] ");
                let w = str_width(&first);
                self.frames.push(Frame {
                    first: vec![Span::styled(first, st)],
                    rest: vec![Span::raw(" ".repeat(w))],
                    used: false,
                    is_item: false,
                });
                self.styles.push(st);
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if self.table.is_none() {
                    self.flush_inline();
                    self.pending_blank = true;
                }
            }
            TagEnd::Heading(_) => {
                self.flush_inline();
                self.pop_style();
                self.pending_blank = true;
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.frames.pop();
                self.pop_style();
                self.pending_blank = true;
            }
            TagEnd::CodeBlock => {
                if let Some((lang, text)) = self.code.take() {
                    let open = self.code_open;
                    if let Some(node) = self.presentations.remove(&self.code_start) {
                        let _offset = crate::image::row_offset(self.out.len());
                        let suppressed = crate::image::suppressed();
                        let lines =
                            crate::render::render_node(&node, self.avail(), self.theme, self.presentation_frame);
                        if node.contains_images()
                            && !lines.iter().any(|line| line.raw.is_some())
                            && crate::image::suppressed() == suppressed
                        {
                            // Missing capability, failed decoding, or a pending artifact must
                            // never replace readable source with a generic image placeholder.
                            self.render_code(&lang, &text, open);
                        } else {
                            for line in lines {
                                self.emit(line, true);
                            }
                        }
                    } else {
                        self.render_code(&lang, &text, open);
                    }
                }
                self.pending_blank = true;
            }
            TagEnd::List(_) => {
                self.flush_inline();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.pending_blank = true;
                }
            }
            TagEnd::Item => {
                self.flush_inline();
                if let Some(f) = self.frames.last()
                    && f.is_item
                    && !f.used
                {
                    self.emit(Line::empty(), true);
                }
                self.frames.pop();
            }
            TagEnd::TableCell => {
                let mut cell = Line::empty();
                for (i, l) in std::mem::take(&mut self.inline).into_iter().enumerate() {
                    if i > 0 {
                        cell.push(Span::raw(" "));
                    }
                    for s in l.spans {
                        cell.push(s);
                    }
                }
                if let Some(t) = &mut self.table {
                    t.cur.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.cur);
                    t.rows.push(row);
                    t.header_rows += 1;
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.cur);
                    t.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.render_table(t);
                }
                self.pending_blank = true;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link => {
                self.pop_style();
                if let Some(link) = self.links.pop() {
                    let text: String = self
                        .inline
                        .iter()
                        .enumerate()
                        .skip(link.text_start.0.saturating_sub(1))
                        .flat_map(|(i, l)| {
                            let skip = if i + 1 == link.text_start.0 { link.text_start.1 } else { 0 };
                            l.spans.iter().skip(skip).map(|s| s.text.clone()).collect::<Vec<_>>()
                        })
                        .collect();
                    let url = link.url.trim_start_matches("mailto:");
                    // without OSC 8 the URL follows the text, dim; the same text and URL once
                    let osc8 = self.theme.md.osc8 && mare_shared::ansi::valid_link(url) && !link.local;
                    if !link.local && !osc8 && !url.is_empty() && url != text && !url.starts_with('#') {
                        let st = self.st("md.link_url");
                        self.styles.push(st);
                        self.text(&format!(" ({url})"));
                        self.styles.pop();
                    }
                }
            }
            TagEnd::Image => {
                if let Some(path) = self.image.take() {
                    // a local image: its own rows
                    self.flush_inline();
                    let _offset = crate::image::row_offset(self.out.len());
                    let avail = self.avail();
                    let lines =
                        super::image::render(&super::image::Source::Path(path), IMAGE_ROWS, avail, avail, self.theme);
                    for l in lines {
                        self.emit(l, true);
                    }
                } else {
                    self.text("]");
                    self.pop_style();
                }
            }
            TagEnd::FootnoteDefinition => {
                self.flush_inline();
                self.frames.pop();
                self.pop_style();
                self.pending_blank = true;
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Line> {
        if let Some((lang, text)) = self.code.take() {
            // the fence is still open: the block is streaming
            self.render_code(&lang, &text, true);
        }
        if let Some(t) = self.table.take() {
            self.render_table(t);
        }
        self.flush_inline();
        let w = self.width;
        self.out
            .into_iter()
            .zip(self.code_rows)
            .map(|(l, code)| if !code && l.width() > w { truncate_line(&l, w) } else { l })
            .collect()
    }
}

/// HTML shown as dim text without its comments.
fn strip_html_comments(h: &str) -> String {
    let mut out = String::new();
    let mut rest = h;
    while let Some(i) = rest.find("<!--") {
        out.push_str(&rest[..i]);
        match rest[i..].find("-->") {
            Some(j) => rest = &rest[i + j + 3..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests;

/// Every element of, in the dark and light 薄暮 palettes and with `ambiwidth =
/// "double"` — screen-text snapshots (text plus styled runs).
#[cfg(test)]
mod style_snapshots;
