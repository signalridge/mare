//! Cell-level text operations: truncation, path elision, slicing and overlay of lines, tab
//! expansion and sanitizing untrusted text.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use super::*;

/// Truncate `s` to at most `max` cells, appending `ellipsis` if it was cut.
pub fn truncate_str(s: &str, max: usize, ellipsis: &str) -> String {
    if str_width(s) <= max {
        return s.to_string();
    }
    let ew = str_width(ellipsis);
    if max <= ew {
        return ellipsis.graphemes(true).take(max).collect();
    }
    let mut out = String::new();
    let mut w = 0;
    for g in s.graphemes(true) {
        let gw = grapheme_width(g);
        if w + gw > max - ew {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    out.push_str(ellipsis);
    out
}

const MAX_CACHED_HOMES: usize = 16;

pub(super) struct HomeCanonCache {
    entries: Vec<(String, Option<String>)>,
}

impl HomeCanonCache {
    pub(super) fn new() -> Self {
        Self { entries: Vec::with_capacity(4) }
    }

    pub(super) fn get_or_resolve(
        &mut self,
        home: &str,
        resolve: impl FnOnce(&Path) -> Option<String>,
    ) -> Option<String> {
        if let Some((_, cached)) = self.entries.iter().find(|(k, _)| k == home) {
            return cached.clone();
        }
        let canon = resolve(Path::new(home)).filter(|c| !c.is_empty() && c != home);
        if self.entries.len() >= MAX_CACHED_HOMES {
            self.entries.remove(0);
        }
        self.entries.push((home.to_owned(), canon.clone()));
        canon
    }
}

fn strip_home_prefix<'a>(path: &'a str, home: &str) -> Option<&'a str> {
    if home.is_empty() {
        return None;
    }
    path.strip_prefix(home).filter(|rest| rest.is_empty() || rest.starts_with(['/', '\\']))
}

fn canonicalize_home_fs(home: &Path) -> Option<String> {
    dunce::canonicalize(home).ok().map(|p| p.to_string_lossy().trim_end_matches(['/', '\\']).to_owned())
}

/// Resolve and cache the canonical form of `home` (trimmed of trailing separators) at most once
/// per `home` string, returning `None` when `home` cannot be canonicalized or already equals its
/// canonical path. Hot-path callers never perform filesystem I/O on the displayed `path`.
fn canonical_home_cached(home: &str) -> Option<String> {
    static CACHE: OnceLock<Mutex<HomeCanonCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HomeCanonCache::new()));
    cache.lock().unwrap_or_else(|e| e.into_inner()).get_or_resolve(home, canonicalize_home_fs)
}

pub(super) fn collapse_home_with(
    path: &str,
    home: &str,
    resolve_canon_home: impl FnOnce(&str) -> Option<String>,
) -> String {
    let home = home.trim_end_matches(['/', '\\']);
    if let Some(rest) = strip_home_prefix(path, home) {
        return format!("~{rest}");
    }
    if !home.is_empty()
        && let Some(canon_home) = resolve_canon_home(home)
        && let Some(rest) = strip_home_prefix(path, &canon_home)
    {
        return format!("~{rest}");
    }
    path.to_string()
}

fn collapse_home(path: &str, home: &str) -> String {
    collapse_home_with(path, home, canonical_home_cached)
}

