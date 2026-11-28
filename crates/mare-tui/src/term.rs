//! Main-screen renderer. Finished content goes into the terminal's native
//! scrollback exactly once; the live area at the bottom is diff-redrawn.
//!
//! - **Scrollback insertion**: once the live area's absolute
//!   position is known, committed lines are written into a scroll region (DECSTBM) above the live
//!   area, so the top rows scroll into history and the live area is not repainted. Without a
//!   known position (before the cursor-position reply, after a resize), inside Zellij (which
//!   doesn't reflow scroll regions) or when the live area fills the screen,
//!   the commit is written at the live area's top and the live area is repainted below it.
//! - **Active-area diff** (OpenTUI `renderer.zig`): rows are compared whole, then cell by cell;
//!   unchanged rows are skipped, a changed row rewrites only its changed run (adjacent same-style
//!   cells merged into one SGR run); no change at all writes zero bytes — not even the
//!   synchronized-output brackets.
//! - Every erase (`ESC [ J`, `ESC [ K`) follows an explicit SGR reset, so a leftover background
//!   can't paint the cleared cells.
//! - Frames are built as bytes by [`Renderer::frame`] (pure: tests read them) and sent to the
//!   single write exit ([`crate::out`]).

use std::fmt::Write as _;

use unicode_segmentation::UnicodeSegmentation;

use super::style::{ColorMode, Line, Span, Style, grapheme_width, line_to_ansi};

/// What the renderer may use from the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderCaps {
    /// synchronized output (CSI ? 2026)
    pub sync: bool,
    /// DECSTBM scroll regions can be used for scrollback insertion (false inside Zellij)
    pub scroll_region: bool,
}

impl Default for RenderCaps {
    fn default() -> Self {
        Self { sync: true, scroll_region: std::env::var_os("ZELLIJ").is_none() }
    }
}

/// One grapheme's bytes, stored inline (no allocation) unless it's unusually long (a long ZWJ
/// emoji sequence): a frame builds one per cell, so a `String` each was the renderer's main
/// allocation cost.
#[derive(Clone)]
enum Grapheme {
    Inline { len: u8, buf: [u8; 22] },
    Heap(Box<str>),
}

impl Grapheme {
    fn new(s: &str) -> Self {
        let mut g = Self::Inline { len: 0, buf: [0; 22] };
        g.push_str(s);
        g
    }

    fn as_str(&self) -> &str {
        match self {
            // only whole UTF-8 strings are copied in
            Self::Inline { len, buf } => std::str::from_utf8(&buf[..*len as usize]).unwrap_or(""),
            Self::Heap(s) => s,
        }
    }

    fn push_str(&mut self, s: &str) {
        match self {
            Self::Inline { len, buf } if *len as usize + s.len() <= buf.len() => {
                buf[*len as usize..*len as usize + s.len()].copy_from_slice(s.as_bytes());
                *len += s.len() as u8;
            }
            _ => {
                let mut owned = self.as_str().to_string();
                owned.push_str(s);
                *self = Self::Heap(owned.into_boxed_str());
            }
        }
    }
}

impl PartialEq for Grapheme {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}
impl Eq for Grapheme {}

impl std::fmt::Debug for Grapheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Cell {
    text: Grapheme,
    width: usize,
    style: Style,
    /// a row's raw suffix ([`Line::raw`]: a core image sequence), carried by a zero-width last
    /// cell so a changed image rewrites it and an unchanged one is left alone
    raw: Option<std::sync::Arc<str>>,
}

/// One row as cells (graphemes with their width and style), fill background expanded.
fn cells_of(line: &Line, width: usize) -> Vec<Cell> {
    let mut out: Vec<Cell> = vec![];
    let mut used = 0;
    'spans: for span in &line.spans {
        for g in span.text.graphemes(true) {
            if g.chars().any(|c| c.is_control()) {
                continue; // dropped by line_to_ansi's final check too
            }
            let w = grapheme_width(g);
            if w == 0 {
                if let Some(last) = out.last_mut() {
                    last.text.push_str(g);
                }
                continue;
            }
            if used + w > width {
                break 'spans;
            }
            used += w;
            out.push(Cell { text: Grapheme::new(g), width: w, style: span.style, raw: None });
        }
    }
    if let Some(bg) = line.fill_bg {
        let mut st = Style::new();
        st.bg = Some(bg);
        while used < width {
            out.push(Cell { text: Grapheme::new(" "), width: 1, style: st, raw: None });
            used += 1;
        }
    }
    if let Some(raw) = &line.raw {
        out.push(Cell { text: Grapheme::new(""), width: 0, style: Style::new(), raw: Some(raw.clone()) });
    }
    out
}

fn cells_width(cells: &[Cell]) -> usize {
    cells.iter().map(|c| c.width).sum()
}

/// Physical terminal rows occupied when auto-wrapping glyphs of the given column widths at
/// `width` columns. When a double-width character does not fit in the remaining 1 column at
/// the end of a row, terminals wrap it onto the next row early, leaving that column blank.
fn wrapped_rows_from_widths(widths: impl IntoIterator<Item = usize>, width: usize) -> usize {
    let w = width.max(1);
    let mut rows = 1usize;
    let mut col = 0usize;
    for gw in widths {
        if gw == 0 {
            continue;
        }
        if col > 0 && col + gw > w {
            rows += 1;
            col = 0;
        }
        col += gw.min(w);
    }
    rows
}

fn wrapped_cell_rows(cells: &[Cell], width: usize) -> usize {
    wrapped_rows_from_widths(cells.iter().map(|c| c.width), width)
}

fn wrapped_line_rows(line: &Line, width: usize) -> usize {
    wrapped_rows_from_widths(
        line.spans
            .iter()
            .flat_map(|span| span.text.graphemes(true))
            .filter(|g| !g.chars().any(|c| c.is_control()))
            .map(grapheme_width),
        width,
    )
}

