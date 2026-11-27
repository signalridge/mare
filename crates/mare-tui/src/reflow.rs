//! Resize reflow. The in-memory records are the source of truth: after
//! the terminal width settles, mare rebuilds what it committed at the new width.
//!
//! - 75 ms trailing debounce: a drag
//!   rebuilds once, at the final width; one more repaint follows another 75 ms later, because
//!   tmux and VS Code may draw a frame or two at a wrong size first;
//! - every rebuild clears the screen and the terminal's whole scrollback (CSI 3J) and replays
//!   the conversation at the new width. The old "visible" mode (clear only the screen,
//!   replay the recent part) left a duplicate screenful in scrollback per resize and is gone;
//! - replayed rows are capped per terminal: replaying more rows
//!   than the terminal keeps is wasted work.
//!
//! Debounce and row caps: Copyright 2025 OpenAI (Apache-2.0).

use std::time::{Duration, Instant};

pub const DEBOUNCE: Duration = Duration::from_millis(75);

const VSCODE_MAX_ROWS: usize = 1_000;
const WINDOWS_TERMINAL_MAX_ROWS: usize = 9_001;
const WEZTERM_MAX_ROWS: usize = 3_500;
const ALACRITTY_MAX_ROWS: usize = 10_000;
/// Default fallback scrollback cap for resize reflow.
const FALLBACK_MAX_ROWS: usize = 1_000;

/// Rows a full replay may write, by terminal (documented scrollback defaults).
pub fn max_rows(var: &dyn Fn(&str) -> Option<String>) -> usize {
    let prog = var("TERM_PROGRAM").unwrap_or_default().to_ascii_lowercase();
    let term = var("TERM").unwrap_or_default().to_ascii_lowercase();
    if prog == "vscode" || var("VSCODE_INJECTION").is_some() {
        VSCODE_MAX_ROWS
    } else if var("WT_SESSION").is_some() {
        WINDOWS_TERMINAL_MAX_ROWS
    } else if prog == "wezterm" || var("WEZTERM_EXECUTABLE").is_some() {
        WEZTERM_MAX_ROWS
    } else if term == "alacritty" || var("ALACRITTY_WINDOW_ID").is_some() {
        ALACRITTY_MAX_ROWS
    } else {
        FALLBACK_MAX_ROWS
    }
}

/// Debounce state: a resize schedules a rebuild 75 ms out (pushed back by each further resize);
/// a rebuild schedules one follow-up repaint.
#[derive(Debug, Default)]
pub struct ReflowState {
    rebuild_at: Option<Instant>,
    repaint_at: Option<Instant>,
    rebuilt_width: Option<usize>,
}

/// What is due now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    Rebuild,
    Repaint,
}

impl ReflowState {
    /// The terminal reported a new size.
    pub fn resized(&mut self, width: usize, now: Instant) {
        if self.rebuilt_width.is_none() {
            self.rebuilt_width = Some(width);
            return;
        }
        if self.rebuilt_width != Some(width) {
            self.rebuild_at = Some(now + DEBOUNCE);
        }
        // a height-only change or a return to the rebuilt width still gets a repaint
        self.repaint_at = Some(now + DEBOUNCE);
    }

    /// Content styling changed at the current width. Coalesce a burst of style updates
    /// into one history replay, using the same bounded reflow path as a resize.
    pub fn invalidated(&mut self, now: Instant) {
        self.rebuild_at = Some(now + DEBOUNCE);
    }

    /// Record the width the first frame was drawn at (no rebuild for it).
    pub fn initial(&mut self, width: usize) {
        self.rebuilt_width.get_or_insert(width);
    }

    /// The next deadline, if any.
    pub fn next(&self) -> Option<Instant> {
        match (self.rebuild_at, self.repaint_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// What's due at `now` (a rebuild wins over a repaint). Taking it clears it; a rebuild
    /// schedules the follow-up repaint.
    pub fn take_due(&mut self, now: Instant, width: usize) -> Option<Due> {
        if self.rebuild_at.is_some_and(|t| now >= t) {
            self.rebuild_at = None;
            self.rebuilt_width = Some(width);
            self.repaint_at = Some(now + DEBOUNCE);
            return Some(Due::Rebuild);
        }
        if self.repaint_at.is_some_and(|t| now >= t) {
            self.repaint_at = None;
            return Some(Due::Repaint);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_changes_replay_at_the_same_width_once_per_burst() {
        let now = Instant::now();
        let mut state = ReflowState::default();
        state.initial(80);
        state.invalidated(now);
        state.invalidated(now + DEBOUNCE / 2);
        assert_eq!(state.take_due(now + DEBOUNCE, 80), None);
        assert_eq!(state.take_due(now + DEBOUNCE * 2, 80), Some(Due::Rebuild));
        assert_eq!(state.take_due(now + DEBOUNCE * 3, 80), Some(Due::Repaint));
        assert_eq!(state.next(), None);
    }

    #[test]
    fn a_drag_rebuilds_once_at_the_final_width_then_repaints_once_more() {
        let t0 = Instant::now();
        let mut r = ReflowState::default();
        r.initial(80);
        for (i, w) in [70, 60, 50].into_iter().enumerate() {
            r.resized(w, t0 + Duration::from_millis(i as u64 * 20));
        }
        let last = t0 + Duration::from_millis(40);
        assert_eq!(r.take_due(last + Duration::from_millis(74), 50), None, "debounced");
        assert_eq!(r.take_due(last + DEBOUNCE, 50), Some(Due::Rebuild));
        assert_eq!(r.take_due(last + DEBOUNCE + Duration::from_millis(10), 50), None);
        assert_eq!(r.take_due(last + DEBOUNCE * 2, 50), Some(Due::Repaint), "the follow-up repaint");
        assert_eq!(r.next(), None);
    }

    #[test]
    fn height_only_changes_repaint_without_rebuilding() {
        let t0 = Instant::now();
        let mut r = ReflowState::default();
        r.initial(80);
        r.resized(80, t0);
        assert_eq!(r.take_due(t0 + DEBOUNCE, 80), Some(Due::Repaint));
    }

    #[test]
    fn row_caps_follow_the_terminal() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(max_rows(&env(&[("TERM_PROGRAM", "vscode")])), VSCODE_MAX_ROWS);
        assert_eq!(max_rows(&env(&[("WT_SESSION", "x")])), WINDOWS_TERMINAL_MAX_ROWS);
        assert_eq!(max_rows(&env(&[("TERM_PROGRAM", "WezTerm")])), WEZTERM_MAX_ROWS);
        assert_eq!(max_rows(&env(&[("TERM", "alacritty")])), ALACRITTY_MAX_ROWS);
        assert_eq!(max_rows(&env(&[])), FALLBACK_MAX_ROWS);
    }
}
