//! Key handling: named `TextOp` line-edit operations, default key table, and install-screen `handle_key`.

use super::*;

/// Core line-editing operations (`text.*` keymap actions).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextOp {
    Newline,
    Left,
    Right,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    Up,
    Down,
    SelectLeft,
    SelectRight,
    SelectWordLeft,
    SelectWordRight,
    SelectLineStart,
    SelectLineEnd,
    SelectUp,
    SelectDown,
    SelectAll,
    DeleteBack,
    DeleteWordBack,
    DeleteForward,
    DeleteWordForward,
    KillLineStart,
    KillLineEnd,
    Yank,
    YankPop,
    Undo,
    Redo,
}

impl TextOp {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Newline => "text.newline",
            Self::Left => "text.left",
            Self::Right => "text.right",
            Self::WordLeft => "text.word_left",
            Self::WordRight => "text.word_right",
            Self::LineStart => "text.line_start",
            Self::LineEnd => "text.line_end",
            Self::Up => "text.up",
            Self::Down => "text.down",
            Self::SelectLeft => "text.select_left",
            Self::SelectRight => "text.select_right",
            Self::SelectWordLeft => "text.select_word_left",
            Self::SelectWordRight => "text.select_word_right",
            Self::SelectLineStart => "text.select_line_start",
            Self::SelectLineEnd => "text.select_line_end",
            Self::SelectUp => "text.select_up",
            Self::SelectDown => "text.select_down",
            Self::SelectAll => "text.select_all",
            Self::DeleteBack => "text.delete_back",
            Self::DeleteWordBack => "text.delete_word_back",
            Self::DeleteForward => "text.delete_forward",
            Self::DeleteWordForward => "text.delete_word_forward",
            Self::KillLineStart => "text.kill_line_start",
            Self::KillLineEnd => "text.kill_line_end",
            Self::Yank => "text.yank",
            Self::YankPop => "text.yank_pop",
            Self::Undo => "text.undo",
            Self::Redo => "text.redo",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "text.newline" => Self::Newline,
            "text.left" => Self::Left,
            "text.right" => Self::Right,
            "text.word_left" => Self::WordLeft,
            "text.word_right" => Self::WordRight,
            "text.line_start" => Self::LineStart,
            "text.line_end" => Self::LineEnd,
            "text.up" => Self::Up,
            "text.down" => Self::Down,
            "text.select_left" => Self::SelectLeft,
            "text.select_right" => Self::SelectRight,
            "text.select_word_left" => Self::SelectWordLeft,
            "text.select_word_right" => Self::SelectWordRight,
            "text.select_line_start" => Self::SelectLineStart,
            "text.select_line_end" => Self::SelectLineEnd,
            "text.select_up" => Self::SelectUp,
            "text.select_down" => Self::SelectDown,
            "text.select_all" => Self::SelectAll,
            "text.delete_back" => Self::DeleteBack,
            "text.delete_word_back" => Self::DeleteWordBack,
            "text.delete_forward" => Self::DeleteForward,
            "text.delete_word_forward" => Self::DeleteWordForward,
            "text.kill_line_start" => Self::KillLineStart,
            "text.kill_line_end" => Self::KillLineEnd,
            "text.yank" => Self::Yank,
            "text.yank_pop" => Self::YankPop,
            "text.undo" => Self::Undo,
            "text.redo" => Self::Redo,
            _ => return None,
        })
    }
}