/// SGR + text for a run of cells; adjacent same-style cells become one span (one SGR run).
fn cells_ansi(cells: &[Cell], mode: ColorMode) -> String {
    let mut line = Line::empty();
    let mut raws = String::new();
    for c in cells {
        if let Some(r) = &c.raw {
            raws.push_str(r);
        } else {
            match line.spans.last_mut() {
                Some(last) if last.style == c.style => last.text.push_str(c.text.as_str()),
                _ => line.push(Span::styled(c.text.as_str().to_string(), c.style)),
            }
        }
    }
    let w = cells_width(cells);
    let mut s = line_to_ansi(&line, w.max(1), mode);
    s.push_str("\x1b[0m");
    s.push_str(&raws);
    s
}

/// Before `rows` committed rows are inserted above a live area at screen rows `top..top+len`
/// (1-based) of a `height`-row screen: when the screen below it is still empty, move the live
/// area down into that room, at most `rows` (insert lines at its top: only empty rows fall off
/// the bottom). Returns how far it moved. Without this, a commit made while the screen isn't
/// full — a prompt that reaches the transcript after a resize replay, a card holding the live
/// area tall — scrolls history into the scrollback and leaves blank rows at the bottom, which no
/// later frame fills (inserts lines to scroll the viewport down to make room; IL is one CSI
/// the write exit's final check already passes, where a bare `ESC M` is dropped).
fn room_below(s: &mut String, top: usize, len: usize, height: usize, rows: usize) -> usize {
    let shift = rows.min(height.saturating_sub(top - 1 + len));
    if shift > 0 {
        let _ = write!(s, "\x1b[{top};1H\x1b[{shift}L");
    }
    shift
}

pub struct Renderer {
    prev: Vec<Vec<Cell>>,
    prev_lines: Vec<Line>,
    prev_cell_width: usize,
    prev_theme_gen: u64,
    prev_color: ColorMode,
    prev_has_raw: bool,
    /// row (within the live area) where the terminal cursor currently sits
    cursor_row: usize,
    /// where the visible cursor is parked (None: hidden)
    cursor: Option<(usize, usize)>,
    cursor_visible: bool,
    /// absolute screen row (1-based) of the live area's top, when known
    top: Option<usize>,
    /// visible committed-history rows currently occupying the viewport above the live area
    /// (retained across `resize_bytes` while `top` is cleared awaiting a fresh CPR anchor)
    visible_history_rows: Option<usize>,
    /// incremented whenever a frame or reset may shift the live area's absolute screen row
    position_epoch: u64,
    pub color: ColorMode,
    pub width: usize,
    pub height: usize,
    pub caps: RenderCaps,
}

impl Renderer {
    pub fn new(width: usize) -> Self {
        Self {
            prev: vec![],
            prev_lines: vec![],
            prev_cell_width: 0,
            prev_theme_gen: 0,
            prev_color: ColorMode::detect(),
            prev_has_raw: false,
            cursor_row: 0,
            cursor: None,
            cursor_visible: true,
            top: None,
            visible_history_rows: None,
            position_epoch: 0,
            color: ColorMode::detect(),
            width,
            height: 24,
            caps: RenderCaps::default(),
        }
    }

    /// The cursor-position reply arrived (row is 1-based): the live area's top is
    /// `row - cursor_row` as of when the query was queued (no frame is drawn in between).
    pub fn anchor(&mut self, cursor_abs_row: usize) {
        self.top = cursor_abs_row.checked_sub(self.cursor_row).filter(|t| *t >= 1);
        if let Some(top) = self.top {
            self.visible_history_rows = Some(top.saturating_sub(1));
        }
    }

    /// Absolute screen row (1-based) of the live area's top, when known.
    pub fn top(&self) -> Option<usize> {
        self.top
    }

    /// Committed history rows currently occupying the visible viewport above the live area,
    /// derived from the renderer's anchored or pre-resize live-area boundary.
    pub fn visible_history_rows(&self) -> usize {
        if let Some(top) = self.top {
            return top.saturating_sub(1);
        }
        if let Some(above) = self.visible_history_rows {
            return above.min(self.height);
        }
        self.height.saturating_sub(self.prev.len())
    }

    /// Row of the live area the cursor is on now (to correlate a cursor-position query).
    pub fn cursor_row(&self) -> usize {
        self.cursor_row
    }

    /// Monotonic counter bumped whenever a frame or reset may shift the live area's absolute row.
    pub fn position_epoch(&self) -> u64 {
        self.position_epoch
    }

    /// Whether committed lines will go through a scroll region (tests, `/profile`).
    pub fn scroll_insert_ready(&self) -> bool {
        self.caps.scroll_region && self.top.is_some()
    }

    /// Width or height changed: the previous live area may have re-wrapped; compute how many
    /// physical rows it occupies now so we can move back to its top and clear it.
    pub fn resize(&mut self, width: usize, height: usize) {
        crate::out::write(self.resize_bytes(width, height));
    }

    pub fn resize_bytes(&mut self, width: usize, height: usize) -> Vec<u8> {
        let changed_w = width != self.width;
        let old_height = self.height;
        let old_above = self
            .top
            .map(|t| t.saturating_sub(1))
            .or(self.visible_history_rows)
            .unwrap_or_else(|| old_height.saturating_sub(self.prev.len()));
        let new_height = height.max(1);
        let adjusted_above = old_above.saturating_sub(old_height.saturating_sub(new_height)).min(new_height);
        self.height = new_height;
        self.visible_history_rows = Some(adjusted_above);
        if !changed_w {
            // the terminal keeps content anchored in its own way: position unknown now
            self.top = None;
            self.position_epoch = self.position_epoch.wrapping_add(1);
            return vec![];
        }
        let old_cursor_rows: usize =
            self.prev[..self.cursor_row.min(self.prev.len())].iter().map(|r| wrapped_cell_rows(r, width)).sum();
        let mut s = String::new();
        if self.caps.sync {
            s.push_str("\x1b[?2026h");
        }
        if old_cursor_rows > 0 {
            let _ = write!(s, "\x1b[{old_cursor_rows}A");
        }
        s.push_str("\r\x1b[0m\x1b[J");
        if self.caps.sync {
            s.push_str("\x1b[?2026l");
        }
        self.prev.clear();
        self.prev_lines.clear();
        self.prev_has_raw = false;
        self.cursor_row = 0;
        self.cursor = None;
        self.top = None;
        self.position_epoch = self.position_epoch.wrapping_add(1);
        self.width = width;
        s.into_bytes()
    }

