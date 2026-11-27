use mlua::{Lua, Table, Value};

use super::json::{json_to_lua, lua_to_json};

// ---- functions inside config ------------------------------------------------------------------
// Config travels as JSON (layering, diffing, /config), which cannot hold Lua functions. Values like
// `mare.plugin("x", { on_done = function() end })` are swapped for a sentinel string that points
// into a registry in the Lua state, and swapped back wherever config becomes Lua again.

pub(crate) const FN_SENTINEL: &str = "\u{1}mare-fn:";
const FN_REGISTRY: &str = "mare.config_fns";

pub(crate) fn fn_registry(lua: &Lua) -> mlua::Result<Table> {
    match lua.named_registry_value::<Value>(FN_REGISTRY)? {
        Value::Table(t) => Ok(t),
        _ => {
            let t = lua.create_table()?;
            lua.set_named_registry_value(FN_REGISTRY, t.clone())?;
            Ok(t)
        }
    }
}

/// Save the current function mapping so an unsuccessful configuration evaluation cannot
/// replace functions referenced by the active JSON configuration.
pub fn snapshot_config_fns(lua: &Lua) -> mlua::Result<Table> {
    let snapshot = lua.create_table()?;
    for pair in fn_registry(lua)?.pairs::<Value, Value>() {
        let (key, value) = pair?;
        snapshot.raw_set(key, value)?;
    }
    Ok(snapshot)
}

/// Restore a mapping saved by [`snapshot_config_fns`].
pub fn restore_config_fn_registry(lua: &Lua, snapshot: Table) -> mlua::Result<()> {
    lua.set_named_registry_value(FN_REGISTRY, snapshot)
}

/// Retire mappings no longer produced by the current evaluation. Old callbacks remain available
/// while some live Lua object still holds them, without keeping every past config alive forever.
pub fn retire_config_fns(lua: &Lua) -> mlua::Result<()> {
    let reg = fn_registry(lua)?;
    let retired = lua.create_table()?;
    let mt = lua.create_table()?;
    mt.raw_set("__mode", "v")?;
    retired.set_metatable(Some(mt))?;
    if let Value::Table(previous) = reg.raw_get::<Value>("__retired")? {
        for pair in previous.pairs::<Value, Value>() {
            let (key, value) = pair?;
            retired.raw_set(key, value)?;
        }
    }
    let mut live = std::collections::HashSet::new();
    if let Value::Table(seen) = reg.raw_get::<Value>("__seen")? {
        for pair in seen.pairs::<Value, Value>() {
            let (key, value) = pair?;
            if matches!(key, Value::Function(_))
                && let Value::String(id) = value
            {
                live.insert(id.to_str()?.to_string());
            }
        }
    }
    let mut stale = Vec::new();
    for pair in reg.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        if let (Value::String(id), Value::Function(f)) = (key, value)
            && !live.contains(id.to_str()?.as_ref())
        {
            stale.push((id, f));
        }
    }
    for (id, f) in stale {
        retired.raw_set(id.clone(), f)?;
        reg.raw_set(id, Value::Nil)?;
    }
    reg.raw_set("__retired", retired)
}

/// Start a config evaluation: sentinel ids restart, so the same definition site gets the same id
/// on every evaluation (a `/reload config` without edits then shows no spurious changes).
pub fn begin_config_eval(lua: &Lua) -> mlua::Result<()> {
    let registry = fn_registry(lua)?;
    let epoch = registry.raw_get::<Option<u64>>("__epoch")?.unwrap_or(0).wrapping_add(1);
    registry.raw_set("__epoch", epoch)?;
    registry.raw_set("__seen", lua.create_table()?)
}

/// The per-VM hash key prevents a config fingerprint from revealing low-entropy captured secrets.
struct ConfigFingerprintKey(std::collections::hash_map::RandomState);

