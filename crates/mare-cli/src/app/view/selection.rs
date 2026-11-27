//! Mouse selection mode and copying the selection.

use crate::app::*;

/// A live-area text selection in progress: (live row, column) of the press and of the pointer.
#[derive(Clone, Copy, Debug, Default)]
pub(in crate::app) struct Selection {
    anchor: Option<(usize, usize)>,
    head: Option<(usize, usize)>,
}

impl Selection {
    /// The selected range, start before end (row-major), when something is selected.
    pub(super) fn range(&self) -> Option<((usize, usize), (usize, usize))> {
        let (a, b) = (self.anchor?, self.head?);
        Some(if a <= b { (a, b) } else { (b, a) })
    }
}

/// Columns `[c0, c1)` of `row` within a selection from `start` to `end` (inclusive end column).
fn sel_cols(row: usize, start: (usize, usize), end: (usize, usize), width: usize) -> (usize, usize) {
    let c0 = if row == start.0 { start.1 } else { 0 };
    let c1 = if row == end.0 { end.1 + 1 } else { width };
    (c0, c1.max(c0))
}

/// The selected text of `live` (rows joined by newlines, trailing blanks trimmed).
fn selection_text(live: &[Line], start: (usize, usize), end: (usize, usize), width: usize) -> String {
    let mut out = vec![];
    for (row, line) in live.iter().enumerate().take(end.0 + 1).skip(start.0) {
        let (c0, c1) = sel_cols(row, start, end, width);
        out.push(slice_cells(line, c0, c1).plain().trim_end().to_string());
    }
    out.join("\n")
}

/// `live` with the selection drawn in reverse video.
pub(super) fn highlight_selection(live: &mut [Line], start: (usize, usize), end: (usize, usize), width: usize) {
    for (row, slot) in live.iter_mut().enumerate().take(end.0 + 1).skip(start.0) {
        let (c0, c1) = sel_cols(row, start, end, width);
        let line = &*slot;
        let mut out = slice_cells(line, 0, c0);
        let mut mid = slice_cells(line, c0, c1.min(width));
        for sp in &mut mid.spans {
            sp.style = sp.style.reverse();
        }
        out.spans.extend(mid.spans);
        out.spans.extend(slice_cells(line, c1, line.width().max(c1)).spans);
        out.fill_bg = line.fill_bg;
        *slot = out;
    }
}

/// Select mode: mouse reporting on, a drag in the live area selects text and
/// releasing copies it through the clipboard primitive (OSC 52 included). Written only here
/// (render records what it drew through [`Self::record`]).
pub(in crate::app) struct SelectMode {
    /// the selection while select mode is on
    pub(in crate::app) sel: Option<Selection>,
    /// the last drawn live area and its screen top (1-based) and width, kept only in select mode
    live: Vec<Line>,
    top: usize,
    width: usize,
    /// frozen `(live, top, width)` captured on left-button press so streaming, notices, or resize
    /// during an active drag cannot drift the highlighted or copied lines
    pub(in crate::app) drag_snapshot: Option<(Vec<Line>, usize, usize)>,
}

impl SelectMode {
    pub(in crate::app) fn new() -> Self {
        Self { sel: None, live: vec![], top: 1, width: 0, drag_snapshot: None }
    }

    pub(in crate::app) fn active(&self) -> bool {
        self.sel.is_some()
    }

    pub(in crate::app) fn enter(&mut self) {
        self.sel = Some(Selection::default());
        self.drag_snapshot = None;
    }

    /// Off: the selection and the kept live area go (the screen top stays).
    fn leave(&mut self) {
        self.sel = None;
        self.live.clear();
        self.drag_snapshot = None;
    }

    /// What the frame drew (before the selection's highlight) and the live area's screen top.
    pub(in crate::app) fn record(&mut self, live: Vec<Line>, top: usize, width: usize) {
        self.live = live;
        self.top = top;
        self.width = width;
    }

    /// Map a 0-based screen coordinate into a live-area cell, clamping when the pointer moves
    /// above the live area's top row or below its bottom row.
    fn clamp_pos(row: u16, col: u16, live_len: usize, top: usize, width: usize) -> (usize, usize) {
        let top0 = top.max(1) - 1;
        let r = row as usize;
        if r < top0 {
            (0, 0)
        } else {
            let rel = r - top0;
            if rel >= live_len { (live_len.saturating_sub(1), width.saturating_sub(1)) } else { (rel, col as usize) }
        }
    }