/// A directory for display, the one rule for every place mare shows the working directory
/// (welcome line, status bar, trust question): the home directory becomes `~`, and a path wider
/// than `max` cells keeps its last components behind `…/` (`…/uxlab/repo`, like fish's
/// prompt_pwd). A last component that alone is too wide keeps its end.
pub fn display_path(path: &str, home: &str, max: usize) -> String {
    let shown = collapse_home(path, home);
    if max == 0 || str_width(&shown) <= max {
        return shown;
    }
    let ew = str_width("…");
    if max < ew {
        return truncate_str(&shown, max, "…");
    }
    let sep = if shown.contains('/') { '/' } else { '\\' };
    let parts: Vec<&str> = shown.split(sep).filter(|p| !p.is_empty()).collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut w = ew; // "…"
    for p in parts.iter().rev() {
        let pw = str_width(p) + 1; // the separator before it
        if w + pw > max && !kept.is_empty() {
            break;
        }
        kept.push(p);
        w += pw;
    }
    kept.reverse();
    let tail = kept.join(&sep.to_string());
    let out = format!("…{sep}{tail}");
    if str_width(&out) <= max {
        return out;
    }
    // only the last component is left and it's still too wide: keep its end
    let budget = max.saturating_sub(ew);
    let mut end: Vec<&str> = Vec::new();
    let mut ew_acc = 0;
    for g in tail.graphemes(true).rev() {
        let gw = grapheme_width(g);
        if ew_acc + gw > budget {
            break;
        }
        end.push(g);
        ew_acc += gw;
    }
    end.reverse();
    format!("…{}", end.concat())
}

/// Truncate a line to `max` cells with an ellipsis.
pub fn truncate_line(line: &Line, max: usize) -> Line {
    if line.width() <= max {
        return line.clone();
    }
    let ew = str_width("…");
    if max < ew {
        return Line { spans: vec![], fill_bg: line.fill_bg, raw: None };
    }
    let mut out = Line { spans: vec![], fill_bg: line.fill_bg, raw: None };
    let mut w = 0;
    let limit = max.saturating_sub(ew);
    'outer: for span in &line.spans {
        let mut buf = String::new();
        for g in span.text.graphemes(true) {
            let gw = grapheme_width(g);
            if w + gw > limit {
                out.push(Span::styled(buf, span.style));
                break 'outer;
            }
            buf.push_str(g);
            w += gw;
        }
        out.push(Span::styled(buf, span.style));
    }
    let st = line.spans.last().map(|s| s.style).unwrap_or_default();
    out.push(Span::styled("…", st));
    out
}

/// Cells `[start, end)` of a line. A wide grapheme cut by a boundary becomes a space.
pub fn slice_cells(line: &Line, start: usize, end: usize) -> Line {
    let mut out = Line { spans: vec![], fill_bg: None, raw: None };
    let mut col = 0;
    for span in &line.spans {
        let mut buf = String::new();
        for g in span.text.graphemes(true) {
            let gw = grapheme_width(g);
            if col >= start && col + gw <= end {
                buf.push_str(g);
            } else if col < end && col + gw > start {
                // straddles a boundary
                buf.push_str(&" ".repeat((col + gw).min(end) - col.max(start)));
            }
            col += gw;
            if col >= end {
                break;
            }
        }
        out.push(Span::styled(buf, span.style));
        if col >= end {
            break;
        }
    }
    out
}

/// Composite `top` over `base` starting at column `x` (popups drawn over the live area).
pub fn overlay_line(base: &Line, x: usize, top: &Line) -> Line {
    let tw = top.width();
    let mut out = slice_cells(base, 0, x);
    let lw = out.width();
    if lw < x {
        out.push(Span::raw(" ".repeat(x - lw)));
    }
    for sp in &top.spans {
        out.push(sp.clone());
    }
    let bw = base.width();
    if bw > x + tw {
        for sp in slice_cells(base, x + tw, bw).spans {
            out.push(sp);
        }
    }
    out
}

/// Expand tabs to spaces (tab stop 4) – terminals disagree on tab rendering.
pub fn expand_tabs(s: &str) -> String {
    if !s.contains('\t') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut col = 0;
    for g in s.graphemes(true) {
        if g == "\t" {
            let n = 4 - (col % 4);
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push_str(g);
            col += grapheme_width(g);
        }
    }
    out
}

/// Strip ANSI escape sequences and control chars from untrusted text (tool output).
pub fn sanitize(s: &str) -> String {
    // the trust-boundary filter: every escape sequence, C1 and C0 control removed,
    // `\r\n` / lone `\r` → `\n`
    crate::ansi::sanitize(s, false)
}