/// Read one upvalue without exposing Lua's unsafe debug library to configuration code.
pub(crate) fn config_upvalue(lua: &Lua, f: &mlua::Function, index: usize) -> mlua::Result<(Value, String)> {
    // SAFETY: `exec_raw` supplies a locked Lua state and a protected stack. The first argument is
    // the function, the second is an integer index, and `lua_getupvalue` pushes at most one value.
    // Copy the upvalue name onto the Lua stack before removing the two arguments; mlua converts
    // both results and restores the stack on return.
    unsafe {
        lua.exec_raw((f.clone(), index as i32), |state| {
            let index = mlua::ffi::lua_tointeger(state, 2) as i32;
            let name = mlua::ffi::lua_getupvalue(state, 1, index);
            if name.is_null() {
                mlua::ffi::lua_pushnil(state);
                mlua::ffi::lua_pushliteral(state, c"");
            } else {
                mlua::ffi::lua_pushstring(state, name);
            }
            mlua::ffi::lua_remove(state, 1);
            mlua::ffi::lua_remove(state, 1);
        })
    }
}

/// Deterministically encode a closure and its inspectable upvalues. `None` means the value could
/// not be compared safely (cycle, alias, opaque handle, metatable, or size limit), so callers must
/// conservatively treat it as changed on the next configuration evaluation.
fn config_fingerprint_value(
    lua: &Lua,
    value: Value,
    stack: &mut Vec<usize>,
    seen: &mut std::collections::HashSet<usize>,
    budget: &mut usize,
    environment: bool,
) -> mlua::Result<Option<Vec<u8>>> {
    if *budget == 0 || stack.len() >= 32 {
        return Ok(None);
    }
    *budget -= 1;
    let mut result = Vec::new();
    let mut append = |part: &[u8]| {
        result.extend_from_slice(&(part.len() as u64).to_le_bytes());
        result.extend_from_slice(part);
    };
    match value {
        Value::Nil => append(b"nil"),
        Value::Boolean(v) => append(if v { b"true" } else { b"false" }),
        Value::Integer(v) => {
            append(b"integer");
            append(&v.to_le_bytes());
        }
        Value::Number(v) => {
            append(b"number");
            append(&v.to_bits().to_le_bytes());
        }
        Value::String(v) => {
            append(b"string");
            append(v.as_bytes().as_ref());
        }
        Value::Table(t) => {
            let isolated_environment = if environment {
                if let Some(mt) = t.metatable() {
                    let only_global_index = mt.raw_get::<Value>("__index")? == Value::Table(lua.globals())
                        && mt.clone().pairs::<Value, Value>().count() == 1;
                    only_global_index && t.raw_get::<Value>("_G")? == Value::Table(t.clone())
                } else {
                    false
                }
            } else {
                false
            };
            if t.metatable().is_some() && !isolated_environment {
                return Ok(None);
            }
            let pointer = t.to_pointer() as usize;
            // A repeated reference can change equality even when both tables have the same
            // contents. Treat aliases as opaque instead of collapsing their identities.
            if !seen.insert(pointer) {
                return Ok(None);
            }
            stack.push(pointer);
            let mut entries = Vec::new();
            for pair in t.pairs::<Value, Value>() {
                let (key, value) = pair?;
                // Isolated environments contain a self-reference and fall back to live globals.
                if isolated_environment && matches!(&key, Value::String(s) if s.as_bytes().as_ref() == b"_G") {
                    continue;
                }
                // Plugin isolated environments bind an owner-scoped `require` closure.
                if isolated_environment
                    && matches!(&key, Value::String(s) if s.as_bytes().as_ref() == b"require")
                    && matches!(&value, Value::Function(_))
                {
                    continue;
                }
                // Config chunks can bind a forwarding mare view in their private environment.
                // It follows the live runtime after commit; its identity is not a captured value.
                if isolated_environment
                    && matches!(&key, Value::String(s) if s.as_bytes().as_ref() == b"mare")
                    && matches!(&value, Value::Table(v) if v.raw_get::<Option<bool>>("__mare_config_view").ok().flatten() == Some(true))
                {
                    continue;
                }
                let Some(key) = config_fingerprint_value(lua, key, stack, seen, budget, false)? else {
                    stack.pop();
                    return Ok(None);
                };
                let Some(value) = config_fingerprint_value(lua, value, stack, seen, budget, false)? else {
                    stack.pop();
                    return Ok(None);
                };
                entries.push((key, value));
            }
            stack.pop();
            entries.sort();
            append(b"table");
            for (key, value) in entries {
                append(&key);
                append(&value);
            }
        }
        Value::Function(f) => {
            let info = f.info();
            if info.what == "C" {
                return Ok(None);
            }
            let pointer = f.to_pointer() as usize;
            // A repeated reference can change equality even when both tables have the same
            // contents. Treat aliases as opaque instead of collapsing their identities.
            if !seen.insert(pointer) {
                return Ok(None);
            }
            stack.push(pointer);
            append(b"function");
            append(&f.dump(true));
            for index in 1..=info.num_upvalues as usize {
                let (value, name) = config_upvalue(lua, &f, index)?;
                let Some(value) = config_fingerprint_value(lua, value, stack, seen, budget, name == "_ENV")? else {
                    stack.pop();
                    return Ok(None);
                };
                append(name.as_bytes());
                append(&value);
            }
            stack.pop();
        }
        _ => return Ok(None),
    }
    Ok(Some(result))
}

