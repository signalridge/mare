//! Edits: undo snapshots, deletion, the kill ring and yank, and completion-token replacement.

use super::*;

impl Editor {
    fn current_snapshot(&self) -> UndoSnapshot {
        (self.lines.clone(), self.row, self.col, self.shell, self.elements.clone())
    }

    fn push_capped(stack: &mut Vec<UndoSnapshot>, snap: UndoSnapshot) {
        stack.push(snap);
        if stack.len() > 200 {
            stack.remove(0);
        }
    }

    pub(super) fn snapshot(&mut self) {
        let snap = self.current_snapshot();
        Self::push_capped(&mut self.undo, snap);
        self.redo.clear();
    }

    pub(super) fn begin(&mut self, op: LastOp) {
        if op == LastOp::Other || op != self.last_op {
            self.snapshot();
        }
        self.last_op = op;
    }

    fn restore_snapshot(&mut self, snap: UndoSnapshot, to_redo: bool) {
        let prev = self.current_snapshot();
        if to_redo {
            Self::push_capped(&mut self.redo, prev);
        } else {
            Self::push_capped(&mut self.undo, prev);
        }
        let (lines, row, col, shell, elements) = snap;
        self.lines = lines;
        self.shell = shell;
        self.elements = elements;
        self.row = row.min(self.lines.len() - 1);
        self.col = col.min(self.lines[self.row].len());
        self.selection_anchor = None;
        self.last_op = LastOp::Other;
        self.bump_text_rev();
    }

    /// Delete the active selection atomically (expanding endpoints across atomic elements).
    pub(super) fn delete_selection_internal(&mut self) -> bool {
        let Some(((sr, mut sc), (er, mut ec))) = self.selection() else {
            self.selection_anchor = None;
            return false;
        };
        for e in &self.elements {
            if e.row == sr && e.range.start < sc && sc < e.range.end {
                sc = sc.min(e.range.start);
            }
            if e.row == er && e.range.start < ec && ec < e.range.end {
                ec = ec.max(e.range.end);
            }
        }
        if sr == er {
            self.delete_range(sr, sc..ec);
        } else {
            let tail = self.lines[er][ec..].to_string();
            self.elements.retain_mut(|e| {
                if e.row < sr {
                    true
                } else if e.row == sr {
                    e.range.end <= sc
                } else if e.row < er {
                    false
                } else if e.row == er {
                    if e.range.start < ec {
                        false
                    } else {
                        e.row = sr;
                        e.range.start = e.range.start - ec + sc;
                        e.range.end = e.range.end - ec + sc;
                        true
                    }
                } else {
                    e.row -= er - sr;
                    true
                }
            });
            self.lines.drain(sr + 1..=er);
            self.lines[sr].truncate(sc);
            self.lines[sr].push_str(&tail);
            self.bump_text_rev();
        }
        self.row = sr;
        self.col = sc;
        self.selection_anchor = None;
        self.preferred_x = None;
        true
    }

    pub(super) fn delete_active_selection(&mut self) -> bool {
        if self.selection().is_none() {
            self.selection_anchor = None;
            return false;
        }
        self.begin(LastOp::Other);
        self.delete_selection_internal()
    }

    pub(super) fn delete_range(&mut self, row: usize, range: Range<usize>) -> String {
        let len = range.len();
        if len > 0 && !self.elements.is_empty() {
            self.elements.retain_mut(|e| {
                if e.row != row {
                    return true;
                }
                if e.range.start < range.end && range.start < e.range.end {
                    return false;
                }
                if e.range.start >= range.end {
                    e.range.start -= len;
                    e.range.end -= len;
                }
                true
            });
        }
        let removed: String = self.lines[row].drain(range.clone()).collect();
        self.col = range.start;
        if !removed.is_empty() {
            self.bump_text_rev();
        }
        removed
    }

    pub(super) fn backspace(&mut self) -> bool {
        if self.col > 0 {
            let start = self.prev_boundary(self.row, self.col);
            self.delete_range(self.row, start..self.col);
            true
        } else if self.row > 0 {
            let joined_row = self.row;
            let line = self.lines.remove(joined_row);
            self.row -= 1;
            let prefix_len = self.lines[self.row].len();
            self.col = prefix_len;
            self.lines[self.row].push_str(&line);
            for e in &mut self.elements {
                if e.row == joined_row {
                    e.row = self.row;
                    e.range.start += prefix_len;
                    e.range.end += prefix_len;
                } else if e.row > joined_row {
                    e.row -= 1;
                }
            }
            self.bump_text_rev();
            true
        } else {
            false
        }
    }

    pub(super) fn delete_forward(&mut self) -> bool {
        if self.col < self.lines[self.row].len() {
            let end = self.next_boundary(self.row, self.col);
            self.delete_range(self.row, self.col..end);
            true
        } else if self.row + 1 < self.lines.len() {
            let joined_row = self.row + 1;
            let next = self.lines.remove(joined_row);
            let prefix_len = self.lines[self.row].len();
            self.lines[self.row].push_str(&next);
            for e in &mut self.elements {
                if e.row == joined_row {
                    e.row = self.row;
                    e.range.start += prefix_len;
                    e.range.end += prefix_len;
                } else if e.row > joined_row {
                    e.row -= 1;
                }
            }
            self.bump_text_rev();
            true
        } else {
            false
        }
    }