    /// A mouse event: press anchors, drag extends; true on a release that ends a selection
    /// (it is to be copied).
    fn on_mouse(&mut self, m: tty::MouseEvent) -> bool {
        use tty::{MouseButton, MouseEventKind};
        let Some(sel) = self.sel.as_mut() else { return false };
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.live.is_empty() {
                    return false;
                }
                let top0 = self.top.max(1) - 1;
                let Some(row) = (m.row as usize).checked_sub(top0) else { return false };
                let row = row.min(self.live.len().saturating_sub(1));
                let pos = (row, m.column as usize);
                sel.anchor = Some(pos);
                sel.head = Some(pos);
                self.drag_snapshot = Some((self.live.clone(), self.top, self.width));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if sel.anchor.is_none() {
                    return false;
                }
                let (live_len, top, width) = self
                    .drag_snapshot
                    .as_ref()
                    .map_or((self.live.len(), self.top, self.width), |(l, t, w)| (l.len(), *t, *w));
                if live_len == 0 {
                    return false;
                }
                sel.head = Some(Self::clamp_pos(m.row, m.column, live_len, top, width));
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if sel.anchor.is_none() {
                    return false;
                }
                let (live_len, top, width) = self
                    .drag_snapshot
                    .as_ref()
                    .map_or((self.live.len(), self.top, self.width), |(l, t, w)| (l.len(), *t, *w));
                if live_len > 0 {
                    sel.head = Some(Self::clamp_pos(m.row, m.column, live_len, top, width));
                }
                return true;
            }
            _ => {}
        }
        false
    }

    /// The selected text, when something is selected.
    fn text(&self, width: usize) -> Option<String> {
        let (a, b) = self.sel.and_then(|s| s.range())?;
        let (live, w) = match self.drag_snapshot.as_ref() {
            Some((l, _, sw)) => (l.as_slice(), if *sw > 0 { *sw } else { width }),
            None => (self.live.as_slice(), if self.width > 0 { self.width } else { width }),
        };
        Some(selection_text(live, a, b, w))
    }
}

impl App {
    pub(in crate::app) fn leave_select_mode(&mut self) {
        self.select.leave();
        mare_tui::modes::set_mouse(false);
    }

    /// A mouse event in select mode: press anchors, drag extends, release copies the selection
    /// through the clipboard primitive and leaves the mode.
    pub(in crate::app) fn handle_mouse(&mut self, m: tty::MouseEvent) {
        if self.transcript.is_open() || self.modal.is_some() {
            if self.select.active() {
                self.leave_select_mode();
            }
            return;
        }
        if self.select.on_mouse(m) {
            self.copy_selection();
        }
    }

    /// Copy the live-area selection, if there is one, and leave select mode (mouse release, or
    /// Ctrl+C while selecting —). `false` when nothing was selected.
    pub(in crate::app) fn copy_selection(&mut self) -> bool {
        let Some(text) = self.select.text(self.width) else { return false };
        self.leave_select_mode();
        if text.trim().is_empty() {
            return true;
        }
        let chars = text.chars().count();
        let bytes = text.len();
        let ui = self.ui.clone();
        let app_tx = self.app_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = mare_binding::clipboard::copy(&text, &ui);
            let _ = app_tx.send(AppMsg::CopyResult { chars, bytes, result });
        });
        true
    }

    pub(in crate::app) fn handle_copy_result(
        &mut self,
        chars: usize,
        bytes: usize,
        result: std::result::Result<mare_binding::clipboard::Method, String>,
    ) {
        let (msg, level) = match result {
            // OSC 52 can't confirm the terminal accepted it
            Ok(mare_binding::clipboard::Method::Osc52) => {
                ("Selection sent to the terminal clipboard (if the terminal allows it)".to_string(), "info")
            }
            Ok(m) => (format!("Copied the selection ({chars} chars, {})", m.name()), "success"),
            Err(e) => (format!("Copy failed: {e}"), "error"),
        };
        self.push_notice((msg, level.into(), mare_tui::render::clock_now()));
        self.emit("copy/completed", serde_json::json!({ "bytes": bytes, "target": "selection" }));
    }
}
