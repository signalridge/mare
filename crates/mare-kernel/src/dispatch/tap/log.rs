//! In-memory ring buffer storage, wire JSON encoding, epoch rotation, and eviction.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use mare_protocol::EventParams;
use serde_json::Value;
use tokio::sync::Notify;

use super::cursors::ConsumerSlot;
use super::{EventLogLimits, TapEvent};
use crate::dispatch::contract_events::EventRetain;

pub(super) struct LogEntry {
    pub(super) offset: u64,
    pub(super) name: String,
    pub(super) session_id: Option<String>,
    pub(super) session_epoch: String,
    pub(super) seq: u64,
    pub(super) turn_id: Option<String>,
    pub(super) ignorable: bool,
    pub(super) wire_json: Arc<str>,
}

impl LogEntry {
    pub(super) fn encoded_bytes(&self) -> usize {
        self.wire_json.len()
    }

    pub(super) fn decode_tap_event(
        name: String,
        session_id: Option<String>,
        session_epoch: String,
        seq: u64,
        turn_id: Option<String>,
        ignorable: bool,
        wire_json: Arc<str>,
    ) -> TapEvent {
        let payload = serde_json::from_str::<EventParams>(&wire_json).map(|e| e.data).unwrap_or(Value::Null);
        TapEvent { name, payload, session_id, session_epoch, seq, turn_id, ignorable, wire_json }
    }

    #[cfg(test)]
    pub(super) fn to_tap_event(&self) -> TapEvent {
        Self::decode_tap_event(
            self.name.clone(),
            self.session_id.clone(),
            self.session_epoch.clone(),
            self.seq,
            self.turn_id.clone(),
            self.ignorable,
            Arc::clone(&self.wire_json),
        )
    }
}

pub(super) struct PendingRetained {
    pub(super) name: String,
    pub(super) payload: Value,
    pub(super) session_epoch: String,
    pub(super) key: Option<String>,
    pub(super) ignorable: bool,
    pub(super) encoded_bytes: usize,
    pub(super) projected: Option<ProjectedPayload>,
    pub(super) admit_gen: u64,
}

/// Identity fixed at emission, before admission can wait on backpressure.
#[derive(Clone)]
pub(super) struct EventIdentity {
    pub(super) session_id: Option<String>,
    pub(super) session_epoch: String,
    // Shared with in-flight emissions so the old epoch's counter outlives a switch.
    // Reads and increments occur under the log mutex; the atomic only provides shared ownership.
    next_seq: Arc<AtomicU64>,
}

impl EventIdentity {
    pub(super) fn current(st: &EventLogState) -> Self {
        Self {
            session_id: st.session_id.clone(),
            session_epoch: st.session_epoch.clone(),
            next_seq: st.next_seq.clone(),
        }
    }

    pub(super) fn next_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
pub(super) struct ProjectedPayload {
    raw: Value,
    pub(super) value: Value,
    projector: Option<mare_shared::wire_projection::WireProjector>,
    revision: u64,
    active: bool,
    pub(super) bounded: bool,
}

/// Contract-declared tap delivery metadata synced from loaded service contracts.
#[derive(Clone, Debug)]
pub struct ContractTapMeta {
    pub reach: crate::contracts::EventReach,
    pub scope: mare_protocol::EventScope,
    pub delivery: mare_protocol::EventDelivery,
    pub ignorable: bool,
    pub retain: Option<EventRetain>,
}

pub(super) enum RetainedPayload {
    Last(Box<ProjectedPayload>),
    Keyed(std::collections::BTreeMap<String, ProjectedPayload>),
}

pub(super) struct EventLogState {
    pub(super) limits: EventLogLimits,
    pub(super) wire_projector: mare_shared::wire_projection::WireProjectorSlot,
    pub(super) epoch_nonce: String,
    pub(super) epoch_counter: u64,
    pub(super) session_id: Option<String>,
    pub(super) session_epoch: String,
    pub(super) next_seq: Arc<AtomicU64>,
    pub(super) tail_offset: u64,
    pub(super) total_bytes: usize,
    pub(super) entries: VecDeque<LogEntry>,
    pub(super) pending_retained: Vec<PendingRetained>,
    pub(super) pending_required_waiters: usize,
    pub(super) next_admission_gen: u64,
    // Tracks the highest admission generation per `(event, retain_key)` within the current session
    // epoch (including clear tombstones so late in-flight projections cannot resurrect cleared keys).
    // Bounded by distinct `(event, key)` pairs emitted in the active epoch and cleared on `rotate_epoch`.
    pub(super) retained_order: std::collections::BTreeMap<(String, Option<String>), u64>,
    pub(super) retained: std::collections::BTreeMap<String, RetainedPayload>,
    pub(super) next_consumer_id: u64,
    pub(super) consumers: Vec<ConsumerSlot>,
}

impl EventLogState {
    pub(super) fn next_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Relaxed)
    }

    pub(super) fn reset_sequence(&mut self) {
        self.next_seq = Arc::new(AtomicU64::new(1));
    }
}

