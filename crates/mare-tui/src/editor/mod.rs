//! Multi-line input editor: grapheme-aware cursor, word ops, kill/yank, undo,
//! bracketed-paste folding and a rounded-box renderer that reports the real
//! terminal cursor position (so IME candidate windows land in the right spot).
//!
//! `Editor`, its types and the text accessors live here; the rest of `impl Editor` is spread
//! over `tokens` (paste placeholders), `motion`, `edit` (undo, kill ring), `keymap`,
//! `wrap` (layout cache) and `draw`; `words` holds the word-boundary functions.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::sync::Arc;

use crate::tty::{KeyCode, KeyEvent, KeyEventKind, Modifiers};
use unicode_segmentation::UnicodeSegmentation;

use super::style::*;

mod draw;
mod edit;
mod keymap;
mod motion;
mod text_line;
mod tokens;
mod words;
mod wrap;

pub use keymap::{DEFAULT_TEXT_KEYS, TextOp};
use text_line::TextLine;
use wrap::WrapLayout;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorAction {
    None,
    Changed,
    /// Enter pressed: carries the full text (paste placeholders expanded). The editor is cleared.
    Submit(String),
    HistoryPrev,
    HistoryNext,
    Unhandled,
}

#[derive(Clone, Debug)]
pub struct EditorRenderOpts {
    pub prompt: String,
    pub placeholder: String,
    /// Non-editable text displayed after the cursor when there is room.
    pub hint: Option<String>,
    /// Max visible content rows (0 = unlimited).
    pub max_height: usize,
    /// Theme slot (or color literal) for the border; defaults to "border".
    pub border_color: Option<String>,
    /// Theme slot for the prompt prefix; defaults to "accent".
    pub prompt_style: Option<String>,
    /// Theme slot for the placeholder text; defaults to "dim".
    pub placeholder_style: Option<String>,
    /// Optional mask glyph (e.g. `'•'`) used for secret inputs while keeping cursor/scroll.
    pub mask_char: Option<char>,
}

