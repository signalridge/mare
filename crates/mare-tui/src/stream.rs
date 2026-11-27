//! Streaming commit pacing.
//!
//! Finished top-level blocks of a streaming answer move from the live area into scrollback
//! while the answer is still streaming; the last block (which the next line may still change —
//! setext `===`, list continuations, fences, tables) stays live. Committed lines are
//! append-only. Lines become committable in bursts (a whole block at once), so they go through a
//! queue drained by an adaptive chunking policy: one line per frame ("smooth") or the whole
//! backlog ("catch-up"), with hysteresis between the two.
//!
//! The chunking policy below is Copyright 2025 OpenAI (Apache-2.0); [`StreamCommit`] is mare's.
//! Commit progress is also recorded as a *source position* (`drained_src`: the byte offset in
//! the markdown source whose rendered lines are all committed), so a width change can recompute
//! how many lines are committed instead of trusting a line count from the old width.

use std::collections::VecDeque;

use crate::style::Line;

// ---- adaptive streaming chunking policy --------------------------------------------------------

use std::time::Duration;
use std::time::Instant;

/// Queue-depth threshold that allows entering catch-up mode.
///
/// Crossing this threshold alone is sufficient to leave smooth mode.
const ENTER_QUEUE_DEPTH_LINES: usize = 8;

/// Oldest-line age threshold that allows entering catch-up mode.
///
/// Crossing this threshold alone is sufficient to leave smooth mode.
const ENTER_OLDEST_AGE: Duration = Duration::from_millis(120);

/// Queue-depth threshold used when evaluating catch-up exit hysteresis.
///
/// Depth must be at or below this value before exit hold timing can begin.
const EXIT_QUEUE_DEPTH_LINES: usize = 2;

/// Oldest-line age threshold used when evaluating catch-up exit hysteresis.
///
/// Age must be at or below this value before exit hold timing can begin.
const EXIT_OLDEST_AGE: Duration = Duration::from_millis(40);

/// Minimum duration queue pressure must stay below exit thresholds to leave catch-up mode.
const EXIT_HOLD: Duration = Duration::from_millis(250);

/// Cooldown window after a catch-up exit that suppresses immediate re-entry.
///
/// Severe backlog still bypasses this hold to avoid unbounded queue-age growth.
const REENTER_CATCH_UP_HOLD: Duration = Duration::from_millis(250);

/// Queue-depth cutoff that marks backlog as severe for faster convergence.
///
/// This threshold is used to bypass re-entry hold after a recent catch-up exit.
const SEVERE_QUEUE_DEPTH_LINES: usize = 64;

/// Oldest-line age cutoff that marks backlog as severe for faster convergence.
const SEVERE_OLDEST_AGE: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChunkingMode {
    /// Drain one line per baseline commit tick.
    #[default]
    Smooth,
    /// Drain multiple lines per tick according to queue pressure.
    CatchUp,
}

/// Captures queue pressure inputs used by adaptive chunking decisions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueSnapshot {
    /// Number of queued stream lines waiting to be displayed.
    pub queued_lines: usize,
    /// Age of the oldest queued line at decision time.
    pub oldest_age: Option<Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DrainPlan {
    /// Emit exactly one queued line.
    Single,
    /// Emit up to `usize` queued lines.
    Batch(usize),
}

/// Represents one policy decision for a specific queue snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkingDecision {
    /// Mode after applying hysteresis transitions for this decision.
    pub mode: ChunkingMode,
    /// Whether this decision transitioned from `Smooth` into `CatchUp`.
    pub entered_catch_up: bool,
    /// Drain plan to execute for the current commit tick.
    pub drain_plan: DrainPlan,
}

/// Maintains adaptive chunking mode and hysteresis state across ticks.
#[derive(Debug, Default)]
pub struct AdaptiveChunkingPolicy {
    mode: ChunkingMode,
    below_exit_threshold_since: Option<Instant>,
    last_catch_up_exit_at: Option<Instant>,
}

impl AdaptiveChunkingPolicy {
    /// Returns the policy mode used by the most recent decision.
    pub fn mode(&self) -> ChunkingMode {
        self.mode
    }