fn new_epoch_nonce() -> String {
    use std::hash::BuildHasher as _;
    let a = std::collections::hash_map::RandomState::new().hash_one(std::process::id());
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let b = std::collections::hash_map::RandomState::new().hash_one(nanos);
    format!("{a:08x}{b:08x}")
}

pub(super) fn format_session_epoch(session_id: Option<&str>, nonce: &str, counter: u64) -> String {
    format!("{}#{nonce}:{counter}", session_id.unwrap_or(""))
}

pub(super) fn set_current_epoch(st: &mut EventLogState, session_id: Option<String>) {
    st.session_id = session_id;
    st.session_epoch = format_session_epoch(st.session_id.as_deref(), &st.epoch_nonce, st.epoch_counter);
}

impl Default for EventLogState {
    fn default() -> Self {
        let epoch_nonce = new_epoch_nonce();
        let session_epoch = format_session_epoch(None, &epoch_nonce, 1);
        Self {
            limits: EventLogLimits::default(),
            wire_projector: mare_shared::wire_projection::WireProjectorSlot::default(),
            epoch_nonce,
            epoch_counter: 1,
            session_id: None,
            session_epoch,
            next_seq: Arc::new(AtomicU64::new(1)),
            tail_offset: 0,
            total_bytes: 0,
            entries: VecDeque::new(),
            pending_retained: Vec::new(),
            pending_required_waiters: 0,
            next_admission_gen: 0,
            retained_order: std::collections::BTreeMap::new(),
            retained: std::collections::BTreeMap::new(),
            next_consumer_id: 1,
            consumers: Vec::new(),
        }
    }
}

pub(super) fn is_process_in_map(contract_events: &HashMap<String, ContractTapMeta>, event: &str) -> bool {
    contract_events.get(event).is_some_and(|m| m.reach == crate::contracts::EventReach::Process)
}

pub(super) fn is_external_in_map(contract_events: &HashMap<String, ContractTapMeta>, event: &str) -> bool {
    if is_process_in_map(contract_events, event) {
        return false;
    }
    mare_protocol::event_registry::is_protocol_event(event)
        || contract_events.get(event).is_some_and(|m| m.reach == crate::contracts::EventReach::Both)
}

pub(super) fn clear_rules_in_map<'a>(
    contract_events: &'a HashMap<String, ContractTapMeta>,
    event: &'a str,
) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
    contract_events.iter().filter_map(move |(name, meta)| match &meta.retain {
        Some(EventRetain::Key { key, cleared_by }) if cleared_by == event => Some((name.as_str(), key.as_str())),
        _ => None,
    })
}

pub(super) struct EventLogInner {
    pub(super) state: Mutex<EventLogState>,
    pub(super) contract_events: RwLock<HashMap<String, ContractTapMeta>>,
    pub(super) required_gate: tokio::sync::Mutex<()>,
    pub(super) space_wake: Arc<Notify>,
}

