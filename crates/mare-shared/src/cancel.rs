//! Cancellation, expiration, and `!Send`-compatible `or_cancel` helpers.
//!
//! Designcore/src/exec.rs` (`ExecExpiration`),
//! without `Send` bounds so `!Send` Lua `spawn_local` futures are supported.

use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Reason an operation stopped before completing naturally.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stop {
    Cancelled,
    TimedOut,
}

impl Stop {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }
}

/// Combined cancellation token and optional deadline.
///
/// When both a deadline and cancellation expire simultaneously, [`Stop::Cancelled`] wins.
#[derive(Clone, Debug, Default)]
pub struct Expiration {
    pub deadline: Option<Instant>,
    pub cancel: Option<CancellationToken>,
}

impl Expiration {
    pub fn never() -> Self {
        Self::default()
    }

    pub fn from_cancel(cancel: CancellationToken) -> Self {
        Self { deadline: None, cancel: Some(cancel) }
    }

    pub fn from_deadline(deadline: Instant) -> Self {
        Self { deadline: Some(deadline), cancel: None }
    }

    pub fn from_timeout(timeout: Duration) -> Self {
        Self::from_deadline(Instant::now() + timeout)
    }

    pub fn both(deadline: Option<Instant>, cancel: Option<CancellationToken>) -> Self {
        Self { deadline, cancel }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(CancellationToken::is_cancelled)
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.deadline.map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// Waits until either cancellation or deadline expires.
    /// If neither is set, pends forever.
    pub async fn wait(&self) -> Stop {
        if self.is_cancelled() {
            return Stop::Cancelled;
        }
        match (&self.cancel, self.deadline) {
            (Some(cancel), Some(deadline)) => {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Stop::Cancelled,
                    _ = tokio::time::sleep_until(deadline) => Stop::TimedOut,
                }
            }
            (Some(cancel), None) => {
                cancel.cancelled().await;
                Stop::Cancelled
            }
            (None, Some(deadline)) => {
                tokio::time::sleep_until(deadline).await;
                Stop::TimedOut
            }
            (None, None) => std::future::pending().await,
        }
    }

    /// Runs `fut` until completion or expiration, preferring [`Stop::Cancelled`] if both are ready.
    pub async fn run<F: Future>(&self, fut: F) -> Result<F::Output, Stop> {
        if self.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        tokio::select! {
            biased;
            stop = self.wait() => Err(stop),
            res = fut => Ok(res),
        }
    }
}

/// `!Send`-compatible cancellation extension for futures (including Lua `spawn_local` futures).
pub trait OrCancelExt: Sized {
    type Output;

    #[allow(async_fn_in_trait)]
    async fn or_cancel(self, token: &CancellationToken) -> Result<Self::Output, Stop>;

    #[allow(async_fn_in_trait)]
    async fn or_expire(self, expiration: &Expiration) -> Result<Self::Output, Stop>;
}

impl<F: Future> OrCancelExt for F {
    type Output = F::Output;

    async fn or_cancel(self, token: &CancellationToken) -> Result<Self::Output, Stop> {
        if token.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        tokio::select! {
            biased;
            _ = token.cancelled() => Err(Stop::Cancelled),
            res = self => Ok(res),
        }
    }

    async fn or_expire(self, expiration: &Expiration) -> Result<Self::Output, Stop> {
        expiration.run(self).await
    }
}