    /// Resets state to baseline smooth mode.
    pub fn reset(&mut self) {
        self.mode = ChunkingMode::Smooth;
        self.below_exit_threshold_since = None;
        self.last_catch_up_exit_at = None;
    }

    /// Computes a drain decision from the current queue snapshot.
    ///
    /// The decision is deterministic for a given `(mode, snapshot, now)` triple. Callers should
    /// avoid inventing synthetic snapshots; stale queue age data can cause premature catch-up exits.
    pub fn decide(&mut self, snapshot: QueueSnapshot, now: Instant) -> ChunkingDecision {
        if snapshot.queued_lines == 0 {
            self.note_catch_up_exit(now);
            self.mode = ChunkingMode::Smooth;
            self.below_exit_threshold_since = None;
            return ChunkingDecision { mode: self.mode, entered_catch_up: false, drain_plan: DrainPlan::Single };
        }

        let entered_catch_up = match self.mode {
            ChunkingMode::Smooth => self.maybe_enter_catch_up(snapshot, now),
            ChunkingMode::CatchUp => {
                self.maybe_exit_catch_up(snapshot, now);
                false
            }
        };

        let drain_plan = match self.mode {
            ChunkingMode::Smooth => DrainPlan::Single,
            ChunkingMode::CatchUp => DrainPlan::Batch(snapshot.queued_lines.max(1)),
        };

        ChunkingDecision { mode: self.mode, entered_catch_up, drain_plan }
    }

    /// Switches from `Smooth` to `CatchUp` when enter thresholds are crossed.
    ///
    /// Returns `true` only on the transition tick so callers can emit one-shot
    /// transition observability.
    fn maybe_enter_catch_up(&mut self, snapshot: QueueSnapshot, now: Instant) -> bool {
        if !should_enter_catch_up(snapshot) {
            return false;
        }
        if self.reentry_hold_active(now) && !is_severe_backlog(snapshot) {
            return false;
        }
        self.mode = ChunkingMode::CatchUp;
        self.below_exit_threshold_since = None;
        self.last_catch_up_exit_at = None;
        true
    }

    /// Applies exit hysteresis while in `CatchUp` mode.
    ///
    /// The policy requires queue pressure to stay below exit thresholds for the
    /// full `EXIT_HOLD` window before returning to `Smooth`.
    fn maybe_exit_catch_up(&mut self, snapshot: QueueSnapshot, now: Instant) {
        if !should_exit_catch_up(snapshot) {
            self.below_exit_threshold_since = None;
            return;
        }

        match self.below_exit_threshold_since {
            Some(since) if now.saturating_duration_since(since) >= EXIT_HOLD => {
                self.mode = ChunkingMode::Smooth;
                self.below_exit_threshold_since = None;
                self.last_catch_up_exit_at = Some(now);
            }
            Some(_) => {}
            None => {
                self.below_exit_threshold_since = Some(now);
            }
        }
    }

    fn note_catch_up_exit(&mut self, now: Instant) {
        if self.mode == ChunkingMode::CatchUp {
            self.last_catch_up_exit_at = Some(now);
        }
    }

    fn reentry_hold_active(&self, now: Instant) -> bool {
        self.last_catch_up_exit_at.is_some_and(|exit| now.saturating_duration_since(exit) < REENTER_CATCH_UP_HOLD)
    }
}

/// Returns whether current queue pressure warrants entering catch-up mode.
///
/// Either depth or age pressure is sufficient to trigger catch-up.
fn should_enter_catch_up(snapshot: QueueSnapshot) -> bool {
    snapshot.queued_lines >= ENTER_QUEUE_DEPTH_LINES
        || snapshot.oldest_age.is_some_and(|oldest| oldest >= ENTER_OLDEST_AGE)
}

/// Returns whether queue pressure is low enough to begin exit hysteresis.
///
/// Both depth and age must be below thresholds; this prevents oscillation when
/// one signal is still under load.
fn should_exit_catch_up(snapshot: QueueSnapshot) -> bool {
    snapshot.queued_lines <= EXIT_QUEUE_DEPTH_LINES
        && snapshot.oldest_age.is_some_and(|oldest| oldest <= EXIT_OLDEST_AGE)
}