    /// Resize reflow: clear the screen and the terminal's whole scrollback,
    /// then write `rows` (committed history re-rendered at the current width) from the top; the
    /// live area is redrawn below them by the next frame. The screen is cleared from its top
    /// down (`CSI H CSI J`, which unlike `CSI 2J` doesn't push the old screen into scrollback on
    /// VTE terminals), then the scrollback (`CSI 3J`).
    pub fn replay_bytes(&mut self, rows: &[Line]) -> Vec<u8> {
        self.replay_bytes_with_scrollback(rows, true)
    }

    /// Replay `rows` on the visible viewport without clearing terminal scrollback (no `CSI 3J`).
    /// Callers pass at most a screenful so redrawing the visible tail preserves older terminal
    /// scrollback above the viewport.
    pub fn replay_visible_bytes(&mut self, rows: &[Line]) -> Vec<u8> {
        self.replay_bytes_with_scrollback(rows, false)
    }

    fn replay_bytes_with_scrollback(&mut self, rows: &[Line], clear_scrollback: bool) -> Vec<u8> {
        let w = self.width.max(1);
        let mut s = String::new();
        if self.caps.sync {
            s.push_str("\x1b[?2026h");
        }
        s.push_str(if clear_scrollback { "\x1b[0m\x1b[H\x1b[J\x1b[3J" } else { "\x1b[0m\x1b[H\x1b[J" });
        for r in rows {
            s.push_str(&line_to_ansi(r, w, self.color));
            s.push_str("\x1b[0m\r\n");
        }
        if self.caps.sync {
            s.push_str("\x1b[?2026l");
        }
        self.prev.clear();
        self.prev_lines.clear();
        self.prev_has_raw = false;
        self.cursor_row = 0;
        self.cursor = None;
        // rows that fit on screen leave the cursor at a known row; more scroll, and the position
        // is unknown again (the next commit uses the repaint-below fallback until re-anchored)
        self.top = (rows.len() < self.height).then_some(rows.len() + 1);
        self.visible_history_rows = Some(rows.len().min(self.height));
        self.position_epoch = self.position_epoch.wrapping_add(1);
        s.into_bytes()
    }

    /// Forget the live area (after leaving the alternate screen, terminal handoff, or clearing the screen).
    pub fn invalidate(&mut self) {
        self.prev.clear();
        self.prev_lines.clear();
        self.prev_has_raw = false;
        self.cursor_row = 0;
        self.cursor = None;
        // Handoffs (`modes::leave`) restore `?25h`, so treat cursor visibility as shown until hidden again.
        self.cursor_visible = true;
        self.top = None;
        self.visible_history_rows = None;
        self.position_epoch = self.position_epoch.wrapping_add(1);
    }

    /// Erase the live area entirely (used before exit / when handing the terminal to others).
    pub fn erase(&mut self) {
        let mut s = String::new();
        if self.cursor_row > 0 {
            let _ = write!(s, "\x1b[{}A", self.cursor_row);
        }
        s.push_str("\r\x1b[0m\x1b[J");
        let top = self.top;
        let visible_history_rows = self.visible_history_rows;
        self.invalidate();
        // the cursor is back at the live area's top: its absolute row is unchanged
        self.top = top;
        self.visible_history_rows = visible_history_rows;
        crate::out::write(s.into_bytes());
    }

    /// Clear the screen and reset renderer state via the same replay path as resize (no `CSI 2J`).
    pub fn clear_screen(&mut self) {
        let bytes = self.replay_bytes(&[]);
        crate::out::write(bytes);
    }

    /// Draw one frame (see [`Renderer::frame`]).
    pub fn draw(&mut self, commit: &[Line], lines: &[Line], cursor: Option<(usize, usize)>) {
        let b = self.frame(commit, lines, cursor);
        crate::out::write(b);
    }

