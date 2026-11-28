//! Paste placeholders: atomic tokens in the buffer that `text()` expands to their content.

use super::*;

impl Editor {
    pub(super) fn alloc_element_id(&mut self) -> u64 {
        let id = self.next_element_id;
        self.next_element_id += 1;
        id
    }

    /// Tracked atomic elements on `row`, in byte order: `(byte range, content, element id)`.
    pub(super) fn tokens(&self, row: usize) -> Vec<(Range<usize>, Arc<str>, u64)> {
        let Some(line) = self.lines.get(row) else {
            return Vec::new();
        };
        if self.elements.is_empty() {
            return Vec::new();
        }
        let len = line.len();
        let mut out: Vec<_> = self
            .elements
            .iter()
            .filter(|e| e.row == row)
            .filter_map(|e| {
                let start = e.range.start.min(len);
                let end = e.range.end.min(len);
                (start < end && line.is_char_boundary(start) && line.is_char_boundary(end))
                    .then(|| (start..end, Arc::clone(&e.content), e.id))
            })
            .collect();
        out.sort_by_key(|(r, _, _)| r.start);
        out
    }

    pub(super) fn token_ranges(&self, row: usize) -> Vec<Range<usize>> {
        self.tokens(row).into_iter().map(|(r, _, _)| r).collect()
    }

    /// Token that the cursor is strictly inside or immediately after (start < col <= end).
    pub(super) fn token_ending_at(&self, row: usize, col: usize) -> Option<Range<usize>> {
        self.token_ranges(row).into_iter().find(|r| r.start < col && col <= r.end)
    }

    /// Token that the cursor is at or inside (start <= col < end).
    pub(super) fn token_starting_at(&self, row: usize, col: usize) -> Option<Range<usize>> {
        self.token_ranges(row).into_iter().find(|r| r.start <= col && col < r.end)
    }

    pub(super) fn snap_out_of_token(&self, row: usize, col: usize) -> usize {
        match self.token_ranges(row).into_iter().find(|r| r.start < col && col < r.end) {
            Some(r) => r.start,
            None => col,
        }
    }

    fn push_element(&mut self, token: &str, content: &str) -> u64 {
        let row = self.row;
        let start = self.col;
        self.insert_str(token);
        let id = self.alloc_element_id();
        self.elements.push(Element { id, content: Arc::from(content), row, range: start..self.col });
        id
    }

    /// Inline paste without folding (shared by `handle_paste` and `Modal::Input`):
    /// strips terminal escape sequences and control characters, normalizes line endings,
    /// and replaces any active selection as one undo step.
    pub fn paste_inline(&mut self, text: &str) {
        let norm = mare_shared::ansi::sanitize(text, false);
        self.begin(LastOp::Other);
        self.delete_selection_internal();
        self.insert_str(&norm);
        self.last_op = LastOp::Other;
    }

    /// Bracketed paste. Strips terminal escape sequences, normalizes line endings, and turns
    /// pasted Tabs into two spaces. Large pastes collapse into an atomic placeholder token.
    pub fn handle_paste(&mut self, text: &str) {
        let norm = mare_shared::ansi::sanitize(text, false).replace('\t', "  ");
        let nlines = norm.split('\n').count();
        let nchars = norm.chars().count();
        if nlines <= 10 && nchars <= 800 {
            self.paste_inline(&norm);
            return;
        }
        self.begin(LastOp::Other);
        self.delete_selection_internal();
        self.paste_counter += 1;
        let token = if nlines == 1 {
            format!("[Pasted text #{} ({} chars)]", self.paste_counter, nchars)
        } else {
            format!("[Pasted text #{} +{} lines]", self.paste_counter, nlines)
        };
        self.push_element(&token, &norm);
        self.last_op = LastOp::Other;
    }

    /// Snapshot tracked atomic elements in document order: `(id, placeholder_token, expanded_content)`.
    pub fn snapshot_tokens(&self) -> Vec<(u64, String, String)> {
        let mut out = Vec::new();
        for (row, line) in self.lines.iter().enumerate() {
            for (range, content, id) in self.tokens(row) {
                out.push((id, line[range].to_string(), content.to_string()));
            }
        }
        out
    }

    /// Restore an existing atomic marker at its exact byte range in `text` without matching
    /// lookalike prose elsewhere in the buffer.
    pub fn register_element_in_text(&mut self, text: &str, byte_range: Range<usize>, token: &str) -> Option<u64> {
        self.register_element_with_content_in_text(text, byte_range, token, token)
    }

    fn resolve_element_span(&self, text: &str, byte_range: Range<usize>, token: &str) -> Option<(usize, usize, usize)> {
        let shell_prefix =
            if self.shell > 0 && text.starts_with(&"!".repeat(self.shell as usize)) { self.shell as usize } else { 0 };
        let adj_start = byte_range.start.checked_sub(shell_prefix)?;
        let adj_end = byte_range.end.checked_sub(shell_prefix)?;
        let prefix = normalize_newlines(&text[shell_prefix..shell_prefix + adj_start]);
        let row = prefix.bytes().filter(|&b| b == b'\n').count();
        let line_prefix = prefix.rsplit('\n').next().unwrap_or("");
        let col_start = expand_tabs_from(line_prefix, 0).len();
        let col_end = col_start + (adj_end - adj_start);
        if self.lines.get(row).and_then(|l| l.get(col_start..col_end)) != Some(token) {
            return None;
        }
        Some((row, col_start, col_end))
    }