/// Returns whether backlog is severe enough to use a faster catch-up target.
///
/// Severe pressure bypasses re-entry hold to avoid queue-age growth after a
/// recent catch-up exit.
fn is_severe_backlog(snapshot: QueueSnapshot) -> bool {
    snapshot.queued_lines >= SEVERE_QUEUE_DEPTH_LINES
        || snapshot.oldest_age.is_some_and(|oldest| oldest >= SEVERE_OLDEST_AGE)
}

// ---- mare: the commit queue ------------------------------------------------------------------

struct Queued {
    line: Line,
    at: Instant,
    /// source offset whose lines are all committed once this line is drained
    src_end: Option<usize>,
}

/// Commit state of one streaming block (lines are the block's rendered rows).
#[derive(Default)]
pub struct StreamCommit {
    policy: AdaptiveChunkingPolicy,
    queue: VecDeque<Queued>,
    /// rows handed out for scrollback (the block shows `rows[drained..]` live)
    pub drained: usize,
    /// source offset whose rows are all committed
    pub drained_src: usize,
    /// rows queued or drained so far
    queued_upto: usize,
}

impl StreamCommit {
    pub fn new() -> Self {
        Self::default()
    }

    /// The block's stable rows (`stable` = rows of completed top-level blocks, which won't change)
    /// grew: queue the new ones. `stable_src` is the source offset those rows cover.
    pub fn sync(&mut self, rows: &[Line], stable: usize, stable_src: usize, now: Instant) {
        let stable = stable.min(rows.len());
        if stable <= self.queued_upto {
            return;
        }
        for (i, line) in rows[self.queued_upto..stable].iter().enumerate() {
            let last = self.queued_upto + i + 1 == stable;
            self.queue.push_back(Queued { line: line.clone(), at: now, src_end: last.then_some(stable_src) });
        }
        self.queued_upto = stable;
    }

    /// [`StreamCommit::sync`] given only the rows from row `skip` on (`skip` ≤ the rows already
    /// queued — the committed ones aren't rebuilt each frame).
    pub fn sync_from(&mut self, rows_from: &[Line], skip: usize, stable: usize, stable_src: usize, now: Instant) {
        debug_assert!(skip <= self.queued_upto, "rows before the queue were skipped");
        let stable = stable.min(skip + rows_from.len());
        if stable <= self.queued_upto || skip > self.queued_upto {
            return;
        }
        for (i, line) in rows_from[self.queued_upto - skip..stable - skip].iter().enumerate() {
            let last = self.queued_upto + i + 1 == stable;
            self.queue.push_back(Queued { line: line.clone(), at: now, src_end: last.then_some(stable_src) });
        }
        self.queued_upto = stable;
    }

    /// One commit tick (a frame): the rows to commit now, per the chunking policy.
    pub fn tick(&mut self, now: Instant) -> Vec<Line> {
        let snapshot = QueueSnapshot {
            queued_lines: self.queue.len(),
            oldest_age: self.queue.front().map(|q| now.saturating_duration_since(q.at)),
        };
        let n = match self.policy.decide(snapshot, now).drain_plan {
            DrainPlan::Single => 1,
            DrainPlan::Batch(n) => n,
        };
        self.take(n.min(self.queue.len()))
    }

    /// Everything queued (the block finished, or a full flush).
    pub fn drain_all(&mut self) -> Vec<Line> {
        self.take(self.queue.len())
    }

    fn take(&mut self, n: usize) -> Vec<Line> {
        let mut out = Vec::with_capacity(n);
        for q in self.queue.drain(..n) {
            if let Some(s) = q.src_end {
                self.drained_src = s;
            }
            out.push(q.line);
        }
        self.drained += out.len();
        out
    }

    /// Whether queued stable lines are still waiting to drain into scrollback.
    pub fn has_pending(&self) -> bool {
        !self.queue.is_empty()
    }

