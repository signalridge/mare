//! Non-yielding baseline snapshot cut, registry, and paged `baseline.read` lifecycle.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use mare_protocol::{BaselineReadResult, BaselineSnapshotParams, SubscribeMode};
use mlua::{Function, Value};
use serde_json::Value as Json;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::Kernel;
use crate::dispatch::tap::{BaselineAnchor, ReliableFilter, SubscribeCursorOutcome};

/// Idle expiry for an unread baseline.
pub const BASELINE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum encoded page payload returned by one `baseline.read` call, keeping JSON-RPC responses
/// well within single-message frame limits (`PEER_MAX_LINE`).
pub const BASELINE_PAGE_MAX_BYTES: usize = 512 * 1024;

/// A connection's live permission to page a baseline from this kernel's effective config.
pub type BaselineAccess = Arc<dyn Fn(&Json) -> bool + Send + Sync>;

struct BaselinePart {
    page: Function,
    release: Option<Function>,
    done: bool,
}

pub(crate) struct BaselineEntry {
    baseline_id: String,
    owner_ctx: u64,
    session_epoch: String,
    parts: BTreeMap<String, BaselinePart>,
    last_touched: Instant,
    access: BaselineAccess,
}

fn release_parts(parts: impl IntoIterator<Item = BaselinePart>) {
    for part in parts {
        if let Some(release) = part.release {
            let _ = release.call::<()>(());
        }
    }
}

impl BaselineEntry {
    fn release_hooks(self) {
        release_parts(self.parts.into_values());
    }
}

#[derive(Default)]
pub(crate) struct BaselineRegistry {
    next_id: u64,
    by_id: HashMap<String, BaselineEntry>,
    active_by_conn: HashMap<u64, String>,
}

impl BaselineRegistry {
    fn next_baseline_id(&mut self, session_epoch: &str) -> String {
        self.next_id += 1;
        format!("bl-{session_epoch}-{}", self.next_id)
    }

    fn remove_by_id(&mut self, baseline_id: &str) -> Option<BaselineEntry> {
        let entry = self.by_id.remove(baseline_id)?;
        if self.active_by_conn.get(&entry.owner_ctx).is_some_and(|id| id == baseline_id) {
            self.active_by_conn.remove(&entry.owner_ctx);
        }
        Some(entry)
    }

