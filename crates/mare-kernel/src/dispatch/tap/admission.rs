//! Event admission into the bounded session log (lossy coalescing & required backpressure).

use mare_protocol::event_registry::{self, EventDelivery};
use serde_json::Value;

use super::EventLog;
use super::bounds::encode_bounded_wire_event;
use super::log::{
    CommitOutcome, EventIdentity, PendingRetained, ProjectedPayload, clear_retained_locked, clear_rules_in_map,
    commit_entry_locked, evict_unpinned, fail_next_blocking_fixed_consumer, flush_retained_locked, has_room,
    is_external_in_map, is_process_in_map, project_payload_with, record_drop_locked, record_retained_locked,
};
use crate::dispatch::contract_events::EventRetain;

struct LossyMetadata {
    retain_key: Option<Option<String>>,
    ignorable: bool,
    admit_gen: u64,
}

impl EventLog {
    pub(super) fn record_refusal_drop(&self, event: &str, identity: &EventIdentity) {
        let mut st = self.lock();
        record_drop_locked(&mut st, event, identity);
    }

    /// Admit a JSON event into the session log, projecting through the VM wire projector if active.
    ///
    /// Oversized or unencodable events never enter the log and take no `seq`: instead every
    /// matching external stream consumer receives `Dropped { count }`, and `Err` is returned only
    /// on JSON serialization failure so the kernel can record a diagnostic without throwing to
    /// the emitter.
    pub async fn admit_json(&self, event: &str, payload: Value) -> Result<(), String> {
        self.admit_json_scoped(event, payload, None).await
    }

    pub(crate) async fn admit_json_scoped(
        &self,
        event: &str,
        payload: Value,
        scope: Option<mare_protocol::EventScope>,
    ) -> Result<(), String> {
        self.admit_json_scoped_seq(event, payload, scope, None).await.map(|_| ())
    }