    /// Restore an atomic marker at `byte_range` in `text`, binding it to `content`.
    pub fn register_element_with_content_in_text(
        &mut self,
        text: &str,
        byte_range: Range<usize>,
        token: &str,
        content: &str,
    ) -> Option<u64> {
        let (row, col_start, col_end) = self.resolve_element_span(text, byte_range, token)?;
        let id = self.alloc_element_id();
        self.elements.push(Element { id, content: Arc::from(content), row, range: col_start..col_end });
        Some(id)
    }

    /// Restore an atomic marker at `byte_range` in `text`, reusing `id` so undo snapshots before
    /// a whole-buffer replacement keep the same element identity as the restored buffer.
    pub fn register_element_with_id_in_text(
        &mut self,
        id: u64,
        text: &str,
        byte_range: Range<usize>,
        token: &str,
        content: &str,
    ) -> bool {
        let Some((row, col_start, col_end)) = self.resolve_element_span(text, byte_range, token) else {
            return false;
        };
        self.next_element_id = self.next_element_id.max(id.saturating_add(1));
        self.elements.push(Element { id, content: Arc::from(content), row, range: col_start..col_end });
        true
    }

    /// Insert an atomic placeholder (deleted as a whole, expanded to `content` by `text()`).
    /// Used for `[Image #N]` attachment markers.
    pub fn insert_token(&mut self, token: &str, content: &str) -> u64 {
        self.begin(LastOp::Other);
        self.delete_selection_internal();
        let id = self.push_element(token, content);
        self.last_op = LastOp::Other;
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_tokens_out_of_bounds_and_mismatched_range_behave_safely() {
        let mut ed = Editor::new();
        assert!(ed.tokens(99).is_empty());
        assert!(ed.snapshot_tokens().is_empty());

        let id = ed.insert_token("[Image #1]", "img-payload");
        let snap = ed.snapshot_tokens();
        assert_eq!(snap, vec![(id, "[Image #1]".to_string(), "img-payload".to_string())]);

        // Restoring an element at a mismatched text range returns None instead of corrupting state.
        let mut ed2 = Editor::new();
        ed2.set_text("hello world");
        assert!(ed2.register_element_in_text("hello world", 0..5, "WRONG").is_none());
    }

    #[test]
    fn register_element_with_id_in_text_preserves_id_across_set_text_and_undo() {
        let mut ed = Editor::new();
        ed.insert_str("before ");
        let old_id = ed.insert_token("[Image #1]", "[Image #1]");
        assert_eq!(ed.snapshot_tokens(), vec![(old_id, "[Image #1]".into(), "[Image #1]".into())]);

        // Whole-buffer replacement (as after external editor) reuses old_id so pre-edit undo
        // snapshots and the restored buffer share the same element identity.
        let edited = "after [Image #1]";
        let start = edited.find("[Image #1]").unwrap();
        let end = start + "[Image #1]".len();
        ed.set_text(edited);
        assert!(ed.register_element_with_id_in_text(old_id, edited, start..end, "[Image #1]", "[Image #1]"));
        assert_eq!(ed.snapshot_tokens(), vec![(old_id, "[Image #1]".into(), "[Image #1]".into())]);

        // Future allocations still advance past old_id.
        let next_id = ed.insert_token("[Image #2]", "[Image #2]");
        assert!(next_id > old_id);

        // Undoing back to the pre-edit buffer restores the same old_id.
        ed.apply(TextOp::Undo);
        ed.apply(TextOp::Undo);
        assert_eq!(ed.raw_text(), "before [Image #1]");
        assert_eq!(ed.snapshot_tokens(), vec![(old_id, "[Image #1]".into(), "[Image #1]".into())]);
        assert!(ed.referenced_element_ids().contains(&old_id));
    }

    #[test]
    fn repro_243_ansi_formatting_stripped_on_paste() {
        let mut ed = Editor::new();
        ed.handle_paste("\x1b[31mhello\x1b[0m");
        assert_eq!(ed.text(), "hello", "ANSI formatting escapes were retained verbatim in pasted text");
    }

    #[test]
    fn paste_strips_csi_osc_and_controls_for_inline_and_folded_pastes() {
        let mut ed = Editor::new();
        ed.paste_inline("\x1b[1;32mgreen\x1b[0m\x1b]8;;https://example.com\x07link\x1b]8;;\x07\r\nline2");
        assert_eq!(ed.text(), "greenlink\nline2");

        let mut folded = Editor::new();
        let big: String = (0..12).map(|i| format!("\x1b[36mrow {i}\x1b[0m\r\n")).collect();
        folded.handle_paste(&big);
        assert!(folded.raw_text().starts_with("[Pasted text #1 +13 lines]"));
        let expected: String = (0..12).map(|i| format!("row {i}\n")).collect();
        assert_eq!(folded.text(), expected);
    }
}