    /// Build one frame. `commit` lines go to scrollback above the live area; `lines` is the new
    /// live area; `cursor` = (row, col) to park the real cursor (for IME), or None to hide it.
    /// Returns the bytes to write — empty when nothing changed.
    pub fn frame(&mut self, commit: &[Line], lines: &[Line], cursor: Option<(usize, usize)>) -> Vec<u8> {
        let w = self.width.max(1);
        let tgen = super::style::theme_gen();
        let cache_valid = self.prev_cell_width == w && self.prev_theme_gen == tgen && self.prev_color == self.color;
        let all_lines_same = cache_valid
            && lines.len() == self.prev_lines.len()
            && lines.iter().zip(self.prev_lines.iter()).all(|(a, b)| a == b);
        if commit.is_empty() && all_lines_same && cursor == self.cursor {
            return vec![];
        }
        // live rows never soft-wrap (that would break the row accounting): an over-wide line —
        // only code blocks are left unwrapped — is cut with `…`
        let mut new_rows: Vec<Option<Vec<Cell>>> = Vec::with_capacity(lines.len());
        let mut any_cell_change = false;
        let mut new_has_raw = false;
        for (i, l) in lines.iter().enumerate() {
            new_has_raw |= l.raw.is_some();
            if cache_valid && i < self.prev.len() && self.prev_lines.get(i) == Some(l) {
                new_rows.push(None);
            } else {
                let cells =
                    if l.width() > w { cells_of(&super::style::truncate_line(l, w), w) } else { cells_of(l, w) };
                if i >= self.prev.len() || cells != self.prev[i] {
                    any_cell_change = true;
                }
                new_rows.push(Some(cells));
            }
        }
        if commit.is_empty() && !any_cell_change && lines.len() == self.prev.len() && cursor == self.cursor {
            self.prev_lines = lines.to_vec();
            self.prev_cell_width = w;
            self.prev_theme_gen = tgen;
            self.prev_color = self.color;
            self.prev_has_raw = new_has_raw;
            return vec![];
        }
        let mut s = String::with_capacity(4096);
        if self.caps.sync {
            s.push_str("\x1b[?2026h");
        }
        if self.cursor_visible {
            s.push_str("\x1b[?25l");
            self.cursor_visible = false;
        }
        // a Kitty image that changed or left the live area is deleted by id; one
        // that moved into the scrollback or was redrawn elsewhere replaces its own placement
        if self.prev_has_raw {
            let old_ids: Vec<u32> =
                self.prev.iter().flatten().filter_map(|c| c.raw.as_deref()).flat_map(super::image::kitty_ids).collect();
            if !old_ids.is_empty() {
                let mut keep: Vec<u32> = new_rows
                    .iter()
                    .enumerate()
                    .flat_map(|(i, slot)| slot.as_deref().unwrap_or_else(|| &self.prev[i]).iter())
                    .filter_map(|c| c.raw.as_deref())
                    .flat_map(super::image::kitty_ids)
                    .collect();
                keep.extend(commit.iter().filter_map(|l| l.raw.as_deref()).flat_map(super::image::kitty_ids));
                let mut gone: Vec<u32> = old_ids.into_iter().filter(|id| !keep.contains(id)).collect();
                gone.sort_unstable();
                gone.dedup();
                for id in gone {
                    s.push_str(&super::image::kitty_delete(id));
                }
            }
        }
        let mut at = self.cursor_row; // the live-area row the cursor is on
        let h = self.height.max(1);
        let old_len = self.prev.len();

        // 1. commit into scrollback
        if !commit.is_empty() {
            self.position_epoch = self.position_epoch.wrapping_add(1);
            let max_committed_img_rows = commit
                .iter()
                .filter_map(|l| l.raw.as_deref())
                .map(|raw| {
                    raw.strip_prefix("\r\x1b[")
                        .and_then(|rest| rest.split_once('A'))
                        .and_then(|(n, _)| n.parse::<usize>().ok())
                        .unwrap_or(0)
                        + 1
                })
                .max()
                .unwrap_or(0);
            let rows: usize = commit.iter().map(|l| wrapped_line_rows(l, w)).sum();
            let region_end = self.top.filter(|_| self.caps.scroll_region && old_len > 0).and_then(|t| t.checked_sub(1));
            let fast_path = region_end.and_then(|r| {
                let shift = rows.min(h.saturating_sub(r + old_len));
                let scroll_bottom = r + shift;
                (r >= 1 && scroll_bottom >= 2 && max_committed_img_rows <= scroll_bottom).then_some(r)
            });
            match fast_path {
                Some(r) => {
                    // DECSTBM: rows 1..=r scroll, the live area below them stays as it is. Empty
                    // rows below the live area are used first: it moves down into them, and the
                    // commit fills the rows it left instead of pushing history into scrollback
                    let shift = room_below(&mut s, r + 1, old_len, h, rows);
                    if shift > 0 {
                        self.top = Some(r + 1 + shift);
                    }
                    let _ = write!(s, "\x1b[1;{}r\x1b[{r};1H", r + shift);
                    for l in commit {
                        s.push_str("\r\n");
                        // inside the scroll region an over-wide line (an unwrapped code line,
                        // ) is written in full and the terminal wraps it there
                        s.push_str(&line_to_ansi(l, w.max(l.width()), self.color));
                        s.push_str("\x1b[0m");
                    }
                    // Back to the cursor's live row by absolute position, never DECSC/DECRC:
                    // xterm.js restores a saved cursor too high once lines have scrolled into
                    // its scrollback. Every later write sets its own column.
                    let _ = write!(s, "\x1b[r\x1b[{};1H", r + 1 + shift + at);
                }
                _ => {
                    // at the live area's top: commit, then repaint the live area below it
                    if at > 0 {
                        let _ = write!(s, "\x1b[{at}A");
                    }
                    s.push_str("\r\x1b[0m\x1b[J");
                    // over-wide lines (unwrapped code, a URL that is never broken) are written
                    // whole: the terminal wraps them, the scrollback keeps every character
                    let mut rows = 0;
                    for l in commit {
                        s.push_str(&line_to_ansi(l, w.max(l.width()), self.color));
                        s.push_str("\x1b[0m\r\n");
                        rows += wrapped_line_rows(l, w);
                    }
                    at = 0;
                    if let Some(t) = self.top {
                        // the live area now starts `rows` rows lower (minus any scroll)
                        let bottom = t + rows; // row the cursor is on now
                        self.top = if bottom > h { Some(h) } else { Some(bottom) };
                    } else if let Some(above) = self.visible_history_rows {
                        self.visible_history_rows = Some((above + rows).min(h.saturating_sub(new_rows.len())));
                    }
                    for (i, slot) in new_rows.iter_mut().enumerate() {
                        if slot.is_none() && i < self.prev.len() {
                            *slot = Some(std::mem::take(&mut self.prev[i]));
                        }
                    }
                    self.prev.clear();
                }
            }
        }

        // 2. diff the live area
        let move_to = |s: &mut String, at: &mut usize, row: usize| {
            if row < *at {
                let _ = write!(s, "\x1b[{}A", *at - row);
            } else if row > *at {
                let _ = write!(s, "\x1b[{}B", row - *at);
            }
            *at = row;
        };
        let have = self.prev.len();
        if new_rows.len() > have {
            // Growing the live area writes newlines that may scroll the terminal and shift `top`.
            self.position_epoch = self.position_epoch.wrapping_add(1);
            if self.top.is_none()
                && let Some(above) = self.visible_history_rows
                && above + new_rows.len() > h
            {
                self.visible_history_rows = Some(h.saturating_sub(new_rows.len()));
            }
        }
        for (i, slot) in new_rows.iter().enumerate() {
            let Some(row) = slot.as_deref() else {
                continue; // identical to self.prev[i]
            };
            if i < have {
                let old = &self.prev[i];
                if old == row {
                    continue;
                }
                // first differing cell; with equal widths also trim the common suffix
                let fc = old.iter().zip(row.iter()).take_while(|(a, b)| a == b).count();
                let (ow, nw) = (cells_width(old), cells_width(row));
                let mut end = row.len();
                if ow == nw {
                    let common = old.iter().rev().zip(row.iter().rev()).take_while(|(a, b)| a == b).count();
                    end = (row.len() - common).max(fc);
                }
                move_to(&mut s, &mut at, i);
                let col = cells_width(&row[..fc]);
                let _ = write!(s, "\r\x1b[{}G", col + 1);
                if fc < end {
                    s.push_str(&cells_ansi(&row[fc..end], self.color));
                }
                if nw < ow {
                    s.push_str("\x1b[0m\x1b[K");
                }
            } else {
                // a new row below the old live area: newline from the last row (may scroll)
                if have == 0 && i == 0 {
                    move_to(&mut s, &mut at, 0);
                    s.push('\r');
                } else {
                    move_to(&mut s, &mut at, i - 1);
                    s.push_str("\r\n");
                    at = i;
                    if let Some(t) = self.top
                        && t + i > h
                    {
                        // the screen scrolled one row: the live area's top moved up
                        self.top = (t > 1).then(|| t - 1);
                    }
                }
                s.push_str(&cells_ansi(row, self.color));
            }
        }
        if new_rows.len() < have {
            // shrink: clear the rows that are gone
            if new_rows.is_empty() {
                move_to(&mut s, &mut at, 0);
                s.push_str("\r\x1b[0m\x1b[J");
            } else {
                move_to(&mut s, &mut at, new_rows.len() - 1);
                s.push_str("\r\n\x1b[0m\x1b[J");
                at = new_rows.len();
                // the cursor sits one row below the area now; step back up for accounting
                s.push_str("\x1b[1A");
                at = at.saturating_sub(1);
            }
        }
        // 3. park the cursor
        if let Some((crow, ccol)) = cursor {
            let crow = crow.min(new_rows.len().saturating_sub(1));
            move_to(&mut s, &mut at, crow);
            let _ = write!(s, "\r\x1b[{}G\x1b[?25h", ccol + 1);
            self.cursor_visible = true;
        }
        if self.caps.sync {
            s.push_str("\x1b[?2026l");
        }
        self.cursor_row = at;
        self.cursor = cursor;
        self.prev.truncate(new_rows.len());
        self.prev_lines.truncate(lines.len());
        for (i, slot) in new_rows.into_iter().enumerate() {
            if let Some(cells) = slot {
                if i < self.prev.len() {
                    self.prev[i] = cells;
                } else {
                    self.prev.push(cells);
                }
                if i < self.prev_lines.len() {
                    self.prev_lines[i] = lines[i].clone();
                } else {
                    self.prev_lines.push(lines[i].clone());
                }
            }
        }
        self.prev_cell_width = w;
        self.prev_theme_gen = tgen;
        self.prev_color = self.color;
        self.prev_has_raw = new_has_raw;
        if let Some(top) = self.top {
            self.visible_history_rows = Some(top.saturating_sub(1));
        }
        s.into_bytes()
    }
}

