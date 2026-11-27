//! Defaults from a `config_schema`: one declaration per option
//! `{ type, default, desc, fields?, … }`. The kernel builds a plugin's `cfg` from it and the config
//! layer checks values against it (`config_api.lua`), so the two never disagree.

use std::collections::HashSet;

use mlua::{Lua, LuaSerdeExt, Table, Value};

const MAX_SCHEMA_DEPTH: usize = 64;

/// The defaults a schema declares: each entry's `default` (a table is copied, never shared), and
/// for an `object` entry with `fields` its fields' defaults with the entry's own `default` on top.
/// An entry without a default is absent (unset), as `nil` is in Lua.
pub fn defaults(lua: &Lua, schema: &Table) -> mlua::Result<Table> {
    let mut active = HashSet::new();
    defaults_inner(lua, schema, 0, &mut active)
}

fn defaults_inner(lua: &Lua, schema: &Table, depth: usize, active: &mut HashSet<usize>) -> mlua::Result<Table> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(mlua::Error::runtime("config_schema defaults exceed maximum nesting depth"));
    }
    let ptr = schema.to_pointer() as usize;
    if !active.insert(ptr) {
        return Err(mlua::Error::runtime("cyclic table in config_schema"));
    }
    let res = (|| {
        let out = lua.create_table()?;
        for pair in schema.pairs::<Value, Value>() {
            let (key, entry) = pair?;
            let Value::Table(entry) = entry else { continue };
            let fields = match (entry.raw_get::<Value>("type")?, entry.raw_get::<Value>("fields")?) {
                (Value::String(t), Value::Table(fields)) if t.to_str().is_ok_and(|t| t == "object") => Some(fields),
                _ => None,
            };
            let value = match (fields, entry.raw_get::<Value>("default")?) {
                (Some(fields), own) => {
                    let nested = defaults_inner(lua, &fields, depth + 1, active)?;
                    if let Value::Table(own) = own {
                        for pair in deep_copy_inner(lua, &own, depth + 1, active)?.pairs::<Value, Value>() {
                            let (k, v) = pair?;
                            nested.raw_set(k, v)?;
                        }
                    }
                    Value::Table(nested)
                }
                (None, Value::Table(t)) => {
                    let copy = deep_copy_inner(lua, &t, depth + 1, active)?;
                    // a list default stays a list when empty (`{}` would encode as an object)
                    if entry
                        .raw_get::<Value>("type")?
                        .as_string()
                        .is_some_and(|t| t.to_str().is_ok_and(|t| t == "list"))
                    {
                        copy.set_metatable(Some(lua.array_metatable()))?;
                    }
                    Value::Table(copy)
                }
                (None, v) => v,
            };
            if !value.is_nil() {
                out.raw_set(key, value)?;
            }
        }
        Ok(out)
    })();
    active.remove(&ptr);
    res
}

fn deep_copy_inner(lua: &Lua, t: &Table, depth: usize, active: &mut HashSet<usize>) -> mlua::Result<Table> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(mlua::Error::runtime("config_schema default exceeds maximum nesting depth"));
    }
    let ptr = t.to_pointer() as usize;
    if !active.insert(ptr) {
        return Err(mlua::Error::runtime("cyclic table in config_schema default"));
    }
    let res = (|| {
        let out = lua.create_table()?;
        for pair in t.pairs::<Value, Value>() {
            let (k, v) = pair?;
            let v = match v {
                Value::Table(inner) => Value::Table(deep_copy_inner(lua, &inner, depth + 1, active)?),
                v => v,
            };
            out.raw_set(k, v)?;
        }
        if let Some(mt) = t.metatable() {
            out.set_metatable(Some(mt))?;
        }
        Ok(out)
    })();
    active.remove(&ptr);
    res
}

#[cfg(test)]
mod tests {
    #[test]
    fn defaults_follow_entries_and_nested_fields() {
        let lua = mlua::Lua::new();
        let schema: mlua::Table = lua
            .load(
                r#"return {
                    n = { type = "integer", default = 1 },
                    unset = { type = "string" },
                    group = { type = "object", default = { b = 3 }, fields = {
                        a = { type = "boolean", default = false }, b = { type = "integer", default = 2 } } },
                    list = { type = "list", default = { "x" } },
                }"#,
            )
            .eval()
            .unwrap();
        let d = super::defaults(&lua, &schema).unwrap();
        assert_eq!(d.get::<i64>("n").unwrap(), 1);
        assert!(d.get::<mlua::Value>("unset").unwrap().is_nil());
        let group: mlua::Table = d.get("group").unwrap();
        assert!(!group.get::<bool>("a").unwrap());
        assert_eq!(group.get::<i64>("b").unwrap(), 3, "the entry's own default over its fields'");
        let list: mlua::Table = d.get("list").unwrap();
        let declared: mlua::Table = schema.get::<mlua::Table>("list").unwrap().get("default").unwrap();
        assert_ne!(list.to_pointer(), declared.to_pointer(), "copied, never shared");
    }

    #[test]
    fn defaults_rejects_cyclic_tables_without_overflow() {
        let lua = mlua::Lua::new();
        let cyclic: mlua::Table = lua
            .load(
                r#"
                local cycle = {}
                cycle.self_ref = cycle
                return { bad = { type = "table", default = cycle } }
                "#,
            )
            .eval()
            .unwrap();
        let err = super::defaults(&lua, &cyclic).unwrap_err().to_string();
        assert!(err.contains("cyclic"), "{err}");

        // Cyclic object fields schema itself must also error cleanly.
        let cyclic_schema: mlua::Table = lua
            .load(
                r#"
                local fields = {}
                fields.sub = { type = "object", fields = fields }
                return fields
                "#,
            )
            .eval()
            .unwrap();
        let err = super::defaults(&lua, &cyclic_schema).unwrap_err().to_string();
        assert!(err.contains("cyclic table in config_schema"), "{err}");

        // Exceeding MAX_SCHEMA_DEPTH in both schema nesting and default table nesting must error.
        let deep_schema: mlua::Table = lua
            .load(
                r#"
                local cur = { leaf = { type = "integer", default = 1 } }
                for _ = 1, 70 do
                    cur = { sub = { type = "object", fields = cur } }
                end
                return cur
                "#,
            )
            .eval()
            .unwrap();
        let err = super::defaults(&lua, &deep_schema).unwrap_err().to_string();
        assert!(err.contains("maximum nesting depth"), "{err}");

        let deep_default: mlua::Table = lua
            .load(
                r#"
                local cur = { v = 1 }
                for _ = 1, 70 do
                    cur = { nested = cur }
                end
                return { deep = { type = "table", default = cur } }
                "#,
            )
            .eval()
            .unwrap();
        let err = super::defaults(&lua, &deep_default).unwrap_err().to_string();
        assert!(err.contains("maximum nesting depth"), "{err}");

        // Metatable on a default table is preserved on the deep copy.
        let with_mt: mlua::Table = lua
            .load(
                r#"
                local mt = { __index = { fallback = 42 } }
                local def = setmetatable({ own = 1 }, mt)
                return { custom = { type = "table", default = def } }
                "#,
            )
            .eval()
            .unwrap();
        let d = super::defaults(&lua, &with_mt).unwrap();
        let custom: mlua::Table = d.get("custom").unwrap();
        assert_eq!(custom.get::<i64>("own").unwrap(), 1);
        assert_eq!(custom.get::<i64>("fallback").unwrap(), 42);
    }
}
