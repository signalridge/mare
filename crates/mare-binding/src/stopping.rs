//! Process trees in the middle of a stop. A stop is polite
//! first — SIGINT, SIGTERM, a grace period — and only then SIGKILL; while one is under way the
//! user can press Ctrl+C again to skip the rest: [`force`] SIGKILLs every tree that is being
//! stopped right now, and trees whose stop starts in the next moments (the interrupt is still
//! reaching them) are killed as soon as they register. Mare keeps its polite-then-forced
//! escalation and makes the forced kill the user's call.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::platform::{self, Signal};

/// A forced stop also covers stops that begin this soon after it (the cancellation is still
/// travelling from the agent to the tool).
const FORCE_WINDOW: Duration = Duration::from_secs(3);

struct State {
    /// pid → number of stops in progress for it
    live: BTreeMap<u32, usize>,
    forced_at: Option<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State { live: BTreeMap::new(), forced_at: None });

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Registration of one stop in progress; dropping it ends the registration.
pub struct Guard(Option<u32>);

impl Guard {
    /// A stop of the tree rooted at `pid` begins. Right after a [`force`], the tree is killed at
    /// once instead.
    pub fn new(pid: Option<u32>) -> Self {
        let Some(pid) = pid else { return Self(None) };
        let forced = {
            let mut s = state();
            *s.live.entry(pid).or_insert(0) += 1;
            s.forced_at.is_some_and(|t| t.elapsed() < FORCE_WINDOW)
        };
        if forced {
            platform::signal_tree(Some(pid), Signal::Kill);
        }
        Self(Some(pid))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let Some(pid) = self.0 else { return };
        let mut s = state();
        if let Some(n) = s.live.get_mut(&pid) {
            *n -= 1;
            if *n == 0 {
                s.live.remove(&pid);
            }
        }
    }
}

/// Whether any process tree is being stopped.
pub fn active() -> bool {
    !state().live.is_empty()
}

/// SIGKILL (Windows: end the job) every tree being stopped, now and for the next moments.
/// Returns how many were signalled now.
pub fn force() -> usize {
    let pids: Vec<u32> = {
        let mut s = state();
        s.forced_at = Some(Instant::now());
        s.live.keys().copied().collect()
    };
    for &pid in &pids {
        platform::signal_tree(Some(pid), Signal::Kill);
    }
    pids.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_register_while_alive() {
        // above every platform's pid limit (nothing is signalled here: no force)
        let pid = 1_000_000_007;
        let g = Guard::new(Some(pid));
        let g2 = Guard::new(Some(pid));
        assert!(state().live.contains_key(&pid));
        drop(g);
        assert!(state().live.contains_key(&pid), "a second stop of the same tree still runs");
        drop(g2);
        assert!(!state().live.contains_key(&pid));
        assert!(Guard::new(None).0.is_none());
    }
}