/// Races `fut` against up to two cancellation tokens (e.g. connection/request and session).
pub async fn or_cancel_both<F: Future>(
    fut: F,
    primary: &CancellationToken,
    secondary: Option<&CancellationToken>,
) -> Result<F::Output, Stop> {
    if primary.is_cancelled() || secondary.is_some_and(CancellationToken::is_cancelled) {
        return Err(Stop::Cancelled);
    }
    match secondary {
        Some(sec) => tokio::select! {
            biased;
            _ = primary.cancelled() => Err(Stop::Cancelled),
            _ = sec.cancelled() => Err(Stop::Cancelled),
            res = fut => Ok(res),
        },
        None => fut.or_cancel(primary).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[tokio::test]
    async fn or_cancel_supports_non_send_futures() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let token = CancellationToken::new();
                let rc = Rc::new(42);
                let res = async move {
                    tokio::task::yield_now().await;
                    *rc
                }
                .or_cancel(&token)
                .await;
                assert_eq!(res, Ok(42));
            })
            .await;
    }

    #[tokio::test]
    async fn simultaneous_cancel_and_timeout_returns_cancelled() {
        let token = CancellationToken::new();
        token.cancel();
        let exp = Expiration::both(Some(Instant::now()), Some(token));
        let res = std::future::pending::<()>().or_expire(&exp).await;
        assert_eq!(res, Err(Stop::Cancelled));
    }

    #[tokio::test]
    async fn expiration_times_out_when_not_cancelled() {
        let token = CancellationToken::new();
        let exp = Expiration::both(Some(Instant::now() + Duration::from_millis(10)), Some(token));
        let res = std::future::pending::<()>().or_expire(&exp).await;
        assert_eq!(res, Err(Stop::TimedOut));
    }

    #[test]
    fn stop_as_str_returns_expected_labels() {
        assert_eq!(Stop::Cancelled.as_str(), "cancelled");
        assert_eq!(Stop::TimedOut.as_str(), "timed_out");
    }

    #[test]
    fn expiration_constructors_and_remaining() {
        let never = Expiration::never();
        assert!(!never.is_cancelled());
        assert_eq!(never.remaining(), None);

        let token = CancellationToken::new();
        let from_cancel = Expiration::from_cancel(token.clone());
        assert!(!from_cancel.is_cancelled());
        assert_eq!(from_cancel.remaining(), None);
        token.cancel();
        assert!(from_cancel.is_cancelled());

        let future_deadline = Instant::now() + Duration::from_secs(60);
        let from_deadline = Expiration::from_deadline(future_deadline);
        assert!(!from_deadline.is_cancelled());
        assert!(from_deadline.remaining().is_some_and(|d| d > Duration::ZERO));

        let past_deadline = Instant::now() - Duration::from_secs(1);
        let expired = Expiration::from_deadline(past_deadline);
        assert_eq!(expired.remaining(), Some(Duration::ZERO));

        let from_timeout = Expiration::from_timeout(Duration::from_secs(30));
        assert!(!from_timeout.is_cancelled());
        assert!(from_timeout.remaining().is_some_and(|d| d > Duration::ZERO));
    }

    #[tokio::test]
    async fn expiration_wait_and_run_cover_all_arms() {
        // (None, None): fut completes before never() expires.
        let res = Expiration::never().run(async { 99 }).await;
        assert_eq!(res, Ok(99));

        // (None, Some(deadline)): deadline-only expiration returns TimedOut.
        let deadline_only = Expiration::from_deadline(Instant::now());
        assert_eq!(deadline_only.wait().await, Stop::TimedOut);

        // (Some(cancel), None): cancel-only expiration waits until token fires.
        let token = CancellationToken::new();
        let cancel_only = Expiration::from_cancel(token.clone());
        let trigger = async {
            tokio::task::yield_now().await;
            token.cancel();
        };
        let (stop, ()) = tokio::join!(cancel_only.wait(), trigger);
        assert_eq!(stop, Stop::Cancelled);

        // (Some(cancel), Some(deadline)): in-flight cancel wins over future deadline.
        let token2 = CancellationToken::new();
        let both = Expiration::both(Some(Instant::now() + Duration::from_secs(60)), Some(token2.clone()));
        let trigger2 = async {
            tokio::task::yield_now().await;
            token2.cancel();
        };
        let (stop2, ()) = tokio::join!(both.wait(), trigger2);
        assert_eq!(stop2, Stop::Cancelled);
    }

    #[tokio::test]
    async fn or_cancel_short_circuits_when_pre_cancelled_and_drops_in_flight() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let token = CancellationToken::new();
        token.cancel();
        let polled = Arc::new(AtomicBool::new(false));
        let polled_clone = polled.clone();
        let res = async move {
            polled_clone.store(true, Ordering::SeqCst);
            123
        }
        .or_cancel(&token)
        .await;
        assert_eq!(res, Err(Stop::Cancelled));
        assert!(!polled.load(Ordering::SeqCst), "pre-cancelled future must not be polled");

        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let live_token = CancellationToken::new();
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = DropFlag(dropped.clone());
        let fut = async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        };
        let cancel_later = async {
            tokio::task::yield_now().await;
            live_token.cancel();
        };
        let (res2, ()) = tokio::join!(fut.or_cancel(&live_token), cancel_later);
        assert_eq!(res2, Err(Stop::Cancelled));
        assert!(dropped.load(Ordering::SeqCst), "cancelled future must be dropped");
    }

    #[tokio::test]
    async fn or_cancel_both_handles_pre_cancelled_and_in_flight_tokens() {
        let primary = CancellationToken::new();
        let secondary = CancellationToken::new();

        // Secondary None delegates to primary.
        assert_eq!(or_cancel_both(async { 7 }, &primary, None).await, Ok(7));

        // Secondary Some completes when neither is cancelled.
        assert_eq!(or_cancel_both(async { 8 }, &primary, Some(&secondary)).await, Ok(8));

        // Pre-cancelled primary short-circuits.
        let p_done = CancellationToken::new();
        p_done.cancel();
        assert_eq!(or_cancel_both(std::future::pending::<()>(), &p_done, Some(&secondary)).await, Err(Stop::Cancelled));

        // Pre-cancelled secondary short-circuits.
        let s_done = CancellationToken::new();
        s_done.cancel();
        assert_eq!(or_cancel_both(std::future::pending::<()>(), &primary, Some(&s_done)).await, Err(Stop::Cancelled));

        // In-flight primary cancellation.
        let p_live = CancellationToken::new();
        let s_live = CancellationToken::new();
        let (res_p, ()) = tokio::join!(or_cancel_both(std::future::pending::<()>(), &p_live, Some(&s_live)), async {
            tokio::task::yield_now().await;
            p_live.cancel();
        });
        assert_eq!(res_p, Err(Stop::Cancelled));

        // In-flight secondary cancellation.
        let p_live2 = CancellationToken::new();
        let s_live2 = CancellationToken::new();
        let (res_s, ()) = tokio::join!(or_cancel_both(std::future::pending::<()>(), &p_live2, Some(&s_live2)), async {
            tokio::task::yield_now().await;
            s_live2.cancel();
        });
        assert_eq!(res_s, Err(Stop::Cancelled));
    }
}
