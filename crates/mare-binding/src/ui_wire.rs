//! Existing UI node descriptions across Lua/RPC: image bytes become bounded standard base64.
//! No frontend closures, userdata, or local image paths cross the boundary.
use std::collections::HashSet;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use mare_shared::lua::rt_err;
use mlua::{Lua, LuaSerdeExt, Table, Value};

const MAX_IMAGE: usize = 8 * 1024 * 1024;
const MAX_ENCODED_IMAGE: usize = 11184812;
const MAX_CONTENT: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Snapshot,
    Encode,
    Decode,
}
struct Budget {
    entries: usize,
    bytes: usize,
    seen: HashSet<usize>,
}
impl Budget {
    fn bytes(&mut self, size: usize) -> mlua::Result<()> {
        self.bytes = self.bytes.checked_add(size).ok_or_else(|| rt_err("UI node content overflow"))?;
        if self.bytes > MAX_CONTENT {
            return Err(rt_err("UI node content exceeds 32 MiB"));
        }
        Ok(())
    }
}
fn copy(lua: &Lua, value: Value, mode: Mode, depth: usize, budget: &mut Budget) -> mlua::Result<Value> {
    if depth > 64 {
        return Err(rt_err("UI node nesting exceeds 64 levels"));
    }
    match value {
        Value::Nil | Value::Boolean(_) | Value::Integer(_) => Ok(value),
        Value::Number(number) if number.is_finite() => Ok(value),
        Value::String(ref string) => {
            budget.bytes(string.as_bytes().len())?;
            if mode != Mode::Snapshot {
                string.to_str()?;
            }
            Ok(value)
        }
        Value::Table(table) => {
            let identity = table.to_pointer() as usize;
            if !budget.seen.insert(identity) {
                return Err(rt_err("cyclic UI node description"));
            }
            let image = table.raw_get::<Option<String>>("type")?.as_deref() == Some("image");
            let out = lua.create_table()?;
            let resource = if image { table.raw_get::<Option<Table>>("resource")? } else { None };
            if let Some(resource) = &resource {
                if mode != Mode::Snapshot
                    && ["bytes", "data", "path"].iter().any(|key| table.contains_key(*key).unwrap_or(true))
                {
                    return Err(rt_err("image resource cannot carry inline data or local path"));
                }
                if mode == Mode::Encode {
                    let wire = lua.create_table()?;
                    for key in ["namespace", "key", "width", "height"] {
                        wire.set(key, resource.raw_get::<Value>(key)?)?;
                    }
                    out.set("resource", wire)?;
                } else if mode == Mode::Decode {
                    if resource.contains_key("resolver_id")? {
                        return Err(rt_err("wire resource cannot carry native resolver identity"));
                    }
                    let descriptor: mare_shared::image_resource::Descriptor =
                        lua.from_value(Value::Table(resource.clone()))?;
                    out.set(
                        "resource",
                        crate::ui_image_resource::reference(
                            lua,
                            &descriptor.namespace,
                            descriptor.key,
                            descriptor.width,
                            descriptor.height,
                        )?,
                    )?;
                    out.set("image_id", crate::ui_node::new_image_id()?)?;
                }
            }
            if image && resource.is_none() && mode == Mode::Encode {
                let bytes = table
                    .raw_get::<Option<mlua::LuaString>>("bytes")?
                    .ok_or_else(|| rt_err("wire images require bytes; local paths are not transferable"))?;
                if bytes.as_bytes().len() > MAX_IMAGE {
                    return Err(rt_err("wire image exceeds 8 MiB"));
                }
                budget.bytes(bytes.as_bytes().len())?;
                out.set("data", STANDARD.encode(bytes.as_bytes()))?;
            } else if image && resource.is_none() && mode == Mode::Decode {
                if table.contains_key("bytes")? || table.contains_key("path")? {
                    return Err(rt_err("wire images accept data, not bytes or local paths"));
                }
                let data: mlua::LuaString = table.raw_get("data")?;
                if data.as_bytes().len() > MAX_ENCODED_IMAGE {
                    return Err(rt_err("encoded wire image exceeds limit"));
                }
                let data = data.to_str()?;
                let bytes = STANDARD.decode(data.as_bytes()).map_err(rt_err)?;
                if bytes.len() > MAX_IMAGE || STANDARD.encode(&bytes) != data.as_ref() {
                    return Err(rt_err("wire image must be bounded canonical padded base64"));
                }
                budget.bytes(bytes.len())?;
                out.set("bytes", lua.create_string(bytes)?)?;
                out.set("image_id", crate::ui_node::new_image_id()?)?;
            }
            for pair in table.pairs::<Value, Value>() {
                let (key, child) = pair?;
                budget.entries += 1;
                if budget.entries > 50000 {
                    return Err(rt_err("UI node description exceeds 50000 entries"));
                }
                if image
                    && resource.is_some()
                    && mode != Mode::Snapshot
                    && key.as_string().is_some_and(|k| k.to_str().is_ok_and(|k| k.as_ref() == "resource"))
                {
                    continue;
                }
                if image
                    && mode != Mode::Snapshot
                    && key
                        .as_string()
                        .is_some_and(|k| k.to_str().is_ok_and(|k| matches!(k.as_ref(), "bytes" | "data" | "path")))
                {
                    continue;
                }
                if image
                    && mode == Mode::Decode
                    && key.as_string().is_some_and(|key| key.to_str().is_ok_and(|key| key.as_ref() == "image_id"))
                {
                    continue;
                }
                match &key {
                    Value::Integer(number) if *number >= 1 => {}
                    Value::String(string) => {
                        string.to_str()?;
                        budget.bytes(string.as_bytes().len())?;
                    }
                    _ => return Err(rt_err("UI node keys must be strings or positive integers")),
                }
                let cloned = copy(lua, child, mode, depth + 1, budget)?;
                if let (Value::String(name), Value::Table(array)) = (&key, &cloned)
                    && name.to_str().is_ok_and(|name| {
                        matches!(
                            name.as_ref(),
                            "items" | "blocks" | "frames" | "spans" | "first" | "rest" | "indent" | "title"
                        )
                    })
                    && (array.is_empty() || array.raw_len() > 0)
                {
                    array.set_metatable(Some(lua.array_metatable()))?;
                }
                out.set(key, cloned)?;
            }
            if mode == Mode::Encode && table.raw_get::<Option<String>>("type")?.as_deref() == Some("presented_markdown")
            {
                let source: String = out.raw_get("text")?;
                let valid = mare_shared::markdown::fences(&source);
                let blocks: Table = out.raw_get("blocks")?;
                for entry in blocks.sequence_values::<Table>() {
                    let entry = entry?;
                    let start: usize = entry.raw_get("start")?;
                    let block = valid
                        .iter()
                        .find(|block| block.start == start)
                        .ok_or_else(|| rt_err("wire presentation has an invalid source position"))?;
                    entry.raw_set("end", block.end)?;
                }
            }
            if table.metatable().is_some_and(|mt| mt == lua.array_metatable()) {
                out.set_metatable(Some(lua.array_metatable()))?;
            }
            budget.seen.remove(&identity);
            Ok(Value::Table(out))
        }
        _ => Err(rt_err("UI node descriptions cannot contain functions, userdata or non-finite numbers")),
    }
}
fn convert(lua: &Lua, value: Value, mode: Mode) -> mlua::Result<Value> {
    copy(lua, value, mode, 0, &mut Budget { entries: 0, bytes: 0, seen: HashSet::new() })
}

