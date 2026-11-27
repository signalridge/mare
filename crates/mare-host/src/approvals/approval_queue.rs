//! Sub-agent approvals waiting behind the one being answered.
//!
//! The parent answers its children's approval requests one at a time, in arrival order; while one
//! dialog is open the others wait here, and every change is announced as
//! `agent/approval-queue { pending = [origin, …] }` (oldest first, the request being answered not
//! included) so a frontend can list them:906-935`
//! (`refresh_pending_thread_approvals`: the threads with outstanding approvals, besides the active
//! one, feed `PendingThreadApprovals`).

use std::cell::{Cell, RefCell};

/// The waiting requests' origins (`{agent_id, id, label?, task?, run_id?, phase?}`) by ticket.
#[derive(Default)]
pub(crate) struct ApprovalQueue {
    waiting: RefCell<Vec<(u64, serde_json::Value)>>,
    seq: Cell<u64>,
}

impl ApprovalQueue {
    /// A request arrived: queue it and return its ticket.
    pub(crate) fn push(&self, origin: &serde_json::Value) -> u64 {
        let id = self.seq.get() + 1;
        self.seq.set(id);
        self.waiting.borrow_mut().push((id, origin.clone()));
        id
    }

    /// Its turn came (or it was dropped): it no longer waits.
    pub(crate) fn remove(&self, id: u64) {
        self.waiting.borrow_mut().retain(|(i, _)| *i != id);
    }

    /// The `agent/approval-queue` payload.
    pub(crate) fn event(&self) -> serde_json::Value {
        let pending: Vec<serde_json::Value> = self.waiting.borrow().iter().map(|(_, o)| o.clone()).collect();
        serde_json::json!({ "pending": pending })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_wait_in_arrival_order_until_their_turn() {
        let q = ApprovalQueue::default();
        let a = q.push(&serde_json::json!({ "label": "a" }));
        let b = q.push(&serde_json::json!({ "label": "b" }));
        assert_eq!(q.event()["pending"], serde_json::json!([{ "label": "a" }, { "label": "b" }]));
        q.remove(a);
        assert_eq!(q.event()["pending"], serde_json::json!([{ "label": "b" }]));
        q.remove(b);
        assert_eq!(q.event()["pending"], serde_json::json!([]));
    }
}