    pub(super) fn insert_newline(&mut self) {
        self.begin(LastOp::Other);
        self.delete_selection_internal();
        self.insert_str("\n");
    }

    pub(super) fn undo(&mut self) -> bool {
        let Some(snap) = self.undo.pop() else {
            return false;
        };
        self.restore_snapshot(snap, true);
        true
    }

    pub(super) fn redo(&mut self) -> bool {
        let Some(snap) = self.redo.pop() else {
            return false;
        };
        self.restore_snapshot(snap, false);
        true
    }

    /// Delete `range` of the current row into the kill ring. Consecutive kills accumulate
    /// into one entry (backward kills prepend, forward kills append)`.
    pub(super) fn kill_range(&mut self, range: Range<usize>, backward: bool) {
        let accumulate = self.last_op == LastOp::Kill;
        self.begin(LastOp::Other);
        self.selection_anchor = None;
        let killed_elements: Vec<_> = self
            .elements
            .iter()
            .filter(|e| e.row == self.row && e.range.start >= range.start && e.range.end <= range.end)
            .map(|e| KilledElement {
                row_offset: 0,
                range: (e.range.start - range.start)..(e.range.end - range.start),
                id: e.id,
                content: Arc::clone(&e.content),
            })
            .collect();
        let text = self.delete_range(self.row, range);
        self.push_kill_entry(KillEntry { text, elements: killed_elements }, backward, accumulate);
        self.last_op = LastOp::Kill;
    }

    pub(super) fn push_kill(&mut self, text: String, backward: bool, accumulate: bool) {
        self.push_kill_entry(KillEntry { text, elements: Vec::new() }, backward, accumulate);
    }

    fn push_kill_entry(&mut self, entry: KillEntry, backward: bool, accumulate: bool) {
        if entry.text.is_empty() {
            return;
        }
        match self.kill_ring.last_mut() {
            Some(last) if accumulate => {
                if backward {
                    let nl = entry.text.bytes().filter(|&b| b == b'\n').count();
                    let tail_len = entry.text.rsplit('\n').next().map_or(0, str::len);
                    for el in &mut last.elements {
                        if el.row_offset == 0 {
                            el.range.start += tail_len;
                            el.range.end += tail_len;
                        }
                        el.row_offset += nl;
                    }
                    let mut combined = entry.elements;
                    combined.append(&mut last.elements);
                    last.elements = combined;
                    last.text.insert_str(0, &entry.text);
                } else {
                    let nl = last.text.bytes().filter(|&b| b == b'\n').count();
                    let tail_len = last.text.rsplit('\n').next().map_or(0, str::len);
                    for mut el in entry.elements {
                        if el.row_offset == 0 {
                            el.range.start += tail_len;
                            el.range.end += tail_len;
                        }
                        el.row_offset += nl;
                        last.elements.push(el);
                    }
                    last.text.push_str(&entry.text);
                }
            }
            _ => {
                self.kill_ring.push(entry);
                if self.kill_ring.len() > 30 {
                    self.kill_ring.remove(0);
                }
            }
        }
    }

    fn insert_kill_entry(&mut self, entry: &KillEntry) {
        let row = self.row;
        let col = self.col;
        self.insert_str(&entry.text);
        for el in &entry.elements {
            let target_row = row + el.row_offset;
            let range =
                if el.row_offset == 0 { (col + el.range.start)..(col + el.range.end) } else { el.range.clone() };
            let Some(line) = self.lines.get(target_row) else { continue };
            if range.start < range.end
                && range.end <= line.len()
                && line.is_char_boundary(range.start)
                && line.is_char_boundary(range.end)
            {
                self.elements.push(Element { id: el.id, content: Arc::clone(&el.content), row: target_row, range });
            }
        }
    }

    /// Ctrl+Y: insert the newest kill.
    pub(super) fn yank(&mut self) -> EditorAction {
        let Some(entry) = self.kill_ring.last().cloned() else { return EditorAction::None };
        self.begin(LastOp::Other);
        self.delete_selection_internal();
        self.yank_base = Some((self.lines.clone(), self.row, self.col, self.elements.clone()));
        self.insert_kill_entry(&entry);
        self.last_op = LastOp::Yank;
        EditorAction::Changed
    }

    /// Alt+Y right after a yank: replace the yanked text with the next older kill.
    pub(super) fn yank_pop(&mut self) -> EditorAction {
        if self.last_op != LastOp::Yank || self.kill_ring.len() < 2 {
            return EditorAction::None;
        }
        let Some((lines, row, col, elements)) = self.yank_base.clone() else { return EditorAction::None };
        let newest = self.kill_ring.pop().expect("len >= 2");
        self.kill_ring.insert(0, newest);
        self.lines = lines;
        self.row = row;
        self.col = col;
        self.elements = elements;
        self.selection_anchor = None;
        self.redo.clear();
        self.bump_text_rev();
        let entry = self.kill_ring.last().cloned();
        if let Some(entry) = entry {
            self.insert_kill_entry(&entry);
        }
        self.last_op = LastOp::Yank;
        EditorAction::Changed
    }