pub(super) fn encode_wire_event(
    identity: &EventIdentity,
    kind: &str,
    seq: u64,
    turn_id: Option<&str>,
    ignorable: bool,
    data: &Value,
) -> Result<Arc<str>, String> {
    let session_id = identity.session_id.as_deref();
    let session_epoch = &identity.session_epoch;
    let mut buf = Vec::with_capacity(128);
    let write_field = |buf: &mut Vec<u8>, key: &str, val: &str| -> Result<(), serde_json::Error> {
        buf.extend_from_slice(key.as_bytes());
        serde_json::to_writer(buf, val)
    };
    (|| -> Result<(), serde_json::Error> {
        buf.push(b'{');
        write_field(&mut buf, "\"type\":", kind)?;
        if let Some(id) = session_id {
            write_field(&mut buf, ",\"session_id\":", id)?;
        }
        write_field(&mut buf, ",\"session_epoch\":", session_epoch)?;
        buf.extend_from_slice(format!(",\"seq\":{seq}").as_bytes());
        if let Some(tid) = turn_id {
            write_field(&mut buf, ",\"turn_id\":", tid)?;
        }
        if ignorable {
            buf.extend_from_slice(b",\"ignorable\":true");
        }
        buf.extend_from_slice(b",\"data\":");
        serde_json::to_writer(&mut buf, data)?;
        buf.push(b'}');
        Ok(())
    })()
    .map_err(|e| format!("JSON serialization failed: {e}"))?;
    let json = String::from_utf8(buf).map_err(|e| format!("UTF-8 serialization failed: {e}"))?;
    Ok(Arc::<str>::from(json))
}

pub(super) fn record_drop_locked(st: &mut EventLogState, event: &str, identity: &EventIdentity) {
    let tail = st.tail_offset;
    for c in &mut st.consumers {
        if c.alive && c.filter.select(event).is_some_and(|(selected, _)| selected) {
            c.record_drop(tail, identity);
        }
    }
}

pub(super) fn current_epoch_and_tail(st: &EventLogState) -> (String, u64) {
    (st.session_epoch.clone(), st.next_seq().saturating_sub(1))
}

pub(super) fn payload_turn_id(payload: &Value) -> Option<String> {
    payload.get("turn_id").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_string)
}

pub(super) fn collect_retained_locked(
    st: &mut EventLogState,
    retain_filter: &dyn Fn(&str) -> bool,
) -> std::collections::BTreeMap<String, Value> {
    let projector = st.wire_projector.get();
    let identity = EventIdentity::current(st);
    let max_event_bytes = st.limits.max_event_bytes;
    let refresh_and_bound = |name: &str, cached: &mut ProjectedPayload| {
        let refreshed = refresh_projected_payload(projector.as_ref(), cached);
        if refreshed || !cached.bounded {
            let _ = super::bounds::encode_bounded_wire_event(
                &identity,
                name,
                identity.next_seq(),
                false,
                &mut cached.value,
                max_event_bytes,
            );
            cached.bounded = true;
        }
        cached.value.clone()
    };
    st.retained
        .iter_mut()
        .filter(|(name, _)| retain_filter(name))
        .map(|(name, retained)| {
            let value = match retained {
                RetainedPayload::Last(cached) => refresh_and_bound(name, cached),
                RetainedPayload::Keyed(values) => {
                    Value::Array(values.values_mut().map(|cached| refresh_and_bound(name, cached)).collect())
                }
            };
            (name.clone(), value)
        })
        .collect()
}

pub(super) fn record_retained_locked(
    st: &mut EventLogState,
    event: &str,
    key: &Option<String>,
    payload: &ProjectedPayload,
    admit_gen: u64,
) {
    let order_key = (event.to_string(), key.clone());
    if st.retained_order.get(&order_key).is_some_and(|&prev| admit_gen < prev) {
        return;
    }
    st.retained_order.insert(order_key, admit_gen);
    if let Some(key) = key {
        let retained =
            st.retained.entry(event.to_string()).or_insert_with(|| RetainedPayload::Keyed(Default::default()));
        if let RetainedPayload::Keyed(values) = retained {
            values.insert(key.clone(), payload.clone());
        } else {
            *retained = RetainedPayload::Keyed([(key.clone(), payload.clone())].into_iter().collect());
        }
    } else {
        st.retained.insert(event.to_string(), RetainedPayload::Last(Box::new(payload.clone())));
    }
}

