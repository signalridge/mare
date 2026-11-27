use mlua::{Lua, LuaSerdeExt, Table, Value};

use super::rt_err;

const SCHEMAS_REGISTRY: &str = "mare.tool_schema_markers";

fn schema_markers(lua: &Lua) -> mlua::Result<Table> {
    match lua.named_registry_value::<Value>(SCHEMAS_REGISTRY)? {
        Value::Table(t) => Ok(t),
        _ => {
            let t = lua.create_table()?;
            let mt = lua.create_table()?;
            mt.raw_set("__mode", "k")?;
            t.set_metatable(Some(mt))?;
            lua.set_named_registry_value(SCHEMAS_REGISTRY, t.clone())?;
            Ok(t)
        }
    }
}

/// Mark a schema value without exposing mutable metadata on the Lua table.
pub fn mark_tool_schema(lua: &Lua, value: &Value) -> mlua::Result<()> {
    let Value::Table(t) = value else {
        return Err(rt_err("schema must be a table"));
    };
    schema_markers(lua)?.raw_set(t.clone(), true)
}

// Most messages contain no schema ordering hint. Detect it while sorting so ordinary
// JSON avoids a second traversal of Lua tables and registry lookups.
pub(super) fn sort_json(v: &mut serde_json::Value) -> bool {
    match v {
        serde_json::Value::Object(m) => {
            m.sort_keys();
            let mut has_order = m.contains_key("x-mare-order");
            for value in m.values_mut() {
                has_order |= sort_json(value);
            }
            has_order
        }
        serde_json::Value::Array(a) => {
            let mut has_order = false;
            for value in a {
                has_order |= sort_json(value);
            }
            has_order
        }
        _ => false,
    }
}

/// Preserve schema identity metadata when a public value transform clones a Lua table.
/// Ordering annotations must remain schema-only; re-marking arbitrary tables changes user data.
pub fn copy_tool_schema_marker(lua: &Lua, source: &Table, target: &Table) -> mlua::Result<()> {
    let markers = schema_markers(lua)?;
    if markers.raw_get::<Option<bool>>(source.clone())? == Some(true) {
        markers.raw_set(target.clone(), true)?;
    }
    Ok(())
}

/// Deep-clone a Lua value so policy listeners cannot mutate shared tables across listeners.
pub fn deep_clone_value(lua: &Lua, value: Value) -> mlua::Result<Value> {
    fn walk(
        lua: &Lua,
        value: Value,
        seen: &mut std::collections::HashMap<usize, Table>,
        depth: usize,
    ) -> mlua::Result<Value> {
        if depth > 128 {
            return Err(mlua::Error::runtime("value nesting limit exceeded"));
        }
        match value {
            Value::Table(t) => {
                let ptr = t.to_pointer() as usize;
                if let Some(existing) = seen.get(&ptr) {
                    return Ok(Value::Table(existing.clone()));
                }
                let copy = lua.create_table()?;
                seen.insert(ptr, copy.clone());
                for pair in t.pairs::<Value, Value>() {
                    let (k, v) = pair?;
                    let ck = walk(lua, k, seen, depth + 1)?;
                    let cv = walk(lua, v, seen, depth + 1)?;
                    copy.raw_set(ck, cv)?;
                }
                copy.set_metatable(t.metatable())?;
                copy_tool_schema_marker(lua, &t, &copy)?;
                Ok(Value::Table(copy))
            }
            other => Ok(other),
        }
    }
    walk(lua, value, &mut std::collections::HashMap::new(), 0)
}

