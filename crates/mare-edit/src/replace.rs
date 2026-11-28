//! String-replacement editing (`old` → `new`) with a whitespace/quote-normalized fallback.

use std::borrow::Cow;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq)]
pub enum ReplaceError {
    #[error("old text must not be empty")]
    EmptyOld,
    #[error("old text not found in the file. Re-read the file and copy the exact text (including indentation).")]
    NotFound,
    /// found only when indentation / leading-trailing whitespace is ignored
    #[error(
        "old text not found exactly, but it matches at line {0} when indentation and leading/trailing whitespace are ignored. Copy the exact indentation from the file."
    )]
    WhitespaceOnly(usize),
    /// `ambiguous_edit`: every matching line number
    #[error(
        "ambiguous_edit: old text matches {count} locations (lines {lines}); read the intended region and add more surrounding context to make it unique, or set replace_all"
    )]
    Ambiguous { count: usize, lines: String },
    #[error("old and new text are identical; nothing to change")]
    NoChange,
}

use crate::norm_char;

/// Normalized text plus, for each normalized byte, the original (start, end) byte range.
struct Norm {
    text: String,
    starts: Vec<usize>,
    ends: Vec<usize>,
}

/// Normalize: unify quotes/dashes/odd spaces, drop CR, strip trailing whitespace on each line.
fn normalize(s: &str) -> Norm {
    let mut n = Norm { text: String::with_capacity(s.len()), starts: vec![], ends: vec![] };
    let mut pos = 0;
    for line in s.split_inclusive('\n') {
        let (body, nl) = match line.strip_suffix('\n') {
            Some(b) => (b, true),
            None => (line, false),
        };
        let body_no_cr = body.strip_suffix('\r').unwrap_or(body);
        let kept = body_no_cr.trim_end_matches(|c: char| c.is_whitespace() || norm_char(c) == ' ');
        for (off, c) in kept.char_indices() {
            let nc = norm_char(c);
            let start = pos + off;
            let end = start + c.len_utf8();
            let before = n.text.len();
            n.text.push(nc);
            for _ in before..n.text.len() {
                n.starts.push(start);
                n.ends.push(end);
            }
        }
        if nl {
            let start = pos + body.len();
            n.text.push('\n');
            n.starts.push(start);
            n.ends.push(start + 1);
        }
        pos += line.len();
    }
    n
}

fn find_all(hay: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle) {
        out.push(from + i);
        from += i + needle.len().max(1);
        if from > hay.len() {
            break;
        }
    }
    out
}

fn find_overlapping(hay: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle) {
        let hit = from + i;
        out.push(hit);
        let step = hay[hit..].chars().next().map_or(1, char::len_utf8);
        from = hit + step;
        if from > hay.len() {
            break;
        }
    }
    out
}

/// Splice `new` into `content` at each range; the new text's line breaks take the line ending of
/// the line where its range starts, so a mixed
/// LF/CRLF file keeps every untouched line's ending.
fn splice(content: &str, ranges: &[(usize, usize)], new: &str) -> String {
    let lf_new: Cow<'_, str> =
        if new.contains("\r\n") { Cow::Owned(new.replace("\r\n", "\n")) } else { Cow::Borrowed(new) };
    let mut crlf_new: Option<String> = None;
    let mut cr_new: Option<String> = None;
    let mut out = String::with_capacity(content.len() + new.len());
    let mut ends_with_cr = false;
    let mut push = |out: &mut String, part: &str| {
        if part.is_empty() {
            return;
        }
        // a kept lone CR followed by an inserted LF would merge into one CRLF: keep them apart
        if ends_with_cr && part.starts_with('\n') {
            out.push('\n');
        }
        out.push_str(part);
        ends_with_cr = part.ends_with('\r');
    };
    let bytes = content.as_bytes();
    let first_lf = content.find('\n');
    let no_lf_ending = if first_lf.is_none() && content.contains('\r') { "\r" } else { "\n" };
    let mut prev_lf: Option<usize> = None;
    let mut next_lf = first_lf;
    let mut last = 0;
    for &(s, e) in ranges {
        push(&mut out, &content[last..s]);
        let ending = if first_lf.is_none() {
            no_lf_ending
        } else {
            let off = s.min(content.len());
            if prev_lf.is_some_and(|p| off <= p) {
                // Fallback if ranges were ever non-monotonic
                crate::line_ending_at(content, s)
            } else {
                while let Some(nl) = next_lf {
                    if nl >= off {
                        break;
                    }
                    prev_lf = Some(nl);
                    next_lf = content[nl + 1..].find('\n').map(|d| nl + 1 + d);
                }
                match next_lf.or(prev_lf) {
                    Some(i) if i > 0 && bytes[i - 1] == b'\r' => "\r\n",
                    _ => "\n",
                }
            }
        };
        let piece: &str = match ending {
            "\n" => lf_new.as_ref(),
            "\r\n" => crlf_new.get_or_insert_with(|| lf_new.replace('\n', "\r\n")),
            _ => cr_new.get_or_insert_with(|| lf_new.replace('\n', ending)),
        };
        push(&mut out, piece);
        last = e;
    }
    push(&mut out, &content[last..]);
    out
}

