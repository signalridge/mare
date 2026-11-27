pub mod apply_patch;
pub mod replace;

/// The one character table both fuzzy matchers use (apply_patch's seek and edit's replace):
/// typographic dashes, quotes and odd spaces count as their ASCII forms.
pub(crate) fn norm_char(c: char) -> char {
    match c {
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' | '\u{2212}' => '-',
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}' | '\u{2007}' | '\u{2008}'
        | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        c => c,
    }
}

/// Line ending of the line containing `offset`: its own LF or
/// CRLF, or that of the line before for an unterminated last line; a lone CR only in files
/// without any LF (classic Mac endings).
pub(crate) fn line_ending_at(content: &str, offset: usize) -> &'static str {
    let off = offset.min(content.len());
    let lf = content[off..].find('\n').map(|i| off + i).or_else(|| content[..off].rfind('\n'));
    match lf {
        None => {
            if content.contains('\r') {
                "\r"
            } else {
                "\n"
            }
        }
        Some(i) if i > 0 && content.as_bytes()[i - 1] == b'\r' => "\r\n",
        Some(_) => "\n",
    }
}

/// A unified diff of two texts with what an edit result keeps beside it.
pub struct UnifiedEdit {
    /// `--- a/<path>` / `+++ b/<path>` and the hunks (`context` lines around each change); empty
    /// when the texts are equal
    pub text: String,
    /// added and removed lines, counted like [`diff_stats`]
    pub added: usize,
    pub removed: usize,
    /// the longer side's `str::lines` count (the diff view's gutter width)
    pub lines: usize,
}

/// One diff of `old` → `new` for a stored edit result: similar's unified diff, except that every
/// row ends in `\n`. similar splits lines at a lone `\r` too and writes such a line without a
/// `\n`, so a line-based reader would glue it to the next row; here it gets one (a row's
/// trailing `\r` is dropped by the diff view either way).
pub fn unified_edit(old: &str, new: &str, context: usize, path: &str) -> UnifiedEdit {
    use similar::{ChangeTag, TextDiff};
    use std::fmt::Write as _;
    let d = TextDiff::from_lines(old, new);
    let mut text = String::new();
    for (i, hunk) in d.unified_diff().context_radius(context).iter_hunks().enumerate() {
        if i == 0 {
            let _ = writeln!(text, "--- a/{path}\n+++ b/{path}");
        }
        let _ = writeln!(text, "{}", hunk.header());
        for c in hunk.iter_changes() {
            text.push(match c.tag() {
                ChangeTag::Insert => '+',
                ChangeTag::Delete => '-',
                ChangeTag::Equal => ' ',
            });
            let v = c.value();
            text.push_str(v);
            if !v.ends_with('\n') {
                text.push('\n');
                if !v.ends_with('\r') {
                    text.push_str("\\ No newline at end of file\n");
                }
            }
        }
    }
    let (added, removed) = text_diff_stats(&d);
    UnifiedEdit { text, added, removed, lines: old.lines().count().max(new.lines().count()) }
}

/// (added, removed) line counts from an existing [`similar::TextDiff`].
pub fn text_diff_stats<'old, 'new, T>(diff: &similar::TextDiff<'old, 'new, T>) -> (usize, usize)
where
    T: similar::DiffableStr + ?Sized,
{
    use similar::ChangeTag;
    let (mut added, mut removed) = (0, 0);
    for c in diff.iter_all_changes() {
        match c.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

/// (added, removed) line counts between two texts.
pub fn diff_stats(old: &str, new: &str) -> (usize, usize) {
    text_diff_stats(&similar::TextDiff::from_lines(old, new))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_char_and_line_ending_at_cover_unicode_and_cr_only_files() {
        assert_eq!(norm_char('\u{2014}'), '-');
        assert_eq!(norm_char('\u{2212}'), '-');
        assert_eq!(norm_char('\u{2019}'), '\'');
        assert_eq!(norm_char('\u{201C}'), '"');
        assert_eq!(norm_char('\u{00A0}'), ' ');
        assert_eq!(norm_char('a'), 'a');

        assert_eq!(line_ending_at("one\rtwo", 2), "\r");
        assert_eq!(line_ending_at("single_line_no_newline", 4), "\n");
        assert_eq!(line_ending_at("a\r\nb\r\n", 1), "\r\n");
        assert_eq!(line_ending_at("a\nb\n", 1), "\n");
    }
}