    /// The policy's current gear (tests, `/profile`).
    pub fn mode(&self) -> ChunkingMode {
        self.policy.mode()
    }

    /// After a width change: rows are re-rendered, so line counts from the old width mean
    /// nothing. `committed_rows` = rows of the source up to `drained_src` at the new width; the
    /// queue (old-width rows) is dropped and refilled by the next [`StreamCommit::sync`].
    pub fn rewidth(&mut self, committed_rows: usize) {
        self.queue.clear();
        self.policy.reset();
        self.drained = committed_rows;
        self.queued_upto = committed_rows;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::{StreamingMarkdown, render_markdown};
    use crate::style::Theme;
    use std::time::Duration;

    // --- chunking tests (ported) ----
    fn snapshot(queued_lines: usize, oldest_age_ms: u64) -> QueueSnapshot {
        QueueSnapshot { queued_lines, oldest_age: Some(Duration::from_millis(oldest_age_ms)) }
    }

    #[test]
    fn smooth_by_default_and_catch_up_on_depth_or_age() {
        let now = Instant::now();
        let mut p = AdaptiveChunkingPolicy::default();
        assert_eq!(p.decide(snapshot(1, 1), now).drain_plan, DrainPlan::Single);
        let mut p = AdaptiveChunkingPolicy::default();
        let d = p.decide(snapshot(ENTER_QUEUE_DEPTH_LINES, 1), now);
        assert_eq!((d.mode, d.drain_plan), (ChunkingMode::CatchUp, DrainPlan::Batch(ENTER_QUEUE_DEPTH_LINES)));
        let mut p = AdaptiveChunkingPolicy::default();
        let age = ENTER_OLDEST_AGE.as_millis() as u64;
        assert_eq!(p.decide(snapshot(EXIT_QUEUE_DEPTH_LINES, age), now).mode, ChunkingMode::CatchUp);
    }

    #[test]
    fn catch_up_exits_only_after_the_hold_and_does_not_flap() {
        let ms = |d: Duration| d.as_millis() as u64;
        let at = |t0: Instant, n: u64| t0 + Duration::from_millis(n);
        let (hold, low_age) = (ms(EXIT_HOLD), ms(EXIT_OLDEST_AGE));
        let t0 = Instant::now();
        let mut p = AdaptiveChunkingPolicy::default();
        p.decide(snapshot(ENTER_QUEUE_DEPTH_LINES + 1, 1), t0);
        // the backlog drops below the exit thresholds: still catching up until the hold passes
        let low_since = hold - 50;
        let low = snapshot(EXIT_QUEUE_DEPTH_LINES, low_age);
        assert_eq!(p.decide(low, at(t0, low_since)).mode, ChunkingMode::CatchUp);
        let exit = low_since + hold + 10;
        assert_eq!(p.decide(low, at(t0, exit)).mode, ChunkingMode::Smooth);
        // re-entry is held back right after an exit (unless the backlog is severe)
        assert!(ms(REENTER_CATCH_UP_HOLD) > 50);
        let shallow = snapshot(ENTER_QUEUE_DEPTH_LINES, 1);
        assert_eq!(p.decide(shallow, at(t0, exit + 40)).mode, ChunkingMode::Smooth);
        let severe = snapshot(SEVERE_QUEUE_DEPTH_LINES, 1);
        assert_eq!(p.decide(severe, at(t0, exit + 50)).mode, ChunkingMode::CatchUp);
    }

    // ---- mare: chunking-independent commits ----

    const DOC: &str = "# Title\n\nIntro paragraph with **bold** text that is long enough to wrap at small widths.\n\n- one\n- two\n  - nested\n\nSetext heading\n===\n\n```rust\nfn main() {\n\n    println!(\"hi\");\n}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\n> quote\n> more\n\nLast line without a newline";

    /// Stream `doc` cut into `cuts`, committing as a frontend would (smooth ticks while
    /// streaming, everything at the end); returns committed rows + the rows left live.
    fn stream(doc: &str, cuts: &[usize], width: usize, smooth: bool) -> Vec<String> {
        let theme = Theme::default();
        let mut md = StreamingMarkdown::new();
        let mut sc = StreamCommit::new();
        let mut committed: Vec<String> = vec![];
        let t0 = Instant::now();
        let mut pos = 0;
        for (step, &c) in cuts.iter().chain(std::iter::once(&doc.len())).enumerate() {
            let c = c.clamp(pos, doc.len());
            if !doc.is_char_boundary(c) {
                continue;
            }
            md.push(&doc[pos..c]);
            pos = c;
            let rows = md.render(width, &theme);
            // committed rows never change: they are a prefix of what is rendered now
            for (i, r) in committed.iter().enumerate() {
                assert_eq!(&rows[i].plain(), r, "committed row {i} changed after step {step}");
            }
            sc.sync(&rows, md.stable_lines(), md.stable_src(), t0);
            let now = t0 + Duration::from_millis(step as u64 * 16);
            let got = if smooth { sc.tick(now) } else { sc.drain_all() };
            committed.extend(got.iter().map(|l| l.plain()));
        }
        // finished: the rest (queued + live tail) is committed as the final render
        let rows = md.render(width, &theme);
        committed.extend(rows[sc.drained..].iter().map(|l| l.plain()));
        committed
    }

    #[test]
    fn sync_from_tail_rows_matches_sync_of_all_rows() {
        // the frontend's per-frame path: render only the rows after the committed ones
        let theme = Theme::default();
        let doc = "# T\n\npara one\n\n- a\n- b\n\n```\ncode\n```\n\nlast para that wraps around a bit\n\nend\n";
        let (mut a_md, mut b_md) = (StreamingMarkdown::new(), StreamingMarkdown::new());
        let (mut a, mut b) = (StreamCommit::new(), StreamCommit::new());
        let (mut ca, mut cb) = (Vec::new(), Vec::new());
        let t0 = Instant::now();
        for (step, ch) in doc.chars().enumerate() {
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            a_md.push(s);
            b_md.push(s);
            let now = t0 + Duration::from_millis(step as u64 * 16);
            let all = a_md.render(20, &theme);
            a.sync(&all, a_md.stable_lines(), a_md.stable_src(), now);
            ca.extend(a.tick(now).iter().map(|l| l.plain()));
            let skip = b.drained;
            let tail = b_md.render_from(20, &theme, skip);
            b.sync_from(&tail, skip, b_md.stable_lines(), b_md.stable_src(), now);
            cb.extend(b.tick(now).iter().map(|l| l.plain()));
        }
        assert_eq!(ca, cb);
        assert!(!ca.is_empty());
    }

    #[test]
    fn commits_are_identical_however_the_stream_is_chunked() {
        let theme = Theme::default();
        for width in [24, 50, 80] {
            let full: Vec<String> = render_markdown(DOC, width, &theme).iter().map(|l| l.plain()).collect();
            let by_char: Vec<usize> = DOC.char_indices().map(|(i, _)| i).collect();
            let by_line: Vec<usize> = DOC.match_indices('\n').map(|(i, _)| i + 1).collect();
            let fixed = |n: usize| (n..DOC.len()).step_by(n).collect::<Vec<_>>();
            // a deterministic pseudo-random chunking (fixed seed)
            let mut seed = 0x2545F4914F6CDD1Du64;
            let mut random = vec![];
            let mut at = 0;
            while at < DOC.len() {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                at += 1 + (seed % 23) as usize;
                random.push(at);
            }
            for (name, cuts) in [
                ("char", by_char),
                ("line", by_line),
                ("7", fixed(7)),
                ("31", fixed(31)),
                ("random", random),
                ("whole", vec![]),
            ] {
                for smooth in [false, true] {
                    assert_eq!(
                        stream(DOC, &cuts, width, smooth),
                        full,
                        "width {width}, chunks {name}, smooth {smooth}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_last_block_stays_live_and_finished_blocks_commit() {
        let theme = Theme::default();
        let mut md = StreamingMarkdown::new();
        let mut sc = StreamCommit::new();
        let t0 = Instant::now();
        // a newline is necessary, not sufficient: the next block's first line must be complete
        md.push("First paragraph.\n\nSecond");
        let rows = md.render(80, &theme);
        sc.sync(&rows, md.stable_lines(), md.stable_src(), t0);
        assert!(sc.drain_all().is_empty());
        md.push(" heading\n");
        let rows = md.render(80, &theme);
        sc.sync(&rows, md.stable_lines(), md.stable_src(), t0);
        assert_eq!(sc.drain_all().iter().map(|l| l.plain()).collect::<Vec<_>>(), ["First paragraph."]);
        // a setext underline changes the meaning of the line above: still live until a new block
        md.push("===\n");
        let rows = md.render(80, &theme);
        sc.sync(&rows, md.stable_lines(), md.stable_src(), t0);
        assert!(sc.drain_all().is_empty(), "the last block is not committed");
        // a table is held whole until a following block starts
        md.push("\n| a | b |\n|---|---|\n| 1 | 2 |\n");
        let rows = md.render(80, &theme);
        sc.sync(&rows, md.stable_lines(), md.stable_src(), t0);
        assert!(sc.drain_all().is_empty(), "heading + table stay live while the table may grow");
        md.push("\nAfter.\n");
        let rows = md.render(80, &theme);
        sc.sync(&rows, md.stable_lines(), md.stable_src(), t0);
        let got: Vec<String> = sc.drain_all().iter().map(|l| l.plain()).collect();
        assert!(got.iter().any(|l| l.contains("Second heading")), "{got:?}");
        assert!(got.iter().any(|l| l.contains('1') && l.contains('2')), "the whole table commits: {got:?}");
        assert!(!got.iter().any(|l| l.contains("After")), "the last block stays live: {got:?}");
        assert_eq!(
            sc.drained_src,
            "First paragraph.\n\nSecond heading\n===\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n".len()
        );
    }

    #[test]
    fn a_width_change_mid_stream_continues_from_the_committed_source_position() {
        let theme = Theme::default();
        let mut md = StreamingMarkdown::new();
        let mut sc = StreamCommit::new();
        let t0 = Instant::now();
        let half = DOC.find("```rust").unwrap();
        md.push(&DOC[..half]);
        let rows = md.render(80, &theme);
        sc.sync(&rows, md.stable_lines(), md.stable_src(), t0);
        sc.drain_all();
        assert!(sc.drained_src > 0 && sc.drained_src <= half);
        // resize to 30 columns: committed rows are recomputed from the source position
        let n = md.rows_for(sc.drained_src, 30, &theme);
        md.drop_cache();
        sc.rewidth(n);
        let full30: Vec<String> = render_markdown(DOC, 30, &theme).iter().map(|l| l.plain()).collect();
        let prefix30: Vec<String> =
            render_markdown(&DOC[..sc.drained_src], 30, &theme).iter().map(|l| l.plain()).collect();
        assert_eq!(&full30[..n], &prefix30[..], "the replayed history is the new-width render of the committed source");
        // the stream goes on at the new width: later commits continue exactly after the replay
        md.push(&DOC[half..]);
        let rows = md.render(30, &theme);
        sc.sync(&rows, md.stable_lines(), md.stable_src(), t0);
        let mut got: Vec<String> = prefix30;
        got.extend(sc.drain_all().iter().map(|l| l.plain()));
        got.extend(md.render(30, &theme)[sc.drained..].iter().map(|l| l.plain()));
        assert_eq!(got, full30);
    }

    #[test]
    fn smooth_drains_one_row_per_frame_and_catches_up_under_a_burst() {
        let t0 = Instant::now();
        let rows: Vec<Line> = (0..20).map(|i| Line::raw(format!("r{i}"))).collect();
        let mut sc = StreamCommit::new();
        sc.sync(&rows, 3, 10, t0);
        assert_eq!(sc.tick(t0).len(), 1, "smooth: one row per frame");
        sc.sync(&rows, 20, 99, t0);
        assert_eq!(sc.tick(t0 + Duration::from_millis(16)).len(), 19, "a burst drains at once");
        assert_eq!(sc.drained_src, 99);
    }
}
