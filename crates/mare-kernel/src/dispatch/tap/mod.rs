//! Unified bounded event log and stream cursors.
//!
//! Public events enter a per-kernel bounded ring (`SESSION_LOG_CAPACITY` entries /
//! `SESSION_LOG_MAX_BYTES` encoded bytes), encoded to JSON once at admission. Each
//! `(session_id, session_epoch)` receives its own contiguous `seq` starting at 1;
//! events waiting for space keep their emission identity.
//!
//! Wire consumers (RPC, headless, ACP, and kernel cursors) read by cursor without duplicating logs:
//! - Ordinary cursors receive `Lagged { from_seq, to_seq }` if evicted before reading.
//! - Fixed cursors (`headless`) pin the ring tail: `Required` events wait up to
//!   `TAP_BLOCK_LIMIT` before failing only that lagging stream; `Lossy` events never wait,
//!   updating `retain` latest state or counting `Dropped { count }` for later delivery.
//! - Oversized (`> EVENT_MAX_BYTES`) or unencodable events never enter the log, take no `seq`,
//!   record `Dropped { count }` on matching external consumers, and never throw to the emitter.

mod admission;
mod bounds;
mod cursors;
mod log;
#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mare_protocol::EventParams;
use mlua::MultiValue;
use serde_json::Value;
use tokio::sync::Notify;

use crate::Kernel;
use crate::dispatch::runtime_fields::project_tap_arg;

pub use cursors::{EventCursor, HeadlessCursor, HeadlessTapHandle};
pub use log::ContractTapMeta;
use log::{
    EventIdentity, EventLogInner, EventLogState, clear_rules_in_map, current_epoch_and_tail, is_external_in_map,
    is_process_in_map, payload_turn_id, rotate_epoch, set_current_epoch,
};

/// Default maximum number of entries retained in a session's in-memory event log.
pub const SESSION_LOG_CAPACITY: usize = 4096;

/// Default maximum encoded JSON bytes retained in a session's in-memory event log (8 MiB).
pub const SESSION_LOG_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Default maximum encoded JSON size for a single event envelope (1 MiB).
pub const EVENT_MAX_BYTES: usize = 1024 * 1024;

/// Default maximum time a `required` event waits when a fixed-cursor consumer holds the log full.
pub const TAP_BLOCK_LIMIT: Duration = Duration::from_secs(5);

/// Configurable bounds for a session event log (production uses the module defaults).
#[derive(Clone, Copy, Debug)]
pub struct EventLogLimits {
    pub capacity: usize,
    pub max_bytes: usize,
    pub max_event_bytes: usize,
    pub block_limit: Duration,
}

impl Default for EventLogLimits {
    fn default() -> Self {
        Self {
            capacity: SESSION_LOG_CAPACITY,
            max_bytes: SESSION_LOG_MAX_BYTES,
            max_event_bytes: EVENT_MAX_BYTES,
            block_limit: TAP_BLOCK_LIMIT,
        }
    }
}

/// An admitted event materialized for a cursor with both its pre-encoded wire JSON and payload.
#[derive(Clone, Debug)]
pub struct TapEvent {
    pub name: String,
    pub payload: Value,
    pub session_id: Option<String>,
    pub session_epoch: String,
    pub seq: u64,
    pub turn_id: Option<String>,
    pub ignorable: bool,
    /// Serialized [`EventParams`] JSON encoded once at admission.
    pub wire_json: Arc<str>,
}

impl TapEvent {
    /// Build the wire `EventParams` envelope for RPC notifications and headless JSON lines.
    pub fn to_wire(&self) -> EventParams {
        EventParams {
            kind: self.name.clone(),
            session_id: self.session_id.clone(),
            session_epoch: Some(self.session_epoch.clone()),
            seq: self.seq,
            turn_id: self.turn_id.clone(),
            ignorable: self.ignorable.then_some(true),
            data: self.payload.clone(),
        }
    }
}

/// Runtime envelope passed as the 2nd argument (`ev`) to bus/policy listeners (`fn(data, ev)`).
#[derive(Clone, Debug)]
pub(crate) struct EventEnvelope {
    pub event: String,
    pub session_id: Option<String>,
    pub session_epoch: String,
    pub seq: Option<u64>,
    pub turn_id: Option<String>,
}

