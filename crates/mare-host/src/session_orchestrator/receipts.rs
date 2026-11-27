use std::collections::{HashMap, VecDeque};
use std::rc::Weak;

use serde_json::Value as J;
use tokio::sync::watch;

use super::{OrchestratorError, SESSION_OP_RECEIPT_CAPACITY, SessionOrchestrator};

pub(super) type ReceiptKey = (String, String, String);
pub(super) type ReceiptOutcome = Result<(String, J), OrchestratorError>;

#[derive(Clone)]
pub(super) struct InFlightReceipt {
    pub(super) fingerprint: J,
    pub(super) done: watch::Receiver<Option<ReceiptOutcome>>,
}

pub(super) struct InFlightReceiptGuard {
    orch: Weak<SessionOrchestrator>,
    key: Option<ReceiptKey>,
    tx: Option<watch::Sender<Option<ReceiptOutcome>>>,
}

impl InFlightReceiptGuard {
    pub(super) fn new(
        orch: Weak<SessionOrchestrator>,
        key: ReceiptKey,
        tx: watch::Sender<Option<ReceiptOutcome>>,
    ) -> Self {
        Self { orch, key: Some(key), tx: Some(tx) }
    }

    pub(super) fn finish(&mut self, outcome: ReceiptOutcome) {
        if let Some(key) = self.key.take()
            && let Some(orch) = self.orch.upgrade()
        {
            orch.in_flight_receipts.borrow_mut().remove(&key);
        }
        if let Some(tx) = self.tx.take() {
            tx.send_replace(Some(outcome));
        }
    }
}

impl Drop for InFlightReceiptGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key.take()
            && let Some(orch) = self.orch.upgrade()
        {
            orch.in_flight_receipts.borrow_mut().remove(&key);
        }
        // Dropping `self.tx` without sending signals waiters that the owner future was cancelled.
    }
}

pub(super) enum ReceiptSlot {
    Replayed(J),
    Reserved(Option<InFlightReceiptGuard>),
}

#[derive(Clone, Debug)]
struct ReceiptEntry {
    key: ReceiptKey,
    fingerprint: J,
    session_id: String,
    snapshot: J,
}

/// Bounded LRU cache of `session.create` / `session.fork` `request_id` receipts.
pub(super) struct SessionOpReceipts {
    cap: usize,
    entries: VecDeque<ReceiptEntry>,
}

impl SessionOpReceipts {
    pub(super) fn new(cap: usize) -> Self {
        Self { cap: cap.max(1), entries: VecDeque::with_capacity(cap.min(SESSION_OP_RECEIPT_CAPACITY)) }
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn get(&mut self, scope: &str, op: &str, req_id: &str) -> Option<(J, String, J)> {
        let pos = self.entries.iter().position(|e| e.key.0 == scope && e.key.1 == op && e.key.2 == req_id)?;
        let entry = self.entries.remove(pos)?;
        let out = (entry.fingerprint.clone(), entry.session_id.clone(), entry.snapshot.clone());
        self.entries.push_back(entry);
        Some(out)
    }

    pub(super) fn put(
        &mut self,
        scope: String,
        op: String,
        req_id: String,
        fingerprint: J,
        session_id: String,
        snapshot: J,
    ) {
        if let Some(pos) = self.entries.iter().position(|e| e.key.0 == scope && e.key.1 == op && e.key.2 == req_id) {
            self.entries.remove(pos);
        } else if self.entries.len() >= self.cap {
            self.entries.pop_front();
        }
        self.entries.push_back(ReceiptEntry { key: (scope, op, req_id), fingerprint, session_id, snapshot });
    }

    pub(super) fn remove_session(&mut self, sid: &str) {
        self.entries.retain(|e| e.session_id != sid);
    }
}

pub(super) fn reserve_in_flight(
    in_flight: &mut HashMap<ReceiptKey, InFlightReceipt>,
    orch: Weak<SessionOrchestrator>,
    key: ReceiptKey,
    fp: &J,
) -> Result<Result<InFlightReceiptGuard, watch::Receiver<Option<ReceiptOutcome>>>, OrchestratorError> {
    if let Some(entry) = in_flight.get(&key) {
        if &entry.fingerprint != fp {
            return Err(OrchestratorError::request_id_conflict("request_id was reused with different parameters"));
        }
        return Ok(Err(entry.done.clone()));
    }
    let (tx, rx) = watch::channel(None);
    in_flight.insert(key.clone(), InFlightReceipt { fingerprint: fp.clone(), done: rx });
    Ok(Ok(InFlightReceiptGuard::new(orch, key, tx)))
}