    fn install(&mut self, entry: BaselineEntry, now: Instant, current_epoch: &str) -> Vec<BaselineEntry> {
        let mut stale = Vec::new();
        let stale_ids: Vec<String> = self
            .by_id
            .iter()
            .filter(|(_, existing)| {
                now.saturating_duration_since(existing.last_touched) >= BASELINE_IDLE_TIMEOUT
                    || existing.session_epoch != current_epoch
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale_ids {
            if let Some(removed) = self.remove_by_id(&id) {
                stale.push(removed);
            }
        }
        if let Some(old_id) = self.active_by_conn.insert(entry.owner_ctx, entry.baseline_id.clone())
            && let Some(old) = self.by_id.remove(&old_id)
        {
            stale.push(old);
        }
        self.by_id.insert(entry.baseline_id.clone(), entry);
        stale
    }

    fn remove_owner(&mut self, owner_ctx: u64) -> Vec<BaselineEntry> {
        let ids: Vec<String> =
            self.by_id.iter().filter(|(_, e)| e.owner_ctx == owner_ctx).map(|(id, _)| id.clone()).collect();
        ids.into_iter().filter_map(|id| self.remove_by_id(&id)).collect()
    }

    fn remove_unauthorized(&mut self, config: &Json) -> Vec<BaselineEntry> {
        let ids: Vec<String> =
            self.by_id.iter().filter(|(_, e)| !(e.access)(config)).map(|(id, _)| id.clone()).collect();
        ids.into_iter().filter_map(|id| self.remove_by_id(&id)).collect()
    }

    fn drain_all(&mut self) -> Vec<BaselineEntry> {
        self.active_by_conn.clear();
        self.by_id.drain().map(|(_, e)| e).collect()
    }
}

#[derive(Default)]
struct CutParts {
    part_names: Vec<String>,
    parts: BTreeMap<String, BaselinePart>,
    retained: BTreeMap<String, Json>,
}

impl Kernel {
    /// Discover `(service, method)` pairs declared with `expose = "lua"`, `baseline = true` on
    /// currently mounted services, in deterministic order.
    pub fn baseline_methods(&self) -> Vec<(String, String)> {
        let st = self.st();
        let mut out: Vec<(String, String)> = st
            .services
            .iter()
            .filter(|(_, entry)| !entry.lifetime.is_cancelled())
            .flat_map(|(service, entry)| {
                entry
                    .contract
                    .methods
                    .iter()
                    .filter(|(_, spec)| spec.baseline)
                    .map(move |(method, _)| (service.clone(), method.clone()))
            })
            .collect();
        out.sort();
        out
    }

    /// Run all `baseline = true` cut hooks synchronously (non-yielding on the Lua thread).
    fn run_baseline_cuts(&self) -> mlua::Result<CutParts> {
        let baseline_methods = self.baseline_methods();
        let mut invocations: Vec<(mlua::Table, Function)> = Vec::with_capacity(baseline_methods.len());
        {
            let st = self.st();
            for (service, method) in &baseline_methods {
                let Some(entry) = st.live_service(service) else { continue };
                let Value::Table(table) = entry.value.clone() else {
                    return Err(mlua::Error::runtime(format!("baseline service `{service}` must be a table")));
                };
                let func: Function = table
                    .get(method.as_str())
                    .map_err(|_| mlua::Error::runtime(format!("baseline method `{service}.{method}` missing")))?;
                invocations.push((table, func));
            }
        }

        let mut part_names = Vec::new();
        let mut parts = BTreeMap::new();
        let mut retained = BTreeMap::new();

        for (table, func) in invocations {
            // Synchronous `Function::call`: Lua yielding is refused, and peer-initiated
            // `events.subscribe` cuts are authorized at the protocol boundary.
            let res: Value = func.call((table, Value::Nil))?;
            let Value::Table(res_table) = res else {
                continue;
            };
            if let Some(ret) = res_table.get::<Option<Value>>("retained")?
                && !ret.is_nil()
                && let Json::Object(map) = mare_shared::lua::lua_to_json(self.lua(), ret)?
            {
                for (k, v) in map {
                    retained.insert(k, v);
                }
            }
            if let Some(part_list) = res_table.get::<Option<mlua::Table>>("parts")? {
                for item in part_list.sequence_values::<mlua::Table>() {
                    let item = item?;
                    let name: String = item.get("name")?;
                    let page: Function = item.get("page")?;
                    let release: Option<Function> = item.get("release")?;
                    if !parts.contains_key(&name) {
                        part_names.push(name.clone());
                    }
                    if let Some(replaced) = parts.insert(name, BaselinePart { page, release, done: false }) {
                        release_parts([replaced]);
                    }
                }
            }
        }

        Ok(CutParts { part_names, parts, retained })
    }

    fn build_snapshot(
        &self,
        owner_ctx: u64,
        anchor: BaselineAnchor,
        cut: CutParts,
        extra_retained: BTreeMap<String, Json>,
        page_access: Option<BaselineAccess>,
    ) -> BaselineSnapshotParams {
        let mut retained = BTreeMap::new();
        retained.insert("events".to_string(), Json::Object(anchor.retained.into_iter().collect()));
        for (k, v) in cut.retained {
            retained.insert(k, v);
        }
        for (k, v) in extra_retained {
            retained.insert(k, v);
        }

        let mut part_names = cut.part_names;
        let baseline_id = if let Some(access) = page_access.filter(|access| access(&self.config())) {
            let now = Instant::now();
            let (_, current_epoch) = self.0.event_log.session_identity();
            let mut reg = self.0.baselines.lock().unwrap_or_else(|e| e.into_inner());
            let baseline_id = reg.next_baseline_id(&anchor.session_epoch);
            let entry = BaselineEntry {
                baseline_id: baseline_id.clone(),
                owner_ctx,
                session_epoch: anchor.session_epoch.clone(),
                parts: cut.parts,
                last_touched: now,
                access,
            };
            let released = reg.install(entry, now, &current_epoch);
            drop(reg);
            for old in released {
                old.release_hooks();
            }
            Some(baseline_id)
        } else {
            let old = {
                let mut reg = self.0.baselines.lock().unwrap_or_else(|e| e.into_inner());
                let id = reg.active_by_conn.get(&owner_ctx).cloned();
                id.and_then(|id| reg.remove_by_id(&id))
            };
            if let Some(old) = old {
                old.release_hooks();
            }
            release_parts(cut.parts.into_values());
            part_names.clear();
            None
        };

        BaselineSnapshotParams {
            session_id: anchor.session_id,
            session_epoch: anchor.session_epoch,
            seq: anchor.seq,
            baseline_id,
            retained,
            parts: part_names,
        }
    }

    /// Atomically subscribe a cursor; when entering `Baseline`, cut SPI parts + event log `seq = S`
    /// in one non-yielding step on the Lua thread and register the new active baseline.
    #[allow(clippy::too_many_arguments)]
    pub fn subscribe_with_baseline(
        &self,
        owner_ctx: u64,
        requested_epoch: Option<&str>,
        last_seq: Option<u64>,
        live_only: bool,
        wants: ReliableFilter,
        wake: Option<std::sync::Arc<Notify>>,
        retain_filter: &dyn Fn(&str) -> bool,
        extra_retained: BTreeMap<String, Json>,
        include_state: bool,
        page_access: Option<BaselineAccess>,
    ) -> mlua::Result<(SubscribeCursorOutcome, Option<BaselineSnapshotParams>)> {
        let needs_baseline = self.0.event_log.will_need_baseline(requested_epoch, last_seq, live_only);
        if !needs_baseline {
            let outcome = self.0.event_log.subscribe_cursor(requested_epoch, last_seq, live_only, wants, wake);
            self.taps_changed();
            return Ok((outcome, None));
        }

        let cut = if include_state { self.run_baseline_cuts()? } else { CutParts::default() };
        let (outcome, session_id, events_retained) = self.0.event_log.subscribe_cursor_with_retained(
            requested_epoch,
            last_seq,
            live_only,
            wants,
            wake,
            retain_filter,
        );
        self.taps_changed();
        if outcome.mode != SubscribeMode::Baseline {
            release_parts(cut.parts.into_values());
            return Ok((outcome, None));
        }
        let anchor = BaselineAnchor {
            session_id,
            session_epoch: outcome.session_epoch.clone(),
            seq: outcome.seq.saturating_sub(1),
            retained: events_retained,
        };
        let snapshot = self.build_snapshot(owner_ctx, anchor, cut, extra_retained, page_access);
        Ok((outcome, Some(snapshot)))
    }

    /// Re-cut a new baseline and atomically re-anchor an evicted live cursor to `S' + 1`.
    #[allow(clippy::too_many_arguments)]
    pub fn reanchor_with_baseline(
        &self,
        owner_ctx: u64,
        cursor_id: u64,
        retain_filter: &dyn Fn(&str) -> bool,
        extra_retained: BTreeMap<String, Json>,
        include_state: bool,
        page_access: Option<BaselineAccess>,
    ) -> mlua::Result<Option<BaselineSnapshotParams>> {
        let cut = if include_state { self.run_baseline_cuts()? } else { CutParts::default() };
        let Some(anchor) = self.0.event_log.reanchor_cursor_at_tail(cursor_id, retain_filter) else {
            release_parts(cut.parts.into_values());
            return Ok(None);
        };
        Ok(Some(self.build_snapshot(owner_ctx, anchor, cut, extra_retained, page_access)))
    }

    /// Read one page of a baseline part for the protocol-layer `baseline.read` method.
    /// Returns coded `baseline.stale` when the handle is missing, owned by another connection,
    /// expired (idle >= 60 s), invalidated by an epoch change, unauthorized, or already finished.
    pub fn read_baseline_page(
        &self,
        owner_ctx: u64,
        baseline_id: &str,
        part: &str,
        cursor: Option<&str>,
        limit: Option<u64>,
    ) -> Result<BaselineReadResult, super::service_error::ServiceCallError> {
        use super::service_error::ServiceCallError;

        if baseline_id.is_empty() || part.is_empty() {
            return Err(ServiceCallError::Arguments("baseline_id and part must be non-empty strings".into()));
        }
        if limit == Some(0) {
            return Err(ServiceCallError::Arguments("limit must be an integer >= 1".into()));
        }
        let stale = || {
            ServiceCallError::coded(
                "baseline.stale",
                "baseline handle is stale; re-subscribe to obtain a fresh baseline",
            )
        };
        let config = self.config();
        let now = Instant::now();
        let page_fn = {
            let mut reg = self.0.baselines.lock().unwrap_or_else(|e| e.into_inner());
            let Some(entry) = reg.by_id.get(baseline_id) else {
                return Err(stale());
            };
            if entry.owner_ctx != owner_ctx {
                return Err(stale());
            }
            let expired = now.saturating_duration_since(entry.last_touched) >= BASELINE_IDLE_TIMEOUT;
            let (_, current_epoch) = self.0.event_log.session_identity();
            let epoch_changed = current_epoch != entry.session_epoch;
            let unauthorized = !(entry.access)(&config);
            if expired || epoch_changed || unauthorized {
                let removed = reg.remove_by_id(baseline_id);
                drop(reg);
                if let Some(removed) = removed {
                    removed.release_hooks();
                }
                return Err(stale());
            }
            let Some(part_entry) = entry.parts.get(part) else {
                return Err(ServiceCallError::coded(
                    "baseline.unknown_part",
                    format!("unknown baseline part `{part}`"),
                ));
            };
            if part_entry.done {
                return Err(stale());
            }
            let page = part_entry.page.clone();
            if let Some(entry_mut) = reg.by_id.get_mut(baseline_id) {
                entry_mut.last_touched = now;
            }
            page
        };

        let lua = self.lua();
        let cursor_arg = match cursor {
            Some(c) => Value::String(lua.create_string(c).map_err(ServiceCallError::from)?),
            None => Value::Nil,
        };
        let limit_arg = match limit {
            Some(n) => Value::Integer(i64::try_from(n).unwrap_or(i64::MAX)),
            None => Value::Nil,
        };
        let res: Value = page_fn.call((cursor_arg, BASELINE_PAGE_MAX_BYTES, limit_arg)).map_err(|err| {
            let mut declared = BTreeMap::new();
            declared
                .insert("item_too_large".to_owned(), super::contracts::ErrorSpec { doc: String::new(), data: None });
            declared.insert("unknown_part".to_owned(), super::contracts::ErrorSpec { doc: String::new(), data: None });
            declared.insert("stale".to_owned(), super::contracts::ErrorSpec { doc: String::new(), data: None });
            super::service_error::settle_error(lua, "baseline", "read", &declared, Value::Error(Box::new(err)))
        })?;
        // The page stays owned by the baseline until the exported value passes strict JSON
        // encoding and schema deserialization (`oversized_final_baseline_page_does_not_consume_the_handle`).
        let json =
            mare_shared::lua::strict_lua_to_json(lua, res).map_err(|e| ServiceCallError::Result(e.to_string()))?;
        let parsed: BaselineReadResult =
            serde_json::from_value(json).map_err(|e| ServiceCallError::Result(e.to_string()))?;

        if parsed.next_cursor.is_none() {
            let mut release_to_run = None;
            let mut full_entry_to_release = None;
            {
                let mut reg = self.0.baselines.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = reg.by_id.get_mut(baseline_id) {
                    if let Some(part_entry) = entry.parts.get_mut(part) {
                        part_entry.done = true;
                        release_to_run = part_entry.release.take();
                    }
                    if entry.parts.values().all(|p| p.done) {
                        full_entry_to_release = reg.remove_by_id(baseline_id);
                    }
                }
            }
            if let Some(rel) = release_to_run {
                let _ = rel.call::<()>(());
            }
            if let Some(entry) = full_entry_to_release {
                entry.release_hooks();
            }
        }
        Ok(parsed)
    }

    pub fn release_baselines_for_owner(&self, owner_ctx: u64) {
        let removed = self.0.baselines.lock().unwrap_or_else(|e| e.into_inner()).remove_owner(owner_ctx);
        for entry in removed {
            entry.release_hooks();
        }
    }

    pub fn owns_baseline(&self, owner_ctx: u64, baseline_id: &str) -> bool {
        self.0
            .baselines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .by_id
            .get(baseline_id)
            .is_some_and(|entry| entry.owner_ctx == owner_ctx)
    }

    pub(crate) fn release_unauthorized_baselines(&self, config: &Json) {
        let removed = self.0.baselines.lock().unwrap_or_else(|e| e.into_inner()).remove_unauthorized(config);
        for entry in removed {
            entry.release_hooks();
        }
    }

    pub fn release_all_baselines(&self) {
        let removed = self.0.baselines.lock().unwrap_or_else(|e| e.into_inner()).drain_all();
        for entry in removed {
            entry.release_hooks();
        }
    }
}