impl EventEnvelope {
    /// Materialize this envelope as a Lua table tagged with metatable `"mare.EventEnvelope"`.
    pub(crate) fn to_lua(&self, lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
        const MT_KEY: &str = "mare.EventEnvelope.mt";
        let mt: mlua::Table = match lua.named_registry_value::<mlua::Table>(MT_KEY) {
            Ok(mt) => mt,
            Err(_) => {
                let mt = lua.create_table()?;
                mt.set("__metatable", "mare.EventEnvelope")?;
                lua.set_named_registry_value(MT_KEY, mt.clone())?;
                mt
            }
        };
        let t = lua.create_table()?;
        t.set("type", self.event.as_str())?;
        if let Some(session_id) = &self.session_id {
            t.set("session_id", session_id.as_str())?;
        }
        t.set("session_epoch", self.session_epoch.as_str())?;
        if let Some(seq) = self.seq {
            t.set("seq", seq)?;
        }
        if let Some(turn_id) = &self.turn_id {
            t.set("turn_id", turn_id.as_str())?;
        }
        t.set_metatable(Some(mt))?;
        Ok(t)
    }
}

/// Extract optional non-empty string `turn_id` from the first payload argument when it is a table.
pub(crate) fn turn_id_from_args(args: &MultiValue) -> Option<String> {
    let mlua::Value::Table(t) = args.front()? else { return None };
    let Ok(Some(s)) = t.get::<Option<String>>("turn_id") else { return None };
    let trimmed = s.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Strip a trailing `mare.EventEnvelope` table from `args` (if present).
pub fn strip_event_envelope(args: MultiValue) -> MultiValue {
    let mut vec: Vec<mlua::Value> = args.into_iter().collect();
    if let Some(mlua::Value::Table(t)) = vec.last()
        && let Some(mt) = t.metatable()
        && mt.get::<Option<String>>("__metatable").ok().flatten().as_deref() == Some("mare.EventEnvelope")
    {
        vec.pop();
    }
    MultiValue::from_vec(vec)
}

/// Headless filter decision: whether an event is emitted (`selected`) and whether delivery is
/// required (`required`, defaulting from the protocol event registry).
#[derive(Clone, Copy, Debug)]
pub struct HeadlessSelection {
    pub required: bool,
    pub selected: bool,
}

/// A record delivered to the headless `--json` stream consumer.
#[derive(Clone, Debug)]
pub enum HeadlessRecord {
    Event(TapEvent),
    Lagged { from_seq: u64, to_seq: u64 },
    Dropped { count: u64, session_id: Option<String>, session_epoch: String },
}

#[derive(Clone, Debug)]
pub struct HeadlessEvent {
    pub record: HeadlessRecord,
    pub selected: bool,
}

impl From<CursorRecord> for HeadlessEvent {
    fn from(rec: CursorRecord) -> Self {
        match rec {
            CursorRecord::Event { event, selected } => Self { record: HeadlessRecord::Event(event), selected },
            CursorRecord::Lagged { from_seq, to_seq } => {
                Self { record: HeadlessRecord::Lagged { from_seq, to_seq }, selected: true }
            }
            CursorRecord::Dropped { count, session_id, session_epoch } => {
                Self { record: HeadlessRecord::Dropped { count, session_id, session_epoch }, selected: true }
            }
        }
    }
}

/// A record read from an ordinary event log cursor.
#[derive(Clone, Debug)]
pub enum CursorRecord {
    Event { event: TapEvent, selected: bool },
    Lagged { from_seq: u64, to_seq: u64 },
    Dropped { count: u64, session_id: Option<String>, session_epoch: String },
}

pub type ReliableFilter = Arc<dyn Fn(&str) -> Option<bool> + Send + Sync>;
pub type HeadlessFilter = Arc<dyn Fn(&str) -> Option<HeadlessSelection> + Send + Sync>;

#[derive(Clone)]
enum ConsumerFilter {
    Reliable(ReliableFilter),
    Headless(HeadlessFilter),
}

impl ConsumerFilter {
    fn select(&self, event: &str) -> Option<(bool, Option<bool>)> {
        match self {
            Self::Reliable(f) => f(event).map(|selected| (selected, None)),
            Self::Headless(f) => f(event).map(|sel| (sel.selected, Some(sel.required))),
        }
    }
}

/// Result of opening a session-scoped / resumptive event cursor.
pub struct SubscribeCursorOutcome {
    pub mode: mare_protocol::SubscribeMode,
    pub session_epoch: String,
    pub seq: u64,
    pub cursor: EventCursor,
}

/// Atomic event-log cut point paired with retained public state payloads.
pub(crate) struct BaselineAnchor {
    pub(crate) session_id: Option<String>,
    pub(crate) session_epoch: String,
    pub(crate) seq: u64,
    pub(crate) retained: std::collections::BTreeMap<String, Value>,
}

/// Shared bounded per-session event log backing all wire stream cursors.
#[derive(Clone)]
pub struct EventLog {
    inner: Arc<EventLogInner>,
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

impl EventLog {
    pub fn new() -> Self {
        Self::with_limits(EventLogLimits::default())
    }

    pub fn with_limits(limits: EventLogLimits) -> Self {
        Self {
            inner: Arc::new(EventLogInner {
                state: Mutex::new(EventLogState { limits, ..EventLogState::default() }),
                contract_events: std::sync::RwLock::new(std::collections::HashMap::new()),
                required_gate: tokio::sync::Mutex::new(()),
                space_wake: Arc::new(Notify::new()),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, EventLogState> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_limits(&self, limits: EventLogLimits) {
        let mut st = self.lock();
        st.limits = limits;
        drop(st);
        self.inner.space_wake.notify_waiters();
    }

    pub fn limits(&self) -> EventLogLimits {
        self.lock().limits
    }

    pub fn set_contract_events(&self, events: std::collections::HashMap<String, ContractTapMeta>) {
        *self.inner.contract_events.write().unwrap_or_else(|e| e.into_inner()) = events;
    }

    pub fn is_external_event(&self, event: &str) -> bool {
        let map = self.inner.contract_events.read().unwrap_or_else(|e| e.into_inner());
        is_external_in_map(&map, event)
    }

    pub fn event_delivery(&self, event: &str) -> Option<mare_protocol::EventDelivery> {
        if let Some(def) = mare_protocol::event_registry::find(event) {
            return Some(def.delivery);
        }
        self.inner.contract_events.read().unwrap_or_else(|e| e.into_inner()).get(event).map(|m| m.delivery)
    }

    pub fn event_ignorable(&self, event: &str) -> bool {
        if let Some(def) = mare_protocol::event_registry::find(event) {
            return def.ignorable;
        }
        self.inner.contract_events.read().unwrap_or_else(|e| e.into_inner()).get(event).is_some_and(|m| m.ignorable)
    }

    /// Bind a VM-shared projector slot eagerly so pure-Rust `admit_json` calls also project.
    pub(crate) fn set_wire_projector_slot(&self, slot: mare_shared::wire_projection::WireProjectorSlot) {
        self.lock().wire_projector = slot;
    }

    pub fn set_wire_projector(&self, projector: mare_shared::wire_projection::WireProjector) {
        self.lock().wire_projector.set(projector);
    }

    /// Initialize or update session identity at startup (`#1` reused if log is still empty at seq 1).
    pub fn set_session(&self, session_id: Option<String>) -> String {
        let mut st = self.lock();
        if st.entries.is_empty() && st.next_seq() == 1 {
            if st.session_id != session_id {
                st.reset_sequence();
            }
            set_current_epoch(&mut st, session_id);
        } else if st.session_id != session_id {
            rotate_epoch(&mut st, session_id);
        }
        st.session_epoch.clone()
    }

    /// Unconditionally rotate `session_epoch` and reset `seq` to 1 on session create/load/switch.
    pub fn switch_session(&self, session_id: Option<String>) -> String {
        let mut st = self.lock();
        rotate_epoch(&mut st, session_id);
        st.session_epoch.clone()
    }

    pub fn session_identity(&self) -> (Option<String>, String) {
        let st = self.lock();
        (st.session_id.clone(), st.session_epoch.clone())
    }

    fn emission_identity(&self) -> EventIdentity {
        EventIdentity::current(&self.lock())
    }

    pub fn session_tail(&self) -> (String, u64) {
        let st = self.lock();
        current_epoch_and_tail(&st)
    }

    /// Whether the current session epoch already contains at least one retained event named `name`.
    pub fn has_event_in_current_epoch(&self, name: &str) -> bool {
        let st = self.lock();
        st.entries.iter().any(|e| e.session_epoch == st.session_epoch && e.name == name)
    }

    pub(crate) fn will_need_baseline(
        &self,
        requested_epoch: Option<&str>,
        last_seq: Option<u64>,
        live_only: bool,
    ) -> bool {
        if live_only {
            return false;
        }
        let st = self.lock();
        cursors::resolve_subscribe_position(&st, requested_epoch, last_seq, live_only).mode
            == mare_protocol::SubscribeMode::Baseline
    }

    pub(crate) fn tapped_status(&self, event: &str) -> (bool, bool) {
        let clears_retained = {
            let map = self.inner.contract_events.read().unwrap_or_else(|e| e.into_inner());
            if is_process_in_map(&map, event) {
                return (false, false);
            }
            if is_external_in_map(&map, event) {
                return (true, true);
            }
            clear_rules_in_map(&map, event).next().is_some()
        };
        let st = self.lock();
        (clears_retained || st.consumers.iter().any(|c| c.alive && c.filter.select(event).is_some()), false)
    }

    pub fn tapped(&self, event: &str) -> bool {
        self.tapped_status(event).0
    }

    pub fn taps_dynamic(&self) -> bool {
        let st = self.lock();
        st.consumers.iter().any(|c| c.alive)
    }
}

impl Kernel {
    pub fn event_log(&self) -> &EventLog {
        &self.0.event_log
    }

    pub fn set_event_log_limits(&self, limits: EventLogLimits) {
        self.0.event_log.set_limits(limits);
    }

    pub fn set_session_identity(&self, session_id: Option<String>) -> String {
        self.0.event_log.set_session(session_id)
    }

    pub fn switch_session_identity(&self, session_id: Option<String>) -> String {
        self.0.event_log.switch_session(session_id)
    }

    pub fn open_cursor(&self, wants: ReliableFilter) -> EventCursor {
        self.open_cursor_with_wake(wants, None)
    }

    pub fn open_cursor_with_wake(&self, wants: ReliableFilter, wake: Option<Arc<Notify>>) -> EventCursor {
        let cursor = self.0.event_log.open_cursor_with_wake(wants, wake);
        self.taps_changed();
        cursor
    }

    pub fn subscribe_cursor(
        &self,
        requested_epoch: Option<&str>,
        last_seq: Option<u64>,
        live_only: bool,
        wants: ReliableFilter,
        wake: Option<Arc<Notify>>,
    ) -> SubscribeCursorOutcome {
        let outcome = self.0.event_log.subscribe_cursor(requested_epoch, last_seq, live_only, wants, wake);
        self.taps_changed();
        outcome
    }

    pub fn open_headless_cursor(&self, wants: HeadlessFilter) -> (HeadlessTapHandle, HeadlessCursor) {
        let pair = self.0.event_log.open_headless_cursor(wants);
        self.taps_changed();
        pair
    }

    pub(crate) fn tapped(&self, event: &str) -> bool {
        self.0.event_log.tapped(event)
    }

    pub(crate) fn taps_dynamic(&self) -> bool {
        self.0.event_log.taps_dynamic()
    }

    pub fn taps_changed(&self) {
        if let Some(f) = self.bus("taps_changed") {
            let _ = f.call::<()>(self.taps_dynamic());
        }
    }

    /// In-process listener envelope metadata (`mare.EventEnvelope`), constructed alongside tap admission.
    pub(crate) fn current_envelope(&self, event: &str, args: &MultiValue, seq: Option<u64>) -> EventEnvelope {
        let (session_id, session_epoch) = self.0.event_log.session_identity();
        let turn_id = turn_id_from_args(args);
        EventEnvelope { event: event.to_owned(), session_id, session_epoch, seq, turn_id }
    }

    /// Encode Lua arguments once and admit the event into the bounded session event log.
    ///
    /// Oversized or unencodable events never enter the log and take no `seq`: instead matching
    /// external stream consumers record `Dropped`, and errors are never thrown back to the emitter.
    pub(crate) async fn tap_event(&self, event: &str, args: &MultiValue) -> EventEnvelope {
        let identity = self.0.event_log.emission_identity();
        let session_id = identity.session_id.clone();
        let session_epoch = identity.session_epoch.clone();
        if !self.0.event_log.tapped(event) {
            return EventEnvelope {
                event: event.to_owned(),
                session_id,
                session_epoch,
                seq: None,
                turn_id: turn_id_from_args(args),
            };
        }
        let payload = match project_tap_arg(self.lua(), args) {
            Ok(payload) => payload,
            Err(err) => {
                self.0.event_log.record_refusal_drop(event, &identity);
                self.handler_failed(
                    mare_shared::KERNEL_SERVICE_NAME,
                    &format!("event tap for `{event}`"),
                    None,
                    &err.to_string(),
                );
                return EventEnvelope {
                    event: event.to_owned(),
                    session_id,
                    session_epoch,
                    seq: None,
                    turn_id: turn_id_from_args(args),
                };
            }
        };
        let turn_id = payload_turn_id(&payload).or_else(|| turn_id_from_args(args));
        let scope = mare_protocol::event_registry::scope_of(event).or_else(|| self.contract_event_scope(event));
        let seq = match self.0.event_log.admit_json_scoped_seq(event, payload, scope, Some(identity)).await {
            Ok(seq) => seq,
            Err(err) => {
                self.handler_failed(mare_shared::KERNEL_SERVICE_NAME, &format!("event tap for `{event}`"), None, &err);
                None
            }
        };
        EventEnvelope { event: event.to_owned(), session_id, session_epoch, seq, turn_id }
    }
}