impl Default for EditorRenderOpts {
    fn default() -> Self {
        Self {
            prompt: "❯ ".into(),
            placeholder: String::new(),
            hint: None,
            max_height: 10,
            border_color: None,
            prompt_style: None,
            placeholder_style: None,
            mask_char: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EditorRender {
    pub lines: Vec<Line>,
    /// (row, col) of the terminal cursor relative to the first returned line.
    pub cursor: (u16, u16),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Element {
    id: u64,
    content: Arc<str>,
    row: usize,
    range: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextElementSnapshot {
    pub id: u64,
    pub range: Range<usize>,
    pub text: String,
}

#[derive(Clone, Debug)]
struct KilledElement {
    row_offset: usize,
    range: Range<usize>,
    id: u64,
    content: Arc<str>,
}

#[derive(Clone, Debug)]
struct KillEntry {
    text: String,
    elements: Vec<KilledElement>,
}

type UndoSnapshot = (Vec<TextLine>, usize, usize, u8, Vec<Element>);

#[derive(Clone, Copy, PartialEq, Eq)]
enum LastOp {
    Other,
    Insert,
    Delete,
    /// A kill (Ctrl+K/U/W, Alt+D…): the next consecutive kill joins the same kill-ring entry.
    Kill,
    /// A yank (Ctrl+Y / Alt+Y): Alt+Y right after it swaps in the previous kill-ring entry.
    Yank,
}

/// Tabs to spaces (tab stop 4), counting columns from `start_col` so a tab typed or pasted
/// mid-line still lands on the next stop.
fn expand_tabs_from(s: &str, start_col: usize) -> String {
    if !s.contains('\t') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut col = start_col;
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

pub struct Editor {
    /// shared lines: undo snapshots copy pointers, and only an edited line is duplicated
    lines: Vec<TextLine>,
    row: usize,
    col: usize,
    elements: Vec<Element>,
    next_element_id: u64,
    last_submitted_elements: Vec<TextElementSnapshot>,
    paste_counter: usize,
    /// Kill ring` (MIT): newest entry last.
    kill_ring: Vec<KillEntry>,
    /// Buffer state before the last yank, so Alt+Y can replace the yanked text.
    yank_base: Option<(Vec<TextLine>, usize, usize, Vec<Element>)>,
    undo: Vec<UndoSnapshot>,
    redo: Vec<UndoSnapshot>,
    selection_anchor: Option<(usize, usize)>,
    last_op: LastOp,
    /// `!` shell mode: a `!` typed on an empty input enters it (a second one makes
    /// it `!!`, output kept out of the context); the marks are not in the buffer, the prompt
    /// shows them. `text()` / `raw_text()` put them back in front, so a submit is `!cmd`.
    shell: u8,
    /// Whether `!` on an empty input enters shell mode (the main input only, not dialogs).
    shell_enabled: bool,
    preferred_x: Option<usize>,
    /// The history entry last put in by `recall`: Up/Down keep browsing history only while the
    /// text still equals it and the cursor is at its start or end (or the input is empty).
    recalled: Option<String>,
    last_width: Cell<usize>,
    scroll: Cell<usize>,
    text_rev: u64,
    wrap: RefCell<WrapLayout>,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    pub fn new() -> Self {
        Self {
            lines: vec![TextLine::default()],
            row: 0,
            col: 0,
            elements: Vec::new(),
            next_element_id: 1,
            last_submitted_elements: Vec::new(),
            paste_counter: 0,
            kill_ring: Vec::new(),
            yank_base: None,
            undo: Vec::new(),
            redo: Vec::new(),
            selection_anchor: None,
            last_op: LastOp::Other,
            shell: 0,
            shell_enabled: false,
            preferred_x: None,
            recalled: None,
            last_width: Cell::new(0),
            scroll: Cell::new(0),
            text_rev: 1,
            wrap: RefCell::new(WrapLayout::default()),
        }
    }

    pub(super) fn bump_text_rev(&mut self) {
        self.text_rev = self.text_rev.wrapping_add(1);
    }

    /// Monotonic revision bumped whenever buffer text or shell-mode prefix changes.
    pub fn text_revision(&self) -> u64 {
        self.text_rev
    }

    /// Current logical line slice.
    pub fn current_line_str(&self) -> &str {
        &self.lines[self.row]
    }

    /// Raw buffer text (paste placeholders left as tokens), shell-mode marks in front.
    pub fn raw_text(&self) -> String {
        let mut out = "!".repeat(self.shell as usize);
        out.push_str(&self.lines.join("\n"));
        out
    }

    /// Let `!` on an empty input enter shell mode (the main input; dialogs keep a literal `!`).
    pub fn enable_shell_mode(&mut self) {
        self.shell_enabled = true;
    }

    /// Back to a literal `!` (the plugin that owns shell mode went away); leaves shell mode.
    pub fn disable_shell_mode(&mut self) {
        self.shell_enabled = false;
        if self.shell != 0 {
            self.shell = 0;
            self.bump_text_rev();
        }
    }

    /// 0: normal input; 1: `!` shell mode; 2: `!!` (output kept out of the context).
    pub fn shell_level(&self) -> u8 {
        self.shell
    }

    fn buffer_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Buffer text with paste placeholders expanded to the pasted content. One pass over the
    /// placeholders' actual positions, so pasted content that
    /// happens to contain another placeholder's text is never expanded a second time.
    pub fn text(&self) -> String {
        if self.elements.is_empty() {
            return self.raw_text();
        }
        self.text_with_elements().0
    }

    /// Buffer text paired with the surviving atomic elements and their byte ranges in the
    /// expanded text.
    pub fn text_with_elements(&self) -> (String, Vec<TextElementSnapshot>) {
        if self.elements.is_empty() {
            return (self.raw_text(), Vec::new());
        }
        let mut out = "!".repeat(self.shell as usize);
        let mut snapshots = Vec::new();
        for (r, line) in self.lines.iter().enumerate() {
            if r > 0 {
                out.push('\n');
            }
            let mut pos = 0;
            for (range, content, id) in self.tokens(r) {
                out.push_str(&line[pos..range.start]);
                let start = out.len();
                out.push_str(&content);
                let end = out.len();
                snapshots.push(TextElementSnapshot { id, range: start..end, text: content.to_string() });
                pos = range.end;
            }
            out.push_str(&line[pos..]);
        }
        (out, snapshots)
    }

    pub fn take_submitted_elements(&mut self) -> Vec<TextElementSnapshot> {
        std::mem::take(&mut self.last_submitted_elements)
    }

    pub fn set_submitted_elements(&mut self, elements: Vec<TextElementSnapshot>) {
        self.last_submitted_elements = elements;
    }

    /// Element IDs still reachable in the current buffer, undo/redo history, kill ring, or last submit.
    pub fn referenced_element_ids(&self) -> std::collections::HashSet<u64> {
        let mut ids: std::collections::HashSet<u64> = self.elements.iter().map(|e| e.id).collect();
        ids.extend(self.undo.iter().flat_map(|(_, _, _, _, elems)| elems.iter().map(|e| e.id)));
        ids.extend(self.redo.iter().flat_map(|(_, _, _, _, elems)| elems.iter().map(|e| e.id)));
        ids.extend(self.kill_ring.iter().flat_map(|entry| entry.elements.iter().map(|e| e.id)));
        ids.extend(self.yank_base.iter().flat_map(|(_, _, _, elems)| elems.iter().map(|e| e.id)));
        ids.extend(self.last_submitted_elements.iter().map(|e| e.id));
        ids
    }

    pub fn set_text(&mut self, text: &str) {
        self.snapshot();
        // a `!cmd` (history, a queued message given back) comes back in shell mode
        let mut text = text;
        self.shell = 0;
        while self.shell_enabled && self.shell < 2 && text.starts_with('!') {
            self.shell += 1;
            text = &text[1..];
        }
        self.lines = normalize_newlines(text).split('\n').map(|l| expand_tabs_from(l, 0).into()).collect();
        self.elements.clear();
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
        self.selection_anchor = None;
        self.preferred_x = None;
        self.last_op = LastOp::Other;
        self.recalled = None;
        self.bump_text_rev();
    }

    /// Prepend `prefix` followed by a blank line in front of the current buffer, preserving
    /// existing elements on the shifted rows.
    pub fn prepend_text(&mut self, prefix: &str) {
        if self.is_empty() {
            self.set_text(prefix);
            return;
        }
        self.snapshot();
        let mut prefix = prefix;
        if self.shell_enabled && self.shell == 0 {
            while self.shell < 2 && prefix.starts_with('!') {
                self.shell += 1;
                prefix = &prefix[1..];
            }
        }
        self.row = 0;
        self.col = 0;
        self.selection_anchor = None;
        self.insert_str(&format!("{prefix}\n\n"));
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
        self.preferred_x = None;
        self.last_op = LastOp::Other;
        self.recalled = None;
    }

    /// Put a history entry in the input. Up-recall leaves the cursor at the start (so the next
    /// Up keeps going back), Down-recall at the end; either way the next Up/Down keeps
    /// browsing while the text is unchanged and the cursor stays at that start/end.
    pub fn recall(&mut self, text: &str, at_start: bool) {
        self.set_text(text);
        if at_start {
            self.row = 0;
            self.col = 0;
        }
        self.recalled = Some(self.raw_text());
    }

    /// Whether Up/Down should browse history instead of moving the cursor:
    /// empty input, or the untouched recalled entry with the cursor
    /// at its very start or end.
    fn history_gate(&self) -> bool {
        if self.is_empty() {
            return true;
        }
        let last = self.lines.len() - 1;
        let at_edge = (self.row == 0 && self.col == 0) || (self.row == last && self.col == self.lines[last].len());
        at_edge && self.recalled.as_deref().is_some_and(|r| r == self.raw_text())
    }

    pub fn clear(&mut self) {
        self.shell = 0;
        self.lines = vec![TextLine::default()];
        self.row = 0;
        self.col = 0;
        self.elements.clear();
        self.selection_anchor = None;
        self.preferred_x = None;
        self.last_op = LastOp::Other;
        self.recalled = None;
        self.yank_base = None;
        self.scroll.set(0);
        self.bump_text_rev();
    }

    /// Ordered `(start, end)` selection range in `(row, byte_col)` when non-empty.
    pub fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.selection_anchor?;
        let head = (self.row, self.col);
        if anchor == head {
            return None;
        }
        if anchor <= head { Some((anchor, head)) } else { Some((head, anchor)) }
    }

    /// Raw `(anchor, head)` selection endpoints when non-empty.
    pub fn selection_endpoints(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.selection_anchor?;
        let head = (self.row, self.col);
        (anchor != head).then_some((anchor, head))
    }

    /// Raw buffer text currently selected, joining lines with `\n`.
    pub fn selected_text(&self) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.selection()?;
        if sr == er {
            return Some(self.lines[sr][sc..ec].to_string());
        }
        let mut out = String::from(&self.lines[sr][sc..]);
        for line in &self.lines[sr + 1..er] {
            out.push('\n');
            out.push_str(line);
        }
        out.push('\n');
        out.push_str(&self.lines[er][..ec]);
        Some(out)
    }

    pub(super) fn begin_selection(&mut self) {
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some((self.row, self.col));
        }
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    /// Clear, but as one undoable edit: undo brings the text back (Ctrl+C on a draft). Elements
    /// own their content via `Arc<str>`, so restored placeholders still expand.
    pub fn clear_undoable(&mut self) {
        if self.is_empty() {
            return;
        }
        self.snapshot();
        self.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.shell == 0 && self.buffer_empty()
    }

    /// (logical row, byte column within that row).
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn insert_str(&mut self, s: &str) {
        self.selection_anchor = None;
        self.preferred_x = None;
        if s.is_empty() {
            return;
        }
        let s = normalize_newlines(s);
        let old_row = self.row;
        let old_col = self.col;
        let mut parts = s.split('\n');
        let first = parts.next().unwrap_or("");
        let first: Cow<'_, str> = if first.contains('\t') {
            let start_col = str_width(&self.lines[self.row][..self.col]);
            Cow::Owned(expand_tabs_from(first, start_col))
        } else {
            Cow::Borrowed(first)
        };
        let first_len = first.len();
        let mut extra_rows = 0usize;
        let mut last_part_len = 0usize;
        if let Some(second) = parts.next() {
            let tail = self.lines[self.row].split_off(self.col);
            self.lines[self.row].push_str(&first);
            let mut inserted: Vec<String> = Vec::new();
            let push_part = |inserted: &mut Vec<String>, p: &str| {
                if p.contains('\t') {
                    inserted.push(expand_tabs_from(p, 0));
                } else {
                    inserted.push(p.to_string());
                }
            };
            push_part(&mut inserted, second);
            for part in parts {
                push_part(&mut inserted, part);
            }
            extra_rows = inserted.len();
            if let Some(last) = inserted.last_mut() {
                last_part_len = last.len();
                last.push_str(&tail);
            }
            self.lines.splice(old_row + 1..old_row + 1, inserted.into_iter().map(TextLine::from));
            self.row = old_row + extra_rows;
            self.col = last_part_len;
        } else {
            let line = &mut self.lines[self.row];
            if self.col == line.len() {
                line.push_str(&first);
            } else {
                line.insert_str(self.col, &first);
            }
            self.col += first_len;
        }
        self.bump_text_rev();
        if !self.elements.is_empty() {
            self.elements.retain_mut(|e| {
                if e.row > old_row {
                    e.row += extra_rows;
                    true
                } else if e.row == old_row {
                    if e.range.end <= old_col {
                        true
                    } else if e.range.start >= old_col {
                        if extra_rows == 0 {
                            e.range.start += first_len;
                            e.range.end += first_len;
                        } else {
                            e.row += extra_rows;
                            e.range.start = e.range.start - old_col + last_part_len;
                            e.range.end = e.range.end - old_col + last_part_len;
                        }
                        true
                    } else {
                        false
                    }
                } else {
                    true
                }
            });
        }
    }
}

fn normalize_newlines(s: &str) -> Cow<'_, str> {
    if !s.contains('\r') {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.replace("\r\n", "\n").replace('\r', "\n"))
}

#[cfg(test)]
mod tests;
