//! How long output waits for its pictures before it moves to the scrollback.
//!
//! A finished block whose picture is still being prepared stays in the live area (its rows
//! reserved, see [`super::image::render`]): the scrollback can't be redrawn, so a commit then
//! would leave blank rows forever. That wait is bounded: once a block has waited
//! [`wait_limit`] (5 s), it is drawn once more with every picture that still isn't ready as its
//! one-line text placeholder ([`super::image::degrade`]) and committed (degrading an image
//! it cannot process in time to a text placeholder). This also ends the wait of a block with more pictures than the encode cache holds, whose pictures keep
//! evicting each other.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static LIMIT_MS: AtomicU64 = AtomicU64::new(5_000);

/// How long a block waits for its pictures.
pub fn wait_limit() -> Duration {
    Duration::from_millis(LIMIT_MS.load(Ordering::Relaxed))
}

/// Change the wait (tests).
pub fn set_wait_limit(d: Duration) {
    LIMIT_MS.store(d.as_millis() as u64, Ordering::Relaxed);
}

/// The wait of the block at the front of the commit queue.
#[derive(Default)]
pub struct Hold {
    since: Option<(u64, Instant)>,
}

impl Hold {
    /// When the currently held block hits [`wait_limit`] and must degrade to text placeholders.
    pub fn deadline(&self) -> Option<Instant> {
        self.since.map(|(_, s)| s + wait_limit())
    }

    /// Deadline only when `front_block_id` matches the currently held block.
    pub fn deadline_for(&self, front_block_id: Option<u64>) -> Option<Instant> {
        match (self.since, front_block_id) {
            (Some((held_id, s)), Some(front_id)) if held_id == front_id => Some(s + wait_limit()),
            _ => None,
        }
    }

    /// Cancel any active hold (e.g. when blocks are cleared).
    pub fn clear(&mut self) {
        self.since = None;
    }

    /// Cancel the active hold when its block was removed.
    pub fn clear_if(&mut self, id: u64) {
        if self.since.is_some_and(|(held_id, _)| held_id == id) {
            self.since = None;
        }
    }

    /// Draw block `id` with `draw`. `(lines, true)`: they can be committed — every picture is
    /// ready, or the block has waited long enough and was drawn again with text placeholders for
    /// the rest. `(lines, false)`: keep it live (rows reserved) and try again next frame.
    pub fn draw<T>(&mut self, id: u64, now: Instant, mut draw: impl FnMut() -> T) -> (T, bool) {
        let waited = super::image::waited();
        let out = draw();
        if super::image::waited() == waited {
            self.since = None;
            return (out, true);
        }
        let since = match self.since {
            Some((i, s)) if i == id => s,
            _ => {
                self.since = Some((id, now));
                now
            }
        };
        if now.saturating_duration_since(since) < wait_limit() {
            return (out, false);
        }
        self.since = None;
        let _text = super::image::degrade();
        (draw(), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_for_and_clear_drop_stale_holds() {
        let mut hold = Hold::default();
        let t0 = Instant::now();
        hold.since = Some((42, t0));
        assert_eq!(hold.deadline_for(Some(42)), Some(t0 + wait_limit()));
        assert_eq!(hold.deadline_for(Some(99)), None);
        assert_eq!(hold.deadline_for(None), None);

        hold.clear_if(99);
        assert!(hold.deadline().is_some());
        hold.clear_if(42);
        assert!(hold.deadline().is_none());

        hold.since = Some((7, t0));
        hold.clear();
        assert!(hold.deadline().is_none());
    }
}