pub(super) fn clear_retained_locked(st: &mut EventLogState, event: &str, key: &str, admit_gen: u64) {
    let order_key = (event.to_string(), Some(key.to_string()));
    if st.retained_order.get(&order_key).is_some_and(|&prev| admit_gen < prev) {
        return;
    }
    st.retained_order.insert(order_key, admit_gen);
    if let Some(RetainedPayload::Keyed(values)) = st.retained.get_mut(event) {
        values.remove(key);
        if values.is_empty() {
            st.retained.remove(event);
        }
    }
    st.pending_retained.retain(|pending| !(pending.name == event && pending.key.as_deref() == Some(key)));
}

pub(super) fn rotate_epoch(st: &mut EventLogState, next_session_id: Option<String>) {
    st.epoch_counter += 1;
    set_current_epoch(st, next_session_id);
    st.reset_sequence();
    st.pending_retained.clear();
    st.retained_order.clear();
    st.retained.clear();
}

pub(super) fn has_room(st: &EventLogState, needed_bytes: usize, limit_cap: usize, limit_bytes: usize) -> bool {
    st.entries.len() < limit_cap && st.total_bytes.saturating_add(needed_bytes) <= limit_bytes
}

pub(super) fn evict_unpinned(st: &mut EventLogState, needed_bytes: usize, limit_cap: usize, limit_bytes: usize) {
    while !has_room(st, needed_bytes, limit_cap, limit_bytes) {
        let Some(front) = st.entries.front() else { break };
        let front_offset = front.offset;
        let pinned = st.consumers.iter().any(|c| c.alive && c.fixed && c.next_offset <= front_offset);
        if pinned {
            break;
        }
        let evicted = st.entries.pop_front().expect("checked front");
        st.total_bytes = st.total_bytes.saturating_sub(evicted.encoded_bytes());
        for c in &mut st.consumers {
            while let Some((off, _)) = c.decisions.front() {
                if *off <= evicted.offset {
                    c.decisions.pop_front();
                } else {
                    break;
                }
            }
        }
    }
}

pub(super) fn fail_next_blocking_fixed_consumer(st: &mut EventLogState) -> bool {
    let Some(front_offset) = st.entries.front().map(|e| e.offset) else { return false };
    let mut failed_any = false;
    for c in &mut st.consumers {
        if c.alive && c.fixed && c.next_offset <= front_offset {
            c.fail("event stream timed out waiting for slow consumer".to_string());
            failed_any = true;
        }
    }
    failed_any
}

pub(super) fn flush_retained_locked(st: &mut EventLogState, reserve_bytes: Option<usize>) {
    if st.pending_required_waiters > 0 {
        return;
    }
    let reserve = reserve_bytes.unwrap_or(0);
    let current_epoch = st.session_epoch.clone();
    let limit_cap = st.limits.capacity;
    let limit_bytes = st.limits.max_bytes;
    let extra_slot = usize::from(reserve_bytes.is_some());
    while let Some(head) = st.pending_retained.first() {
        if head.session_epoch != current_epoch {
            st.pending_retained.remove(0);
            continue;
        }
        let needed = head.encoded_bytes + reserve;
        evict_unpinned(st, needed, limit_cap.saturating_sub(extra_slot), limit_bytes);
        if st.entries.len() + extra_slot < limit_cap && st.total_bytes.saturating_add(needed) <= limit_bytes {
            let mut item = st.pending_retained.remove(0);
            let identity = EventIdentity::current(st);
            if let CommitOutcome::Full(needed) = commit_entry_locked(
                st,
                &identity,
                &item.name,
                &item.payload,
                item.ignorable,
                reserve_bytes,
                &mut item.projected,
            ) {
                item.encoded_bytes = needed;
                st.pending_retained.insert(0, item);
                break;
            }
        } else {
            break;
        }
    }
}

pub(super) enum CommitOutcome {
    Committed(u64),
    Full(usize),
    Dropped,
}

pub(super) fn project_payload(st: &EventLogState, payload: &Value, cache: &mut Option<ProjectedPayload>) {
    let projector = st.wire_projector.get();
    project_payload_with(projector.as_ref(), payload, cache);
}

pub(super) fn project_payload_with(
    projector: Option<&mare_shared::wire_projection::WireProjector>,
    payload: &Value,
    cache: &mut Option<ProjectedPayload>,
) {
    let cached = cache.get_or_insert_with(|| ProjectedPayload {
        raw: payload.clone(),
        value: payload.clone(),
        projector: None,
        revision: 0,
        active: false,
        bounded: false,
    });
    refresh_projected_payload(projector, cached);
}