pub(super) fn apply_marked_schemas(
    lua: &Lua,
    source: Value,
    json: &mut serde_json::Value,
    depth: usize,
    stack: &mut Vec<usize>,
) -> mlua::Result<()> {
    if depth > 64 {
        return Ok(());
    }
    let Value::Table(t) = source else {
        return Ok(());
    };
    if schema_markers(lua)?.raw_get::<Option<bool>>(t.clone())? == Some(true) {
        order_schema(json);
        return Ok(());
    }
    let ptr = t.to_pointer() as usize;
    if stack.contains(&ptr) {
        return Ok(());
    }
    stack.push(ptr);
    for pair in t.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let child = match (&key, &mut *json) {
            (Value::String(k), serde_json::Value::Object(m)) => k.to_str().ok().and_then(|k| m.get_mut(k.as_ref())),
            (Value::Integer(i), serde_json::Value::Array(a)) if *i > 0 => a.get_mut(*i as usize - 1),
            _ => None,
        };
        if let Some(child) = child {
            apply_marked_schemas(lua, value, child, depth + 1, stack)?;
        }
    }
    stack.pop();
    Ok(())
}

/// Strip/order annotations only in schema positions, never in const/enum/default data or
/// property names. Called on a fresh JSON copy, immediately before sending to a provider.
pub fn order_schema(v: &mut serde_json::Value) {
    let Some(m) = v.as_object_mut() else {
        return;
    };
    let order = m.remove("x-mare-order");
    if let Some(serde_json::Value::Object(props)) = m.get_mut("properties") {
        for schema in props.values_mut() {
            order_schema(schema);
        }
        if let Some(serde_json::Value::Array(order)) = order {
            let mut sorted = serde_json::Map::new();
            for k in order.iter().filter_map(|k| k.as_str()) {
                if let Some(value) = props.remove(k) {
                    sorted.insert(k.to_string(), value);
                }
            }
            sorted.append(props);
            *props = sorted;
        }
    }
    for key in ["items", "additionalProperties", "not", "if", "then", "else", "propertyNames", "contains"] {
        if let Some(value) = m.get_mut(key) {
            order_schema(value);
        }
    }
    for key in ["oneOf", "anyOf", "allOf", "prefixItems"] {
        if let Some(serde_json::Value::Array(branches)) = m.get_mut(key) {
            for branch in branches {
                order_schema(branch);
            }
        }
    }
    for key in ["$defs", "definitions", "patternProperties"] {
        if let Some(serde_json::Value::Object(defs)) = m.get_mut(key) {
            for schema in defs.values_mut() {
                order_schema(schema);
            }
        }
    }
}

const MAX_SHAPE_DEPTH: usize = 64;

pub fn copy_json_metadata(lua: &Lua, source: &Value, target: &Value) -> mlua::Result<()> {
    fn walk(
        lua: &Lua,
        source: &Value,
        target: &Value,
        depth: usize,
        active: &mut std::collections::HashSet<usize>,
    ) -> mlua::Result<()> {
        if depth > MAX_SHAPE_DEPTH {
            return Err(rt_err("Lua value exceeds maximum nesting depth"));
        }
        let (Value::Table(source), Value::Table(target)) = (source, target) else { return Ok(()) };
        let ptr = source.to_pointer() as usize;
        if !active.insert(ptr) {
            return Err(rt_err("cyclic table in copy_json_metadata"));
        }
        let res = (|| {
            if schema_markers(lua)?.raw_get::<Option<bool>>(source.clone())? == Some(true) {
                mark_tool_schema(lua, &Value::Table(target.clone()))?;
            }
            for pair in source.clone().pairs::<Value, Value>() {
                let (key, value) = pair?;
                let cloned = target.raw_get::<Value>(key)?;
                walk(lua, &value, &cloned, depth + 1, active)?;
            }
            Ok(())
        })();
        active.remove(&ptr);
        res
    }
    let mut active = std::collections::HashSet::new();
    walk(lua, source, target, 0, &mut active)
}

