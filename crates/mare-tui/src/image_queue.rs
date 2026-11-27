//! The image preprocessing queue: decoding, scaling and encoding run on
//! at most `workers` threads at once, deduplicated per picture; requests beyond that wait in
//! order and are never dropped.
//!
//! Deduplicates queued / in-flight work by image, publishes a repaint when a
//! picture is ready, and lets the draw path skip a picture that isn't ready
//! instead of decoding on the UI thread. Nothing is coalesced away: every distinct
//! request stays in the queue until a worker has prepared it.
//!
//! The queue knows nothing about images: [`Queue::new`] takes the work and a `done` hook that runs
//! on the worker before waiters wake (the image module stores the result in its encode cache
//! there and wakes the UI loop, see `image::ready`).

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, Condvar, Mutex};

/// The work for one key, and the hook run with its result before anyone waiting is woken.
type Work<K, V> = Arc<dyn Fn(&K) -> V + Send + Sync>;
type DoneHook<K, V> = Arc<dyn Fn(&K, &V) + Send + Sync>;

/// One request: queued, being prepared, or finished. Everyone asking for the same key while it's
/// pending holds the same job.
pub struct Job<V> {
    done: Mutex<Option<V>>,
    cv: Condvar,
}

impl<V: Clone> Job<V> {
    /// The result, if it's ready (never blocks).
    pub fn get(&self) -> Option<V> {
        self.done.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Block until the result is ready.
    pub fn wait(&self) -> V {
        let mut d = self.done.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(r) = d.as_ref() {
                return r.clone();
            }
            d = self.cv.wait(d).unwrap_or_else(|e| e.into_inner());
        }
    }
}

struct State<K, V> {
    /// queued or running, by key: a second request joins the first
    jobs: HashMap<K, Arc<Job<V>>>,
    /// not started yet, oldest first
    waiting: VecDeque<K>,
    /// worker threads alive
    running: usize,
}

struct Shared<K, V> {
    state: Mutex<State<K, V>>,
    workers: usize,
    work: Work<K, V>,
    done: DoneHook<K, V>,
}

/// A bounded, deduplicating work queue. Cheap to clone (shared state).
pub struct Queue<K, V> {
    shared: Arc<Shared<K, V>>,
}

impl<K, V> Clone for Queue<K, V> {
    fn clone(&self) -> Self {
        Self { shared: self.shared.clone() }
    }
}

impl<K, V> Queue<K, V>
where
    K: Hash + Eq + Clone + Send + 'static,
    V: Clone + Send + 'static,
{
    /// `work` must not panic (the image module catches panics itself); `done` runs on the
    /// worker with every result, before the job leaves the queue and its waiters wake.
    pub fn new(
        workers: usize,
        work: impl Fn(&K) -> V + Send + Sync + 'static,
        done: impl Fn(&K, &V) + Send + Sync + 'static,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State { jobs: HashMap::new(), waiting: VecDeque::new(), running: 0 }),
                workers: workers.max(1),
                work: Arc::new(work),
                done: Arc::new(done),
            }),
        }
    }

    /// Queue `key` (or join the job already queued or running for it). Returns at once unless no
    /// worker thread can be started, in which case the queue is drained on this thread.
    pub fn submit(&self, key: K) -> Arc<Job<V>> {
        let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(j) = st.jobs.get(&key) {
            return j.clone();
        }
        let job = Arc::new(Job { done: Mutex::new(None), cv: Condvar::new() });
        st.jobs.insert(key.clone(), job.clone());
        st.waiting.push_back(key);
        if st.running < self.shared.workers {
            st.running += 1;
            drop(st);
            let shared = self.shared.clone();
            let spawned = std::thread::Builder::new().name("mare-image".into()).spawn(move || run(&shared));
            if spawned.is_err() {
                // no thread: drain the queue here rather than leave it waiting forever
                run(&self.shared);
            }
        }
        job
    }

    /// Requests queued or running (tests, `/profile`).
    pub fn pending(&self) -> usize {
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner()).jobs.len()
    }
}

