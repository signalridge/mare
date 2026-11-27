//! Test-only observation of callback waits: every `call_result_tagged` wait records
//! its method, the waker of its latest poll and each poll's (clock time, finished?). A test can
//! wake a wait itself and know it was driven at a given clock time — a scheduling barrier that
//! `yield_now` loops cannot give (Tokio does not promise which task runs after a yield).

use std::cell::RefCell;
use std::future::Future;
use std::task::Waker;

use tokio::time::Instant;

pub(super) struct Wait {
    pub(super) method: String,
    pub(super) waker: Option<Waker>,
    pub(super) polls: Vec<(Instant, bool)>,
}

thread_local! {
    pub(super) static WAITS: RefCell<Vec<Wait>> = const { RefCell::new(Vec::new()) };
}

pub(super) fn observe<F: Future>(method: &str, fut: F) -> impl Future<Output = F::Output> {
    let i = WAITS.with_borrow_mut(|w| {
        w.push(Wait { method: method.into(), waker: None, polls: Vec::new() });
        w.len() - 1
    });
    let mut fut = Box::pin(fut);
    std::future::poll_fn(move |cx| {
        let r = fut.as_mut().poll(cx);
        WAITS.with_borrow_mut(|w| {
            w[i].waker = Some(cx.waker().clone());
            w[i].polls.push((Instant::now(), r.is_ready()));
        });
        r
    })
}
