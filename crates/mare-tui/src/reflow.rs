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

/// Policy for width-triggered scrollback reflow (`terminal.reflow.mode`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReflowMode {
    /// Reflow with `CSI 3J` when committed history fits within the replay cap; preserve existing
    /// terminal scrollback instead of wiping it when history exceeds the cap.
    #[default]
    Auto,
    /// Always clear scrollback (`CSI 3J`) and replay up to the configured row cap.
    Full,
    /// Never clear terminal scrollback (`CSI 3J`) on width resize.
    Preserve,
}

impl ReflowMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "full" => Some(Self::Full),
            "preserve" => Some(Self::Preserve),
            _ => None,
        }
    }
}

/// Resolve the effective replay row cap from `terminal.reflow.max_rows` (`Some(0)` = unlimited,
/// `Some(n)` = explicit cap, `None` = `default_cap` from [`max_rows`]).
pub fn resolve_max_rows(configured: Option<usize>, default_cap: usize) -> usize {
    match configured {
        Some(0) => usize::MAX,
        Some(n) => n,
        None => default_cap,
    }
}

/// Why a debounced [`Due::Rebuild`] was scheduled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum RebuildTrigger {
    /// Passive width change (honors `terminal.reflow.mode`).
    #[default]
    Resize,
    /// Committed content or styling changed (theme, color mode, visible block update).
    Content,
    /// Explicit user view toggle (`Ctrl+O`, `/show-reasoning`): always replay.
    Force,
}

/// Debounce state: a resize schedules a rebuild 75 ms out (pushed back by each further resize);
/// a rebuild schedules one follow-up repaint.
#[derive(Debug, Default)]
pub struct ReflowState {
    rebuild_at: Option<Instant>,
    repaint_at: Option<Instant>,
    rebuilt_width: Option<usize>,
    pending_trigger: Option<RebuildTrigger>,
    last_trigger: RebuildTrigger,
}

/// What is due now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Due {
    Rebuild,
    Repaint,
}

impl ReflowState {
    fn schedule(&mut self, trigger: RebuildTrigger, now: Instant) {
        self.pending_trigger = Some(self.pending_trigger.map_or(trigger, |prev| prev.max(trigger)));
        self.rebuild_at = Some(now + DEBOUNCE);
    }

    /// The terminal reported a new size.
    pub fn resized(&mut self, width: usize, now: Instant) {
        if self.rebuilt_width.is_none() {
            self.rebuilt_width = Some(width);
            return;
        }
        if self.rebuilt_width != Some(width) {
            self.schedule(RebuildTrigger::Resize, now);
        }
        // a height-only change or a return to the rebuilt width still gets a repaint
        self.repaint_at = Some(now + DEBOUNCE);
    }

    /// Content styling or a visible committed block changed at the current width. Coalesce a
    /// burst of updates into one history replay, distinct from a passive width resize.
    pub fn invalidated(&mut self, now: Instant) {
        self.schedule(RebuildTrigger::Content, now);
    }

    /// Explicit user view toggle (`Ctrl+O`, `/show-reasoning`): always replay on rebuild even
    /// when `terminal.reflow.mode` would otherwise preserve scrollback on passive resizes.
    pub fn force_invalidated(&mut self, now: Instant) {
        self.schedule(RebuildTrigger::Force, now);
    }

    /// Re-schedule a deferred rebuild (for example while the transcript overlay was open) while
    /// preserving its original [`RebuildTrigger`].
    pub fn requeue(&mut self, trigger: RebuildTrigger, now: Instant) {
        self.schedule(trigger, now);
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

    /// Why the most recently taken [`Due::Rebuild`] was scheduled.
    pub fn last_rebuild_trigger(&self) -> RebuildTrigger {
        self.last_trigger
    }

    /// What's due at `now` (a rebuild wins over a repaint). Taking it clears it; a rebuild
    /// schedules the follow-up repaint.
    pub fn take_due(&mut self, now: Instant, width: usize) -> Option<Due> {
        if self.rebuild_at.is_some_and(|t| now >= t) {
            self.rebuild_at = None;
            self.rebuilt_width = Some(width);
            self.repaint_at = Some(now + DEBOUNCE);
            self.last_trigger = self.pending_trigger.take().unwrap_or(RebuildTrigger::Resize);
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
        assert_eq!(resolve_max_rows(None, 1_000), 1_000);
        assert_eq!(resolve_max_rows(Some(0), 1_000), usize::MAX);
        assert_eq!(resolve_max_rows(Some(250), 1_000), 250);
    }

    #[test]
    fn rebuild_trigger_distinguishes_resize_content_and_force() {
        let now = Instant::now();
        let mut state = ReflowState::default();
        state.initial(80);

        state.resized(70, now);
        assert_eq!(state.take_due(now + DEBOUNCE, 70), Some(Due::Rebuild));
        assert_eq!(state.last_rebuild_trigger(), RebuildTrigger::Resize);

        // Coalescing a resize and a content update preserves Content.
        state.resized(60, now + DEBOUNCE);
        state.invalidated(now + DEBOUNCE);
        assert_eq!(state.take_due(now + DEBOUNCE * 2, 60), Some(Due::Rebuild));
        assert_eq!(state.last_rebuild_trigger(), RebuildTrigger::Content);

        // Coalescing a force invalidation and a resize preserves Force.
        state.force_invalidated(now + DEBOUNCE * 2);
        state.resized(50, now + DEBOUNCE * 2);
        assert_eq!(state.take_due(now + DEBOUNCE * 3, 50), Some(Due::Rebuild));
        assert_eq!(state.last_rebuild_trigger(), RebuildTrigger::Force);
    }
}