/// One worker: take the oldest waiting key until none is left.
fn run<K: Hash + Eq + Clone, V: Clone>(shared: &Shared<K, V>) {
    loop {
        let key = {
            let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            match st.waiting.pop_front() {
                Some(k) => k,
                None => {
                    st.running -= 1;
                    return;
                }
            }
        };
        let res = (shared.work)(&key);
        // the result is stored (the hook) before the job leaves the map: a request in between
        // finds either the job or the stored result, never neither
        (shared.done)(&key, &res);
        let job = shared.state.lock().unwrap_or_else(|e| e.into_inner()).jobs.remove(&key);
        if let Some(job) = job {
            *job.done.lock().unwrap_or_else(|e| e.into_inner()) = Some(res);
            job.cv.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::mpsc;
    use std::time::Duration;

    /// Generous ceiling for conditions that hold almost at once.
    const WAIT: Duration = Duration::from_secs(60);

    /// Work that blocks until the test opens the gate; counts runs and the peak concurrency.
    struct Gated {
        open: Mutex<bool>,
        cv: Condvar,
        active: AtomicUsize,
        peak: AtomicUsize,
        runs: Mutex<Vec<u32>>,
    }

    impl Gated {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                open: Mutex::new(false),
                cv: Condvar::new(),
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                runs: Mutex::new(vec![]),
            })
        }
        fn run(&self, k: u32) -> u32 {
            let n = self.active.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(n, SeqCst);
            self.runs.lock().unwrap().push(k);
            let g = self.open.lock().unwrap();
            let _g = self.cv.wait_while(g, |open| !*open).unwrap();
            self.active.fetch_sub(1, SeqCst);
            k * 10
        }
        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.cv.notify_all();
        }
        /// Wait (bounded) until `n` jobs are running at once and have each recorded their run:
        /// `run` bumps `active` before it pushes to `runs`, so `active` alone doesn't say that
        /// `runs` is up to date.
        fn until_active(&self, n: usize) {
            let end = std::time::Instant::now() + WAIT;
            while self.active.load(SeqCst) < n || self.runs.lock().unwrap().len() < n {
                assert!(std::time::Instant::now() < end, "never {n} active");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn queue(g: &Arc<Gated>, tx: mpsc::Sender<(u32, u32)>) -> Queue<u32, u32> {
        let g = g.clone();
        let tx = Mutex::new(tx);
        Queue::new(2, move |k| g.run(*k), move |k, v| tx.lock().unwrap().send((*k, *v)).unwrap())
    }

    /// A burst of N > 2 pictures: never more than 2 prepared at once, all of them prepared, in
    /// the order asked for, and each result delivered once through the `done` hook.
    #[test]
    fn a_burst_runs_two_at_a_time_and_drops_nothing() {
        let g = Gated::new();
        let (tx, rx) = mpsc::channel();
        let q = queue(&g, tx);
        let jobs: Vec<_> = (0..9).map(|k| q.submit(k)).collect();
        g.until_active(2);
        // the rest wait: queued, not dropped, not running
        assert_eq!(q.pending(), 9);
        assert_eq!(g.runs.lock().unwrap().len(), 2);
        assert!(jobs.iter().all(|j| j.get().is_none()));
        g.open();
        for (k, j) in jobs.iter().enumerate() {
            assert_eq!(j.wait(), k as u32 * 10);
        }
        let mut delivered: Vec<_> = (0..9).map(|_| rx.recv_timeout(WAIT).unwrap()).collect();
        delivered.sort();
        assert_eq!(delivered, (0..9).map(|k| (k, k * 10)).collect::<Vec<_>>());
        assert_eq!(g.peak.load(SeqCst), 2);
        let mut runs = g.runs.lock().unwrap().clone();
        // two workers take from the front: the order may swap between neighbours only
        runs.sort();
        assert_eq!(runs, (0..9).collect::<Vec<_>>());
        assert_eq!(q.pending(), 0);
    }

    /// A second request for a picture that is running, or still queued, joins the same job.
    #[test]
    fn requests_for_the_same_picture_share_one_job() {
        let g = Gated::new();
        let (tx, rx) = mpsc::channel();
        let q = queue(&g, tx);
        let a = q.submit(1);
        let b = q.submit(2);
        g.until_active(2);
        // 3 is queued behind the two running ones
        let c = q.submit(3);
        assert!(Arc::ptr_eq(&a, &q.submit(1)), "running: joined");
        assert!(Arc::ptr_eq(&c, &q.submit(3)), "queued: joined");
        assert_eq!(q.pending(), 3);
        g.open();
        assert_eq!((a.wait(), b.wait(), c.wait()), (10, 20, 30));
        for _ in 0..3 {
            rx.recv_timeout(WAIT).unwrap();
        }
        assert_eq!(g.runs.lock().unwrap().len(), 3, "each picture prepared once");
        // finished: a new request is new work (the caller's cache answers before that)
        assert!(!Arc::ptr_eq(&a, &q.submit(1)));
    }

    /// The `done` hook sees the result before the job leaves the queue, so a UI that looks in its
    /// cache when woken always finds it.
    #[test]
    fn results_are_stored_before_waiters_wake() {
        let store: Arc<Mutex<HashMap<u32, u32>>> = Default::default();
        let (tx, rx) = mpsc::channel::<u32>();
        let tx = Mutex::new(tx);
        let s = store.clone();
        let q = Queue::new(
            2,
            |k: &u32| k + 1,
            move |k, v| {
                s.lock().unwrap().insert(*k, *v);
                tx.lock().unwrap().send(*k).unwrap();
            },
        );
        for k in 0..5 {
            q.submit(k);
        }
        for _ in 0..5 {
            let k = rx.recv_timeout(WAIT).unwrap();
            assert_eq!(store.lock().unwrap().get(&k), Some(&(k + 1)), "stored when woken");
        }
    }
}
