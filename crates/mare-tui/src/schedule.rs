//! Frame scheduling.
//!
//! - at most 60 frames a second; input renders immediately (no frame-interval wait);
//! - while the previous frame is still being written, the next one waits for the writer to
//!   drain (backpressure) instead of piling up;
//! - **Invariant (no stored derived deadlines)**: the scheduler never stores a derived absolute
//!   animation deadline (`anim_at`). Instead it retains only primitive source state (`epoch`,
//!   `last_anim_tick`, `last_activity`, `focused`) and computes [`Schedule::next_anim_deadline`]
//!   fresh from current state on every event-loop iteration. Any state change that affects
//!   cadence — focus, key/mouse/paste/resize activity, `working` start/end, `reduce_motion`
//!   toggles — therefore takes effect immediately without risking stale sleep deadlines.
//! - Timed redraw cadence:
//!   - while `streaming` (`working` active): 80 ms focused (`ANIMATE`), capped at 2× (160 ms,
//!     `ANIMATE_UNFOCUSED`) when unfocused; **never** enters `DEEP_IDLE` (5 s) while working;
//!   - while not `streaming`: 250 ms focused (`IDLE`), 500 ms unfocused (`IDLE_UNFOCUSED`),
//!     5 s (`DEEP_IDLE`) after 30 s (`DEEP_AFTER`) without activity;
//!   - under `reduce_motion`: 1 s (`REDUCE_MOTION`) so elapsed seconds still tick.
//! - Deadlines align to `epoch + k * interval` boundaries so OS wakeup jitter never accumulates
//!   into phase drift.
//!
//! Pure: every decision takes `now`, so tests drive it with a synthetic clock.

use std::time::{Duration, Instant};

pub const FRAME: Duration = Duration::from_millis(16);
pub const ANIMATE: Duration = Duration::from_millis(80);
pub const ANIMATE_UNFOCUSED: Duration = Duration::from_millis(160);
pub const IDLE: Duration = Duration::from_millis(250);
pub const IDLE_UNFOCUSED: Duration = Duration::from_millis(500);
pub const REDUCE_MOTION: Duration = Duration::from_secs(1);
pub const DEEP_IDLE: Duration = Duration::from_secs(5);
pub const DEEP_AFTER: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct Schedule {
    epoch: Instant,
    last_draw: Option<Instant>,
    last_activity: Instant,
    last_anim_tick: Option<Instant>,
    focused: bool,
}

impl Schedule {
    pub fn new(now: Instant) -> Self {
        Self { epoch: now, last_draw: None, last_activity: now, last_anim_tick: None, focused: true }
    }

