//! Stream cursor subscription, reading, lag calculation, and headless delivery handles.

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::{Notify, watch};

use super::log::{EventLogState, collect_retained_locked, current_epoch_and_tail, flush_retained_locked};
use super::{
    BaselineAnchor, ConsumerFilter, CursorRecord, EventLog, HeadlessEvent, HeadlessFilter, ReliableFilter,
    SubscribeCursorOutcome,
};

pub(super) struct ConsumerSlot {
    pub(super) id: u64,
    pub(super) fixed: bool,
    pub(super) next_offset: u64,
    pub(super) last_epoch: String,
    pub(super) last_seq: u64,
    pub(super) decisions: VecDeque<(u64, Option<bool>)>,
    pub(super) dropped: VecDeque<DropBatch>,
    pub(super) alive: bool,
    pub(super) error: Option<String>,
    pub(super) failure_tx: watch::Sender<Option<String>>,
    pub(super) filter: ConsumerFilter,
    pub(super) wake: Arc<Notify>,
}

pub(super) struct DropBatch {
    at_offset: u64,
    count: u64,
    session_id: Option<String>,
    session_epoch: String,
}

pub(super) struct SubscribePosition {
    pub(super) mode: mare_protocol::SubscribeMode,
    pub(super) session_epoch: String,
    pub(super) next_seq: u64,
    pub(super) next_offset: u64,
    pub(super) start_last_seq: u64,
}

pub(super) fn resolve_subscribe_position(
    st: &EventLogState,
    requested_epoch: Option<&str>,
    last_seq: Option<u64>,
    live_only: bool,
) -> SubscribePosition {
    let (session_epoch, tail_seq) = current_epoch_and_tail(st);
    let (mode, next_seq, next_offset, start_last_seq) = if live_only {
        (mare_protocol::SubscribeMode::Live, tail_seq + 1, st.tail_offset, tail_seq)
    } else if requested_epoch == Some(session_epoch.as_str()) && last_seq == Some(tail_seq) {
        (mare_protocol::SubscribeMode::Live, tail_seq + 1, st.tail_offset, tail_seq)
    } else if requested_epoch == Some(session_epoch.as_str())
        && let Some(ls) = last_seq
        && ls < tail_seq
    {
        let first_for_epoch = st.entries.iter().find(|e| e.session_epoch == session_epoch);
        if let Some(first) = first_for_epoch
            && first.seq <= ls + 1
            && let Some(replay_entry) = st.entries.iter().find(|e| e.session_epoch == session_epoch && e.seq == ls + 1)
        {
            (mare_protocol::SubscribeMode::Replay, ls + 1, replay_entry.offset, ls)
        } else {
            (mare_protocol::SubscribeMode::Baseline, tail_seq + 1, st.tail_offset, tail_seq)
        }
    } else {
        // Missing `last_seq`, mismatched `session_epoch`, or `last_seq > tail` -> baseline.
        (mare_protocol::SubscribeMode::Baseline, tail_seq + 1, st.tail_offset, tail_seq)
    };
    SubscribePosition { mode, session_epoch, next_seq, next_offset, start_last_seq }
}

impl ConsumerSlot {
    pub(super) fn fail(&mut self, reason: String) {
        if self.alive {
            self.alive = false;
            self.error = Some(reason.clone());
            self.failure_tx.send_replace(Some(reason));
            self.wake.notify_waiters();
        }
    }

    pub(super) fn record_drop(&mut self, at_offset: u64, identity: &super::log::EventIdentity) {
        if let Some(batch) = self.dropped.back_mut().filter(|batch| batch.session_epoch == identity.session_epoch) {
            batch.count = batch.count.saturating_add(1);
        } else {
            self.dropped.push_back(DropBatch {
                at_offset,
                count: 1,
                session_id: identity.session_id.clone(),
                session_epoch: identity.session_epoch.clone(),
            });
        }
        if !self.fixed {
            self.wake.notify_waiters();
        }
    }

    fn take_pending_drop(&mut self) -> Option<CursorRecord> {
        if self.dropped.front().is_some_and(|batch| self.next_offset >= batch.at_offset) {
            let batch = self.dropped.pop_front().expect("checked above");
            Some(CursorRecord::Dropped {
                count: batch.count,
                session_id: batch.session_id,
                session_epoch: batch.session_epoch,
            })
        } else {
            None
        }
    }
}

impl EventLog {
    /// Open an ordinary (`fixed = false`) cursor starting at the current log tail.
    pub fn open_cursor(&self, wants: ReliableFilter) -> EventCursor {
        self.open_cursor_with_wake(wants, None)
    }

