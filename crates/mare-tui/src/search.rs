//! Search inside the Ctrl+T transcript overlay: the
//! scrollback only holds the compact form, so expanded content can't be found with the
//! terminal's own search. `/` searches, `n` / `N` jump, matches are highlighted.
//! Matching is a case-insensitive substring match (smart case: an uppercase letter in the query
//! makes it case-sensitive), like list searches.

use crate::style::{Line, Span, Style};

fn needle(query: &str) -> (String, bool) {
    let sensitive = query.chars().any(char::is_uppercase);
    (if sensitive { query.to_string() } else { query.to_lowercase() }, sensitive)
}

fn hay(text: &str, sensitive: bool) -> String {
    if sensitive { text.to_string() } else { text.to_lowercase() }
}

/// Indexes of the lines containing `query`.
pub fn find(lines: &[Line], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return vec![];
    }
    let (q, sensitive) = needle(query);
    lines.iter().enumerate().filter(|(_, l)| hay(&l.plain(), sensitive).contains(&q)).map(|(i, _)| i).collect()
}

/// `line` with every occurrence of `query` restyled with `hl` (other styling kept).
pub fn highlight(line: &Line, query: &str, hl: Style) -> Line {
    if query.is_empty() {
        return line.clone();
    }
    let (q, sensitive) = needle(query);
    // character ranges of the matches in the plain text (lowercasing can change byte lengths,
    // so work on chars)
    let plain: Vec<char> = line.plain().chars().collect();
    let hay_chars: Vec<char> = hay(&plain.iter().collect::<String>(), sensitive).chars().collect();
    let qc: Vec<char> = q.chars().collect();
    if hay_chars.len() != plain.len() || qc.is_empty() {
        return line.clone(); // case mapping changed the length: leave the line as is
    }
    let mut hit = vec![false; plain.len()];
    let mut i = 0;
    while i + qc.len() <= hay_chars.len() {
        if hay_chars[i..i + qc.len()] == qc[..] {
            hit[i..i + qc.len()].iter_mut().for_each(|h| *h = true);
            i += qc.len();
        } else {
            i += 1;
        }
    }
    let mut out = Line::new(vec![]);
    out.fill_bg = line.fill_bg;
    out.raw = line.raw.clone();
    let mut ci = 0;
    for span in &line.spans {
        let mut cur = String::new();
        let mut cur_hit = false;
        for ch in span.text.chars() {
            let h = hit.get(ci).copied().unwrap_or(false);
            if h != cur_hit && !cur.is_empty() {
                out.push(Span::styled(std::mem::take(&mut cur), if cur_hit { hl } else { span.style }));
            }
            cur_hit = h;
            cur.push(ch);
            ci += 1;
        }
        if !cur.is_empty() {
            out.push(Span::styled(cur, if cur_hit { hl } else { span.style }));
        }
    }
    out
}

/// Next match at or after `from` (wrapping), or the previous one before `from` when `back`.
pub fn step(matches: &[usize], from: usize, back: bool) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    let idx = matches.partition_point(|&m| m < from);
    if back {
        idx.checked_sub(1).and_then(|i| matches.get(i)).or_else(|| matches.last()).copied()
    } else {
        matches.get(idx).or_else(|| matches.first()).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_lines_smart_case_and_highlights_keeping_styles() {
        let bold = Style::new().bold();
        let lines = vec![
            Line::new(vec![Span::raw("fn "), Span::styled("Main", bold), Span::raw("() {}")]),
            Line::raw("nothing here"),
            Line::raw("main again"),
        ];
        assert_eq!(find(&lines, "main"), [0, 2]);
        assert_eq!(find(&lines, "Main"), [0], "an uppercase letter makes it case-sensitive");
        let hl = Style::new().reverse();
        let h = highlight(&lines[0], "ain(", hl);
        assert_eq!(h.plain(), "fn Main() {}");
        let styled: Vec<(&str, Style)> = h.spans.iter().map(|s| (s.text.as_str(), s.style)).collect();
        assert_eq!(styled, [("fn ", Style::new()), ("M", bold), ("ain(", hl), (") {}", Style::new())]);
    }

    #[test]
    fn highlight_preserves_raw_escape_payload() {
        let mut img_line = Line::raw("placeholder");
        img_line.raw = Some("\x1b_Ga=T,i=7;AAAA\x1b\\".into());
        let hl = Style::new().reverse();
        let out = highlight(&img_line, "hold", hl);
        assert_eq!(out.raw.as_deref(), Some("\x1b_Ga=T,i=7;AAAA\x1b\\"));
    }

    #[test]
    fn n_and_shift_n_wrap_around() {
        let m = [3, 10, 20];
        assert_eq!(step(&m, 0, false), Some(3));
        assert_eq!(step(&m, 11, false), Some(20));
        assert_eq!(step(&m, 21, false), Some(3), "wraps to the first");
        assert_eq!(step(&m, 10, true), Some(3));
        assert_eq!(step(&m, 3, true), Some(20), "wraps to the last");
        assert_eq!(step(&[], 0, false), None);
    }
}