    pub(super) async fn admit_json_scoped_seq(
        &self,
        event: &str,
        payload: Value,
        scope: Option<mare_protocol::EventScope>,
        identity: Option<EventIdentity>,
    ) -> Result<Option<u64>, String> {
        let encoded = {
            let (contract_meta, public, clear_rules) = {
                let map = self.inner.contract_events.read().unwrap_or_else(|e| e.into_inner());
                if is_process_in_map(&map, event) {
                    return Ok(None);
                }
                let clear_rules: Vec<_> =
                    clear_rules_in_map(&map, event).map(|(name, key)| (name.to_owned(), key.to_owned())).collect();
                (map.get(event).cloned(), is_external_in_map(&map, event), clear_rules)
            };
            let mut st = self.lock();
            let identity = identity.unwrap_or_else(|| EventIdentity::current(&st));
            let resolved_scope = scope
                .or_else(|| event_registry::scope_of(event))
                .or_else(|| contract_meta.as_ref().map(|m| m.scope))
                .unwrap_or(mare_protocol::EventScope::Session);
            if resolved_scope == mare_protocol::EventScope::Turn
                && payload.get("turn_id").and_then(Value::as_str).is_none_or(|s| s.trim().is_empty())
            {
                record_drop_locked(&mut st, event, &identity);
                return Err(format!("turn-scoped event `{event}` requires non-empty string `turn_id`"));
            }
            let mut filter_wants = false;
            let mut headless_required = false;
            for c in &st.consumers {
                if !c.alive {
                    continue;
                }
                if let Some((_, req_hint)) = c.filter.select(event) {
                    filter_wants = true;
                    if req_hint == Some(true) {
                        headless_required = true;
                    }
                }
            }
            if !public && !filter_wants && clear_rules.is_empty() {
                return Ok(None);
            }
            let def = event_registry::find(event);
            let required = def
                .map(|d| d.delivery == EventDelivery::Required)
                .or_else(|| contract_meta.as_ref().map(|m| m.delivery == EventDelivery::Required))
                .unwrap_or(headless_required);
            let policy = def
                .and_then(|d| d.retain.then_some(EventRetain::Last))
                .or_else(|| contract_meta.as_ref().and_then(|m| m.retain.clone()));
            let key_for = |field: &str| payload.get(field).and_then(|value| serde_json::to_string(value).ok());
            let retain_key = match policy {
                Some(EventRetain::Last) => Some(None),
                Some(EventRetain::Key { key, .. }) => key_for(&key).map(Some),
                None => None,
            };
            st.next_admission_gen += 1;
            let admit_gen = st.next_admission_gen;
            let clear: Vec<_> =
                clear_rules.into_iter().filter_map(|(name, field)| key_for(&field).map(|key| (name, key))).collect();
            if st.session_epoch == identity.session_epoch {
                for (name, key) in clear {
                    clear_retained_locked(&mut st, &name, &key, admit_gen);
                }
            }
            if !public && !filter_wants {
                return Ok(None);
            }
            let ignorable =
                def.map(|d| d.ignorable).or_else(|| contract_meta.as_ref().map(|m| m.ignorable)).unwrap_or(false);
            let max_event_bytes = st.limits.max_event_bytes;
            let projector = st.wire_projector.get();
            drop(st);

            // Project registered values and bound oversized wire payloads outside the EventLog lock.
            let mut projected = None;
            project_payload_with(projector.as_ref(), &payload, &mut projected);
            let cached = projected.as_mut().expect("projection cache initialized");
            let wire_res = encode_bounded_wire_event(
                &identity,
                event,
                identity.next_seq(),
                ignorable,
                &mut cached.value,
                max_event_bytes,
            );
            cached.bounded = true;
            let mut st = self.lock();
            let wire_json = match wire_res {
                Ok(Some(wire_json)) => wire_json,
                Ok(None) => {
                    record_drop_locked(&mut st, event, &identity);
                    return Ok(None);
                }
                Err(err) => {
                    record_drop_locked(&mut st, event, &identity);
                    return Err(err);
                }
            };
            if let Some(key) = &retain_key
                && st.session_epoch == identity.session_epoch
            {
                record_retained_locked(
                    &mut st,
                    event,
                    key,
                    projected.as_ref().expect("projection cache initialized"),
                    admit_gen,
                );
            }
            (identity, required, retain_key, ignorable, admit_gen, wire_json.len(), projected)
        };
        let (identity, required, retain_key, ignorable, admit_gen, encoded_bytes, projected) = encoded;

        if !required {
            return Ok(self.admit_lossy(
                &identity,
                event,
                payload,
                encoded_bytes,
                LossyMetadata { retain_key, ignorable, admit_gen },
                projected,
            ));
        }

        let _req_guard = self.inner.required_gate.lock().await;
        self.admit_required(&identity, event, &payload, ignorable, projected).await
    }