    pub(super) fn delete_word_back(&mut self) -> EditorAction {
        if self.delete_active_selection() {
            return EditorAction::Changed;
        }
        if self.col == 0 {
            if self.row == 0 {
                return EditorAction::None;
            }
            self.begin(LastOp::Delete);
            return if self.backspace() { EditorAction::Changed } else { EditorAction::None };
        }
        let start = match self.token_ending_at(self.row, self.col) {
            Some(r) => r.start,
            None => self.word_left(self.row, self.col),
        };
        self.kill_range(start..self.col, true);
        EditorAction::Changed
    }

    pub(super) fn delete_word_forward(&mut self) -> EditorAction {
        if self.delete_active_selection() {
            return EditorAction::Changed;
        }
        let end = self.word_right(self.row, self.col);
        if end > self.col {
            self.kill_range(self.col..end, false);
            EditorAction::Changed
        } else {
            EditorAction::None
        }
    }

    /// Whitespace-delimited token before the cursor on the current line: (byte start, token).
    /// An unclosed `@"path with spaces` (the quoted file reference) is one token.
    pub fn current_token(&self) -> Option<(usize, String)> {
        let line = &self.lines[self.row];
        let before = &line[..self.col];
        if let Some(q) = before.rfind("@\"")
            && (q == 0 || before[..q].ends_with(char::is_whitespace))
            && !before[q + 2..].contains('"')
        {
            return Some((q, before[q..].to_string()));
        }
        let start =
            before.rfind(char::is_whitespace).map(|i| i + before[i..].chars().next().unwrap().len_utf8()).unwrap_or(0);
        if start == self.col {
            return None;
        }
        Some((start, before[start..].to_string()))
    }

    /// Clamp `(row, byte_col)` to valid buffer coordinates on a grapheme boundary.
    pub fn clamp_pos(&self, row: usize, col: usize) -> (usize, usize) {
        let r = row.min(self.lines.len().saturating_sub(1));
        let line = &self.lines[r];
        let snapped = mare_shared::ui::clamp_line_grapheme(line, col);
        (r, self.snap_out_of_token(r, snapped))
    }

    /// Move cursor to `(row, byte_col)` clamped to a grapheme boundary and clear selection.
    pub fn set_cursor(&mut self, row: usize, col: usize) {
        let (r, c) = self.clamp_pos(row, col);
        self.row = r;
        self.col = c;
        self.selection_anchor = None;
        self.preferred_x = None;
        self.last_op = LastOp::Other;
    }

    /// Set `(anchor, head)` selection (or clear when either is `None`).
    pub fn set_selection(&mut self, anchor: Option<(usize, usize)>, head: Option<(usize, usize)>) {
        match (anchor, head) {
            (Some((ar, ac)), Some((hr, hc))) => {
                let a = self.clamp_pos(ar, ac);
                let h = self.clamp_pos(hr, hc);
                self.row = h.0;
                self.col = h.1;
                self.selection_anchor = (a != h).then_some(a);
                self.preferred_x = None;
                self.last_op = LastOp::Other;
            }
            _ => {
                self.selection_anchor = None;
            }
        }
    }

    /// Insert `text` at the cursor (replacing any active selection) as a single undoable edit.
    pub fn insert_at_cursor(&mut self, text: &str) {
        self.begin(LastOp::Other);
        self.delete_selection_internal();
        if !text.is_empty() {
            self.insert_str(text);
        }
    }

    /// Replace `(start..end)` (or current selection/cursor when either bound is `None`) with `text`.
    pub fn replace_range(&mut self, start: Option<(usize, usize)>, end: Option<(usize, usize)>, text: &str) {
        let (Some((sr, sc)), Some((er, ec))) = (start, end) else {
            self.insert_at_cursor(text);
            return;
        };
        let a = self.clamp_pos(sr, sc);
        let b = self.clamp_pos(er, ec);
        let (from, to) = if a <= b { (a, b) } else { (b, a) };
        self.begin(LastOp::Other);
        if from == to {
            self.row = from.0;
            self.col = from.1;
            self.selection_anchor = None;
        } else {
            self.selection_anchor = Some(from);
            self.row = to.0;
            self.col = to.1;
            self.delete_selection_internal();
        }
        if !text.is_empty() {
            self.insert_str(text);
        }
    }

    /// Replace the token before the cursor with `s` (cursor ends after it).
    pub fn replace_current_token(&mut self, s: &str) {
        let start = self.current_token().map(|(st, _)| st).unwrap_or(self.col);
        self.begin(LastOp::Other);
        self.selection_anchor = None;
        self.delete_range(self.row, start..self.col);
        self.insert_str(s);
    }
}