/// Encode a Lua UI node description (`Node`, `Line`, string, etc.) into a validated JSON wire value
/// (images encoded as bounded base64 `data`; local paths, closures, cycles, and >32 MiB payloads rejected).
pub fn encode_node_json(lua: &Lua, value: Value) -> mlua::Result<serde_json::Value> {
    let wire = convert(lua, value, Mode::Encode)?;
    mare_shared::lua::strict_lua_to_json(lua, wire)
}

/// Encode an optional Lua UI node or string into wire JSON (`Nil` -> `None`).
pub fn encode_node_or_string_json(lua: &Lua, value: Value) -> mlua::Result<Option<serde_json::Value>> {
    match value {
        Value::Nil => Ok(None),
        other => encode_node_json(lua, other).map(Some),
    }
}

pub(crate) fn install(lua: &Lua, ui: &Table) -> mlua::Result<()> {
    for (name, mode) in
        [("snapshot_node", Mode::Snapshot), ("node_to_wire", Mode::Encode), ("node_from_wire", Mode::Decode)]
    {
        ui.set(name, lua.create_function(move |lua, value: Value| convert(lua, value, mode))?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_descriptions_roundtrip_without_paths_or_byte_loss() {
        let lua = Lua::new();
        let node = lua.create_table().unwrap();
        node.set("type", "image").unwrap();
        node.set("bytes", lua.create_string([0, 255, 137, 80, 78, 71]).unwrap()).unwrap();
        node.set("path", "/private/never-on-wire").unwrap();
        let Value::Table(wire) = convert(&lua, Value::Table(node.clone()), Mode::Encode).unwrap() else { panic!() };
        assert!(!wire.contains_key("bytes").unwrap());
        assert!(!wire.contains_key("path").unwrap());
        assert_eq!(wire.get::<String>("data").unwrap(), "AP+JUE5H");
        let Value::Table(decoded) = convert(&lua, Value::Table(wire), Mode::Decode).unwrap() else { panic!() };
        assert_eq!(decoded.get::<mlua::LuaString>("bytes").unwrap().as_bytes().as_ref(), &[0, 255, 137, 80, 78, 71]);
        let Value::Table(snapshot) = convert(&lua, Value::Table(node.clone()), Mode::Snapshot).unwrap() else {
            panic!()
        };
        node.set("max_rows", 99).unwrap();
        assert!(!snapshot.contains_key("max_rows").unwrap());
    }
    #[test]
    fn empty_component_arrays_and_size_limits_are_preserved() {
        let lua = Lua::new();
        let node = lua.create_table().unwrap();
        node.set("type", "lines").unwrap();
        node.set("items", lua.create_table().unwrap()).unwrap();
        let wire = convert(&lua, Value::Table(node), Mode::Encode).unwrap();
        assert_eq!(mare_shared::lua::strict_lua_to_json(&lua, wire).unwrap()["items"], serde_json::json!([]));
        let image = lua.create_table().unwrap();
        image.set("type", "image").unwrap();
        image.set("bytes", lua.create_string(vec![0; MAX_IMAGE + 1]).unwrap()).unwrap();
        assert!(convert(&lua, Value::Table(image), Mode::Encode).is_err());
        let image = lua.create_table().unwrap();
        image.set("type", "image").unwrap();
        image.set("data", STANDARD.encode(vec![0; MAX_IMAGE + 1])).unwrap();
        assert!(convert(&lua, Value::Table(image), Mode::Decode).is_err());
    }
    #[test]
    fn rejects_cycles_callbacks_local_paths_and_noncanonical_base64() {
        let lua = Lua::new();
        let cycle = lua.create_table().unwrap();
        cycle.set("child", cycle.clone()).unwrap();
        assert!(convert(&lua, Value::Table(cycle), Mode::Snapshot).is_err());
        let callback = lua.create_function(|_, ()| Ok(())).unwrap();
        assert!(convert(&lua, Value::Function(callback), Mode::Encode).is_err());
        for data in ["AA", "AB==", "AA==\n", "!"] {
            let image = lua.create_table().unwrap();
            image.set("type", "image").unwrap();
            image.set("data", data).unwrap();
            assert!(convert(&lua, Value::Table(image), Mode::Decode).is_err());
        }
        let image = lua.create_table().unwrap();
        image.set("type", "image").unwrap();
        image.set("path", "/private").unwrap();
        assert!(convert(&lua, Value::Table(image), Mode::Encode).is_err());
    }
}