/// Canonical lowercase key → `TextOp` defaults shared with `keymap-default`.
pub const DEFAULT_TEXT_KEYS: &[(&str, TextOp)] = &[
    ("shift+enter", TextOp::Newline),
    ("alt+enter", TextOp::Newline),
    ("alt+shift+enter", TextOp::Newline),
    ("ctrl+j", TextOp::Newline),
    ("left", TextOp::Left),
    ("shift+left", TextOp::SelectLeft),
    ("ctrl+b", TextOp::Left),
    ("right", TextOp::Right),
    ("shift+right", TextOp::SelectRight),
    ("ctrl+f", TextOp::Right),
    ("ctrl+left", TextOp::WordLeft),
    ("ctrl+shift+left", TextOp::SelectWordLeft),
    ("alt+left", TextOp::WordLeft),
    ("alt+shift+left", TextOp::SelectWordLeft),
    ("alt+b", TextOp::WordLeft),
    ("ctrl+right", TextOp::WordRight),
    ("ctrl+shift+right", TextOp::SelectWordRight),
    ("alt+right", TextOp::WordRight),
    ("alt+shift+right", TextOp::SelectWordRight),
    ("alt+f", TextOp::WordRight),
    ("home", TextOp::LineStart),
    ("shift+home", TextOp::SelectLineStart),
    ("ctrl+home", TextOp::LineStart),
    ("ctrl+a", TextOp::LineStart),
    ("end", TextOp::LineEnd),
    ("shift+end", TextOp::SelectLineEnd),
    ("ctrl+end", TextOp::LineEnd),
    ("ctrl+e", TextOp::LineEnd),
    ("up", TextOp::Up),
    ("shift+up", TextOp::SelectUp),
    ("ctrl+p", TextOp::Up),
    ("down", TextOp::Down),
    ("shift+down", TextOp::SelectDown),
    ("ctrl+n", TextOp::Down),
    ("backspace", TextOp::DeleteBack),
    ("shift+backspace", TextOp::DeleteBack),
    ("ctrl+h", TextOp::DeleteBack),
    ("ctrl+backspace", TextOp::DeleteWordBack),
    ("alt+backspace", TextOp::DeleteWordBack),
    ("ctrl+w", TextOp::DeleteWordBack),
    ("delete", TextOp::DeleteForward),
    ("shift+delete", TextOp::DeleteForward),
    ("ctrl+d", TextOp::DeleteForward),
    ("ctrl+delete", TextOp::DeleteWordForward),
    ("alt+delete", TextOp::DeleteWordForward),
    ("alt+d", TextOp::DeleteWordForward),
    ("ctrl+u", TextOp::KillLineStart),
    ("ctrl+k", TextOp::KillLineEnd),
    ("ctrl+y", TextOp::Yank),
    ("alt+y", TextOp::YankPop),
    ("ctrl+z", TextOp::Undo),
    ("ctrl+_", TextOp::Undo),
    ("ctrl+shift+z", TextOp::Redo),
    ("alt+shift+z", TextOp::Redo),
];

impl Editor {
    fn move_cursor_op(&mut self, op: TextOp) -> EditorAction {
        match op {
            TextOp::Left | TextOp::SelectLeft => {
                self.move_left();
                EditorAction::None
            }
            TextOp::Right | TextOp::SelectRight => {
                self.move_right();
                EditorAction::None
            }
            TextOp::WordLeft | TextOp::SelectWordLeft => {
                self.move_word_left();
                EditorAction::None
            }
            TextOp::WordRight | TextOp::SelectWordRight => {
                self.move_word_right();
                EditorAction::None
            }
            TextOp::LineStart | TextOp::SelectLineStart => {
                self.col = 0;
                EditorAction::None
            }
            TextOp::LineEnd | TextOp::SelectLineEnd => {
                self.col = self.lines[self.row].len();
                EditorAction::None
            }
            TextOp::Up => self.move_vertical(-1, true),
            TextOp::Down => self.move_vertical(1, true),
            TextOp::SelectUp => self.move_vertical(-1, false),
            TextOp::SelectDown => self.move_vertical(1, false),
            _ => unreachable!(),
        }
    }