fn config_function_fingerprint(lua: &Lua, reg: &Table, f: &mlua::Function) -> mlua::Result<u64> {
    use std::hash::{BuildHasher, Hash, Hasher};
    let key = match lua.app_data_ref::<ConfigFingerprintKey>() {
        Some(key) => key.0.clone(),
        None => {
            let key = std::collections::hash_map::RandomState::new();
            lua.set_app_data(ConfigFingerprintKey(key.clone()));
            key
        }
    };
    let mut hash = key.build_hasher();
    match config_fingerprint_value(
        lua,
        Value::Function(f.clone()),
        &mut Vec::new(),
        &mut Default::default(),
        &mut 4096,
        false,
    )? {
        Some(value) => value.hash(&mut hash),
        None => {
            // For state that cannot be compared, never infer equality across evaluations.
            f.dump(true).hash(&mut hash);
            reg.raw_get::<Option<u64>>("__epoch")?.unwrap_or(0).hash(&mut hash);
        }
    }
    Ok(hash.finish())
}

fn stash_fns(lua: &Lua, reg: &Table, v: Value, depth: usize) -> mlua::Result<Value> {
    Ok(match v {
        Value::Function(f) => {
            // The fingerprint covers bytecode and inspectable captured values. Unknown state
            // is treated as changed rather than retaining an obsolete callback.
            let body = config_function_fingerprint(lua, reg, &f)?;
            // id = definition site + body + occurrence index within this evaluation
            let info = f.info();
            let site = format!("{}:{}", info.source.as_deref().unwrap_or("?"), info.line_defined.unwrap_or(0));
            let seen: Table = match reg.raw_get::<Value>("__seen")? {
                Value::Table(t) => t,
                _ => {
                    let t = lua.create_table()?;
                    reg.raw_set("__seen", t.clone())?;
                    t
                }
            };
            // the same function value reused in several places keeps one id
            let id = match seen.raw_get::<Option<String>>(f.clone())? {
                Some(id) => id,
                None => {
                    let n = seen.raw_get::<Option<i64>>(site.clone())?.unwrap_or(0) + 1;
                    seen.raw_set(site.clone(), n)?;
                    let id = format!("{site}#{n}:{body:016x}");
                    seen.raw_set(f.clone(), id.clone())?;
                    id
                }
            };
            reg.raw_set(id.clone(), f)?;
            Value::String(lua.create_string(format!("{FN_SENTINEL}{id}"))?)
        }
        Value::Table(t) if depth < 32 => {
            // copy only when the table holds a function somewhere; plain data passes through
            let out = lua.create_table()?;
            let mut changed = false;
            for kv in t.pairs::<Value, Value>() {
                let (k, val) = kv?;
                let nv = stash_fns(lua, reg, val.clone(), depth + 1)?;
                changed |= nv != val;
                out.raw_set(k, nv)?;
            }
            if changed {
                if let Some(mt) = t.metatable() {
                    out.set_metatable(Some(mt))?;
                }
                Value::Table(out)
            } else {
                Value::Table(t)
            }
        }
        v => v,
    })
}

/// Lua → JSON for config: like [`lua_to_json`], but functions survive as registry sentinels
/// (restore them with [`restore_config_fns`]).
pub fn config_to_json(lua: &Lua, v: Value) -> mlua::Result<serde_json::Value> {
    let reg = fn_registry(lua)?;
    lua_to_json(lua, stash_fns(lua, &reg, v, 0)?)
}