/// LF-normalized `content` plus a map back to original byte offsets (only CRLF changes length):
/// the n-th CRLF's LF sits at normalized offset p - n.
struct Lf<'a> {
    text: Cow<'a, str>,
    crlf_lf: Vec<usize>,
}

impl<'a> Lf<'a> {
    fn new(content: &'a str) -> Self {
        let mut crlf_lf = vec![];
        let mut n = 0;
        let b = content.as_bytes();
        for (i, w) in b.windows(2).enumerate() {
            if w == b"\r\n" {
                crlf_lf.push(i - n);
                n += 1;
            }
        }
        let text = if crlf_lf.is_empty() { Cow::Borrowed(content) } else { Cow::Owned(content.replace("\r\n", "\n")) };
        Self { text, crlf_lf }
    }
    fn to_original(&self, offset: usize) -> usize {
        if self.crlf_lf.is_empty() { offset } else { offset + self.crlf_lf.partition_point(|&p| p < offset) }
    }
}

/// The ambiguous-match error: every matching line number.
fn ambiguous(content: &str, offsets: &[usize]) -> ReplaceError {
    use std::fmt::Write as _;
    let bytes = content.as_bytes();
    let mut cursor = 0usize;
    let mut line = 1usize;
    let mut lines = String::new();
    for (idx, &o) in offsets.iter().enumerate() {
        let target = o.min(bytes.len());
        if target < cursor {
            line = bytes[..target].iter().filter(|&&b| b == b'\n').count() + 1;
        } else {
            line += bytes[cursor..target].iter().filter(|&&b| b == b'\n').count();
        }
        cursor = target;
        if idx > 0 {
            lines.push_str(", ");
        }
        let _ = write!(lines, "{line}");
    }
    ReplaceError::Ambiguous { count: offsets.len(), lines }
}

/// The first line where `old` matches once every line is trimmed (indentation and trailing
/// whitespace ignored): the "only whitespace differs" hint.
fn whitespace_only_match(content: &str, old: &str) -> Option<usize> {
    let want: Vec<&str> = old.lines().map(str::trim).collect();
    if want.iter().all(|l| l.is_empty()) {
        return None;
    }
    let have: Vec<&str> = content.lines().map(str::trim).collect();
    (0..have.len().saturating_sub(want.len()) + 1)
        .find(|&i| have.get(i..i + want.len()) == Some(&want[..]))
        .map(|i| i + 1)
}

pub fn replace(content: &str, old: &str, new: &str, replace_all: bool) -> Result<String, ReplaceError> {
    // a UTF-8 BOM never takes part in matching and is kept as it was
    if let Some(rest) = content.strip_prefix('\u{feff}') {
        return replace_inner(rest, old, new, replace_all).map(|s| format!("\u{feff}{s}"));
    }
    replace_inner(content, old, new, replace_all)
}

