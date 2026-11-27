//! Screen-text snapshots for tests. Each line is
//! its text, then the styled runs on it — color, background and attributes — so a theme or style
//! change shows up in the diff while layout stays readable. Used by the renderer's own tests and
//! the TUI end-to-end harness.

use super::style::{Color, Line, Style};

fn color(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

fn style_tag(s: &Style) -> Option<String> {
    let mut t = vec![];
    if let Some(c) = s.fg {
        t.push(color(c));
    }
    if let Some(c) = s.bg {
        t.push(format!("bg{}", color(c)));
    }
    for (on, name) in
        [(s.bold, "b"), (s.italic, "i"), (s.underline, "u"), (s.strike, "s"), (s.dim, "d"), (s.link != 0, "link")]
    {
        if on {
            t.push(name.into());
        }
    }
    (!t.is_empty()).then(|| t.join(","))
}

/// Lines as `|text|` plus, below a line with styling, `  ⟶ "run"=style …` (fill and raw noted).
pub fn describe(lines: &[Line]) -> String {
    let mut out = String::new();
    for l in lines {
        out.push('|');
        out.push_str(&l.plain());
        out.push_str("|\n");
        let mut runs: Vec<String> = l
            .spans
            .iter()
            .filter(|s| !s.text.trim().is_empty() || s.style.bg.is_some())
            .filter_map(|s| style_tag(&s.style).map(|t| format!("{:?}={t}", s.text)))
            .collect();
        if let Some(bg) = l.fill_bg {
            runs.push(format!("fill={}", color(bg)));
        }
        if l.raw.is_some() {
            runs.push("raw".into());
        }
        if !runs.is_empty() {
            out.push_str("  ⟶ ");
            out.push_str(&runs.join(" "));
            out.push('\n');
        }
    }
    out
}
