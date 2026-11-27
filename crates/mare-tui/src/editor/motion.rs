//! Cursor movement: grapheme and word steps that skip over placeholders, and vertical moves
//! over the wrapped rows.

use super::words::{next_word_end, prev_word_start};
use super::*;

impl Editor {
    pub(super) fn prev_boundary(&self, row: usize, col: usize) -> usize {
        if let Some(r) = self.token_ending_at(row, col) {
            return r.start;
        }
        self.lines[row][..col].grapheme_indices(true).next_back().map(|(i, _)| i).unwrap_or(0)
    }

    pub(super) fn next_boundary(&self, row: usize, col: usize) -> usize {
        if let Some(r) = self.token_starting_at(row, col) {
            return r.end;
        }
        let line = &self.lines[row];
        line[col..].graphemes(true).next().map(|g| col + g.len()).unwrap_or(col)
    }

    pub(super) fn word_left(&self, row: usize, col: usize) -> usize {
        let pos = prev_word_start(&self.lines[row], col);
        self.snap_out_of_token(row, pos)
    }

    pub(super) fn word_right(&self, row: usize, col: usize) -> usize {
        let pos = next_word_end(&self.lines[row], col);
        match self.token_ranges(row).into_iter().find(|r| r.start < pos && pos < r.end) {
            Some(r) => r.end,
            None => pos,
        }
    }

    pub(super) fn move_left(&mut self) {
        if self.col > 0 {
            self.col = self.prev_boundary(self.row, self.col);
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
    }

    pub(super) fn move_right(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.col = self.next_boundary(self.row, self.col);
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub(super) fn move_word_left(&mut self) {
        if self.col == 0 {
            self.move_left();
        } else {
            self.col = self.word_left(self.row, self.col);
        }
    }

    pub(super) fn move_word_right(&mut self) {
        if self.col >= self.lines[self.row].len() {
            self.move_right();
        } else {
            self.col = self.word_right(self.row, self.col);
        }
    }

    pub(super) fn move_vertical(&mut self, dir: i32, allow_history: bool) -> EditorAction {
        if allow_history && self.history_gate() {
            return if dir < 0 { EditorAction::HistoryPrev } else { EditorAction::HistoryNext };
        }
        let w = match self.last_width.get() {
            0 => 76,
            w => w,
        };
        self.sync_wrap(w);
        let (r, seg, x, is_last_seg) = {
            let layout = self.wrap.borrow();
            let (v, x) = self.cursor_visual_in(&layout, w);
            let total = self.total_visual_rows(&layout, w);
            let target = if dir < 0 {
                if v == 0 {
                    // already on the top row of a draft: jump to its start
                    self.col = 0;
                    return EditorAction::None;
                }
                v - 1
            } else {
                if v + 1 >= total {
                    self.col = self.lines[self.row].len();
                    return EditorAction::None;
                }
                v + 1
            };
            let (r, seg) = self.visual_row_at(&layout, w, target);
            let is_last_seg = target + 1 >= total || self.visual_row_at(&layout, w, target + 1).0 != r;
            (r, seg, x, is_last_seg)
        };
        let want = *self.preferred_x.get_or_insert(x);
        let line = &self.lines[r];
        let mut col = seg.start;
        let mut acc = 0;
        for (i, g) in line[seg.clone()].grapheme_indices(true) {
            let gw = grapheme_width(g);
            if acc + gw > want {
                break;
            }
            acc += gw;
            col = seg.start + i + g.len();
        }
        // Don't land at a segment end that belongs to the next segment, unless it's the line end.
        if !is_last_seg && col == seg.end && col > seg.start {
            col = line[seg.start..col]
                .grapheme_indices(true)
                .next_back()
                .map(|(i, _)| seg.start + i)
                .unwrap_or(seg.start);
        }
        self.row = r;
        self.col = self.snap_out_of_token(r, col);
        EditorAction::None
    }
}
