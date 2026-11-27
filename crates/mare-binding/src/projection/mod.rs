//! Explicit, VM-local sensitive value projection. No environment discovery or persistence.
use mlua::{Lua, Table, UserData, UserDataFields, UserDataMethods, Value};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

mod json;
mod lua_walk;
mod stream;
#[cfg(test)]
mod tests;

pub use json::{JsonProjector, authorize, restore_json};
pub use lua_walk::{project, project_lossy_bounded, project_preserving_bounded};

pub(super) const PREFIX: &str = "[[mare-value:";

#[derive(Clone, Default)]
pub(super) struct Registry {
    pub(super) entries: Vec<Entry>,
    pub(super) revision: u64,
}

#[derive(Clone)]
pub(super) struct Entry {
    pub(super) value: String,
    pub(super) reference: String,
    pub(super) active: bool,
}

#[derive(Clone)]
pub(super) struct State(
    pub(super) Arc<Mutex<Registry>>,
    pub(super) tokio::sync::watch::Sender<u64>,
    pub(super) Arc<AtomicBool>,
);

impl Default for State {
    fn default() -> Self {
        Self(
            Arc::new(Mutex::new(Registry::default())),
            tokio::sync::watch::channel(0).0,
            Arc::new(AtomicBool::new(false)),
        )
    }
}

struct Changes {
    receiver: tokio::sync::Mutex<tokio::sync::watch::Receiver<u64>>,
    closed: tokio_util::sync::CancellationToken,
}

impl UserData for Changes {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("close", |_, this, ()| {
            this.closed.cancel();
            Ok(())
        });
        m.add_async_method("recv", |_, this, cancel: Option<mlua::AnyUserData>| async move {
            let cancel =
                cancel.map(|u| u.borrow::<crate::task::Token>().map(|t| t.0.clone())).transpose()?.unwrap_or_default();
            let mut receiver = tokio::select! {
                biased;
                _ = this.closed.cancelled() => return Ok(None),
                _ = cancel.cancelled() => return Ok(None),
                receiver = this.receiver.lock() => receiver,
            };
            tokio::select! {
                biased;
                _ = this.closed.cancelled() => Ok(None),
                _ = cancel.cancelled() => Ok(None),
                changed = receiver.changed() => {
                    if changed.is_err() { Ok(None) } else { Ok(Some(*receiver.borrow_and_update())) }
                }
            }
        });
    }
}

impl Drop for Changes {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

pub(super) fn error(message: &str) -> mlua::Error {
    mlua::Error::runtime(message)
}

struct Lease {
    state: State,
    reference: String,
}

impl Lease {
    fn close(&self) {
        let mut r = self.state.0.lock().expect("projection registry");
        if let Some(e) = r.entries.iter_mut().find(|e| e.reference == self.reference)
            && e.active
        {
            e.active = false;
            r.revision += 1;
            self.state.1.send_replace(r.revision);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.close();
    }
}

impl UserData for Lease {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_field_method_get("reference", |_, this| Ok(this.reference.clone()));
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("close", |_, this, ()| {
            this.close();
            Ok(())
        });
    }
}

impl Registry {
    /// Project characters up to `cut` (a valid UTF-8 byte boundary in `input`) using full `input`
    /// for longest-match lookahead. Returns `(projected, consumed_bytes)`.
    pub(super) fn project_prefix(&self, input: &str, cut: usize) -> (String, usize) {
        let mut consumed = 0;
        let mut out = String::new();
        while consumed < cut {
            let rest = &input[consumed..];
            // References are opaque; never run literal replacement inside them.
            if rest.starts_with(PREFIX)
                && let Some(end) = rest.find("]]")
                && self.entries.iter().any(|e| e.reference == rest[..end + 2])
            {
                out.push_str(&rest[..end + 2]);
                consumed += end + 2;
                continue;
            }
            let found = self
                .entries
                .iter()
                .rev()
                .filter(|e| rest.starts_with(&e.value))
                .max_by_key(|e| (e.value.len(), e.active));
            if let Some(e) = found {
                out.push_str(&e.reference);
                consumed += e.value.len();
            } else {
                let c = rest.chars().next().expect("nonempty");
                out.push(c);
                consumed += c.len_utf8();
            }
        }
        (out, consumed)
    }

    pub(super) fn project(&self, input: &str) -> String {
        if self.entries.is_empty() {
            return input.to_owned();
        }
        self.project_prefix(input, input.len()).0
    }

    pub(super) fn restore(&self, input: &str) -> mlua::Result<String> {
        let mut out = String::new();
        let mut rest = input;
        while let Some(start) = rest.find(PREFIX) {
            out.push_str(&rest[..start]);
            rest = &rest[start..];
            let end = rest.find("]]").ok_or_else(|| error("PROJECTION_UNKNOWN: malformed value reference"))? + 2;
            let reference = &rest[..end];
            let e = self
                .entries
                .iter()
                .find(|e| e.active && e.reference == reference)
                .ok_or_else(|| error("PROJECTION_UNKNOWN: unknown or revoked value reference"))?;
            out.push_str(&e.value);
            rest = &rest[end..];
        }
        out.push_str(rest);
        Ok(out)
    }