    /// Input or output happened (keys, mouse, paste, resize, focus-in, UI commands, streamed text).
    pub fn activity(&mut self, now: Instant) {
        self.last_activity = now;
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn drawn(&mut self, now: Instant) {
        self.last_draw = Some(now);
    }

    pub fn idle_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_activity)
    }

    /// Whether an animation cycle is currently anchored.
    pub fn has_anim_tick(&self) -> bool {
        self.last_anim_tick.is_some()
    }

    /// Anchor a new animation cycle at `now` (transitions `!animating -> animating`), resetting
    /// the alignment epoch so the first tick waits a full interval and subsequent ticks align
    /// cleanly without cumulative drift.
    pub fn anchor_anim_tick(&mut self, now: Instant) {
        self.epoch = now;
        self.last_anim_tick = Some(now);
    }

    /// Record that an animation tick fired at `now` while preserving the alignment `epoch`.
    pub fn note_anim_tick(&mut self, now: Instant) {
        self.last_anim_tick = Some(now);
    }

    /// Clear animation anchoring when nothing is animating.
    pub fn clear_anim_tick(&mut self) {
        self.last_anim_tick = None;
    }

    /// May a frame be drawn now? `urgent` (input) skips the 60 fps limit; a busy writer
    /// (`writer_busy`) defers everything until it drains.
    pub fn can_draw(&self, now: Instant, urgent: bool, writer_busy: bool) -> bool {
        if writer_busy {
            return false;
        }
        urgent || self.last_draw.is_none_or(|t| now.saturating_duration_since(t) >= FRAME)
    }

    /// How long until a pending (dirty) frame may be drawn.
    pub fn frame_delay(&self, now: Instant) -> Duration {
        self.last_draw.map_or(Duration::ZERO, |t| FRAME.saturating_sub(now.saturating_duration_since(t)))
    }

    /// Interval of the timed redraw while something on screen animates.
    ///
    /// Working (`streaming = true`) is checked before `DEEP_IDLE` so a long silent tool or
    /// thinking phase never stalls to 5 s, and unfocused slowdown is capped at 2×.
    pub fn animation_interval(&self, now: Instant, streaming: bool) -> Duration {
        if streaming {
            if self.focused { ANIMATE } else { ANIMATE_UNFOCUSED }
        } else if self.idle_for(now) >= DEEP_AFTER {
            DEEP_IDLE
        } else if self.focused {
            IDLE
        } else {
            IDLE_UNFOCUSED
        }
    }

    /// Compute the next animation tick deadline directly from current state (`now`, `streaming`,
    /// `reduce_motion`) and `last_anim_tick`, aligned to `epoch + k * interval` boundaries.
    ///
    /// Returns `now` immediately if a cadence speedup (e.g. `FocusIn`, `working` start, or
    /// `reduce_motion` off) made the aligned next tick already due.
    pub fn next_anim_deadline(&self, now: Instant, streaming: bool, reduce_motion: bool) -> Instant {
        let interval = if reduce_motion { REDUCE_MOTION } else { self.animation_interval(now, streaming) };
        let Some(last) = self.last_anim_tick else {
            return now + interval;
        };
        let step_us = interval.as_micros().max(1);
        let elapsed_us = last.saturating_duration_since(self.epoch).as_micros();
        let next_k = u32::try_from((elapsed_us / step_us) + 1).unwrap_or(u32::MAX);
        let aligned = self.epoch + interval.saturating_mul(next_k);
        aligned.max(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_redraw_never_deep_idles_while_working_and_caps_unfocused_to_2x() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0);
        assert_eq!(s.animation_interval(t0, true), ANIMATE);
        assert_eq!(s.animation_interval(t0, false), IDLE);
        // Past DEEP_AFTER (30s): idle drops to DEEP_IDLE, but working stays at ANIMATE.
        assert_eq!(s.animation_interval(t0 + Duration::from_secs(31), false), DEEP_IDLE);
        assert_eq!(s.animation_interval(t0 + Duration::from_secs(31), true), ANIMATE);
        s.activity(t0 + Duration::from_secs(31));
        assert_eq!(s.animation_interval(t0 + Duration::from_secs(32), false), IDLE, "activity wakes idle");
        // Unfocused: working caps at 2x (160ms), idle caps at 2x (500ms) until DEEP_AFTER.
        s.set_focused(false);
        assert_eq!(s.animation_interval(t0 + Duration::from_secs(32), true), ANIMATE_UNFOCUSED);
        assert_eq!(s.animation_interval(t0 + Duration::from_secs(32), false), IDLE_UNFOCUSED);
        // Even past DEEP_AFTER while unfocused, working never enters DEEP_IDLE.
        assert_eq!(s.animation_interval(t0 + Duration::from_secs(70), true), ANIMATE_UNFOCUSED);
        assert_eq!(s.animation_interval(t0 + Duration::from_secs(70), false), DEEP_IDLE);
    }

    #[test]
    fn next_anim_deadline_aligns_without_drift_and_reacts_to_state_changes() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0);
        s.anchor_anim_tick(t0);
        assert_eq!(s.next_anim_deadline(t0, true, false), t0 + Duration::from_millis(80));
        // Woke up 7ms late -> next boundary stays at 160ms, not 167ms (zero drift).
        let t1 = t0 + Duration::from_millis(87);
        s.note_anim_tick(t1);
        assert_eq!(s.next_anim_deadline(t1, true, false), t0 + Duration::from_millis(160));
        // Woke up 5ms late on tick 2 -> stays locked to 240ms.
        let t2 = t0 + Duration::from_millis(165);
        s.note_anim_tick(t2);
        assert_eq!(s.next_anim_deadline(t2, true, false), t0 + Duration::from_millis(240));
        // FocusOut slows to 160ms grid (next after 165ms is 320ms).
        s.set_focused(false);
        assert_eq!(s.next_anim_deadline(t2, true, false), t0 + Duration::from_millis(320));
        // FocusIn at 250ms immediately wakes (240ms <= 250ms -> clamped to now).
        let t_fi = t0 + Duration::from_millis(250);
        s.set_focused(true);
        assert_eq!(s.next_anim_deadline(t_fi, true, false), t_fi);
        // reduce_motion on -> 1s grid; off -> immediate wakeup when past 80ms.
        s.note_anim_tick(t0 + Duration::from_millis(1000));
        assert_eq!(
            s.next_anim_deadline(t0 + Duration::from_millis(1000), true, true),
            t0 + Duration::from_millis(2000)
        );
        let t_rm_off = t0 + Duration::from_millis(1300);
        assert_eq!(s.next_anim_deadline(t_rm_off, true, false), t_rm_off);
    }

    #[test]
    fn input_skips_the_frame_limit_but_never_a_busy_writer() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0);
        assert!(s.can_draw(t0, false, false));
        s.drawn(t0);
        let soon = t0 + Duration::from_millis(5);
        assert!(!s.can_draw(soon, false, false));
        assert!(s.can_draw(soon, true, false), "input renders immediately");
        assert!(!s.can_draw(soon, true, true), "backpressure wins");
        assert_eq!(s.frame_delay(soon), Duration::from_millis(11));
        assert!(s.can_draw(t0 + FRAME, false, false));
    }

    #[test]
    fn frame_cadence_anchors_to_frame_start_preserving_urgent_and_backpressure() {
        let t0 = Instant::now();
        let mut s = Schedule::new(t0);
        // Frame started at t0 and finished 4ms later; anchoring to t0 leaves 12ms until next frame.
        s.drawn(t0);
        let t_done = t0 + Duration::from_millis(4);
        assert_eq!(s.frame_delay(t_done), Duration::from_millis(12));
        assert!(!s.can_draw(t0 + Duration::from_millis(15), false, false));
        assert!(s.can_draw(t0 + Duration::from_millis(16), false, false));
        assert!(s.can_draw(t_done, true, false));
        assert!(!s.can_draw(t_done, true, true));
        // Frame overrun (> 16ms) schedules next draw immediately once writer drains.
        let t_over = t0 + Duration::from_millis(21);
        assert_eq!(s.frame_delay(t_over), Duration::ZERO);
        assert!(s.can_draw(t_over, false, false));
        assert!(!s.can_draw(t_over, false, true));
    }
}