    /// Open a cursor with a caller-supplied [`Notify`] (shared across a connection's cursors).
    pub fn open_cursor_with_wake(&self, wants: ReliableFilter, wake: Option<Arc<Notify>>) -> EventCursor {
        let (cursor, _) = self.register_cursor(false, ConsumerFilter::Reliable(wants), wake);
        cursor
    }

    /// Subscribe from `requested_epoch` / `last_seq` per ``.
    /// When `live_only` is true, starts at the current tail in `Live` mode without `lagged`.
    pub fn subscribe_cursor(
        &self,
        requested_epoch: Option<&str>,
        last_seq: Option<u64>,
        live_only: bool,
        wants: ReliableFilter,
        wake: Option<Arc<Notify>>,
    ) -> SubscribeCursorOutcome {
        self.subscribe_cursor_with_retained(requested_epoch, last_seq, live_only, wants, wake, &|_| false).0
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn subscribe_cursor_with_retained(
        &self,
        requested_epoch: Option<&str>,
        last_seq: Option<u64>,
        live_only: bool,
        wants: ReliableFilter,
        wake: Option<Arc<Notify>>,
        retain_filter: &dyn Fn(&str) -> bool,
    ) -> (SubscribeCursorOutcome, Option<String>, std::collections::BTreeMap<String, serde_json::Value>) {
        let mut st = self.lock();
        let pos = resolve_subscribe_position(&st, requested_epoch, last_seq, live_only);
        let snapshot_session_id = st.session_id.clone();
        let retained = collect_retained_locked(&mut st, retain_filter);

        let (cursor, _) = self.push_consumer_locked(
            &mut st,
            false,
            pos.next_offset,
            pos.session_epoch.clone(),
            pos.start_last_seq,
            ConsumerFilter::Reliable(wants),
            wake,
        );
        drop(st);
        (
            SubscribeCursorOutcome { mode: pos.mode, session_epoch: pos.session_epoch, seq: pos.next_seq, cursor },
            snapshot_session_id,
            retained,
        )
    }

    pub(crate) fn reanchor_cursor_at_tail(
        &self,
        cursor_id: u64,
        retain_filter: &dyn Fn(&str) -> bool,
    ) -> Option<BaselineAnchor> {
        let mut st = self.lock();
        let tail_offset = st.tail_offset;
        let session_id = st.session_id.clone();
        let session_epoch = st.session_epoch.clone();
        let tail_seq = st.next_seq().saturating_sub(1);
        let idx = st.consumers.iter().position(|c| c.id == cursor_id && c.alive)?;
        let retained = collect_retained_locked(&mut st, retain_filter);
        let c = &mut st.consumers[idx];
        c.next_offset = tail_offset;
        c.last_epoch = session_epoch.clone();
        c.last_seq = tail_seq;
        c.decisions.clear();
        Some(BaselineAnchor { session_id, session_epoch, seq: tail_seq, retained })
    }

    /// Open a fixed-cursor (`fixed = true`) headless stream starting at the current log tail.
    pub fn open_headless_cursor(&self, wants: HeadlessFilter) -> (HeadlessTapHandle, HeadlessCursor) {
        let (cursor, failure) = self.register_cursor(true, ConsumerFilter::Headless(wants), None);
        let id = cursor.id;
        let handle = HeadlessTapHandle { failure, log: self.clone(), consumer_id: id };
        let hcursor = HeadlessCursor { inner: cursor };
        (handle, hcursor)
    }

    #[allow(clippy::too_many_arguments)]
    fn push_consumer_locked(
        &self,
        st: &mut EventLogState,
        fixed: bool,
        next_offset: u64,
        last_epoch: String,
        last_seq: u64,
        filter: ConsumerFilter,
        wake: Option<Arc<Notify>>,
    ) -> (EventCursor, watch::Receiver<Option<String>>) {
        let id = st.next_consumer_id;
        st.next_consumer_id += 1;
        let (failure_tx, failure_rx) = watch::channel(None);
        let wake = wake.unwrap_or_else(|| Arc::new(Notify::new()));
        st.consumers.push(ConsumerSlot {
            id,
            fixed,
            next_offset,
            last_epoch,
            last_seq,
            decisions: VecDeque::new(),
            dropped: VecDeque::new(),
            alive: true,
            error: None,
            failure_tx,
            filter,
            wake: wake.clone(),
        });
        let cursor = EventCursor { log: self.clone(), id, wake, failure: failure_rx.clone(), active: true };
        (cursor, failure_rx)
    }

    fn register_cursor(
        &self,
        fixed: bool,
        filter: ConsumerFilter,
        wake: Option<Arc<Notify>>,
    ) -> (EventCursor, watch::Receiver<Option<String>>) {
        let mut st = self.lock();
        let next_offset = st.tail_offset;
        let (last_epoch, last_seq) = current_epoch_and_tail(&st);
        self.push_consumer_locked(&mut st, fixed, next_offset, last_epoch, last_seq, filter, wake)
    }

    fn flush_retained_and_wait(&self, consumer_id: u64) -> Result<(), String> {
        let mut st = self.lock();
        flush_retained_locked(&mut st, None);
        if let Some(c) = st.consumers.iter().find(|c| c.id == consumer_id) {
            if let Some(err) = &c.error {
                return Err(err.clone());
            }
            c.wake.notify_waiters();
        }
        Ok(())
    }

    fn next_for_cursor(&self, consumer_id: u64) -> Result<Option<CursorRecord>, String> {
        let mut st = self.lock();
        flush_retained_locked(&mut st, None);
        let head_offset = st.entries.front().map_or(st.tail_offset, |e| e.offset);

        let idx = match st.consumers.iter().position(|c| c.id == consumer_id) {
            Some(i) => i,
            None => return Err("consumer closed".to_string()),
        };

        if !st.consumers[idx].alive {
            let err = st.consumers[idx].error.clone().unwrap_or_else(|| "consumer closed".to_string());
            return Err(err);
        }

        let head_epoch_seq = st.entries.front().map(|e| (e.session_epoch.clone(), e.seq));

        // Ordinary cursor fell behind the evicted ring head -> emit `Lagged { from_seq, to_seq }`.
        if !st.consumers[idx].fixed && st.consumers[idx].next_offset < head_offset {
            let (from_seq, to_seq, new_epoch, new_last_seq) =
                compute_lag_range(&st, &st.consumers[idx], head_epoch_seq);
            let c = &mut st.consumers[idx];
            while let Some((off, _)) = c.decisions.front() {
                if *off < head_offset {
                    c.decisions.pop_front();
                } else {
                    break;
                }
            }
            c.next_offset = head_offset;
            c.last_epoch = new_epoch;
            c.last_seq = new_last_seq;
            if from_seq <= to_seq {
                return Ok(Some(CursorRecord::Lagged { from_seq, to_seq }));
            }
        }

        if let Some(dropped) = st.consumers[idx].take_pending_drop() {
            return Ok(Some(dropped));
        }

        let start_offset = st.consumers[idx].next_offset;
        if start_offset >= st.tail_offset {
            return Ok(None);
        }
        let mut scan = (start_offset - head_offset) as usize;
        let mut advanced_fixed = false;
        while scan < st.entries.len() {
            let entry_offset = st.entries[scan].offset;
            let entry_name = st.entries[scan].name.clone();
            let entry_epoch = st.entries[scan].session_epoch.clone();
            let entry_seq = st.entries[scan].seq;

            let decision = {
                let c = &mut st.consumers[idx];
                let d = pop_decision(c, entry_offset).or_else(|| c.filter.select(&entry_name).map(|(s, _)| s));
                c.next_offset = entry_offset + 1;
                c.last_epoch = entry_epoch;
                c.last_seq = entry_seq;
                if c.fixed {
                    advanced_fixed = true;
                }
                d
            };

            if let Some(selected) = decision {
                let entry = &st.entries[scan];
                let entry_session_id = entry.session_id.clone();
                let entry_session_epoch = entry.session_epoch.clone();
                let entry_turn_id = entry.turn_id.clone();
                let entry_ignorable = entry.ignorable;
                let wire_json = Arc::clone(&entry.wire_json);
                if advanced_fixed {
                    flush_retained_locked(&mut st, None);
                }
                drop(st);
                if advanced_fixed {
                    self.inner.space_wake.notify_waiters();
                }
                let tap_event = super::log::LogEntry::decode_tap_event(
                    entry_name,
                    entry_session_id,
                    entry_session_epoch,
                    entry_seq,
                    entry_turn_id,
                    entry_ignorable,
                    wire_json,
                );
                return Ok(Some(CursorRecord::Event { event: tap_event, selected }));
            }
            if let Some(dropped) = st.consumers[idx].take_pending_drop() {
                if advanced_fixed {
                    flush_retained_locked(&mut st, None);
                    self.inner.space_wake.notify_waiters();
                }
                return Ok(Some(dropped));
            }
            scan += 1;
        }

        if advanced_fixed {
            flush_retained_locked(&mut st, None);
            self.inner.space_wake.notify_waiters();
        }
        Ok(st.consumers[idx].take_pending_drop())
    }

    fn clear_cursor_decisions(&self, consumer_id: u64) {
        let mut st = self.lock();
        if let Some(c) = st.consumers.iter_mut().find(|c| c.id == consumer_id) {
            c.decisions.clear();
        }
    }

    pub(crate) fn clear_all_cursor_decisions(&self) {
        let mut st = self.lock();
        for c in &mut st.consumers {
            c.decisions.clear();
        }
    }

    fn close_cursor(&self, consumer_id: u64) {
        let mut st = self.lock();
        if let Some(pos) = st.consumers.iter().position(|c| c.id == consumer_id) {
            let was_fixed = st.consumers[pos].fixed;
            st.consumers.remove(pos);
            if was_fixed {
                flush_retained_locked(&mut st, None);
                self.inner.space_wake.notify_waiters();
            }
        }
    }
}

fn pop_decision(c: &mut ConsumerSlot, offset: u64) -> Option<bool> {
    while let Some((off, dec)) = c.decisions.front().copied() {
        if off < offset {
            c.decisions.pop_front();
        } else if off == offset {
            c.decisions.pop_front();
            return dec;
        } else {
            break;
        }
    }
    None
}

fn compute_lag_range(
    st: &EventLogState,
    c: &ConsumerSlot,
    head_epoch_seq: Option<(String, u64)>,
) -> (u64, u64, String, u64) {
    match head_epoch_seq {
        Some((head_epoch, head_seq)) => {
            if c.last_epoch == head_epoch {
                let from = c.last_seq.saturating_add(1);
                let to = head_seq.saturating_sub(1);
                (from, to, head_epoch, to)
            } else if head_seq > 1 {
                let to = head_seq.saturating_sub(1);
                (1, to, head_epoch, to)
            } else {
                (1, 0, head_epoch, 0)
            }
        }
        None => (1, 0, st.session_epoch.clone(), st.next_seq().saturating_sub(1)),
    }
}

/// Cursor reading events, `Lagged`, and `Dropped` notifications from the shared `EventLog`.
pub struct EventCursor {
    log: EventLog,
    id: u64,
    wake: Arc<Notify>,
    failure: watch::Receiver<Option<String>>,
    active: bool,
}

impl EventCursor {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn failure(&self) -> watch::Receiver<Option<String>> {
        self.failure.clone()
    }