    /// Project registered values inside each valid UTF-8 run, keeping invalid bytes verbatim.
    /// A registered value is valid UTF-8, so every occurrence lies within one valid run.
    pub(super) fn project_bytes(&self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for chunk in bytes.utf8_chunks() {
            out.extend_from_slice(self.project(chunk.valid()).as_bytes());
            out.extend_from_slice(chunk.invalid());
        }
        out
    }
}

pub(super) fn state(lua: &Lua) -> State {
    lua.app_data_ref::<State>().expect("projection installed").clone()
}

pub(super) fn registry_or_default(lua: &Lua) -> mlua::Result<Option<Registry>> {
    let Some(state) = lua.app_data_ref::<State>() else {
        return Ok(None);
    };
    Ok(Some(state.0.lock().map_err(|_| error("projection registry unavailable"))?.clone()))
}

pub(super) fn snapshot_if_nonempty(reg: &Mutex<Registry>) -> Option<Registry> {
    let guard = reg.lock().expect("projection registry unavailable");
    (!guard.entries.is_empty()).then(|| guard.clone())
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let state_val = State::default();
    lua.set_app_data(state_val.clone());
    let has_entries = Arc::clone(&state_val.2);
    let r_json = Arc::clone(&state_val.0);
    let r_keys = Arc::clone(&state_val.0);
    let r_str = Arc::clone(&state_val.0);
    let r_bytes = Arc::clone(&state_val.0);
    let r_stream = Arc::clone(&state_val.0);
    let revisions = state_val.1.clone();
    let projector = mare_shared::wire_projection::WireProjector::new(
        has_entries,
        Arc::new(move |value| {
            let Some(r) = snapshot_if_nonempty(&r_json) else { return };
            JsonProjector::from_registry(r).project_json(value);
        }),
        Arc::new(move |input| match snapshot_if_nonempty(&r_str) {
            Some(r) => r.project(input),
            None => input.to_owned(),
        }),
        Arc::new(move |bytes| match snapshot_if_nonempty(&r_bytes) {
            Some(r) => r.project_bytes(bytes),
            None => bytes.to_vec(),
        }),
        Arc::new(move || Box::new(stream::ByteStream::new(Arc::clone(&r_stream)))),
    )
    .with_revision(Arc::new(move || *revisions.borrow()))
    .with_key_check(Arc::new(move |value| {
        let Some(r) = snapshot_if_nonempty(&r_keys) else { return Ok(()) };
        JsonProjector::from_registry(r).check_json_keys(value)
    }));
    mare_shared::wire_projection::install(lua, projector);
    let api = lua.create_table()?;
    api.set(
        "register",
        lua.create_function(|lua, value: String| {
            if value.is_empty() || value.contains(PREFIX) || PREFIX.contains(&value) {
                return Err(error(
                    "PROJECTION_INVALID: value must be nonempty UTF-8 without reserved-reference overlap",
                ));
            }
            let state = state(lua);
            let reference;
            {
                let mut r = state.0.lock().map_err(|_| error("projection registry unavailable"))?;

                if r.entries.iter().any(|e| e.reference.contains(&value)) {
                    return Err(error("PROJECTION_INVALID: value overlaps an existing reference"));
                }
                if r.entries.iter().any(|e| e.active && e.value == value) {
                    return Err(error("PROJECTION_INVALID: value already registered"));
                }
                reference = (0..16)
                    .find_map(|_| {
                        let candidate = format!("{PREFIX}{}]]", uuid::Uuid::new_v4());
                        (!candidate.contains(&value)
                            && !r.entries.iter().any(|e| e.reference == candidate || candidate.contains(&e.value)))
                        .then_some(candidate)
                    })
                    .ok_or_else(|| error("PROJECTION_COLLISION: could not allocate a safe reference"))?;
                r.entries.push(Entry { value, reference: reference.clone(), active: true });
                r.revision += 1;
                state.2.store(true, Ordering::Release);
                state.1.send_replace(r.revision);
            }
            Ok(Lease { state, reference })
        })?,
    )?;
    lua_walk::install(lua, &api)?;
    json::install(lua, &api)?;
    api.set(
        "revision",
        lua.create_function(|lua, ()| {
            Ok(state(lua).0.lock().map_err(|_| error("projection registry unavailable"))?.revision)
        })?,
    )?;
    // Includes revoked tombstones: output from a cancelled task can still arrive afterwards.
    api.set(
        "active",
        lua.create_function(|lua, ()| {
            Ok(!state(lua).0.lock().map_err(|_| error("projection registry unavailable"))?.entries.is_empty())
        })?,
    )?;
    api.set("contains", lua.create_function(|lua, v: Value| lua_walk::contains_marker(lua, v))?)?;
    api.set(
        "subscribe",
        lua.create_function(|lua, ()| {
            Ok(Changes {
                receiver: tokio::sync::Mutex::new(state(lua).1.subscribe()),
                closed: tokio_util::sync::CancellationToken::new(),
            })
        })?,
    )?;
    stream::install(lua, &api)?;
    mare.set("projection", api)
}