// Notifications moved to the single write exit; kept here for existing callers.
pub use crate::out::notification_sequence;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_replay_clears_the_screen_then_the_scrollback() {
        let mut r = Renderer::new(20);
        r.height = 10;
        r.caps = RenderCaps { sync: true, scroll_region: true };
        let rows = vec![Line::raw("one"), Line::raw("two")];
        let out = String::from_utf8(r.replay_bytes(&rows)).unwrap();
        // CSI H CSI J then CSI 3J; never CSI 2J (VTE pushes the old screen into scrollback)
        assert!(out.contains("\x1b[H\x1b[J\x1b[3J") && !out.contains("\x1b[2J"), "{out:?}");
        assert!(out.find("\x1b[3J").unwrap() < out.find("one").unwrap());
        assert!(out.find("one").unwrap() < out.find("two").unwrap());
        assert!(r.scroll_insert_ready(), "rows fit: the live area's position is known again");
        let many: Vec<Line> = (0..12).map(|i| Line::raw(format!("r{i}"))).collect();
        r.replay_bytes(&many);
        assert!(!r.scroll_insert_ready(), "rows scrolled: position unknown until re-anchored");
    }

    fn r(width: usize, height: usize) -> Renderer {
        let mut r = Renderer::new(width);
        r.height = height;
        r.color = ColorMode::TrueColor;
        r.caps = RenderCaps { sync: true, scroll_region: true };
        r
    }
    fn lines(v: &[&str]) -> Vec<Line> {
        v.iter().map(|s| Line::raw(*s)).collect()
    }
    fn s(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap()
    }

    #[test]
    fn a_changed_row_rewrites_only_its_changed_run() {
        let mut t = r(20, 10);
        t.frame(&[], &lines(&["status: idle  12s", "> hello"]), None);
        let out = s(t.frame(&[], &lines(&["status: busy  12s", "> hello"]), None));
        // row 1 untouched, row 0 rewritten from column 9 ("idle" → "busy") only
        assert!(out.contains("\x1b[9G"), "{out:?}");
        assert!(out.contains("busy") && !out.contains("status") && !out.contains("hello"), "{out:?}");
    }

    #[test]
    fn commits_go_through_a_scroll_region_once_the_position_is_known() {
        let mut t = r(20, 10);
        t.frame(&[], &lines(&["live 1", "live 2"]), None);
        assert!(!t.scroll_insert_ready());
        t.anchor(9 + t.cursor_row()); // live area at rows 9..=10
        let out = s(t.frame(&lines(&["done A", "done B"]), &lines(&["live 1", "live 2"]), None));
        assert!(out.contains("\x1b[?2026h\x1b[1;8r\x1b[8;1H\r\n"), "{out:?}");
        assert!(out.contains("done A") && out.contains("done B") && out.contains("\x1b[r\x1b[10;1H"), "{out:?}");
        assert!(!out.contains("live"), "the live area is not repainted: {out:?}");
    }

    #[test]
    fn a_scroll_region_commit_returns_to_the_live_area_by_absolute_position() {
        // xterm.js (VS Code's terminal) restores a DECSC cursor rows too high once lines have
        // scrolled into its scrollback, so later live-area writes would land on history rows
        let mut t = r(20, 10);
        t.frame(&[], &lines(&["live 1", "live 2"]), Some((1, 0)));
        t.anchor(9 + t.cursor_row()); // live area at rows 9..=10, cursor on its second row
        let out = s(t.frame(&lines(&["done A", "done B"]), &lines(&["live 1", "live 2"]), Some((1, 0))));
        assert!(!out.contains("\x1b7") && !out.contains("\x1b8"), "no saved cursor across the scroll: {out:?}");
        assert!(out.contains("\x1b[r\x1b[10;1H"), "back on the cursor's live row (10): {out:?}");
        // with room below, the live area moves down first and the cursor follows it there
        let mut t = r(20, 10);
        t.frame(&[], &lines(&["live 1", "live 2"]), Some((1, 0)));
        t.anchor(4 + t.cursor_row()); // live area at rows 4..=5
        let out = s(t.frame(&lines(&["done A", "done B"]), &lines(&["live 1", "live 2"]), Some((1, 0))));
        assert!(out.contains("\x1b[r\x1b[7;1H") && !out.contains("\x1b8"), "live area now at 6..=7: {out:?}");
    }

    #[test]
    fn a_commit_uses_the_empty_rows_below_the_live_area_before_scrolling_history() {
        let mut t = r(20, 10);
        t.frame(&[], &lines(&["live 1", "live 2"]), Some((1, 0)));
        t.anchor(4 + t.cursor_row()); // live area at rows 4..=5, rows 6..=10 empty
        let out = s(t.frame(&lines(&["done A", "done B"]), &lines(&["live 1", "live 2"]), Some((1, 0))));
        // the live area moves down two rows (two lines inserted at its top), then the commit
        // fills rows 4..=5 from row 3 on without scrolling rows 1..=3
        assert!(out.contains("\x1b[4;1H\x1b[2L\x1b[1;5r\x1b[3;1H\r\n"), "{out:?}");
        assert!(out.contains("\x1b[r\x1b[7;1H"), "the cursor follows the live area: {out:?}");
        assert_eq!(t.top(), Some(6));
        // the write exit's final check keeps every byte of it
        let bytes = out.as_bytes();
        assert_eq!(crate::out::guard(bytes, crate::out::Origin::Frame), (bytes.to_vec(), 0));
        // more rows than the room: it moves to the bottom, the rest scrolls history
        let out = s(t.frame(&lines(&["c", "d", "e", "f", "g"]), &lines(&["live 1", "live 2"]), Some((1, 0))));
        assert!(out.contains("\x1b[6;1H\x1b[3L\x1b[1;8r\x1b[5;1H"), "{out:?}");
        assert_eq!(t.top(), Some(9));
        // a full screen: no room, the plain scroll-region insert
        let out = s(t.frame(&lines(&["h"]), &lines(&["live 1", "live 2"]), Some((1, 0))));
        assert!(out.contains("\x1b[1;8r\x1b[8;1H\r\n") && !out.contains('L'), "{out:?}");
        assert_eq!(t.top(), Some(9));
    }

    #[test]
    fn without_a_position_or_in_zellij_commits_repaint_below() {
        let mut t = r(20, 10);
        t.frame(&[], &lines(&["live"]), None);
        let out = s(t.frame(&lines(&["done"]), &lines(&["live"]), None));
        assert!(!out.contains("\x1b[1;") && out.contains("done") && out.contains("live"), "{out:?}");
        let mut z = r(20, 10);
        z.caps.scroll_region = false;
        z.frame(&[], &lines(&["live"]), None);
        z.anchor(10);
        let out = s(z.frame(&lines(&["done"]), &lines(&["live"]), None));
        assert!(!out.contains('r') || !out.contains("\x1b[1;"), "{out:?}");
    }

    #[test]
    fn without_sync_the_cursor_is_hidden_while_drawing() {
        let mut t = r(20, 10);
        t.caps.sync = false;
        let out = s(t.frame(&[], &lines(&["a"]), Some((0, 0))));
        assert!(out.starts_with("\x1b[?25l") && out.ends_with("\x1b[?25h") && !out.contains("2026"), "{out:?}");
    }

    #[test]
    fn every_erase_follows_an_sgr_reset() {
        let mut t = r(20, 10);
        let bg = |txt: &str| {
            let mut st = Style::new();
            st.bg = Some(crate::style::Color::Rgb(40, 40, 40));
            Line::styled(txt, st).with_fill(crate::style::Color::Rgb(40, 40, 40))
        };
        let mut out = String::new();
        out += &s(t.frame(&[], &[bg("a long highlighted row"), bg("x")], None));
        out += &s(t.frame(&lines(&["done"]), &lines(&["short"]), None));
        out += &s(t.frame(&[], &[], None));
        for (i, _) in out.match_indices("\x1b[J").chain(out.match_indices("\x1b[K")) {
            assert!(out[..i].ends_with("\x1b[0m"), "erase without reset at {i}: {out:?}");
        }
        t.resize_bytes(10, 10);
    }

    #[test]
    fn shrinking_styled_rows_erase_to_default_or_paint_the_fill_background() {
        let old_bg = crate::style::Color::Rgb(10, 20, 30);
        let new_bg = crate::style::Color::Rgb(40, 50, 60);
        let mut t = r(8, 10);
        t.frame(&[], &[Line::styled("long", Style::new().bg(old_bg))], None);
        let cleared = s(t.frame(&[], &[Line::styled("x", Style::new().bg(new_bg))], None));
        assert!(cleared.contains("\x1b[48;2;40;50;60mx\x1b[0m\x1b[0m\x1b[0m\x1b[K"), "{cleared:?}");

        t.frame(&[], &[Line::styled("long", Style::new().bg(old_bg)).with_fill(old_bg)], None);
        let filled = s(t.frame(&[], &[Line::styled("x", Style::new().bg(new_bg)).with_fill(new_bg)], None));
        assert!(filled.contains("\x1b[48;2;40;50;60mx       \x1b[0m"), "{filled:?}");
        assert!(!filled.contains("\x1b[K"), "filled cells must keep the requested background: {filled:?}");
    }

    #[test]
    fn clear_screen_resets_background_before_erasing() {
        let _guard = crate::out::test_lock();
        crate::out::capture(true);
        crate::out::write(b"\x1b[48;2;10;20;30mstyled".to_vec());
        r(8, 10).clear_screen();
        let out = s(crate::out::capture(false));
        assert!(!out.contains("\x1b[2J"), "clear_screen must use replay path, not CSI 2J: {out:?}");
        assert_eq!(out, "\x1b[48;2;10;20;30mstyled\x1b[?2026h\x1b[0m\x1b[H\x1b[J\x1b[3J\x1b[?2026l");
    }

    #[test]
    fn rows_grow_and_shrink() {
        let mut t = r(20, 10);
        t.frame(&[], &lines(&["a"]), None);
        let grow = s(t.frame(&[], &lines(&["a", "b", "c"]), None));
        assert_eq!(grow.matches("\r\n").count(), 2, "{grow:?}");
        let shrink = s(t.frame(&[], &lines(&["a"]), None));
        assert!(shrink.contains("\x1b[J"), "{shrink:?}");
    }

    #[test]
    fn a_kitty_image_that_changes_or_leaves_the_live_area_is_deleted_by_id() {
        let img = |id: u32| {
            let mut l = Line::empty();
            l.raw = Some(format!("\r\x1b_Ga=T,f=100,q=2,C=1,c=1,r=1,i={id},p=1;AAAA\x1b\\").into());
            l
        };
        let del = |id: u32| format!("\x1b_Ga=d,d=i,i={id},p=1,q=2\x1b\\");
        let mut t = r(20, 10);
        t.frame(&[], &[Line::raw("a"), img(7)], None);
        // redrawn one row lower: the same id replaces its placement, nothing is deleted
        let moved = s(t.frame(&[], &[Line::raw("a"), Line::raw("b"), img(7)], None));
        assert!(!moved.contains("a=d"), "{moved:?}");
        // committed into the scrollback: still shown there, not deleted
        let committed = s(t.frame(&[img(7)], &[Line::raw("c")], None));
        assert!(!committed.contains("a=d"), "{committed:?}");
        // changed to another picture, then removed: each old one is deleted once
        t.frame(&[], &[img(8)], None);
        let changed = s(t.frame(&[], &[img(9)], None));
        assert_eq!(changed.matches(&del(8)).count(), 1, "{changed:?}");
        assert!(crate::out::image_allowed(del(8).as_bytes()));
        let removed = s(t.frame(&[], &[Line::raw("done")], None));
        assert!(removed.contains(&del(9)) && !removed.contains(&del(8)), "{removed:?}");
        // Identical cached pixels can have independent occurrence ids. Removing one live
        // placement must not erase the other occurrence.
        let mut pair = r(20, 10);
        pair.frame(&[], &[img(31), img(32)], None);
        let one = s(pair.frame(&[], &[img(32)], None));
        assert_eq!(one.matches(&del(31)).count(), 1, "{one:?}");
        assert!(!one.contains(&del(32)), "{one:?}");
    }

    #[test]
    fn sparse_row_updates_cursor_moves_and_width_rule_changes_reuse_cells_correctly() {
        let _g = crate::image::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::style::set_ambiwidth_double(false);
        let mut t = r(20, 10);
        let initial = lines(&["row zero", "row one", "row two"]);
        assert!(!t.frame(&[], &initial, Some((0, 1))).is_empty());
        // Unchanged frame writes 0 bytes.
        assert!(t.frame(&[], &initial, Some((0, 1))).is_empty());
        // Cursor-only move writes cursor sequence without rewriting row text.
        let cur_only = s(t.frame(&[], &initial, Some((2, 3))));
        assert!(!cur_only.contains("row "), "{cur_only:?}");
        assert!(cur_only.contains("\x1b[4G"), "{cur_only:?}");
        // Single-row sparse change rewrites only the changed row.
        let updated = lines(&["row zero", "row ONE", "row two"]);
        let sparse = s(t.frame(&[], &updated, Some((2, 3))));
        assert!(sparse.contains("ONE") && !sparse.contains("zero") && !sparse.contains("two"), "{sparse:?}");
        // Width rule change (ambiwidth) invalidates cached cells for affected rows.
        let ambi = lines(&["• item"]);
        t.frame(&[], &ambi, Some((0, 0)));
        assert_eq!(t.prev[0][0].width, 1);

        crate::style::set_ambiwidth_double(true);
        let widened = s(t.frame(&[], &ambi, Some((0, 0))));
        assert!(!widened.is_empty(), "ambiwidth change must re-render row");
        assert_eq!(t.prev[0][0].width, 2);
        let mut fresh_double = r(20, 10);
        fresh_double.frame(&[], &ambi, Some((0, 0)));
        assert_eq!(t.prev, fresh_double.prev, "cached renderer must match fresh double-width renderer");

        crate::style::set_ambiwidth_double(false);
        let narrowed = s(t.frame(&[], &ambi, Some((0, 0))));
        assert!(!narrowed.is_empty(), "restoring ambiwidth must re-render row");
        assert_eq!(t.prev[0][0].width, 1);
        let mut fresh_single = r(20, 10);
        fresh_single.frame(&[], &ambi, Some((0, 0)));
        assert_eq!(t.prev, fresh_single.prev, "cached renderer must match fresh single-width renderer");
    }

    #[test]
    fn decstbm_fast_path_requires_valid_scroll_bottom_and_tall_enough_region() {
        let mut t = r(20, 10);
        // Live area at rows 3..=4 (`top = Some(3)`, so `r = 2`), plus a 5-row committed image
        // (`\r\x1b[4A...` => 5 rows) exceeds `scroll_bottom` when `shift` cannot fit it.
        t.frame(&[], &lines(&["", "", "live0", "live1", "live2", "live3", "live4", "live5", "live6"]), None);
        // `top` is at row 2 (`r = 1`), `old_len = 7`, `h = 10` -> `shift = min(1, 10 - 8) = 1`, `scroll_bottom = 2`.
        // A 4-row image (`\r\x1b[3A`) is taller than `scroll_bottom = 2`, so DECSTBM fast path must NOT trigger.
        let mut img_line = Line::empty();
        let seq = "\x1b_Ga=T,f=100,q=2;AAAA\x1b\\";
        crate::out::allow_image(seq.as_bytes());
        img_line.raw = Some(format!("\r\x1b[3A{seq}\x1b[3B").into());
        let out = s(t.frame(&[img_line], &lines(&["live"]), None));
        assert!(!out.contains("\x1b[1;2r"), "must fall back from DECSTBM when image > scroll_bottom: {out:?}");
    }

    #[test]
    fn double_width_trailing_wrap_counts_physical_rows_on_resize_and_commit() {
        // "aa中中" has total display width 6, which at terminal width 3 occupies 3 physical rows:
        // row 1: "aa " (1 col left, '中' does not fit -> wraps early)
        // row 2: "中 " (1 col left, second '中' does not fit -> wraps early)
        // row 3: "中"
        // `width.div_ceil(3)` would undercount this as 2 rows.
        let wide = Line::raw("aa中中");

        // 1. resize_bytes must move up 3 physical rows from row 1 to row 0 when narrowing to width 3.
        let mut tr = r(10, 10);
        tr.frame(&[], &[wide.clone(), Line::raw("cursor")], Some((1, 0)));
        let resized = s(tr.resize_bytes(3, 10));
        assert!(resized.contains("\x1b[3A"), "resize must move up 3 wrapped rows, got: {resized:?}");

        // 2. DECSTBM fast-path commit must reserve 3 rows (not 2) for "aa中中" at width 3.
        let mut tf = r(3, 10);
        tf.frame(&[], &lines(&["live"]), Some((0, 0)));
        tf.anchor(3); // live area at row 3, rows 4..=10 empty (plenty of room below)
        let fast = s(tf.frame(std::slice::from_ref(&wide), &lines(&["live"]), Some((0, 0))));
        assert!(fast.contains("\x1b[3L"), "DECSTBM commit must insert 3 rows for wrapped CJK line: {fast:?}");
        assert_eq!(tf.top(), Some(6), "live area top must advance by 3 physical rows");

        // 3. Repaint-below fallback commit must also advance `top` by 3 rows at width 3.
        let mut tb = r(3, 10);
        tb.caps.scroll_region = false;
        tb.frame(&[], &lines(&["live"]), Some((0, 0)));
        tb.anchor(3);
        let _ = tb.frame(&[wide], &lines(&["live"]), Some((0, 0)));
        assert_eq!(tb.top(), Some(6), "fallback commit must advance live area top by 3 physical rows");

        // Edge cases for wrapped row counting.
        assert_eq!(wrapped_line_rows(&Line::empty(), 3), 1);
        assert_eq!(wrapped_line_rows(&Line::raw("a中"), 3), 1);
        assert_eq!(wrapped_line_rows(&Line::raw("a中b"), 3), 2);
        assert_eq!(wrapped_line_rows(&Line::raw("中中"), 4), 1);
        assert_eq!(wrapped_line_rows(&Line::raw("中中"), 1), 2);
        assert_eq!(wrapped_line_rows(&Line::raw("aa\u{0301}中中"), 3), 3);
    }

    #[test]
    fn invalidate_after_handoff_resets_cursor_visible_mirror() {
        let mut tr = r(40, 10);
        // Drawing a frame with no cursor (e.g. Select modal) hides the cursor.
        let first = s(tr.frame(&[], &lines(&["modal row"]), None));
        assert!(first.contains("\x1b[?25l"), "first cursorless frame must hide cursor: {first:?}");
        // Terminal handoff emits `?25h`, then calls `invalidate()` on return.
        tr.invalidate();
        // Next cursorless frame must emit `?25l` again instead of assuming the cursor stayed hidden.
        let resumed = s(tr.frame(&[], &lines(&["modal row"]), None));
        assert!(resumed.contains("\x1b[?25l"), "post-handoff cursorless frame must re-hide cursor: {resumed:?}");
    }
}
