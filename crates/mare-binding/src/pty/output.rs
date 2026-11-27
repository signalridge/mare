//! Bounded backpressure that also wakes when the owned tree is killed. A retained PTY/job
//! must not leave a producer thread blocked forever on an unread full queue after close.
use super::*;
#[derive(Clone)]
pub(super) struct Output {
    tx: mpsc::Sender<Vec<u8>>,
    closed: Arc<AtomicBool>,
}
pub(super) fn channel() -> (Output, mpsc::Receiver<Vec<u8>>, Arc<AtomicBool>) {
    let (tx, rx) = mpsc::channel(QUEUE_CHUNKS);
    let closed = Arc::new(AtomicBool::new(false));
    (Output { tx, closed: closed.clone() }, rx, closed)
}
impl Output {
    pub(super) fn blocking_send(&self, mut data: Vec<u8>) -> Result<(), ()> {
        loop {
            if self.closed.load(Ordering::Acquire) {
                return Err(());
            }
            match self.tx.try_send(data) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(()),
                Err(mpsc::error::TrySendError::Full(b)) => data = b,
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    pub(super) async fn send(&self, data: Vec<u8>) -> Result<(), ()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(());
        }
        let mut data = data;
        loop {
            if self.closed.load(Ordering::Acquire) {
                return Err(());
            }
            match self.tx.try_send(data) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(()),
                Err(mpsc::error::TrySendError::Full(b)) => data = b,
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn killing_a_full_output_queue_releases_the_producer_without_dropping_the_pty() {
        let (tx, _rx, closed) = channel();
        for _ in 0..QUEUE_CHUNKS {
            tx.blocking_send(vec![0; 8192]).unwrap();
        }
        let (done, wait) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            done.send(tx.blocking_send(vec![0; 8192])).unwrap();
        });
        assert!(wait.recv_timeout(std::time::Duration::from_millis(30)).is_err());
        closed.store(true, Ordering::Release);
        assert!(wait.recv_timeout(std::time::Duration::from_secs(1)).unwrap().is_err());
        worker.join().unwrap();
    }
}