/// Give the Lua value the empty-container shapes of `shaped` (its JSON after
/// schema::shape_empty): an empty table where `shaped` has `[]` gets the array mark, one where it
/// has `{}` loses it. The tables themselves are kept (identity, callbacks), only marked. A table
/// reached again in a place that needs different shapes below it (one table shared by two
/// places) is replaced there, inside its parent, by a copy marked for that place; `ShapeMarks`
/// carries what each table was given across the calls of one delivery. A top-level value has no
/// parent to replace it in: an error.
pub fn apply_shape(lua: &Lua, value: &Value, shaped: &serde_json::Value) -> mlua::Result<()> {
    let array_mt = lua.array_metatable();
    let mut seen: std::collections::HashMap<usize, &serde_json::Value> = std::collections::HashMap::new();
    if !mark_borrowed(lua, value, shaped, &mut seen, &array_mt, 0)? {
        return Err(rt_err("one table is passed in two places that need different shapes"));
    }
    Ok(())
}

/// What each table was shaped as during one delivery (see [`apply_shape`]).
#[derive(Default)]
pub struct ShapeMarks(std::collections::HashMap<usize, serde_json::Value>, bool);

impl ShapeMarks {
    /// Marks that only check: the same conflicts are found, no table is changed (a delivery that
    /// may still be refused checks first, so a refused call leaves the caller's tables alone).
    pub fn dry() -> Self {
        Self(Default::default(), true)
    }
}

pub fn apply_shape_with(
    lua: &Lua,
    value: &Value,
    shaped: &serde_json::Value,
    marks: &mut ShapeMarks,
) -> mlua::Result<()> {
    let array_mt = lua.array_metatable();
    if !mark(lua, value, shaped, marks, &array_mt, 0)? {
        return Err(rt_err("one table is passed in two places that need different shapes"));
    }
    Ok(())
}

/// A copy of `table` (nested tables copied, other values shared) to mark for another place.
fn copy_table(lua: &Lua, table: &Table) -> mlua::Result<Table> {
    fn copy_inner(
        lua: &Lua,
        table: &Table,
        depth: usize,
        active: &mut std::collections::HashSet<usize>,
    ) -> mlua::Result<Table> {
        if depth > MAX_SHAPE_DEPTH {
            return Err(rt_err("Lua table exceeds maximum nesting depth"));
        }
        let ptr = table.to_pointer() as usize;
        if !active.insert(ptr) {
            return Err(rt_err("cyclic table cannot be copied for shaping"));
        }
        let res = (|| {
            let copy = lua.create_table()?;
            for pair in table.pairs::<Value, Value>() {
                let (key, value) = pair?;
                let value = match value {
                    Value::Table(inner) => Value::Table(copy_inner(lua, &inner, depth + 1, active)?),
                    other => other,
                };
                copy.raw_set(key, value)?;
            }
            copy.set_metatable(table.metatable())?;
            Ok(copy)
        })();
        active.remove(&ptr);
        res
    }
    let mut active = std::collections::HashSet::new();
    copy_inner(lua, table, 0, &mut active)
}

#[inline]
fn is_json_container(v: &serde_json::Value) -> bool {
    matches!(v, serde_json::Value::Array(_) | serde_json::Value::Object(_))
}

