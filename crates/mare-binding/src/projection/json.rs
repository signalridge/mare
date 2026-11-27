//! JSON projection, key checking, and native-authorized restoration.
use mlua::{Lua, Table, UserData, Value};
use std::{
    collections::HashSet,
    sync::atomic::{AtomicBool, Ordering},
};

use super::lua_walk::{InvalidUtf8, WalkState};
use super::{PREFIX, Registry, State, error, state};

#[derive(Clone, Default)]
pub struct JsonProjector(Option<Registry>);

impl JsonProjector {
    pub(super) fn from_registry(r: Registry) -> Self {
        Self(Some(r))
    }

    pub fn from_lua(lua: &Lua) -> mlua::Result<Self> {
        let Some(state) = lua.app_data_ref::<State>() else {
            return Ok(Self(None));
        };
        let r = state.0.lock().map_err(|_| error("projection registry unavailable"))?.clone();
        Ok(Self(if r.entries.is_empty() { None } else { Some(r) }))
    }

    pub fn check_json_keys(&self, value: &serde_json::Value) -> Result<(), String> {
        let Some(r) = &self.0 else {
            return Ok(());
        };
        fn walk(r: &Registry, value: &serde_json::Value) -> Result<(), String> {
            match value {
                serde_json::Value::Array(items) => {
                    for v in items {
                        walk(r, v)?;
                    }
                }
                serde_json::Value::Object(items) => {
                    for (key, v) in items {
                        if key.contains(PREFIX) || r.project(key) != *key {
                            return Err("PROJECTION_INVALID: sensitive or reserved table key".to_string());
                        }
                        walk(r, v)?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        walk(r, value)
    }

    pub fn project_json(&self, value: &mut serde_json::Value) {
        let Some(r) = &self.0 else {
            return;
        };
        fn walk(r: &Registry, value: &mut serde_json::Value) {
            match value {
                serde_json::Value::String(text) => *text = r.project(text),
                serde_json::Value::Array(items) => items.iter_mut().for_each(|v| walk(r, v)),
                serde_json::Value::Object(items) => items.values_mut().for_each(|v| walk(r, v)),
                _ => {}
            }
        }
        walk(r, value);
    }

    pub fn project_json_checked(&self, value: &mut serde_json::Value) -> mlua::Result<()> {
        self.check_json_keys(value).map_err(|e| error(&e))?;
        self.project_json(value);
        Ok(())
    }
}

/// Resolve a private native JSON copy for schema validation, without returning plaintext to Lua
/// or invoking any callback. Execution still resolves separately against the approved revision.
pub fn restore_json(lua: &Lua, mut value: serde_json::Value, revision: u64) -> mlua::Result<serde_json::Value> {
    fn walk(r: &Registry, value: &mut serde_json::Value, depth: usize, budget: &mut usize) -> mlua::Result<()> {
        if depth > 64 || *budget == 0 {
            return Err(error("PROJECTION_LIMIT: value exceeds validation limit"));
        }
        *budget -= 1;
        match value {
            serde_json::Value::String(text) => *text = r.restore(text)?,
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(r, item, depth + 1, budget)?;
                }
            }
            serde_json::Value::Object(items) => {
                for (key, item) in items {
                    if key.contains(PREFIX) || r.project(key) != *key {
                        return Err(error("PROJECTION_INVALID: sensitive or reserved table key"));
                    }
                    walk(r, item, depth + 1, budget)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let state =
        lua.app_data_ref::<State>().map(|s| s.clone()).ok_or_else(|| error("projection registry unavailable"))?;
    let r = state.0.lock().map_err(|_| error("projection registry unavailable"))?;
    if r.revision != revision {
        return Err(error("PROJECTION_CHANGED: value registrations changed"));
    }
    walk(&r, &mut value, 0, &mut 10_000)?;
    Ok(value)
}

// Derived only by native approval receipt consumption. Nothing in the Lua API can mint one.
struct RestoreAuthorization {
    arguments: serde_json::Value,
    revision: u64,
    snapshot: String,
    used: AtomicBool,
}

impl UserData for RestoreAuthorization {}

/// Bind the existing native human approval to one exact projected argument value. The caller
/// must first consume its native receipt against the arguments captured when the UI was issued.
/// This is a Rust-only bridge for that receipt owner, not another approval or Lua minting API.
pub fn authorize(
    lua: &Lua,
    arguments: serde_json::Value,
    revision: u64,
    snapshot: &str,
) -> mlua::Result<mlua::AnyUserData> {
    if snapshot.is_empty() {
        return Err(error("PROJECTION_AUTHORIZATION: missing approved snapshot"));
    }
    // Validate every reference now; revision is checked again immediately before restoration.
    restore_json(lua, arguments.clone(), revision)?;
    lua.create_userdata(RestoreAuthorization {
        arguments,
        revision,
        snapshot: snapshot.to_owned(),
        used: AtomicBool::new(false),
    })
}

pub(super) fn install(lua: &Lua, api: &Table) -> mlua::Result<()> {
    api.set(
        "restore",
        lua.create_function(|lua, (v, revision): (Value, u64)| {
            let context = mare_shared::exec_ctx::current(lua)
                .ok_or_else(|| error("PROJECTION_CONTEXT: restoration requires an approved execution context"))?;
            let authorization = context
                .get::<Option<mlua::AnyUserData>>("projection_authorization")?
                .ok_or_else(|| error("PROJECTION_AUTHORIZATION: restoration requires native human authorization"))?;
            let authorization = authorization
                .borrow::<RestoreAuthorization>()
                .map_err(|_| error("PROJECTION_AUTHORIZATION: invalid restoration capability"))?;
            if context.get::<Option<String>>("approval_snapshot")?.as_deref() != Some(authorization.snapshot.as_str())
                || context.get::<Option<u64>>("projection_version")? != Some(revision)
                || authorization.revision != revision
                || mare_shared::lua::strict_lua_to_json(lua, v.clone())? != authorization.arguments
            {
                return Err(error("PROJECTION_AUTHORIZATION: arguments are not bound to this approved execution"));
            }

            let state = state(lua);
            let r = state.0.lock().map_err(|_| error("projection registry unavailable"))?.clone();
            if r.revision != revision {
                return Err(error("PROJECTION_CHANGED: value registrations changed"));
            }
            if authorization.used.swap(true, Ordering::SeqCst) {
                return Err(error("PROJECTION_AUTHORIZATION: restoration capability was already consumed"));
            }
            let mut walk_state = WalkState {
                seen: HashSet::new(),
                budget: mare_shared::lua::HISTORY_LOSSLESS_MAX_NODES,
                invalid: InvalidUtf8::Reject,
            };
            let restored = r.walk(lua, v, true, 0, &mut walk_state)?;
            if state.0.lock().map_err(|_| error("projection registry unavailable"))?.revision != revision {
                return Err(error("PROJECTION_CHANGED: value registrations changed during restoration"));
            }
            Ok(restored)
        })?,
    )
}
