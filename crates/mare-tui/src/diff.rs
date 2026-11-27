//! Diff rendering: line-number gutter, +/- markers, add/remove rows painted with a background.

use similar::{ChangeTag, TextDiff};

use super::highlight::{exceeds_aggregate_highlight_limits, highlight_line_for_path};
use super::style::*;

const CONTEXT: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ctx,
    Add,
    Del,
}

struct Row<'a> {
    kind: Kind,
    num: usize,
    text: &'a str,
    /// for a changed line paired with its counterpart: byte ranges (in the cleaned text) of the
    /// words that differ — painted with a stronger background (word-level diff)
    words: Vec<std::ops::Range<usize>>,
}

fn clean(text: &str) -> String {
    expand_tabs(&sanitize(text.trim_end_matches(['\n', '\r'])))
}

/// Byte ranges of the words that differ between a removed and an added line.
fn word_changes(old: &str, new: &str) -> (Vec<std::ops::Range<usize>>, Vec<std::ops::Range<usize>>) {
    let d = TextDiff::from_words(old, new);
    // pointless when almost everything changed: the line background already says it
    if d.ratio() < 0.4 {
        return (vec![], vec![]);
    }
    let (mut o, mut n) = (vec![], vec![]);
    let (mut oi, mut ni) = (0usize, 0usize);
    for c in d.iter_all_changes() {
        let len = c.value().len();
        match c.tag() {
            ChangeTag::Equal => {
                oi += len;
                ni += len;
            }
            ChangeTag::Delete => {
                if !c.value().trim().is_empty() {
                    o.push(oi..oi + len);
                }
                oi += len;
            }
            ChangeTag::Insert => {
                if !c.value().trim().is_empty() {
                    n.push(ni..ni + len);
                }
                ni += len;
            }
        }
    }
    (o, n)
}

/// Overlay `st` on the parts of `line` that fall in `ranges` (byte offsets into its text).
fn emphasize(line: Line, ranges: &[std::ops::Range<usize>], st: Style) -> Line {
    if ranges.is_empty() {
        return line;
    }
    let mut out = Line { spans: vec![], fill_bg: line.fill_bg, raw: None };
    let mut pos = 0usize;
    for span in line.spans {
        let mut start = 0usize;
        let bytes = span.text.len();
        // split the span at range boundaries
        let mut cuts: Vec<usize> = ranges
            .iter()
            .flat_map(|r| [r.start, r.end])
            .filter(|&b| b > pos && b < pos + bytes)
            .map(|b| b - pos)
            .filter(|&b| span.text.is_char_boundary(b))
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        cuts.push(bytes);
        for end in cuts {
            let piece = &span.text[start..end];
            let at = pos + start;
            let hit = ranges.iter().any(|r| at >= r.start && at < r.end);
            out.push(Span::styled(piece, if hit { span.style.patch(st) } else { span.style }));
            start = end;
        }
        pos += bytes;
    }
    out
}

