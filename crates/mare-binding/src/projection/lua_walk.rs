//! Lua value walking for projection, bounded lossy/preserving copies, and marker inspection.
use mlua::{Lua, Table, Value};
use std::collections::HashSet;

use super::{PREFIX, Registry, error, registry_or_default};

/// What a walk does with a string value that is not valid UTF-8 (keys always require UTF-8).
pub(super) enum InvalidUtf8<'a> {
    /// Fail with `PROJECTION_INVALID`.
    Reject,
    /// Replace with U+FFFD and set the flag (caller emits the visible warning).
    Replace(&'a mut bool),
    /// Keep the bytes; registered values inside its valid UTF-8 runs are still projected.
    Preserve,
}

pub(super) struct WalkState<'a> {
    pub(super) seen: HashSet<usize>,
    pub(super) budget: usize,
    pub(super) invalid: InvalidUtf8<'a>,
}

impl Registry {
    pub(super) fn walk(
        &self,
        lua: &Lua,
        v: Value,
        restore: bool,
        depth: usize,
        state: &mut WalkState<'_>,
    ) -> mlua::Result<Value> {
        if depth > 64 {
            return Err(error("PROJECTION_LIMIT: value exceeds depth limit"));
        }
        if state.budget == 0 {
            return Err(error("PROJECTION_LIMIT: value exceeds resource limit"));
        }
        state.budget -= 1;
        match v {
            Value::String(s) => {
                let text = match s.to_str() {
                    Ok(s) => s.to_owned(),
                    Err(_) => {
                        let flag = match &mut state.invalid {
                            InvalidUtf8::Reject => return Err(error("PROJECTION_INVALID: expected UTF-8 text")),
                            InvalidUtf8::Preserve => {
                                if self.entries.is_empty() {
                                    return Ok(Value::String(s));
                                }
                                return Ok(Value::String(lua.create_string(self.project_bytes(&s.as_bytes()))?));
                            }
                            InvalidUtf8::Replace(flag) => flag,
                        };
                        let bytes = s.as_bytes();
                        **flag = true;
                        tracing::warn!(
                            byte_len = bytes.len(),
                            "projection: lossily replacing non-UTF-8 bytes in string value"
                        );
                        let replaced = String::from_utf8_lossy(&bytes).into_owned();
                        let projected = if restore { self.restore(&replaced)? } else { self.project(&replaced) };
                        return Ok(Value::String(lua.create_string(projected)?));
                    }
                };
                if !restore && self.entries.is_empty() {
                    return Ok(Value::String(s));
                }
                let projected = if restore { self.restore(&text)? } else { self.project(&text) };
                Ok(Value::String(lua.create_string(projected)?))
            }
            Value::Table(t) => {
                let id = t.to_pointer() as usize;
                if !state.seen.insert(id) {
                    return Err(error("PROJECTION_INVALID: cyclic table"));
                }
                // Always a detached copy: callers publish it (e.g. into session memory) and may
                // keep mutating the original.
                let copy = lua.create_table()?;
                for pair in t.pairs::<Value, Value>() {
                    if state.budget == 0 {
                        return Err(error("PROJECTION_LIMIT: value exceeds resource limit"));
                    }
                    let (key, value) = pair?;
                    if let Value::String(ref s) = key {
                        let key = s.to_str().map_err(|_| error("PROJECTION_INVALID: expected UTF-8 key"))?;
                        if key.contains(PREFIX) || self.project(&key) != key.as_ref() {
                            return Err(error("PROJECTION_INVALID: sensitive or reserved table key"));
                        }
                    }
                    copy.raw_set(key, self.walk(lua, value, restore, depth + 1, state)?)?;
                }
                state.seen.remove(&id);
                copy.set_metatable(t.metatable())?;
                mare_shared::lua::copy_tool_schema_marker(lua, &t, &copy)?;
                Ok(Value::Table(copy))
            }
            other => Ok(other),
        }
    }
}

