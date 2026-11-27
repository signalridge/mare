//! Unified deterministic test barriers: TCP accept barrier with clean shutdown and
//! reusable two-way async gate for synchronizing concurrent tasks without fixed sleeps.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

/// Single-connection TCP listener barrier whose background `accept` thread is woken and joined on drop.
pub struct AcceptBarrier {
    cancelled: Arc<AtomicBool>,
    rx: std::sync::mpsc::Receiver<std::io::Result<(std::net::TcpStream, std::net::SocketAddr)>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AcceptBarrier {
    pub fn spawn(listener: std::net::TcpListener) -> Self {
        Self::spawn_with_done(listener, None)
    }

    pub fn spawn_with_done(listener: std::net::TcpListener, done: Option<Arc<AtomicBool>>) -> Self {
        listener.set_nonblocking(true).expect("set_nonblocking");
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_thread = Arc::clone(&cancelled);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            loop {
                if cancelled_thread.load(Ordering::Acquire) {
                    break;
                }
                match listener.accept() {
                    Ok((stream, addr)) => {
                        let _ = stream.set_nonblocking(false);
                        if !cancelled_thread.load(Ordering::Acquire) {
                            let _ = tx.send(Ok((stream, addr)));
                        }
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::park_timeout(Duration::from_millis(10));
                    }
                    Err(e) => {
                        if !cancelled_thread.load(Ordering::Acquire) {
                            let _ = tx.send(Err(e));
                        }
                        break;
                    }
                }
            }
            if let Some(done) = done {
                done.store(true, Ordering::Release);
            }
        });
        Self { cancelled, rx, thread: Some(thread) }
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<std::io::Result<(std::net::TcpStream, std::net::SocketAddr)>, std::sync::mpsc::RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }
}

impl Drop for AcceptBarrier {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

/// Two-way async test barrier: a worker signals arrival and waits until the test releases it,
/// while the test awaits arrival without fixed sleeps.
#[derive(Clone, Default)]
pub struct Barrier {
    entered: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
    on_enter: Arc<Notify>,
    on_release: Arc<Notify>,
}

impl Barrier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record arrival and wake any waiter in [`Self::wait_entered`].
    pub fn mark_entered(&self) {
        self.entered.store(true, Ordering::Release);
        self.on_enter.notify_waiters();
    }

    /// Mark arrival and suspend until [`Self::release`] is called.
    pub async fn enter_and_wait(&self) {
        let notified = self.on_release.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        self.mark_entered();
        if self.released.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }

    /// Wait until [`Self::mark_entered`] or [`Self::enter_and_wait`] has been reached, panicking on timeout.
    pub async fn wait_entered(&self, timeout: Duration, label: &str) {
        let notified = self.on_enter.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.entered.load(Ordering::Acquire) {
            return;
        }
        tokio::time::timeout(timeout, notified)
            .await
            .unwrap_or_else(|_| panic!("barrier timed out waiting for `{label}` after {timeout:?}"));
    }

    /// Release any waiter parked in [`Self::enter_and_wait`] (safe to call before arrival).
    pub fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.on_release.notify_waiters();
    }

    /// Underlying release `Notify` for integration with [`super::MockServer::set_hold`] / [`super::Resp::hold`].
    pub fn release_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.on_release)
    }

    pub fn is_entered(&self) -> bool {
        self.entered.load(Ordering::Acquire)
    }

    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::{AcceptBarrier, Barrier};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn barrier_synchronizes_entry_and_release() {
        let b = Barrier::new();
        let worker_b = b.clone();
        let finished = Arc::new(AtomicBool::new(false));
        let finished_w = Arc::clone(&finished);
        let handle = tokio::spawn(async move {
            worker_b.enter_and_wait().await;
            finished_w.store(true, Ordering::Release);
        });
        b.wait_entered(Duration::from_secs(2), "worker entry").await;
        assert!(b.is_entered());
        assert!(!finished.load(Ordering::Acquire));
        b.release();
        handle.await.unwrap();
        assert!(b.is_released());
        assert!(finished.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn barrier_release_before_enter_does_not_hang() {
        let b = Barrier::new();
        b.release();
        tokio::time::timeout(Duration::from_secs(1), b.enter_and_wait()).await.unwrap();
        assert!(b.is_entered() && b.is_released());
    }

    #[test]
    fn accept_barrier_drop_wakes_and_joins_listener_thread() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let barrier = AcceptBarrier::spawn_with_done(listener, Some(Arc::clone(&done)));
        drop(barrier);
        assert!(done.load(Ordering::Acquire), "AcceptBarrier drop must join its background thread");
    }
}