    fn admit_lossy(
        &self,
        identity: &EventIdentity,
        event: &str,
        payload: Value,
        mut encoded_bytes: usize,
        metadata: LossyMetadata,
        mut projected: Option<ProjectedPayload>,
    ) -> Option<u64> {
        let LossyMetadata { retain_key, ignorable, admit_gen } = metadata;
        let mut st = self.lock();
        if let Some(key) = &retain_key
            && st.session_epoch == identity.session_epoch
            && st.retained_order.get(&(event.to_string(), key.clone())).is_some_and(|&prev| admit_gen < prev)
        {
            return None;
        }
        let limit_cap = st.limits.capacity;
        let limit_bytes = st.limits.max_bytes;
        // Yield ring capacity to waiting Required events rather than stealing slots on wake.
        if st.pending_required_waiters == 0 && encoded_bytes <= limit_bytes {
            evict_unpinned(&mut st, encoded_bytes, limit_cap, limit_bytes);
            if has_room(&st, encoded_bytes, limit_cap, limit_bytes) {
                flush_retained_locked(&mut st, Some(encoded_bytes));
                if has_room(&st, encoded_bytes, limit_cap, limit_bytes) {
                    match commit_entry_locked(&mut st, identity, event, &payload, ignorable, None, &mut projected) {
                        CommitOutcome::Committed(seq) => return Some(seq),
                        CommitOutcome::Dropped => return None,
                        CommitOutcome::Full(needed) => encoded_bytes = needed,
                    }
                }
            }
        }
        // Full, pinned, or yielding to Required: never wait and never consume `seq`.
        let session_epoch = identity.session_epoch.clone();
        if let Some(key) = retain_key {
            if let Some(existing) = st
                .pending_retained
                .iter_mut()
                .find(|r| r.name == event && r.session_epoch == session_epoch && r.key == key)
            {
                if existing.admit_gen > admit_gen {
                    return None;
                }
                existing.payload = payload;
                existing.session_epoch = session_epoch;
                existing.ignorable = ignorable;
                existing.encoded_bytes = encoded_bytes;
                existing.projected = projected;
                existing.admit_gen = admit_gen;
            } else {
                st.pending_retained.push(PendingRetained {
                    name: event.to_string(),
                    payload,
                    session_epoch,
                    key,
                    ignorable,
                    encoded_bytes,
                    projected,
                    admit_gen,
                });
            }
        } else {
            record_drop_locked(&mut st, event, identity);
        }
        None
    }

    async fn admit_required(
        &self,
        identity: &EventIdentity,
        event: &str,
        payload: &Value,
        ignorable: bool,
        mut projected: Option<ProjectedPayload>,
    ) -> Result<Option<u64>, String> {
        struct WaiterGuard<'a> {
            log: &'a EventLog,
            active: bool,
        }
        impl Drop for WaiterGuard<'_> {
            fn drop(&mut self) {
                if self.active {
                    let mut st = self.log.lock();
                    st.pending_required_waiters = st.pending_required_waiters.saturating_sub(1);
                }
            }
        }
        let block_limit = self.lock().limits.block_limit;
        let deadline = tokio::time::Instant::now() + block_limit;
        let mut waiter = WaiterGuard { log: self, active: false };
        loop {
            let notified = self.inner.space_wake.notified();
            {
                let mut st = self.lock();
                match commit_entry_locked(&mut st, identity, event, payload, ignorable, None, &mut projected) {
                    CommitOutcome::Committed(seq) => {
                        if waiter.active {
                            st.pending_required_waiters = st.pending_required_waiters.saturating_sub(1);
                            waiter.active = false;
                        }
                        flush_retained_locked(&mut st, None);
                        return Ok(Some(seq));
                    }
                    CommitOutcome::Dropped => {
                        if waiter.active {
                            st.pending_required_waiters = st.pending_required_waiters.saturating_sub(1);
                            waiter.active = false;
                        }
                        flush_retained_locked(&mut st, None);
                        return Ok(None);
                    }
                    CommitOutcome::Full(_) => {}
                }
                if tokio::time::Instant::now() >= deadline {
                    if waiter.active {
                        st.pending_required_waiters = st.pending_required_waiters.saturating_sub(1);
                        waiter.active = false;
                    }
                    // Fail blocking fixed consumers until unpinned eviction frees enough room.
                    let mut committed_seq = None;
                    loop {
                        if !fail_next_blocking_fixed_consumer(&mut st) {
                            record_drop_locked(&mut st, event, identity);
                            break;
                        }
                        match commit_entry_locked(&mut st, identity, event, payload, ignorable, None, &mut projected) {
                            CommitOutcome::Committed(seq) => {
                                flush_retained_locked(&mut st, None);
                                committed_seq = Some(seq);
                                break;
                            }
                            CommitOutcome::Dropped => {
                                flush_retained_locked(&mut st, None);
                                break;
                            }
                            CommitOutcome::Full(_) => {}
                        }
                    }
                    return Ok(committed_seq);
                }
                if !waiter.active {
                    st.pending_required_waiters += 1;
                    waiter.active = true;
                }
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep_until(deadline) => {}
            }
        }
    }
}