    /// Apply a line-edit operation; returns `Unhandled` when the operation declines the key.
    pub fn apply(&mut self, op: TextOp) -> EditorAction {
        if !matches!(op, TextOp::Up | TextOp::Down | TextOp::SelectUp | TextOp::SelectDown) {
            self.preferred_x = None;
        }
        let action = match op {
            TextOp::Newline => {
                self.insert_newline();
                EditorAction::Changed
            }
            TextOp::Left => {
                if let Some((start, _)) = self.selection() {
                    (self.row, self.col) = start;
                    self.clear_selection();
                } else {
                    self.clear_selection();
                    self.move_left();
                }
                EditorAction::None
            }
            TextOp::Right => {
                if let Some((_, end)) = self.selection() {
                    (self.row, self.col) = end;
                    self.clear_selection();
                } else {
                    self.clear_selection();
                    self.move_right();
                }
                EditorAction::None
            }
            TextOp::WordLeft | TextOp::WordRight | TextOp::LineStart | TextOp::LineEnd | TextOp::Up | TextOp::Down => {
                self.clear_selection();
                self.move_cursor_op(op)
            }
            TextOp::SelectLeft
            | TextOp::SelectRight
            | TextOp::SelectWordLeft
            | TextOp::SelectWordRight
            | TextOp::SelectLineStart
            | TextOp::SelectLineEnd
            | TextOp::SelectUp
            | TextOp::SelectDown => {
                self.begin_selection();
                self.move_cursor_op(op)
            }
            TextOp::SelectAll => {
                self.selection_anchor = Some((0, 0));
                self.row = self.lines.len() - 1;
                self.col = self.lines[self.row].len();
                EditorAction::None
            }
            TextOp::DeleteWordBack => self.delete_word_back(),
            TextOp::DeleteBack => {
                if self.delete_active_selection() {
                    EditorAction::Changed
                } else if self.shell > 0 && self.buffer_empty() {
                    self.snapshot();
                    self.shell -= 1;
                    self.bump_text_rev();
                    EditorAction::Changed
                } else if self.col == 0 && self.row == 0 {
                    EditorAction::None
                } else {
                    self.begin(LastOp::Delete);
                    if self.backspace() { EditorAction::Changed } else { EditorAction::None }
                }
            }
            TextOp::DeleteWordForward => self.delete_word_forward(),
            TextOp::DeleteForward => {
                if self.delete_active_selection() {
                    EditorAction::Changed
                } else if self.is_empty() {
                    EditorAction::Unhandled
                } else {
                    let last = self.lines.len() - 1;
                    if self.row == last && self.col == self.lines[last].len() {
                        EditorAction::None
                    } else {
                        self.begin(LastOp::Delete);
                        if self.delete_forward() { EditorAction::Changed } else { EditorAction::None }
                    }
                }
            }
            TextOp::KillLineStart => {
                if self.col == 0 {
                    self.clear_selection();
                    EditorAction::None
                } else {
                    self.kill_range(0..self.col, true);
                    EditorAction::Changed
                }
            }
            TextOp::KillLineEnd => {
                let len = self.lines[self.row].len();
                if self.col < len {
                    self.kill_range(self.col..len, false);
                    EditorAction::Changed
                } else if self.row + 1 < self.lines.len() {
                    let accumulate = self.last_op == LastOp::Kill;
                    self.begin(LastOp::Other);
                    self.clear_selection();
                    self.delete_forward();
                    self.push_kill("\n".into(), false, accumulate);
                    self.last_op = LastOp::Kill;
                    EditorAction::Changed
                } else {
                    self.clear_selection();
                    EditorAction::None
                }
            }
            TextOp::Yank => self.yank(),
            TextOp::YankPop => self.yank_pop(),
            TextOp::Undo => {
                if self.undo() {
                    EditorAction::Changed
                } else {
                    EditorAction::None
                }
            }
            TextOp::Redo => {
                if self.redo() {
                    EditorAction::Changed
                } else {
                    EditorAction::None
                }
            }
        };
        if !matches!(action, EditorAction::Changed) && action != EditorAction::Unhandled {
            // movement breaks undo coalescing
            self.last_op = LastOp::Other;
        }
        action
    }

    /// Enter key: backslash line-continuation inserts a newline; otherwise submits the buffer.
    pub fn enter(&mut self) -> EditorAction {
        self.preferred_x = None;
        let action = if self.col > 0 && self.lines[self.row][..self.col].ends_with('\\') && self.selection().is_none() {
            self.begin(LastOp::Other);
            self.delete_range(self.row, self.col - 1..self.col);
            self.insert_str("\n");
            EditorAction::Changed
        } else {
            let (text, elements) = self.text_with_elements();
            self.last_submitted_elements = elements;
            self.snapshot();
            self.clear();
            EditorAction::Submit(text)
        };
        if !matches!(action, EditorAction::Changed) {
            self.last_op = LastOp::Other;
        }
        action
    }

    /// Printable character insertion (including `!` shell-mode entry on an empty buffer).
    pub fn insert_char(&mut self, c: char) -> EditorAction {
        self.preferred_x = None;
        let replaced = self.delete_active_selection();
        if c == '!' && self.shell_enabled && self.shell < 2 && self.buffer_empty() {
            if !replaced {
                self.snapshot();
            }
            self.last_op = LastOp::Other;
            self.shell += 1;
            self.bump_text_rev();
            return EditorAction::Changed;
        }
        // Word-grouped undo units: whitespace starts a new unit, so one undo removes the
        // last word together with the space before it
        if !replaced && (c.is_whitespace() || self.last_op != LastOp::Insert) {
            self.snapshot();
        }
        self.last_op = LastOp::Insert;
        let mut buf = [0u8; 4];
        self.insert_str(c.encode_utf8(&mut buf));
        EditorAction::Changed
    }

    /// Direct key handling for core-owned screens (missing-package install screen) and kernel tests.
    pub fn handle_key(&mut self, ev: KeyEvent) -> EditorAction {
        if ev.kind == KeyEventKind::Release {
            return EditorAction::None;
        }
        let ks = crate::keys::key_to_string(&ev);
        if let Some(&(_, op)) = DEFAULT_TEXT_KEYS.iter().find(|&&(k, _)| k == ks) {
            return self.apply(op);
        }
        if ev.code == KeyCode::Enter && !ev.modifiers.contains(Modifiers::CONTROL) {
            return self.enter();
        }
        if let Some(c) = crate::keys::printable_char(&ev) {
            return self.insert_char(c);
        }
        EditorAction::Unhandled
    }
}