/// Project a detached copy. Non-text userdata/functions remain opaque and are not inspected.
pub fn project(lua: &Lua, value: Value) -> mlua::Result<Value> {
    let Some(r) = registry_or_default(lua)? else {
        return Ok(value);
    };
    // Do not hold a Rust mutex across Lua allocation: GC may close a registration lease.
    let mut state = WalkState {
        seen: HashSet::new(),
        budget: mare_shared::lua::HISTORY_LOSSLESS_MAX_NODES,
        invalid: InvalidUtf8::Reject,
    };
    r.walk(lua, value, false, 0, &mut state)
}

/// Project a detached copy with an explicit node budget and optional lossy UTF-8 replacement
/// for string values (keys still require valid UTF-8).
pub fn project_lossy_bounded(lua: &Lua, value: Value, max_nodes: usize) -> mlua::Result<(Value, bool)> {
    let r = registry_or_default(lua)?.unwrap_or_default();
    let mut lossy = false;
    let mut state = WalkState { seen: HashSet::new(), budget: max_nodes, invalid: InvalidUtf8::Replace(&mut lossy) };
    let projected = r.walk(lua, value, false, 0, &mut state)?;
    Ok((projected, lossy))
}

/// Project a detached copy with an explicit node budget, keeping non-UTF-8 string values
/// byte-for-byte (registered values in their valid runs are still projected). Keys still
/// require UTF-8. Session memory uses this: replacement happens only when a file is written.
pub fn project_preserving_bounded(lua: &Lua, value: Value, max_nodes: usize) -> mlua::Result<Value> {
    let r = registry_or_default(lua)?.unwrap_or_default();
    let mut state = WalkState { seen: HashSet::new(), budget: max_nodes, invalid: InvalidUtf8::Preserve };
    r.walk(lua, value, false, 0, &mut state)
}

pub(super) fn contains_marker(lua: &Lua, v: Value) -> mlua::Result<bool> {
    fn contains(v: Value, seen: &mut HashSet<usize>, depth: usize) -> mlua::Result<bool> {
        if depth > 64 {
            return Err(error("PROJECTION_LIMIT: value exceeds depth limit"));
        }
        match v {
            Value::String(s) => Ok(s.to_string_lossy().contains(PREFIX)),
            Value::Table(t) => {
                if !seen.insert(t.to_pointer() as usize) {
                    return Err(error("PROJECTION_INVALID: cyclic table"));
                }
                for pair in t.pairs::<Value, Value>() {
                    let (k, v) = pair?;
                    if contains(k, seen, depth + 1)? || contains(v, seen, depth + 1)? {
                        return Ok(true);
                    }
                }
                seen.remove(&(t.to_pointer() as usize));
                Ok(false)
            }
            _ => Ok(false),
        }
    }
    let _ = lua;
    contains(v, &mut HashSet::new(), 0)
}

pub(super) fn install(lua: &Lua, api: &Table) -> mlua::Result<()> {
    api.set(
        "project",
        lua.create_function(|lua, (value, opts): (Value, Option<Table>)| {
            let flag =
                |name: &str| opts.as_ref().and_then(|t| t.get::<Option<bool>>(name).ok().flatten()).unwrap_or(false);
            let (lossy, preserve) = (flag("lossy"), flag("preserve_invalid"));
            if lossy && preserve {
                return Err(error("PROJECTION_INVALID: lossy and preserve_invalid are exclusive"));
            }
            if preserve {
                let projected = project_preserving_bounded(lua, value, mare_shared::lua::HISTORY_LOSSLESS_MAX_NODES)?;
                Ok(mlua::MultiValue::from_vec(vec![projected]))
            } else if lossy {
                let (projected, was_lossy) =
                    project_lossy_bounded(lua, value, mare_shared::lua::HISTORY_LOSSLESS_MAX_NODES)?;
                Ok(mlua::MultiValue::from_vec(vec![projected, Value::Boolean(was_lossy)]))
            } else {
                Ok(mlua::MultiValue::from_vec(vec![project(lua, value)?]))
            }
        })?,
    )
}