/// Replace function sentinels (from [`config_to_json`]) inside `v` with the functions, in place.
pub fn restore_config_fns(lua: &Lua, v: Value) -> mlua::Result<Value> {
    fn walk(reg: &Table, v: Value, depth: usize) -> mlua::Result<Value> {
        Ok(match v {
            Value::String(s) => match s.to_str().ok().and_then(|x| x.strip_prefix(FN_SENTINEL).map(str::to_string)) {
                Some(id) => match reg.raw_get::<Value>(id.as_str())? {
                    Value::Nil => match reg.raw_get::<Value>("__retired")? {
                        Value::Table(retired) => match retired.raw_get::<Value>(id)? {
                            Value::Nil => Value::String(s),
                            f => f,
                        },
                        _ => Value::String(s),
                    },
                    f => f,
                },
                None => Value::String(s),
            },
            Value::Table(t) if depth < 32 => {
                for kv in t.clone().pairs::<Value, Value>() {
                    let (k, val) = kv?;
                    if matches!(val, Value::String(_) | Value::Table(_)) {
                        let nv = walk(reg, val.clone(), depth + 1)?;
                        if nv != val {
                            t.raw_set(k, nv)?;
                        }
                    }
                }
                Value::Table(t)
            }
            v => v,
        })
    }
    walk(&fn_registry(lua)?, v, 0)
}

/// JSON → Lua for config, restoring functions stashed by [`config_to_json`].
pub fn config_to_lua(lua: &Lua, v: &serde_json::Value) -> mlua::Result<Value> {
    restore_config_fns(lua, json_to_lua(lua, v)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_fingerprint_handles_nil_bool_number_and_budget_exhaustion() {
        let lua = Lua::new();
        let mut stack = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut budget = 16;
        assert!(
            config_fingerprint_value(&lua, Value::Nil, &mut stack, &mut seen, &mut budget, false).unwrap().is_some()
        );
        assert!(
            config_fingerprint_value(&lua, Value::Boolean(true), &mut stack, &mut seen, &mut budget, false)
                .unwrap()
                .is_some()
        );
        assert!(
            config_fingerprint_value(&lua, Value::Boolean(false), &mut stack, &mut seen, &mut budget, false)
                .unwrap()
                .is_some()
        );
        assert!(
            config_fingerprint_value(&lua, Value::Number(3.5), &mut stack, &mut seen, &mut budget, false)
                .unwrap()
                .is_some()
        );
        let mut zero = 0;
        assert!(config_fingerprint_value(&lua, Value::Nil, &mut stack, &mut seen, &mut zero, false).unwrap().is_none());

        // Cyclic table, functions, and userdata return None (uncacheable fingerprint).
        let cyc: Table = lua.load("local t = {}; t.self = t; return t").eval().unwrap();
        let mut b2 = 64;
        assert!(
            config_fingerprint_value(&lua, Value::Table(cyc), &mut stack, &mut seen, &mut b2, false).unwrap().is_none()
        );

        let with_fn: Table = lua.load("return { f = function() end, k = 1 }").eval().unwrap();
        let mut b3 = 64;
        assert!(
            config_fingerprint_value(&lua, Value::Table(with_fn), &mut stack, &mut seen, &mut b3, true)
                .unwrap()
                .is_some()
        );

        let ud = lua.create_any_userdata(1u32).unwrap();
        let mut b4 = 64;
        assert!(
            config_fingerprint_value(&lua, Value::UserData(ud), &mut stack, &mut seen, &mut b4, false)
                .unwrap()
                .is_none()
        );

        // Metatable-bearing table, C function, and aliased Lua closure return None.
        let with_mt: Table = lua.load("return setmetatable({}, {})").eval().unwrap();
        let mut b5 = 64;
        assert!(
            config_fingerprint_value(&lua, Value::Table(with_mt), &mut stack, &mut seen, &mut b5, false)
                .unwrap()
                .is_none()
        );

        let c_fn = lua.create_function(|_, ()| Ok(())).unwrap();
        let mut b6 = 64;
        assert!(
            config_fingerprint_value(&lua, Value::Function(c_fn), &mut stack, &mut seen, &mut b6, false)
                .unwrap()
                .is_none()
        );

        let dup_fn: Table = lua.load("local f = function() return 1 end; return { a = f, b = f }").eval().unwrap();
        let mut b7 = 64;
        assert!(
            config_fingerprint_value(&lua, Value::Table(dup_fn), &mut stack, &mut seen, &mut b7, false)
                .unwrap()
                .is_none()
        );
    }
}
