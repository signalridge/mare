//! Word boundaries for word motion and deletion (Unicode word bounds split at ASCII punctuation).

use super::*;

// Word pieces` (Apache-2.0): Unicode word bounds
// (UAX #29, so every CJK ideograph is its own word), each further split where it switches
// between ASCII punctuation separators and other text ("bar.baz" -> "bar", ".", "baz").
const WORD_SEPARATORS: &str = "`~!@#$%^&*()-=+[{]}\\|;:'\",.<>/?";

fn is_word_separator(ch: char) -> bool {
    WORD_SEPARATORS.contains(ch)
}

fn split_word_pieces(run: &str) -> Vec<(usize, &str)> {
    let mut pieces = Vec::new();
    for (segment_start, segment) in run.split_word_bound_indices() {
        let mut piece_start = 0;
        let mut chars = segment.char_indices();
        let Some((_, first)) = chars.next() else { continue };
        let mut in_sep = is_word_separator(first);
        for (idx, ch) in chars {
            let sep = is_word_separator(ch);
            if sep == in_sep {
                continue;
            }
            pieces.push((segment_start + piece_start, &segment[piece_start..idx]));
            piece_start = idx;
            in_sep = sep;
        }
        pieces.push((segment_start + piece_start, &segment[piece_start..]));
    }
    pieces
}

/// Start of the word before byte `col` of `line`.
pub(super) fn prev_word_start(line: &str, col: usize) -> usize {
    let prefix = &line[..col];
    let Some((first_non_ws, ch)) = prefix.char_indices().rev().find(|&(_, c)| !c.is_whitespace()) else {
        return 0;
    };
    let run_start = prefix[..first_non_ws]
        .char_indices()
        .rev()
        .find(|&(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let run_end = first_non_ws + ch.len_utf8();
    let mut pieces = split_word_pieces(&prefix[run_start..run_end]).into_iter().rev().peekable();
    let Some((piece_start, piece)) = pieces.next() else { return run_start };
    let mut start = run_start + piece_start;
    if piece.chars().all(is_word_separator) {
        while let Some((i, p)) = pieces.peek() {
            if !p.chars().all(is_word_separator) {
                break;
            }
            start = run_start + *i;
            pieces.next();
        }
    }
    start
}

/// End of the word at/after byte `col` of `line`.
pub(super) fn next_word_end(line: &str, col: usize) -> usize {
    let suffix = &line[col..];
    let Some(first_non_ws) = suffix.find(|c: char| !c.is_whitespace()) else { return line.len() };
    let run = &suffix[first_non_ws..];
    let run = &run[..run.find(char::is_whitespace).unwrap_or(run.len())];
    let mut pieces = split_word_pieces(run).into_iter().peekable();
    let Some((start, piece)) = pieces.next() else { return col + first_non_ws };
    let base = col + first_non_ws;
    let mut end = base + start + piece.len();
    if piece.chars().all(is_word_separator) {
        while let Some((i, p)) = pieces.peek() {
            if !p.chars().all(is_word_separator) {
                break;
            }
            end = base + *i + p.len();
            pieces.next();
        }
    }
    end
}