fn refresh_projected_payload(
    projector: Option<&mare_shared::wire_projection::WireProjector>,
    cached: &mut ProjectedPayload,
) -> bool {
    let revision = projector.map_or(0, |p| p.revision());
    let active = projector.is_some_and(|p| p.is_active());
    let current = cached.revision == revision
        && cached.active == active
        && match (&cached.projector, projector) {
            (None, None) => true,
            (Some(old), Some(new)) => old.same_registry(new),
            _ => false,
        };
    if !current {
        // Re-project from the untruncated raw payload so newly registered sensitive values
        // that spanned an earlier wire-truncation boundary cannot leave an unredacted prefix.
        cached.value = cached.raw.clone();
        if let Some(proj) = projector.filter(|_| active) {
            proj.project_json(&mut cached.value);
        }
        cached.projector = projector.cloned();
        cached.revision = revision;
        cached.active = active;
        cached.bounded = false;
        return true;
    }
    false
}

/// Project only when an event can be committed, so deferred events see current registrations.
pub(super) fn commit_entry_locked(
    st: &mut EventLogState,
    identity: &EventIdentity,
    event: &str,
    payload: &Value,
    ignorable: bool,
    reserve_bytes: Option<usize>,
    cache: &mut Option<ProjectedPayload>,
) -> CommitOutcome {
    project_payload(st, payload, cache);
    let max_event_bytes = st.limits.max_event_bytes;
    let cached = cache.as_mut().expect("projection cache initialized");
    let seq = identity.next_seq();
    let wire_json = match super::bounds::encode_bounded_wire_event(
        identity,
        event,
        seq,
        ignorable,
        &mut cached.value,
        max_event_bytes,
    ) {
        Ok(Some(wire)) => {
            cached.bounded = true;
            wire
        }
        Ok(None) | Err(_) => {
            record_drop_locked(st, event, identity);
            return CommitOutcome::Dropped;
        }
    };
    let turn_id = payload_turn_id(&cached.value);
    let needed = wire_json.len();
    if needed > st.limits.max_bytes {
        record_drop_locked(st, event, identity);
        return CommitOutcome::Dropped;
    }
    let extra_slot = usize::from(reserve_bytes.is_some());
    let reserved = reserve_bytes.unwrap_or(0);
    let capacity = st.limits.capacity.saturating_sub(extra_slot);
    let max_bytes = st.limits.max_bytes;
    evict_unpinned(st, needed.saturating_add(reserved), capacity, max_bytes);
    if !has_room(st, needed.saturating_add(reserved), capacity, max_bytes) {
        return CommitOutcome::Full(needed);
    }

    identity.next_seq.fetch_add(1, Ordering::Relaxed);
    let offset = st.tail_offset;
    st.tail_offset += 1;

    st.total_bytes += wire_json.len();
    st.entries.push_back(LogEntry {
        offset,
        name: event.to_string(),
        session_id: identity.session_id.clone(),
        session_epoch: identity.session_epoch.clone(),
        seq,
        turn_id,
        ignorable,
        wire_json,
    });

    for c in &mut st.consumers {
        if !c.alive {
            continue;
        }
        let decision = c.filter.select(event).map(|(sel, _)| sel);
        if c.decisions.len() < st.limits.capacity + 64 {
            c.decisions.push_back((offset, decision));
        }
        if decision.is_some() {
            c.wake.notify_waiters();
        }
    }
    CommitOutcome::Committed(seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_retained_transitions_last_to_keyed() {
        let mut st = EventLogState::default();
        let payload = ProjectedPayload {
            raw: Value::String("ok".into()),
            value: Value::String("ok".into()),
            projector: None,
            revision: 0,
            active: false,
            bounded: true,
        };
        record_retained_locked(&mut st, "demo/ev", &None, &payload, 1);
        assert!(matches!(st.retained.get("demo/ev"), Some(RetainedPayload::Last(_))));
        record_retained_locked(&mut st, "demo/ev", &Some("k1".into()), &payload, 2);
        assert!(matches!(st.retained.get("demo/ev"), Some(RetainedPayload::Keyed(_))));
    }
}
