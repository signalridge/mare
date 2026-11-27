//! Generic fallback structures for UI components when Lua returns nil/error or is unavailable
//! :
//! - `Frame { title, border, body }`: bordered box with title and body
//! - `Choice { options, selected, gap }`: selectable options; selected uses prefix marker + reverse
//! - `Lines(Vec<Line>)`: wrapped text lines

use mare_shared::style::{Line, Span, Theme, wrap_line};

use crate::frame;

/// Structural fallback representation rendered by the core without Lua.
#[derive(Clone, Debug)]
pub enum Fallback {
    /// A bordered frame with title, named border style, and nested fallback body.
    Frame { title: Vec<Span>, border: String, body: Vec<Self> },
    /// A horizontal list of selectable options; the selected option has a prefix marker + reverse.
    Choice { options: Vec<String>, selected: usize, gap: usize },
    /// Plain or styled lines, word-wrapped to the available width.
    Lines(Vec<Line>),
}

impl Fallback {
    pub const CHOICE_MARKER: &'static str = "▸ ";
    pub const CHOICE_MARKER_ASCII: &'static str = "> ";
    pub const CHOICE_PAD: &'static str = "  ";
    pub const DEFAULT_GAP: usize = 3;

    pub fn frame(title: Vec<Span>, border: impl Into<String>, body: Vec<Self>) -> Self {
        Self::Frame { title, border: border.into(), body }
    }

    pub fn choice(options: Vec<String>, selected: usize) -> Self {
        Self::Choice { options, selected, gap: Self::DEFAULT_GAP }
    }

    pub fn lines(lines: Vec<Line>) -> Self {
        Self::Lines(lines)
    }
}

/// Build one horizontally centered line of selectable choice options using `MareSelection` + `MareMuted`.
pub fn choice_line(options: &[String], selected: usize, gap: usize, width: usize, theme: &Theme) -> Line {
    if options.is_empty() {
        return Line::empty();
    }
    let marker = if theme.icons == "ascii" { Fallback::CHOICE_MARKER_ASCII } else { Fallback::CHOICE_MARKER };
    let mut spans = Vec::new();
    let sep = " ".repeat(gap);
    for (i, label) in options.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(sep.clone()));
        }
        let on = i == selected;
        let prefix = if on { marker } else { Fallback::CHOICE_PAD };
        let style = if on { theme.style("MareSelection").bold().reverse() } else { theme.style("MareMuted") };
        spans.push(Span::styled(format!("{prefix}{label} "), style));
    }
    let line = Line::new(spans);
    let pad = width.saturating_sub(line.width()) / 2;
    if pad > 0 { line.prefixed(vec![Span::raw(" ".repeat(pad))]) } else { line }
}

/// Render a generic fallback structure into lines at `width` columns.
pub fn render(fb: &Fallback, width: usize, theme: &Theme) -> Vec<Line> {
    let w = width.max(1);
    match fb {
        Fallback::Frame { title, border, body } => {
            let inner = w.saturating_sub(4).max(1);
            let mut rows = Vec::new();
            for part in body {
                rows.extend(render(part, inner, theme));
            }
            let mut opts = frame::BoxOpts::for_theme(theme, theme.style(border));
            opts.title = title.clone();
            frame::boxed(&rows, w, &opts)
        }
        Fallback::Choice { options, selected, gap } => {
            if options.is_empty() {
                return vec![];
            }
            let centered = choice_line(options, *selected, *gap, w, theme);
            wrap_line(&centered, w)
        }
        Fallback::Lines(lines) => {
            let mut out = Vec::new();
            for line in lines {
                if line.spans.is_empty() {
                    out.push(Line::empty());
                } else {
                    out.extend(wrap_line(line, w));
                }
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> String {
        lines.iter().map(|l| l.spans.iter().map(|s| s.text.as_str()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn choice_marks_selected_with_prefix_and_reverse_style() {
        let theme = Theme::default();
        let fb = Fallback::choice(vec!["[Y]es".into(), "[N]o".into()], 1);
        let lines = render(&fb, 40, &theme);
        let text = plain(&lines);
        assert!(text.contains("  [Y]es"), "unselected option has space pad: {text}");
        assert!(text.contains("▸ [N]o"), "selected option has prefix marker: {text}");
        let selected_span = lines[0].spans.iter().find(|s| s.text.contains("[N]o")).expect("selected span present");
        assert!(selected_span.style.reverse, "selected option must use reverse style");
    }

    #[test]
    fn frame_wraps_lines_and_choice_with_border_and_title() {
        let theme = Theme::default();
        let fb = Fallback::frame(
            vec![Span::styled("Confirm", theme.style("MareTitle"))],
            "MareBorderActive",
            vec![
                Fallback::lines(vec![Line::empty(), Line::raw("Delete file?"), Line::empty()]),
                Fallback::choice(vec!["[Y]es".into(), "[N]o".into()], 0),
            ],
        );
        let lines = render(&fb, 40, &theme);
        let text = plain(&lines);
        assert!(text.contains("Confirm"), "title in top border:\n{text}");
        assert!(text.contains("Delete file?"), "body line:\n{text}");
        assert!(text.contains("▸ [Y]es"), "selected choice:\n{text}");
        assert!(text.starts_with('╭') || text.starts_with('+'), "has top border:\n{text}");
    }

    #[test]
    fn lines_wrap_double_width_cjk_within_narrow_width() {
        let theme = Theme::default();
        let fb = Fallback::lines(vec![Line::raw("确认删除这个文件吗")]);
        let lines = render(&fb, 8, &theme);
        assert!(lines.len() >= 2, "CJK text should wrap into multiple lines at width 8");
        for l in &lines {
            assert!(l.width() <= 8, "wrapped line width {} exceeds 8", l.width());
        }
    }
}
