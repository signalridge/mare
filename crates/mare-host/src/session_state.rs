//! Per-session runtime state held by `Host` and `SessionRuntime`:
//! - `AnswererSlot`: exclusive UI/approval/elicitation answerer lease
//! - `InboundHeld`: per-session inbound peer message hold quota (replaces process-global `HELD`)
//! - `SessionPromptReceipts`: per-session prompt idempotency cache & in-flight map

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use mlua::{Function, Lua, Table, Value};
use serde_json::Value as J;
use sha2::Digest;

/// Stable per-connection identifier assigned when a transport connection opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConnId(pub u64);

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a unique [`ConnId`] within this process.
pub fn alloc_conn_id() -> ConnId {
    ConnId(NEXT_CONN_ID.fetch_add(1, Ordering::Relaxed))
}

pub type RevokeFn = Box<dyn FnOnce(&str)>;

struct AnswererState {
    conn_id: ConnId,
    lease_id: u64,
    approval_voters: Vec<Function>,
    saved_ui: Option<(Table, Vec<(&'static str, Value)>)>,
    active_selects: HashMap<String, Function>,
    elicitation_release: Option<Function>,
    on_revoke: Option<RevokeFn>,
}

/// Single-answerer lease for one host/session.
/// At most one connection may hold `approval_voters`, `saved_ui`, and `elicitation` forwarding at
/// a time. Replacing or releasing the holder immediately restores Lua UI, releases voters, and
/// invokes `on_revoke` so pending interactive callbacks finish without silent transfer.
pub struct AnswererSlot {
    inner: RefCell<Option<AnswererState>>,
    next_lease_id: Cell<u64>,
}

impl Default for AnswererSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl AnswererSlot {
    pub fn new() -> Self {
        Self { inner: RefCell::new(None), next_lease_id: Cell::new(1) }
    }

    pub fn holder(&self) -> Option<ConnId> {
        self.inner.borrow().as_ref().map(|s| s.conn_id)
    }

    pub fn is_holder(&self, conn_id: ConnId) -> bool {
        self.inner.borrow().as_ref().is_some_and(|s| s.conn_id == conn_id)
    }

    /// Acquire the answerer lease for `conn_id`. If another connection holds the lease, it is
    /// revoked first; if `conn_id` already holds it, its revocation callback is refreshed.
    pub fn acquire(&self, lua: &Lua, conn_id: ConnId, on_revoke: RevokeFn) -> u64 {
        {
            let mut guard = self.inner.borrow_mut();
            if let Some(state) = guard.as_mut()
                && state.conn_id == conn_id
            {
                state.on_revoke = Some(on_revoke);
                return state.lease_id;
            }
        }
        self.revoke_current(lua, "answerer replaced");
        let lease_id = self.next_lease_id.get();
        self.next_lease_id.set(lease_id.wrapping_add(1));
        *self.inner.borrow_mut() = Some(AnswererState {
            conn_id,
            lease_id,
            approval_voters: Vec::new(),
            saved_ui: None,
            active_selects: HashMap::new(),
            elicitation_release: None,
            on_revoke: Some(on_revoke),
        });
        lease_id
    }

    /// Release the lease if `conn_id` is the current holder.
    pub fn release(&self, lua: &Lua, conn_id: ConnId, reason: &str) -> bool {
        if !self.is_holder(conn_id) {
            return false;
        }
        self.revoke_current(lua, reason)
    }

    fn revoke_current(&self, _lua: &Lua, reason: &str) -> bool {
        let Some(mut state) = self.inner.borrow_mut().take() else {
            return false;
        };
        for release in state.approval_voters.drain(..) {
            if let Err(e) = release.call::<()>(()) {
                tracing::warn!("answerer approval voter release: {e}");
            }
        }
        if let Some(release) = state.elicitation_release.take()
            && let Err(e) = release.call::<()>(())
        {
            tracing::warn!("answerer elicitation release: {e}");
        }
        restore_saved_ui(state.saved_ui);
        state.active_selects.clear();
        if let Some(cb) = state.on_revoke.take() {
            cb(reason);
        }
        true
    }

    pub fn has_approval_voters(&self, conn_id: ConnId) -> bool {
        self.inner.borrow().as_ref().is_some_and(|s| s.conn_id == conn_id && !s.approval_voters.is_empty())
    }

    pub fn push_approval_voter(&self, conn_id: ConnId, release: Function) -> bool {
        let mut guard = self.inner.borrow_mut();
        let Some(state) = guard.as_mut().filter(|s| s.conn_id == conn_id) else {
            return false;
        };
        state.approval_voters.push(release);
        true
    }

    pub fn take_approval_voters(&self, conn_id: ConnId) -> Vec<Function> {
        let mut guard = self.inner.borrow_mut();
        let Some(state) = guard.as_mut().filter(|s| s.conn_id == conn_id) else {
            return Vec::new();
        };
        std::mem::take(&mut state.approval_voters)
    }

    pub fn has_saved_ui(&self, conn_id: ConnId) -> bool {
        self.inner
            .borrow()
            .as_ref()
            .is_some_and(|s| s.conn_id == conn_id && s.saved_ui.as_ref().is_some_and(|(_, v)| !v.is_empty()))
    }

    pub fn set_saved_ui(&self, conn_id: ConnId, ui_table: Table, saved: Vec<(&'static str, Value)>) -> bool {
        let mut guard = self.inner.borrow_mut();
        let Some(state) = guard.as_mut().filter(|s| s.conn_id == conn_id) else {
            return false;
        };
        state.saved_ui = Some((ui_table, saved));
        true
    }

    pub fn restore_ui(&self, _lua: &Lua, conn_id: ConnId) {
        let saved = {
            let mut guard = self.inner.borrow_mut();
            let Some(state) = guard.as_mut().filter(|s| s.conn_id == conn_id) else {
                return;
            };
            state.active_selects.clear();
            state.saved_ui.take()
        };
        restore_saved_ui(saved);
    }

    pub fn insert_active_select(&self, conn_id: ConnId, id: String, cb: Function) {
        if let Some(state) = self.inner.borrow_mut().as_mut().filter(|s| s.conn_id == conn_id) {
            state.active_selects.insert(id, cb);
        }
    }

    pub fn remove_active_select(&self, conn_id: ConnId, id: &str) {
        if let Some(state) = self.inner.borrow_mut().as_mut().filter(|s| s.conn_id == conn_id) {
            state.active_selects.remove(id);
        }
    }

    pub fn get_active_select(&self, conn_id: ConnId, id: &str) -> Option<Function> {
        self.inner.borrow().as_ref().filter(|s| s.conn_id == conn_id).and_then(|s| s.active_selects.get(id).cloned())
    }

    pub fn has_elicitation(&self, conn_id: ConnId) -> bool {
        self.inner.borrow().as_ref().is_some_and(|s| s.conn_id == conn_id && s.elicitation_release.is_some())
    }

    pub fn set_elicitation_release(&self, conn_id: ConnId, release: Function) -> bool {
        let mut guard = self.inner.borrow_mut();
        let Some(state) = guard.as_mut().filter(|s| s.conn_id == conn_id) else {
            return false;
        };
        state.elicitation_release = Some(release);
        true
    }

    pub fn take_elicitation_release(&self, conn_id: ConnId) -> Option<Function> {
        self.inner.borrow_mut().as_mut().filter(|s| s.conn_id == conn_id).and_then(|s| s.elicitation_release.take())
    }
}

fn restore_saved_ui(saved: Option<(Table, Vec<(&'static str, Value)>)>) {
    let Some((ui, entries)) = saved else {
        return;
    };
    for (k, v) in entries {
        let _ = ui.set(k, v);
    }
}

/// Default per-session cap on inbound peer messages waiting for user confirmation.
pub const DEFAULT_MAX_INBOUND_HELD: usize = 32;

/// Per-session counter of inbound peer messages currently waiting for confirmation.
#[derive(Clone)]
pub struct InboundHeld {
    max: usize,
    count: Rc<Cell<usize>>,
}

impl Default for InboundHeld {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_INBOUND_HELD)
    }
}

impl InboundHeld {
    pub fn new(max: usize) -> Self {
        Self { max, count: Rc::new(Cell::new(0)) }
    }

    pub fn max(&self) -> usize {
        self.max
    }

    pub fn count(&self) -> usize {
        self.count.get()
    }

    pub fn try_acquire(&self) -> Option<InboundHeldGuard> {
        let cur = self.count.get();
        if cur >= self.max {
            return None;
        }
        self.count.set(cur + 1);
        Some(InboundHeldGuard { count: self.count.clone() })
    }
}

/// RAII slot released when an inbound peer message is answered or dropped.
pub struct InboundHeldGuard {
    count: Rc<Cell<usize>>,
}

impl Drop for InboundHeldGuard {
    fn drop(&mut self) {
        self.count.set(self.count.get().saturating_sub(1));
    }
}

pub type PendingPromptOutcome = Result<J, mare_protocol::RpcError>;

/// In-flight prompt entry shared by concurrent duplicate `requestId` callers.
pub struct PendingPrompt {
    pub fingerprint: [u8; 32],
    pub notify: Rc<tokio::sync::Notify>,
    pub result: RefCell<Option<PendingPromptOutcome>>,
}

struct PromptRequestEntry {
    fingerprint: [u8; 32],
    answer: J,
    bytes: usize,
}

struct SessionPromptRequests {
    entries: HashMap<String, PromptRequestEntry>,
    order: VecDeque<String>,
    bytes: usize,
}

pub struct PromptRequestCache {
    per_session_max: usize,
    max_sessions: usize,
    max_total_entries: usize,
    max_total_bytes: usize,
    total_entries: usize,
    total_bytes: usize,
    sessions: HashMap<String, SessionPromptRequests>,
    session_order: VecDeque<String>,
}

pub const PROMPT_REQUESTS_MAX: usize = 4096;
pub const PROMPT_REQUEST_SESSIONS_MAX: usize = 64;
pub const PROMPT_REQUESTS_TOTAL_MAX: usize = 16_384;
pub const PROMPT_REQUESTS_TOTAL_BYTES_MAX: usize = 16 * 1024 * 1024;
const PROMPT_REQUEST_ENTRY_OVERHEAD: usize = 2 * std::mem::size_of::<String>() + 16;
const _: () = assert!(2 * mare_protocol::PROMPT_REQUEST_ID_MAX_BYTES + 4096 < PROMPT_REQUESTS_TOTAL_BYTES_MAX);

pub fn prompt_content_fingerprint(content: &str) -> [u8; 32] {
    sha2::Sha256::digest(content.as_bytes()).into()
}

fn json_estimated_bytes(v: &J) -> usize {
    match v {
        J::Null => 4,
        J::Bool(_) => 5,
        J::Number(_) => 16,
        J::String(s) => s.len() + 2,
        J::Array(items) => 2 + items.iter().map(json_estimated_bytes).sum::<usize>(),
        J::Object(map) => 2 + map.iter().map(|(k, val)| k.len() + 4 + json_estimated_bytes(val)).sum::<usize>(),
    }
}

pub fn prompt_entry_bytes(rid: &str, answer: &J) -> usize {
    std::mem::size_of::<PromptRequestEntry>()
        + PROMPT_REQUEST_ENTRY_OVERHEAD
        + 2 * rid.len()
        + json_estimated_bytes(answer)
}

impl PromptRequestCache {
    pub fn new(per_session_max: usize, max_sessions: usize, max_total_entries: usize, max_total_bytes: usize) -> Self {
        Self {
            per_session_max,
            max_sessions,
            max_total_entries,
            max_total_bytes,
            total_entries: 0,
            total_bytes: 0,
            sessions: HashMap::new(),
            session_order: VecDeque::new(),
        }
    }

    fn touch_session(&mut self, sid: &str) {
        if let Some(pos) = self.session_order.iter().position(|s| s == sid) {
            self.session_order.remove(pos);
        }
        self.session_order.push_back(sid.to_string());
    }

    pub fn remove_session(&mut self, sid: &str) -> bool {
        if let Some(pos) = self.session_order.iter().position(|s| s == sid) {
            self.session_order.remove(pos);
        }
        if let Some(removed) = self.sessions.remove(sid) {
            self.total_entries = self.total_entries.saturating_sub(removed.entries.len());
            self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
            true
        } else {
            false
        }
    }

    fn evict_lru_session_except(&mut self, keep_sid: Option<&str>) -> bool {
        let Some(pos) = self.session_order.iter().position(|s| keep_sid.is_none_or(|keep| s != keep)) else {
            return false;
        };
        let Some(evict_sid) = self.session_order.remove(pos) else {
            return false;
        };
        if let Some(removed) = self.sessions.remove(&evict_sid) {
            self.total_entries = self.total_entries.saturating_sub(removed.entries.len());
            self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
        }
        true
    }

    fn pop_oldest_in_session(&mut self, sid: &str) -> bool {
        let Some(session) = self.sessions.get_mut(sid) else {
            return false;
        };
        while let Some(oldest) = session.order.pop_front() {
            if let Some(removed) = session.entries.remove(&oldest) {
                session.bytes = session.bytes.saturating_sub(removed.bytes);
                self.total_entries = self.total_entries.saturating_sub(1);
                self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
                return true;
            }
        }
        false
    }

    pub fn get(&mut self, sid: &str, rid: &str) -> Option<([u8; 32], J)> {
        let entry = self.sessions.get(sid)?.entries.get(rid)?;
        let found = (entry.fingerprint, entry.answer.clone());
        self.touch_session(sid);
        Some(found)
    }

    pub fn insert(&mut self, sid: String, rid: String, fingerprint: [u8; 32], answer: J) {
        if self.per_session_max == 0
            || self.max_sessions == 0
            || self.max_total_entries == 0
            || self.max_total_bytes == 0
        {
            return;
        }
        let entry_bytes = prompt_entry_bytes(&rid, &answer);
        if entry_bytes > self.max_total_bytes {
            return;
        }
        if let Some(session) = self.sessions.get_mut(&sid)
            && let Some(entry) = session.entries.get_mut(&rid)
        {
            let old_bytes = entry.bytes;
            *entry = PromptRequestEntry { fingerprint, answer, bytes: entry_bytes };
            session.bytes = session.bytes.saturating_sub(old_bytes).saturating_add(entry_bytes);
            self.total_bytes = self.total_bytes.saturating_sub(old_bytes).saturating_add(entry_bytes);
            while self.total_bytes > self.max_total_bytes {
                if self.evict_lru_session_except(Some(&sid)) {
                    continue;
                }
                if !self.pop_oldest_in_session(&sid) {
                    break;
                }
            }
            if self.sessions.contains_key(&sid) {
                self.touch_session(&sid);
            }
            return;
        }
        let mut is_new_session = true;
        if let Some(session) = self.sessions.get_mut(&sid) {
            is_new_session = false;
            while session.entries.len() >= self.per_session_max {
                if let Some(oldest) = session.order.pop_front() {
                    if let Some(removed) = session.entries.remove(&oldest) {
                        session.bytes = session.bytes.saturating_sub(removed.bytes);
                        self.total_entries = self.total_entries.saturating_sub(1);
                        self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
                    }
                } else {
                    break;
                }
            }
        }
        if is_new_session {
            while self.sessions.len() >= self.max_sessions {
                if !self.evict_lru_session_except(None) {
                    break;
                }
            }
        }
        while self.total_entries >= self.max_total_entries
            || self.total_bytes.saturating_add(entry_bytes) > self.max_total_bytes
        {
            if self.evict_lru_session_except(Some(&sid)) {
                continue;
            }
            if !self.pop_oldest_in_session(&sid) {
                break;
            }
        }
        let session = self.sessions.entry(sid.clone()).or_insert_with(|| SessionPromptRequests {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
        });
        session.order.push_back(rid.clone());
        session.bytes = session.bytes.saturating_add(entry_bytes);
        session.entries.insert(rid, PromptRequestEntry { fingerprint, answer, bytes: entry_bytes });
        self.total_entries += 1;
        self.total_bytes = self.total_bytes.saturating_add(entry_bytes);
        self.touch_session(&sid);
    }

    #[cfg(test)]
    fn remove(&mut self, sid: &str, rid: &str) {
        if let Some(session) = self.sessions.get_mut(sid)
            && let Some(removed) = session.entries.remove(rid)
        {
            if let Some(pos) = session.order.iter().position(|id| id == rid) {
                session.order.remove(pos);
            }
            session.bytes = session.bytes.saturating_sub(removed.bytes);
            self.total_entries = self.total_entries.saturating_sub(1);
            self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
            if session.entries.is_empty() {
                self.sessions.remove(sid);
                if let Some(pos) = self.session_order.iter().position(|s| s == sid) {
                    self.session_order.remove(pos);
                }
            }
        }
    }
}

/// Per-host / per-session prompt idempotency cache and pending-prompt registry,
/// replacing process-wide `thread_local!` `PROMPT_REQUESTS` / `PENDING_PROMPTS`.
pub struct SessionPromptReceipts {
    cache: RefCell<PromptRequestCache>,
    pending: RefCell<HashMap<(String, String), Rc<PendingPrompt>>>,
}

impl Default for SessionPromptReceipts {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionPromptReceipts {
    pub fn new() -> Self {
        Self::with_limits(
            PROMPT_REQUESTS_MAX,
            PROMPT_REQUEST_SESSIONS_MAX,
            PROMPT_REQUESTS_TOTAL_MAX,
            PROMPT_REQUESTS_TOTAL_BYTES_MAX,
        )
    }

    pub fn with_limits(
        per_session_max: usize,
        max_sessions: usize,
        max_total_entries: usize,
        max_total_bytes: usize,
    ) -> Self {
        Self {
            cache: RefCell::new(PromptRequestCache::new(
                per_session_max,
                max_sessions,
                max_total_entries,
                max_total_bytes,
            )),
            pending: RefCell::new(HashMap::new()),
        }
    }

    pub fn get(&self, sid: &str, rid: &str) -> Option<([u8; 32], J)> {
        self.cache.borrow_mut().get(sid, rid)
    }

    pub fn insert(&self, sid: String, rid: String, fingerprint: [u8; 32], answer: J) {
        self.cache.borrow_mut().insert(sid, rid, fingerprint, answer);
    }

    pub fn remove_session(&self, sid: &str) -> bool {
        self.pending.borrow_mut().retain(|(pending_sid, _), _| pending_sid != sid);
        self.cache.borrow_mut().remove_session(sid)
    }

    pub fn get_pending(&self, sid: &str, rid: &str) -> Option<Rc<PendingPrompt>> {
        self.pending.borrow().get(&(sid.to_string(), rid.to_string())).cloned()
    }

    pub fn insert_pending(&self, sid: String, rid: String, entry: Rc<PendingPrompt>) {
        self.pending.borrow_mut().insert((sid, rid), entry);
    }

    pub fn remove_pending(&self, sid: &str, rid: &str) {
        self.pending.borrow_mut().remove(&(sid.to_string(), rid.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_held_instances_are_isolated() {
        let a = InboundHeld::new(2);
        let b = InboundHeld::new(2);
        let _g1 = a.try_acquire().unwrap();
        let _g2 = a.try_acquire().unwrap();
        assert!(a.try_acquire().is_none());
        let g_b = b.try_acquire();
        assert!(g_b.is_some());
        drop(_g1);
        assert!(a.try_acquire().is_some());
    }

    #[test]
    fn prompt_receipts_instances_are_isolated() {
        let r1 = SessionPromptReceipts::new();
        let r2 = SessionPromptReceipts::new();
        let fp = prompt_content_fingerprint("hello");
        r1.insert("same-sid".into(), "rid-1".into(), fp, serde_json::json!({"started": true}));
        assert!(r1.get("same-sid", "rid-1").is_some());
        assert!(r2.get("same-sid", "rid-1").is_none());
    }

    #[test]
    fn prompt_entry_bytes_counts_both_key_copies_and_container_overhead() {
        let answer = serde_json::json!({"turnId": "t1"});
        let base = prompt_entry_bytes("", &answer);
        assert_eq!(
            base,
            std::mem::size_of::<PromptRequestEntry>() + PROMPT_REQUEST_ENTRY_OVERHEAD + json_estimated_bytes(&answer)
        );
        assert_eq!(prompt_entry_bytes("abcd", &answer) - base, 8);
        // The longest accepted requestId always fits the global budget, so it is always cached.
        let longest = "r".repeat(mare_protocol::PROMPT_REQUEST_ID_MAX_BYTES);
        let mut cache = PromptRequestCache::new(16, 16, 1024, PROMPT_REQUESTS_TOTAL_BYTES_MAX);
        cache.insert("s1".into(), longest.clone(), fp("c1"), answer.clone());
        assert_eq!(cache.get("s1", &longest).unwrap().1, answer);
        assert_eq!(cache.total_bytes, prompt_entry_bytes(&longest, &answer));
    }

    fn fp(s: &str) -> [u8; 32] {
        prompt_content_fingerprint(s)
    }

    #[test]
    fn prompt_request_cache_evicts_per_session_without_touching_other_sessions() {
        let mut cache = PromptRequestCache::new(3, 8, 5, 1024 * 1024);
        cache.insert("s1".into(), "r1".into(), fp("c1"), serde_json::json!({"turn": 1}));
        cache.insert("s2".into(), "r1".into(), fp("other-1"), serde_json::json!({"turn": 10}));
        cache.insert("s2".into(), "r2".into(), fp("other-2"), serde_json::json!({"turn": 11}));

        cache.insert("s1".into(), "r2".into(), fp("c2"), serde_json::json!({"turn": 2}));
        cache.insert("s1".into(), "r3".into(), fp("c3"), serde_json::json!({"turn": 3}));
        cache.insert("s1".into(), "r4".into(), fp("c4"), serde_json::json!({"turn": 4}));

        assert!(cache.get("s1", "r1").is_none());
        assert_eq!(cache.get("s1", "r2").unwrap().1, serde_json::json!({"turn": 2}));
        assert_eq!(cache.get("s1", "r3").unwrap().1, serde_json::json!({"turn": 3}));
        assert_eq!(cache.get("s1", "r4").unwrap().1, serde_json::json!({"turn": 4}));
        assert_eq!(cache.get("s2", "r1").unwrap().1, serde_json::json!({"turn": 10}));
        assert_eq!(cache.get("s2", "r2").unwrap().1, serde_json::json!({"turn": 11}));
        assert_eq!(cache.total_entries, 5);
    }

    #[test]
    fn prompt_request_cache_evicts_lru_session_on_global_session_limit() {
        let mut cache = PromptRequestCache::new(4, 2, 16, 1024 * 1024);
        cache.insert("s1".into(), "r1".into(), fp("c1"), serde_json::json!({"turn": 1}));
        cache.insert("s2".into(), "r1".into(), fp("c2"), serde_json::json!({"turn": 2}));
        assert_eq!(cache.get("s1", "r1").unwrap().1, serde_json::json!({"turn": 1}));

        cache.insert("s3".into(), "r1".into(), fp("c3"), serde_json::json!({"turn": 3}));
        assert!(cache.get("s2", "r1").is_none());
        assert_eq!(cache.get("s1", "r1").unwrap().1, serde_json::json!({"turn": 1}));
        assert_eq!(cache.get("s3", "r1").unwrap().1, serde_json::json!({"turn": 3}));
        assert_eq!(cache.sessions.len(), 2);
        assert_eq!(cache.total_entries, 2);
    }

    #[test]
    fn prompt_request_cache_evicts_lru_session_on_global_total_entry_budget() {
        let mut cache = PromptRequestCache::new(4, 10, 4, 1024 * 1024);
        cache.insert("s1".into(), "r1".into(), fp("c1"), serde_json::json!({"turn": 1}));
        cache.insert("s1".into(), "r2".into(), fp("c2"), serde_json::json!({"turn": 2}));
        cache.insert("s1".into(), "r3".into(), fp("c3"), serde_json::json!({"turn": 3}));
        cache.insert("s2".into(), "r1".into(), fp("d1"), serde_json::json!({"turn": 10}));
        assert_eq!(cache.total_entries, 4);

        cache.insert("s3".into(), "r1".into(), fp("e1"), serde_json::json!({"turn": 20}));
        assert!(cache.get("s1", "r1").is_none());
        assert_eq!(cache.get("s2", "r1").unwrap().1, serde_json::json!({"turn": 10}));
        assert_eq!(cache.get("s3", "r1").unwrap().1, serde_json::json!({"turn": 20}));
        assert_eq!(cache.total_entries, 2);
    }

    #[test]
    fn prompt_request_cache_evicts_lru_session_on_global_byte_budget() {
        let small = serde_json::json!({"turn": 1});
        let single_bytes = prompt_entry_bytes("r1", &small);
        let max_bytes = single_bytes * 2 + 16;
        let mut cache = PromptRequestCache::new(16, 16, 1024, max_bytes);

        cache.insert("s1".into(), "r1".into(), fp("c1"), small.clone());
        cache.insert("s2".into(), "r1".into(), fp("c2"), small.clone());
        assert_eq!(cache.total_entries, 2);
        assert!(cache.total_bytes <= max_bytes);
        assert!(cache.get("s1", "r1").is_some());

        cache.insert("s3".into(), "r1".into(), fp("c3"), small);
        assert!(cache.get("s2", "r1").is_none());
        assert!(cache.get("s1", "r1").is_some());
        assert!(cache.get("s3", "r1").is_some());
        assert!(cache.total_bytes <= max_bytes);
    }

    #[test]
    fn prompt_request_cache_skips_oversized_entry_without_evicting_existing() {
        let small = serde_json::json!({"turn": 1});
        let max_bytes = prompt_entry_bytes("r1", &small) + 32;
        let mut cache = PromptRequestCache::new(16, 16, 1024, max_bytes);
        cache.insert("s1".into(), "r1".into(), fp("c1"), small.clone());

        let huge_answer = serde_json::json!({"turn": 2, "note": "x".repeat(max_bytes)});
        cache.insert("s2".into(), "r2".into(), fp("c2"), huge_answer);

        assert_eq!(cache.get("s1", "r1").unwrap().1, small);
        assert!(cache.get("s2", "r2").is_none());
        assert!(cache.total_bytes <= max_bytes);
    }

    #[test]
    fn prompt_request_cache_stores_fixed_fingerprint_for_large_content() {
        let answer = serde_json::json!({"turnId": "t1", "duplicate": true});
        let max_bytes = prompt_entry_bytes("r1", &answer) + 64;
        let mut cache = PromptRequestCache::new(16, 16, 1024, max_bytes);
        let large_prompt = "a".repeat(1024 * 1024);
        cache.insert("s1".into(), "r1".into(), fp(&large_prompt), answer.clone());

        let (stored_fp, stored_answer) = cache.get("s1", "r1").unwrap();
        assert_eq!(stored_fp, fp(&large_prompt));
        assert_ne!(stored_fp, fp("different"));
        assert_eq!(stored_answer, answer);
        assert_eq!(cache.total_bytes, prompt_entry_bytes("r1", &answer));
    }

    #[test]
    fn prompt_request_cache_remove_session_clears_entries_and_frees_budget() {
        let mut cache = PromptRequestCache::new(4, 2, 4, 1024 * 1024);
        cache.insert("s1".into(), "r1".into(), fp("c1"), serde_json::json!({"turn": 1}));
        cache.insert("s1".into(), "r2".into(), fp("c2"), serde_json::json!({"turn": 2}));
        cache.insert("s2".into(), "r1".into(), fp("d1"), serde_json::json!({"turn": 10}));
        assert_eq!(cache.total_entries, 3);
        let before_bytes = cache.total_bytes;

        assert!(cache.remove_session("s1"));
        assert!(!cache.remove_session("s1"));
        assert!(cache.get("s1", "r1").is_none());
        assert_eq!(cache.total_entries, 1);
        assert!(cache.total_bytes < before_bytes);
        assert_eq!(cache.sessions.len(), 1);
        assert_eq!(cache.get("s2", "r1").unwrap().1, serde_json::json!({"turn": 10}));

        cache.insert("s3".into(), "r1".into(), fp("e1"), serde_json::json!({"turn": 20}));
        assert_eq!(cache.get("s2", "r1").unwrap().1, serde_json::json!({"turn": 10}));
        assert_eq!(cache.get("s3", "r1").unwrap().1, serde_json::json!({"turn": 20}));
        assert_eq!(cache.total_entries, 2);
    }

    #[test]
    fn prompt_request_cache_remove_cleans_order_and_bounds_capacity() {
        let mut cache = PromptRequestCache::new(4, 4, 16, PROMPT_REQUESTS_TOTAL_BYTES_MAX);
        let answer = serde_json::json!({"started": true, "turnId": "t1"});
        for i in 0..100 {
            let rid = format!("r-{i}");
            cache.insert("s1".into(), rid.clone(), prompt_content_fingerprint(&rid), answer.clone());
            cache.remove("s1", &rid);
        }
        assert!(cache.sessions.is_empty(), "empty session must be removed after all entries removed");
        assert!(cache.session_order.is_empty(), "session_order must be cleaned when session becomes empty");
        assert_eq!(cache.total_entries, 0);
        assert_eq!(cache.total_bytes, 0);

        // Removing and re-inserting the same rid must not leave a stale order entry that evicts it early.
        cache.insert("s1".into(), "reused".into(), prompt_content_fingerprint("reused"), answer.clone());
        cache.remove("s1", "reused");
        cache.insert("s1".into(), "a".into(), prompt_content_fingerprint("a"), answer.clone());
        cache.insert("s1".into(), "b".into(), prompt_content_fingerprint("b"), answer.clone());
        cache.insert("s1".into(), "c".into(), prompt_content_fingerprint("c"), answer.clone());
        cache.insert("s1".into(), "reused".into(), prompt_content_fingerprint("reused"), answer.clone());
        assert!(cache.get("s1", "reused").is_some(), "reused rid must not be prematurely evicted");
    }
}
