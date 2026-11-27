//! Code blocks: a code card with its language label and optional line numbers.

use super::*;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct CodeCardStyle {
    pub(super) card: Style,
    pub(super) line_nr: Style,
    pub(super) edge: Style,
    pub(super) lang: Style,
    pub(super) bg: Option<Color>,
    pub(super) numbers: bool,
    pub(super) nerd_icons: bool,
    pub(super) top_glyph: String,
    pub(super) bottom_glyph: String,
    pub(super) streaming_glyph: String,
}

pub(super) fn code_card_style(theme: &Theme) -> CodeCardStyle {
    CodeCardStyle {
        card: theme.style("md.code_block"),
        line_nr: theme.style("md.code_line_nr"),
        edge: theme.style("md.code_edge"),
        lang: theme.style("md.code_lang"),
        bg: theme.style("md.code_block").bg.or(theme.style("code_bg").bg),
        numbers: theme.md.code_line_numbers,
        nerd_icons: theme.icons == "nerd",
        top_glyph: glyphs::theme_glyph(theme, "code_top"),
        bottom_glyph: glyphs::theme_glyph(theme, "code_bottom"),
        streaming_glyph: glyphs::theme_glyph(theme, "streaming"),
    }
}

pub(super) fn format_code_card_line(spans_in: Vec<Span>, idx: usize, nw: usize, st: &CodeCardStyle) -> Line {
    let mut spans = Vec::with_capacity(spans_in.len() + 2 + usize::from(st.numbers));
    spans.push(Span::raw(" "));
    if st.numbers {
        spans.push(Span::styled(format!("{:>nw$} ", idx + 1), st.line_nr));
    }
    spans.extend(spans_in);
    spans.push(Span::raw(" "));
    let mut line = Line::new(spans).base_style(st.card);
    if let Some(c) = st.bg {
        line.fill_bg = Some(c);
    }
    line
}

pub(super) fn code_edge_line(glyph: &str, avail: usize, style: Style) -> Line {
    Line::styled(glyph.repeat(avail / str_width(glyph).max(1)), style)
}

pub(super) fn code_label(lang: &str, streaming: bool, st: &CodeCardStyle) -> String {
    if lang.is_empty() {
        return if streaming { st.streaming_glyph.clone() } else { String::new() };
    }
    let base = match st.nerd_icons.then(|| glyphs::code_lang_nerd_icon(lang)).flatten() {
        Some(icon) => format!("{icon} {lang}"),
        None => lang.to_string(),
    };
    if streaming { format!("{base} {}", st.streaming_glyph) } else { base }
}

pub(super) fn top_label_row(first_w: usize, avail: usize, label: &str, st: &CodeCardStyle) -> Option<Line> {
    let label_w = str_width(label);
    if label_w == 0 || first_w + label_w + 2 <= avail {
        return None;
    }
    let shown = if label_w + 1 > avail { truncate_str(label, avail.saturating_sub(1), "") } else { label.to_string() };
    let shown_w = str_width(&shown);
    let pad = avail.saturating_sub(shown_w + 1);
    let lab = Span::styled(format!("{shown} "), st.card.patch(st.lang));
    let mut line = Line::new(vec![Span::styled(" ".repeat(pad), st.card), lab]);
    if let Some(c) = st.bg {
        line.fill_bg = Some(c);
    }
    Some(line)
}

pub(super) fn attach_inline_label(first_row: &mut Line, first_w: usize, avail: usize, label: &str, st: &CodeCardStyle) {
    let label_w = str_width(label);
    if label_w > 0 && first_w + label_w + 2 <= avail {
        let lab = Span::styled(format!("{label} "), st.card.patch(st.lang));
        let pad = avail - first_w - label_w - 1;
        first_row.spans.push(Span::styled(" ".repeat(pad), st.card));
        first_row.spans.push(lab);
    }
}

impl Renderer<'_> {
    /// A code block as a card: `code_bg` rows between `▄` / `▀` edges, the language
    /// top right (`lang ⋯` while it streams), optional line numbers, never wrapped.
    pub(super) fn render_code(&mut self, lang: &str, text: &str, streaming: bool) {
        let text = text.strip_suffix('\n').unwrap_or(text);
        let avail = self.avail();
        let st = code_card_style(self.theme);
        let mut lines = if streaming {
            match self.highlight.as_deref_mut() {
                Some(h) => h.highlight(text, lang, self.theme),
                None => highlight(text, lang, self.theme),
            }
        } else {
            highlight(text, lang, self.theme)
        };
        if lines.is_empty() {
            lines.push(Line::empty());
        }
        let nw = lines.len().to_string().len();
        // padding: one cell each side
        let mut body: Vec<Line> =
            lines.into_iter().enumerate().map(|(i, l)| format_code_card_line(l.spans, i, nw, &st)).collect();
        let label = code_label(lang, streaming, &st);
        self.emit_code(code_edge_line(&st.top_glyph, avail, st.edge));
        let first_w = body.first().map(Line::width).unwrap_or(0);
        if let Some(extra) = top_label_row(first_w, avail, &label, &st) {
            body.insert(0, extra);
        } else if let Some(first_row) = body.first_mut() {
            attach_inline_label(first_row, first_w, avail, &label, &st);
        }
        for l in body {
            self.emit_code(l);
        }
        self.emit_code(code_edge_line(&st.bottom_glyph, avail, st.edge));
    }
}