/// Single-call borrowed variant of [`mark`]: avoids cloning every nested `serde_json::Value` into
/// `ShapeMarks` while enforcing identical shared-table conflict checks and empty-table marking.
fn mark_borrowed<'a>(
    lua: &Lua,
    value: &Value,
    shaped: &'a serde_json::Value,
    seen: &mut std::collections::HashMap<usize, &'a serde_json::Value>,
    array_mt: &Table,
    depth: usize,
) -> mlua::Result<bool> {
    if depth > MAX_SHAPE_DEPTH {
        return Err(rt_err("Lua value exceeds maximum nesting depth"));
    }
    let Value::Table(table) = value else { return Ok(true) };
    let pointer = table.to_pointer() as usize;
    if let Some(given) = seen.get(&pointer) {
        return Ok(crate::schema::json_equal(given, shaped));
    }
    seen.insert(pointer, shaped);
    match shaped {
        serde_json::Value::Array(items) if items.is_empty() => {
            if table.is_empty() {
                table.set_metatable(Some(array_mt.clone()))?;
            }
            Ok(true)
        }
        serde_json::Value::Object(map) if map.is_empty() => {
            if table.is_empty() && table.metatable().is_some_and(|meta| meta == *array_mt) {
                table.set_metatable(None)?;
            }
            Ok(true)
        }
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if !is_json_container(child) {
                    continue;
                }
                let field = table.raw_get::<Value>(key.as_str())?;
                if !mark_borrowed(lua, &field, child, seen, array_mt, depth + 1)? {
                    let Value::Table(shared) = field else { continue };
                    let copy = Value::Table(copy_table(lua, &shared)?);
                    mark_borrowed(lua, &copy, child, seen, array_mt, depth + 1)?;
                    table.raw_set(key.as_str(), copy)?;
                }
            }
            Ok(true)
        }
        serde_json::Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                if !is_json_container(child) {
                    continue;
                }
                let idx = i as i64 + 1;
                let field = table.raw_get::<Value>(idx)?;
                if !mark_borrowed(lua, &field, child, seen, array_mt, depth + 1)? {
                    let Value::Table(shared) = field else { continue };
                    let copy = Value::Table(copy_table(lua, &shared)?);
                    mark_borrowed(lua, &copy, child, seen, array_mt, depth + 1)?;
                    table.raw_set(idx, copy)?;
                }
            }
            Ok(true)
        }
        _ => Ok(true),
    }
}

/// Marks `value` for `shaped`; false when this very table was already marked for different
/// shapes (the caller replaces it with a copy).
fn mark(
    lua: &Lua,
    value: &Value,
    shaped: &serde_json::Value,
    marks: &mut ShapeMarks,
    array_mt: &Table,
    depth: usize,
) -> mlua::Result<bool> {
    if depth > MAX_SHAPE_DEPTH {
        return Err(rt_err("Lua value exceeds maximum nesting depth"));
    }
    let Value::Table(table) = value else { return Ok(true) };
    let pointer = table.to_pointer() as usize;
    if let Some(given) = marks.0.get(&pointer) {
        return Ok(crate::schema::json_equal(given, shaped));
    }
    marks.0.insert(pointer, shaped.clone());
    match shaped {
        serde_json::Value::Array(items) if items.is_empty() => {
            if table.is_empty() && !marks.1 {
                table.set_metatable(Some(array_mt.clone()))?;
            }
            Ok(true)
        }
        serde_json::Value::Object(map) if map.is_empty() => {
            if !marks.1 && table.is_empty() && table.metatable().is_some_and(|meta| meta == *array_mt) {
                table.set_metatable(None)?;
            }
            Ok(true)
        }
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if !is_json_container(child) {
                    continue;
                }
                let field = table.raw_get::<Value>(key.as_str())?;
                if !mark(lua, &field, child, marks, array_mt, depth + 1)? {
                    let Value::Table(shared) = field else { continue };
                    if marks.1 {
                        continue;
                    }
                    let copy = Value::Table(copy_table(lua, &shared)?);
                    mark(lua, &copy, child, marks, array_mt, depth + 1)?;
                    table.raw_set(key.as_str(), copy)?;
                }
            }
            Ok(true)
        }
        serde_json::Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                if !is_json_container(child) {
                    continue;
                }
                let idx = i as i64 + 1;
                let field = table.raw_get::<Value>(idx)?;
                if !mark(lua, &field, child, marks, array_mt, depth + 1)? {
                    let Value::Table(shared) = field else { continue };
                    if marks.1 {
                        continue;
                    }
                    let copy = Value::Table(copy_table(lua, &shared)?);
                    mark(lua, &copy, child, marks, array_mt, depth + 1)?;
                    table.raw_set(idx, copy)?;
                }
            }
            Ok(true)
        }
        _ => Ok(true),
    }
}
