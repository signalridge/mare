//! Active-time deadlines shared by an operation and its human-input callbacks.
//! Guards are independent of callback names and frontends. Overlapping waits and waits
//! completed before the timer next polls preserve the elapsed active time.

use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Clone)]
pub(crate) struct HumanWaits(watch::Sender<Clock>);

#[derive(Clone)]
struct Clock {
    waits: usize,
    elapsed: Duration,
    since: Instant,
}

impl Clock {
    fn active_time(&self) -> Duration {
        self.elapsed + if self.waits == 0 { self.since.elapsed() } else { Duration::ZERO }
    }
}

impl Default for HumanWaits {
    fn default() -> Self {
        Self(watch::channel(Clock { waits: 0, elapsed: Duration::ZERO, since: Instant::now() }).0)
    }
}

impl HumanWaits {
    pub(crate) fn enter(&self) -> HumanWaitGuard {
        self.0.send_modify(|clock| {
            clock.elapsed = clock.active_time();
            clock.waits += 1;
        });
        HumanWaitGuard { waits: self.clone() }
    }

    pub(crate) fn active_time(&self) -> Duration {
        self.0.borrow().active_time()
    }

    /// Wait for an absolute deadline on this clock. The caller can poll its operation and
    /// cancellation alongside this future even while its time budget is paused.
    pub(crate) async fn until(&self, deadline: Duration) {
        let mut changes = self.0.subscribe();
        loop {
            let (remaining, paused) = {
                let clock = changes.borrow_and_update();
                (deadline.saturating_sub(clock.active_time()), clock.waits > 0)
            };
            if remaining.is_zero() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep(remaining), if !paused => {},
                _ = changes.changed() => {}
            }
        }
    }
}

pub(crate) struct HumanWaitGuard {
    waits: HumanWaits,
}

impl Drop for HumanWaitGuard {
    fn drop(&mut self) {
        self.waits.0.send_modify(|clock| {
            clock.waits -= 1;
            if clock.waits == 0 {
                clock.since = Instant::now();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn overlapping_and_unobserved_waits_preserve_active_budget() {
        let clock = HumanWaits::default();
        let deadline = clock.active_time() + Duration::from_millis(100);
        tokio::time::advance(Duration::from_millis(40)).await;
        let first = clock.enter();
        tokio::time::advance(Duration::from_secs(5)).await;
        let second = clock.enter();
        drop(first);
        tokio::time::advance(Duration::from_secs(5)).await;
        drop(second);
        assert_eq!(clock.active_time(), Duration::from_millis(40));
        // Neither transition needed a timer poll: exactly 60ms of budget remain.
        let before = Instant::now();
        clock.until(deadline).await;
        assert_eq!(before.elapsed(), Duration::from_millis(60));
    }
}
