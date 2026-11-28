//! Word wrapping of styled lines (hanging indent, URLs kept whole).

use super::*;

/// Word-wrap a styled line to `width` cells. Continuation lines get `indent` (spans) prepended.
/// Long words are hard-broken on grapheme boundaries.
pub fn wrap_line(line: &Line, width: usize) -> Vec<Line> {
    wrap_line_indent(line, width, &[])
}

/// Like [`wrap_line`], but also breaks URL tokens that exceed `width` so bounded overlays
/// (modals, approval details) never clip a long URL.
pub fn wrap_line_hard(line: &Line, width: usize) -> Vec<Line> {
    wrap_line_inner(line, width, &[], true)
}

pub fn wrap_line_indent(line: &Line, width: usize, indent: &[Span]) -> Vec<Line> {
    wrap_line_inner(line, width, indent, false)
}

/// Like [`wrap_line_indent`], but also breaks URL tokens that exceed `width`.
pub fn wrap_line_indent_hard(line: &Line, width: usize, indent: &[Span]) -> Vec<Line> {
    wrap_line_inner(line, width, indent, true)
}

fn wrap_line_inner(line: &Line, width: usize, indent: &[Span], break_urls: bool) -> Vec<Line> {
    let width = width.max(1);
    if line.width() <= width {
        return vec![line.clone()];
    }
    let indent_w: usize = indent.iter().map(Span::width).sum();
    // Tokenize into (grapheme, style, is_space) and wrap greedily at word boundaries.
    struct Tok<'a> {
        g: &'a str,
        st: Style,
        w: usize,
        space: bool,
    }
    let mut toks: Vec<Tok> = Vec::new();
    for span in &line.spans {
        for g in span.text.graphemes(true) {
            let w = grapheme_width(g);
            toks.push(Tok { g, st: span.style, w, space: g == " " });
        }
    }
    let mut lines: Vec<Line> = Vec::new();
    let mut cur = Line { spans: vec![], fill_bg: line.fill_bg, raw: None };
    let mut cur_w = 0usize;
    let mut first = true;
    let mut i = 0;
    while i < toks.len() {
        let avail = if first { width } else { width.saturating_sub(indent_w).max(1) };
        // find word extent [i, j)
        let mut j = i;
        let mut word_w = 0;
        if toks[i].space {
            j = i + 1;
            word_w = toks[i].w;
        } else {
            while j < toks.len() && !toks[j].space {
                word_w += toks[j].w;
                j += 1;
            }
        }
        if cur_w + word_w <= avail {
            for t in &toks[i..j] {
                cur.push(Span::styled(t.g, t.st));
            }
            cur_w += word_w;
            i = j;
            continue;
        }
        if toks[i].space {
            // drop the space at a wrap point
            i += 1;
            lines.push(std::mem::replace(&mut cur, Line { spans: vec![], fill_bg: line.fill_bg, raw: None }));
            cur_w = 0;
            first = false;
            continue;
        }
        if cur_w > 0 && word_w <= avail_for_next(width, indent_w) {
            lines.push(std::mem::replace(&mut cur, Line { spans: vec![], fill_bg: line.fill_bg, raw: None }));
            cur_w = 0;
            first = false;
            continue;
        }
        // a URL is never broken:
        // it gets a row of its own and may run past the width — scrollback commits write it
        // whole and the terminal soft-wraps it; the live area cuts it with `…`
        if !break_urls && is_url_like(&toks[i..j].iter().map(|t| t.g).collect::<String>()) {
            if cur_w > 0 {
                lines.push(std::mem::replace(&mut cur, Line { spans: vec![], fill_bg: line.fill_bg, raw: None }));
                first = false;
            }
            for t in &toks[i..j] {
                cur.push(Span::styled(t.g, t.st));
            }
            cur_w = word_w;
            i = j;
            continue;
        }
        // hard break the word
        while i < j {
            let avail = if first { width } else { width.saturating_sub(indent_w).max(1) };
            if cur_w + toks[i].w > avail && cur_w > 0 {
                lines.push(std::mem::replace(&mut cur, Line { spans: vec![], fill_bg: line.fill_bg, raw: None }));
                cur_w = 0;
                first = false;
                continue;
            }
            cur.push(Span::styled(toks[i].g, toks[i].st));
            cur_w += toks[i].w;
            i += 1;
        }
    }
    if !cur.spans.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    // trim trailing spaces and apply indent to continuation lines
    for (n, l) in lines.iter_mut().enumerate() {
        if let Some(last) = l.spans.last_mut() {
            let trimmed = last.text.trim_end_matches(' ').len();
            last.text.truncate(trimmed);
        }
        if n > 0 && !indent.is_empty() {
            let mut sp = indent.to_vec();
            sp.append(&mut l.spans);
            l.spans = sp;
        }
    }
    lines
}

/// Whether a whitespace-free token looks like a URL: `scheme://host…` or a bare
/// `host[:port]/path` with a real domain, `localhost` or an IPv4 host (`www.` hosts need no
/// path). File paths like `src/main.rs` are not URLs. Surrounding punctuation is ignored.
/// (Copyright 2025 OpenAI, Apache-2.0), without the `url` crate.
pub fn is_url_like(raw: &str) -> bool {
    let t = raw.trim_matches(|c: char| "()[]{}<>,.;:!'\"".contains(c));
    if t.is_empty() || t.chars().any(char::is_whitespace) {
        return false;
    }
    if let Some((scheme, rest)) = t.split_once("://") {
        let mut sc = scheme.chars();
        let scheme_ok = sc.next().is_some_and(|c| c.is_ascii_alphabetic())
            && sc.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
        if !scheme_ok || rest.is_empty() {
            return false;
        }
        // well-known schemes need a host
        if matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https" | "ftp" | "ftps" | "ws" | "wss") {
            let host = rest.split(['/', '?', '#']).next().unwrap_or("");
            let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
            return !host.is_empty();
        }
        return true;
    }
    let (host_port, trailer) = match t.find(['/', '?', '#']) {
        Some(i) => (&t[..i], true),
        None => (t, false),
    };
    if host_port.is_empty() || host_port.starts_with('[') {
        return false;
    }
    if !trailer && !host_port.to_ascii_lowercase().starts_with("www.") {
        return false;
    }
    let host = match host_port.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            if p.len() > 5 || p.parse::<u16>().is_err() {
                return false;
            }
            h
        }
        _ => host_port,
    };
    let ipv4 = {
        let parts: Vec<&str> = host.split('.').collect();
        parts.len() == 4 && parts.iter().all(|p| !p.is_empty() && p.parse::<u8>().is_ok())
    };
    let domain = || {
        let h = host.to_ascii_lowercase();
        let mut labels: Vec<&str> = h.split('.').collect();
        if labels.len() < 2 {
            return false;
        }
        let tld = labels.pop().unwrap_or("");
        (2..=63).contains(&tld.len())
            && tld.chars().all(|c| c.is_ascii_alphabetic())
            && labels.iter().all(|l| {
                !l.is_empty()
                    && l.len() <= 63
                    && l.starts_with(|c: char| c.is_ascii_alphanumeric())
                    && l.ends_with(|c: char| c.is_ascii_alphanumeric())
                    && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            })
    };
    host.eq_ignore_ascii_case("localhost") || ipv4 || domain()
}

fn avail_for_next(width: usize, indent_w: usize) -> usize {
    width.saturating_sub(indent_w).max(1)
}