fn replace_inner(content: &str, old: &str, new: &str, replace_all: bool) -> Result<String, ReplaceError> {
    if old.is_empty() {
        return Err(ReplaceError::EmptyOld);
    }
    if old == new {
        return Err(ReplaceError::NoChange);
    }
    // 1) exact — on the LF-normalized text, mapped back to the original bytes
    let lf = Lf::new(content);
    let old_lf: Cow<'_, str> =
        if old.contains("\r\n") { Cow::Owned(old.replace("\r\n", "\n")) } else { Cow::Borrowed(old) };
    let hits = if replace_all { find_all(&lf.text, &old_lf) } else { find_overlapping(&lf.text, &old_lf) };
    if !hits.is_empty() {
        if hits.len() > 1 && !replace_all {
            return Err(ambiguous(&lf.text, &hits));
        }
        let ranges: Vec<_> = hits.iter().map(|&s| (lf.to_original(s), lf.to_original(s + old_lf.len()))).collect();
        return Ok(splice(content, &ranges, new));
    }

    // 2) fuzzy (normalized)
    let nc = normalize(content);
    let no = normalize(old).text;
    if no.is_empty() {
        return Err(ReplaceError::NotFound);
    }
    let hits = if replace_all { find_all(&nc.text, &no) } else { find_overlapping(&nc.text, &no) };
    if hits.is_empty() {
        return Err(match whitespace_only_match(content, old) {
            Some(line) => ReplaceError::WhitespaceOnly(line),
            None => ReplaceError::NotFound,
        });
    }
    if hits.len() > 1 && !replace_all {
        let offsets: Vec<usize> = hits.iter().map(|&h| nc.starts[h]).collect();
        return Err(ambiguous(content, &offsets));
    }
    let ranges: Vec<_> = hits.iter().map(|&s| (nc.starts[s], nc.ends[s + no.len() - 1])).collect();
    Ok(splice(content, &ranges, new))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact() {
        assert_eq!(replace("a b c", "b", "X", false).unwrap(), "a X c");
    }

    #[test]
    fn ambiguous_and_all() {
        match replace("x\ny\nx\nz\nx", "x", "q", false) {
            Err(ReplaceError::Ambiguous { count, lines }) => {
                assert_eq!((count, lines.as_str()), (3, "1, 3, 5"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(replace("x x x", "x", "y", true).unwrap(), "y y y");
    }

    #[test]
    fn not_found() {
        assert_eq!(replace("abc", "zzz", "y", false), Err(ReplaceError::NotFound));
        // only indentation differs: say so, with the line
        assert_eq!(
            replace("fn a() {\n        let x = 1;\n}\n", "  let x = 1;\n  let y = 2;", "q", false),
            Err(ReplaceError::NotFound)
        );
        assert_eq!(
            replace("fn a() {\n        let x = 1;\n    let y = 2;\n}\n", "let x = 1;\n\tlet y = 2;", "q", false),
            Err(ReplaceError::WhitespaceOnly(2))
        );
    }

    #[test]
    fn fuzzy_trailing_ws_and_quotes() {
        let c = "let s = \u{201C}hi\u{201D};   \nnext\n";
        let r = replace(c, "let s = \"hi\";\nnext", "let s = \"yo\";\nnext", false).unwrap();
        assert_eq!(r, "let s = \"yo\";\nnext\n");
    }

    /// No exact match, and two locations become the same text only after normalization
    /// (curly quotes either way round): the fuzzy fallback must refuse with both line numbers,
    /// never edit the first one it finds.
    #[test]
    fn fuzzy_match_in_two_places_is_ambiguous() {
        let c = "a = \u{201C}v\u{201D};\nb = 1;\na = \u{201D}v\u{201C};\n";
        match replace(c, "a = \"v\";", "a = \"w\";", false) {
            Err(ReplaceError::Ambiguous { count, lines }) => assert_eq!((count, lines.as_str()), (2, "1, 3")),
            other => panic!("{other:?}"),
        }
        // replace_all takes both, the rest of the file unchanged
        assert_eq!(replace(c, "a = \"v\";", "a = \"w\";", true).unwrap(), "a = \"w\";\nb = 1;\na = \"w\";\n");
    }

    #[test]
    fn bom_preserved_and_ignored() {
        assert_eq!(replace("\u{feff}a\nb\n", "a", "A", false).unwrap(), "\u{feff}A\nb\n");
    }

    #[test]
    fn crlf_preserved() {
        let r = replace("a\r\nb\r\nc\r\n", "a\nb", "A\nB", false).unwrap();
        assert_eq!(r, "A\r\nB\r\nc\r\n");
    }

    #[test]
    fn fuzzy_nbsp() {
        let r = replace("foo\u{00A0}bar\n", "foo bar", "baz", false).unwrap();
        assert_eq!(r, "baz\n");
    }

    /// mixed LF/CRLF: only the replaced range changes, and its line breaks take the ending of
    /// the line where it starts
    #[test]
    fn mixed_line_endings_keep_untouched_lines() {
        let content = "a\r\nb\nc\r\n";
        assert_eq!(replace(content, "b\nc", "B\nC", false).unwrap(), "a\r\nB\nC\r\n");
        assert_eq!(replace(content, "a", "A1\nA2", false).unwrap(), "A1\r\nA2\r\nb\nc\r\n");
        // a file without a trailing newline stays without one
        assert_eq!(replace("x\ny", "y", "Y", false).unwrap(), "x\nY");
    }

    #[test]
    fn dense_multimatch_replace_all_and_ambiguous_preserve_crlf_lf_cr_and_bom() {
        // 500-line mixed LF/CRLF file with BOM and UTF-8 multi-byte chars.
        let mut doc = String::from("\u{feff}");
        for i in 0..500 {
            let eol = if i % 2 == 0 { "\r\n" } else { "\n" };
            let _ = std::fmt::Write::write_fmt(&mut doc, format_args!("héllo target_{i} val=TOK{eol}"));
        }
        // Ambiguous reports exact 1-based line numbers across CRLF/LF and BOM.
        match replace(&doc, "val=TOK", "val=NEW", false) {
            Err(ReplaceError::Ambiguous { count, lines }) => {
                assert_eq!(count, 500);
                assert!(lines.starts_with("1, 2, 3, "));
                assert!(lines.ends_with(", 499, 500"));
            }
            other => panic!("{other:?}"),
        }
        // replace_all preserves BOM, UTF-8, and each line's own LF vs CRLF on multi-line replacement.
        let out = replace(&doc, "val=TOK", "lineA\nlineB", true).unwrap();
        assert!(out.starts_with('\u{feff}'));
        for (i, seg) in out.strip_prefix('\u{feff}').unwrap().split_inclusive('\n').enumerate() {
            let want_crlf = (i / 2).is_multiple_of(2);
            assert_eq!(seg.ends_with("\r\n"), want_crlf, "row {i}: {seg:?}");
        }
        // Isolated CR file without LF keeps \r line endings.
        let cr = "a TOK\rb TOK\rc TOK";
        assert_eq!(replace(cr, "TOK", "X\nY", true).unwrap(), "a X\rY\rb X\rY\rc X\rY");
    }

    #[test]
    fn repro_309_single_replace_rejects_overlapping_matches_as_ambiguous() {
        let content = "#endif\n#endif\n#endif\n";
        let needle = "#endif\n#endif";
        // Overlapping starts exist at line 1 and line 2; single replacement must reject as ambiguous.
        let res = replace(content, needle, "X", false);
        match res {
            Err(ReplaceError::Ambiguous { count, lines }) => {
                assert_eq!((count, lines.as_str()), (2, "1, 2"));
            }
            other => {
                panic!("repro #309: expected Ambiguous(2, \"1, 2\") for overlapping single replace, got {other:?}")
            }
        }
    }

    #[test]
    fn overlapping_utf8_fuzzy_and_replace_all_behavior() {
        // Multi-byte UTF-8 overlapping matches advance by char boundary and report ambiguity.
        match replace("ééé", "éé", "X", false) {
            Err(ReplaceError::Ambiguous { count, lines }) => assert_eq!((count, lines.as_str()), (2, "1, 1")),
            other => panic!("expected Ambiguous for UTF-8 overlap, got {other:?}"),
        }
        // replace_all keeps non-overlapping semantics.
        assert_eq!(replace("#endif\n#endif\n#endif\n", "#endif\n#endif", "X", true).unwrap(), "X\n#endif\n");
        // Fuzzy normalized overlapping matches are also rejected for single replace.
        let fuzzy = "a = \u{201C}v\u{201D};\na = \u{201D}v\u{201C};\na = \u{201C}v\u{201D};\n";
        match replace(fuzzy, "a = \"v\";\na = \"v\";", "X", false) {
            Err(ReplaceError::Ambiguous { count, lines }) => assert_eq!((count, lines.as_str()), (2, "1, 2")),
            other => panic!("expected Ambiguous for fuzzy overlap, got {other:?}"),
        }
    }
}