fn render_row(row: &Row, gutter: usize, path: Option<&str>, width: usize, theme: &Theme) -> Line {
    let text = clean(row.text);
    let mut content = match path {
        Some(p) => highlight_line_for_path(&text, p, theme),
        None => Line::styled(text, theme.fg("text")),
    };
    if !row.words.is_empty() {
        let st = theme.style(if row.kind == Kind::Add { "diff_add_word" } else { "diff_del_word" });
        content = emphasize(content, &row.words, st);
    }
    let (sign, sign_st, bg) = match row.kind {
        Kind::Ctx => (" ", theme.fg("dim"), None),
        Kind::Add => ("+", theme.fg("diff_add_fg"), Some(theme.get("diff_add_bg"))),
        Kind::Del => ("-", theme.fg("diff_del_fg"), Some(theme.get("diff_del_bg"))),
    };
    let scopes: &[&str] = match row.kind {
        Kind::Add => &["markup.inserted", "diff.inserted"],
        Kind::Del => &["markup.deleted", "diff.deleted"],
        Kind::Ctx => &[],
    };
    let mut sign_st = crate::highlight::scope_style(theme, scopes, sign_st);
    // Alpha 0x01 (#00000001, decoded as Color::Reset) disables the diff fill and gutter bg.
    let bg = match sign_st.bg {
        Some(Color::Reset) => {
            sign_st.bg = None;
            None
        }
        Some(c) => Some(c),
        None => bg,
    };
    let mut line = Line::new(vec![
        Span::styled(format!("{:>gutter$} ", row.num), theme.fg("dim")),
        Span::styled(format!("{sign} "), sign_st),
    ]);
    for s in content.spans {
        line.push(s);
    }
    if let Some(bg) = bg {
        let mut b = Style::new();
        b.bg = Some(bg);
        line = line.base_style(b).with_fill(bg);
    } else if row.kind == Kind::Ctx {
        line = line.base_style(Style::new().dim());
    }
    if line.width() > width { truncate_line(&line, width) } else { line }
}

fn separator(gutter: usize, theme: &Theme) -> Line {
    let ch = if theme.icons == "ascii" { ":" } else { "⋮" };
    Line::styled(format!("{:>gutter$}", ch), theme.fg("dim"))
}

fn digits(n: usize) -> usize {
    n.max(1).to_string().len()
}

/// "+N −M" summary above an edit.
fn stats_line(adds: usize, dels: usize, theme: &Theme) -> Line {
    let minus = if theme.icons == "ascii" { "-" } else { "−" };
    Line::new(vec![
        Span::styled(format!("+{adds}"), theme.fg("diff_add_fg")),
        Span::styled(" ", Style::new()),
        Span::styled(format!("{minus}{dels}"), theme.fg("diff_del_fg")),
    ])
}

/// Render a diff between two texts with 3 lines of context.
pub fn render_diff(old: &str, new: &str, path: Option<&str>, width: usize, theme: &Theme) -> Vec<Line> {
    let diff = TextDiff::from_lines(old, new);
    let gutter = digits(old.lines().count().max(new.lines().count()));
    let (adds, dels) = mare_edit::text_diff_stats(&diff);
    let mut out = vec![stats_line(adds, dels, theme)];
    let skip_hl = exceeds_aggregate_highlight_limits(old.len() + new.len(), old.lines().count() + new.lines().count());
    let hl_path = if skip_hl { None } else { path };
    for (gi, group) in diff.grouped_ops(CONTEXT).iter().enumerate() {
        if gi > 0 {
            out.push(separator(gutter, theme));
        }
        for op in group {
            let changes: Vec<_> = diff.iter_changes(op).collect();
            // a replaced block: pair the i-th removed line with the i-th added one
            let dels: Vec<usize> = (0..changes.len()).filter(|&i| changes[i].tag() == ChangeTag::Delete).collect();
            let adds: Vec<usize> = (0..changes.len()).filter(|&i| changes[i].tag() == ChangeTag::Insert).collect();
            let mut words: Vec<Vec<std::ops::Range<usize>>> = vec![vec![]; changes.len()];
            for (&d, &a) in dels.iter().zip(&adds) {
                let (o, n) = word_changes(&clean(changes[d].value()), &clean(changes[a].value()));
                words[d] = o;
                words[a] = n;
            }
            for (i, change) in changes.iter().enumerate() {
                let (kind, num) = match change.tag() {
                    ChangeTag::Equal => (Kind::Ctx, change.new_index().unwrap_or(0) + 1),
                    ChangeTag::Insert => (Kind::Add, change.new_index().unwrap_or(0) + 1),
                    ChangeTag::Delete => (Kind::Del, change.old_index().unwrap_or(0) + 1),
                };
                let row = Row { kind, num, text: change.value(), words: std::mem::take(&mut words[i]) };
                out.push(render_row(&row, gutter, hl_path, width, theme));
            }
        }
    }
    out
}