    pub fn clear_decisions(&self) {
        if self.active {
            self.log.clear_cursor_decisions(self.id);
        }
    }

    pub fn close(&mut self) {
        if self.active {
            self.active = false;
            self.log.close_cursor(self.id);
        }
    }

    pub fn try_recv(&mut self) -> Result<Option<CursorRecord>, String> {
        if !self.active {
            return Ok(None);
        }
        self.log.next_for_cursor(self.id)
    }

    pub async fn recv(&mut self) -> Option<CursorRecord> {
        loop {
            if !self.active {
                return None;
            }
            let notified = self.wake.notified();
            match self.log.next_for_cursor(self.id) {
                Ok(Some(rec)) => return Some(rec),
                Ok(None) => {}
                Err(_) => return None,
            }
            notified.await;
        }
    }
}

impl Drop for EventCursor {
    fn drop(&mut self) {
        if self.active {
            self.log.close_cursor(self.id);
        }
    }
}

/// Fixed-cursor headless consumer reading `HeadlessEvent` directly from the shared `EventLog`.
pub struct HeadlessCursor {
    inner: EventCursor,
}

impl HeadlessCursor {
    pub fn close(&mut self) {
        self.inner.close();
    }

    pub fn try_recv(&mut self) -> Option<HeadlessEvent> {
        self.inner.try_recv().ok().flatten().map(HeadlessEvent::from)
    }

    pub async fn recv(&mut self) -> Option<HeadlessEvent> {
        self.inner.recv().await.map(HeadlessEvent::from)
    }
}

/// Handle for a headless stream cursor: failure watch and end-of-turn flush.
#[derive(Clone)]
pub struct HeadlessTapHandle {
    pub failure: watch::Receiver<Option<String>>,
    log: EventLog,
    consumer_id: u64,
}

impl HeadlessTapHandle {
    pub fn failed(&self) -> Option<String> {
        self.failure.borrow().clone()
    }

    pub async fn flush_lagged(&self) -> Result<(), String> {
        self.log.flush_retained_and_wait(self.consumer_id)
    }
}