fn parse_hunk(h: &str) -> Option<(usize, usize, usize, usize)> {
    // @@ -a,b +c,d @@
    let inner = h.strip_prefix("@@ ")?;
    let end = inner.find(" @@")?;
    let mut parts = inner[..end].split(' ');
    let range = |s: &str| -> Option<(usize, usize)> {
        let s = s.get(1..)?;
        let mut it = s.split(',');
        let a = it.next()?.parse().ok()?;
        let b = it.next().map(|x| x.parse().unwrap_or(1)).unwrap_or(1);
        Some((a, b))
    };
    let (a, b) = range(parts.next()?)?;
    let (c, d) = range(parts.next()?)?;
    Some((a, b, c, d))
}

/// Render an existing unified diff (e.g. from `git diff` or a patch). Replaced lines get the
/// same word-level highlight as [`render_diff`]. `path` overrides the `+++` headers for
/// highlighting and `lines` (the longer side's line count) sizes the gutter: with both, a
/// 3-line-context diff of two texts renders exactly like [`render_diff`] of them (a stored edit
/// result).
pub fn render_unified(
    diff: &str,
    path_override: Option<&str>,
    lines: Option<usize>,
    width: usize,
    theme: &Theme,
) -> Vec<Line> {
    let gutter = digits(lines.unwrap_or_else(|| {
        let mut max_line = 1;
        for l in diff.lines() {
            if let Some((a, b, c, d)) = parse_hunk(l) {
                max_line = max_line.max(a.saturating_add(b)).max(c.saturating_add(d));
            }
        }
        max_line
    }));
    // rows per hunk; None = separator
    enum Item<'a> {
        Sep,
        Row(Kind, usize, &'a str, Option<String>),
    }
    let mut items: Vec<Item> = vec![];
    let (mut old_n, mut new_n) = (0usize, 0usize);
    let mut hunks = 0;
    let mut in_hunk = false;
    let mut path: Option<String> = path_override.map(str::to_string);
    for l in diff.lines() {
        if let Some((a, _, c, _)) = parse_hunk(l) {
            if hunks > 0 {
                items.push(Item::Sep);
            }
            hunks += 1;
            old_n = a;
            new_n = c;
            in_hunk = true;
            continue;
        }
        if l.starts_with("diff ") || l.starts_with("index ") {
            in_hunk = false;
            continue;
        }
        if !in_hunk && (l.starts_with("--- ") || l.starts_with("+++ ")) {
            if let Some(p) = l.strip_prefix("+++ ")
                && path_override.is_none()
            {
                let p = p.trim_start_matches("b/").trim();
                if p != "/dev/null" {
                    path = Some(p.to_string());
                }
            }
            continue;
        }
        if !in_hunk || l.starts_with('\\') {
            continue;
        }
        let (kind, num, text) = match l.chars().next() {
            Some('+') => {
                let num = new_n;
                new_n = new_n.saturating_add(1);
                (Kind::Add, num, &l[1..])
            }
            Some('-') => {
                let num = old_n;
                old_n = old_n.saturating_add(1);
                (Kind::Del, num, &l[1..])
            }
            _ => {
                let num = new_n;
                old_n = old_n.saturating_add(1);
                new_n = new_n.saturating_add(1);
                (Kind::Ctx, num, l.get(1..).unwrap_or(""))
            }
        };
        items.push(Item::Row(kind, num, text, path.clone()));
    }
    // counted from the hunk rows: a removed `-- comment` line is `--- comment` in the text
    let count = |k: Kind| items.iter().filter(|it| matches!(it, Item::Row(kind, ..) if *kind == k)).count();
    let mut out = vec![stats_line(count(Kind::Add), count(Kind::Del), theme)];
    // word-level pairs: a run of removed lines followed by a run of added lines
    let mut words: Vec<Vec<std::ops::Range<usize>>> = vec![vec![]; items.len()];
    let kind_at = |i: usize| match &items[i] {
        Item::Row(k, ..) => Some(*k),
        Item::Sep => None,
    };
    let text_at = |i: usize| match &items[i] {
        Item::Row(_, _, t, _) => clean(t),
        Item::Sep => String::new(),
    };
    let mut i = 0;
    while i < items.len() {
        if kind_at(i) == Some(Kind::Del) {
            let ds = i;
            while i < items.len() && kind_at(i) == Some(Kind::Del) {
                i += 1;
            }
            let as_ = i;
            while i < items.len() && kind_at(i) == Some(Kind::Add) {
                i += 1;
            }
            for k in 0..(as_ - ds).min(i - as_) {
                let (o, n) = word_changes(&text_at(ds + k), &text_at(as_ + k));
                words[ds + k] = o;
                words[as_ + k] = n;
            }
        } else {
            i += 1;
        }
    }
    let skip_hl = exceeds_aggregate_highlight_limits(diff.len(), diff.lines().count());
    for (idx, it) in items.iter().enumerate() {
        match it {
            Item::Sep => out.push(separator(gutter, theme)),
            Item::Row(kind, num, text, p) => {
                let row = Row { kind: *kind, num: *num, text, words: std::mem::take(&mut words[idx]) };
                let hl_path = if skip_hl { None } else { p.as_deref() };
                out.push(render_row(&row, gutter, hl_path, width, theme));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_level_changes() {
        let (o, n) = word_changes("let x = foo(1, 2);", "let x = bar(1, 2);");
        assert_eq!(o.len(), 1);
        assert_eq!(n.len(), 1);
        assert!(&"let x = foo(1, 2);"[o[0].clone()].contains("foo"));
        let theme = Theme::default();
        let lines = render_diff("let x = foo(1, 2);\n", "let x = bar(1, 2);\n", None, 80, &theme);
        let word_bg = theme.style("diff_add_word").bg;
        assert_eq!(lines[0].plain(), "+1 −1");
        assert!(lines[2].spans.iter().any(|s| s.style.bg == word_bg && s.text.contains("bar")));
    }

    fn sample() -> (String, String) {
        let old: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 5\n", "line five 中文\n").replace("line 25\n", "").replace(
            "line 26\n",
            "line 26\nadded a very long line that goes beyond the width of the terminal for sure yes\n",
        );
        (old, new)
    }

    #[test]
    fn diff_basic() {
        let t = Theme::default();
        let (old, new) = sample();
        let lines = render_diff(&old, &new, Some("a.rs"), 40, &t);
        let p: Vec<String> = lines.iter().map(|l| l.plain()).collect();
        assert!(p.iter().any(|l| l.starts_with(" 5 - line 5")));
        assert!(p.iter().any(|l| l.starts_with(" 5 + line five")));
        assert!(p.iter().any(|l| l.trim() == "⋮"));
        assert!(lines.iter().all(|l| l.width() <= 40));
        assert!(lines.iter().any(|l| l.fill_bg == Some(t.get("diff_add_bg"))));
        assert_eq!(mare_edit::diff_stats(&old, &new), (2, 2));
    }

    #[test]
    fn unified() {
        let t = Theme::default();
        let (old, new) = sample();
        let u =
            TextDiff::from_lines(&old, &new).unified_diff().context_radius(3).header("a/x.rs", "b/x.rs").to_string();
        let a: Vec<String> = render_unified(&u, None, None, 60, &t).iter().map(|l| l.plain()).collect();
        let b: Vec<String> = render_diff(&old, &new, Some("x.rs"), 60, &t).iter().map(|l| l.plain()).collect();
        assert_eq!(a, b);
    }

    /// A stored edit result (`mare.text.diff` + path + line count) renders exactly — styles,
    /// gutter, counts, word highlights — like the two texts it came from: random
    /// edits over lines that look like diff headers (`-- x`, `++y`), CRLF, no final newline.
    #[test]
    fn stored_unified_renders_like_the_texts() {
        let t = Theme::default();
        let pool = ["a\n", "b b\n", "-- c\n", "++d\n", "@@ e\n", "\n", "f\r\n", "h\r", "let x = foo(1);\n", "\t g\n"];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..400 {
            let old: Vec<&str> = (0..next(40)).map(|_| pool[next(pool.len())]).collect();
            let mut new = old.clone();
            for _ in 0..1 + next(4) {
                let at = next(new.len() + 1);
                match next(3) {
                    0 if at < new.len() => drop(new.remove(at)),
                    1 if at < new.len() => new[at] = pool[next(pool.len())],
                    _ => new.insert(at, pool[next(pool.len())]),
                }
            }
            let (mut old, mut new) = (old.concat(), new.concat());
            if next(4) == 0 {
                old = old.trim_end_matches('\n').to_string();
            }
            if next(4) == 0 {
                new = new.trim_end_matches('\n').to_string();
            }
            let e = mare_edit::unified_edit(&old, &new, 3, "x");
            for path in [Some("x.lua"), Some("x.rs")] {
                let want = render_diff(&old, &new, path, 50, &t);
                assert_eq!(render_unified(&e.text, path, Some(e.lines), 50, &t), want, "{old:?} -> {new:?}");
            }
            assert_eq!((e.added, e.removed), mare_edit::diff_stats(&old, &new));
        }
    }

    #[test]
    fn terminal_default_diff_background_clears_fill_and_gutter_bg() {
        let doc = |bg: &str| {
            format!(
                r##"<?xml version="1.0"?>
<plist version="1.0"><dict><key>settings</key><array>
<dict><key>settings</key><dict><key>foreground</key><string>#D0D0D0</string></dict></dict>
<dict><key>scope</key><string>markup.inserted</string><key>settings</key><dict><key>foreground</key><string>#00AF00</string><key>background</key><string>{bg}</string></dict></dict>
<dict><key>scope</key><string>markup.deleted</string><key>settings</key><dict><key>foreground</key><string>#FF5F5F</string><key>background</key><string>{bg}</string></dict></dict>
</array></dict></plist>"##
            )
        };
        let no_fill = Theme { syntax: Some(doc("#00000001")), ..Default::default() };
        let lines = render_diff("old\n", "new\n", None, 40, &no_fill);
        for line in &lines[1..] {
            assert!(line.fill_bg.is_none(), "terminal-default diff bg must clear line fill: {line:?}");
            assert!(
                line.spans.iter().take(2).all(|s| s.style.bg.is_none()),
                "gutter/sign bg must stay unset when diff fill is disabled: {line:?}"
            );
        }

        let tinted = Theme { syntax: Some(doc("#213A2B")), ..Default::default() };
        let tinted_lines = render_diff("", "new\n", None, 40, &tinted);
        assert!(tinted_lines.iter().any(|l| l.fill_bg == Some(Color::Rgb(0x21, 0x3a, 0x2b))));
    }

    #[test]
    fn render_unified_ignores_malformed_hunk_headers_without_panicking() {
        let theme = Theme::default();
        for bad in [
            "@@  @@\n+added\n",
            "@@  -1 +1 @@\n+added\n",
            "@@ é1 +1 @@\n+added\n",
            "@@ -1 é1 @@\n+added\n",
            "@@ - + @@\n+added\n",
            "@@ -18446744073709551615,1 +1,1 @@\n+added\n",
            "@@ -1,1 +1,1 @@\n中文无前导空格\n+added\n",
        ] {
            let lines = render_unified(bad, Some("a.rs"), None, 60, &theme);
            assert!(!lines.is_empty(), "expected stats line for {bad:?}");
        }
        assert!(parse_hunk("@@  @@").is_none());
        assert!(parse_hunk("@@  -1 +1 @@").is_none());
        assert!(parse_hunk("@@ é1 +1 @@").is_none());
    }
}
