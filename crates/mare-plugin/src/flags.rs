//! `mare.flags`: declarative and imperative CLI extension flag registration and lookup.
//!
//! - `register(name, opts)`: register a CLI flag (`{ type = "boolean" | "string", description?,
//!   default? }`)
//! - `get(name)` -> `boolean | string | nil`: CLI override -> default -> nil (nil if unregistered)
//! - `list()` -> `{ { name, type, description?, default?, value?, source? } }`

use mlua::{Lua, Table, Value};
use serde_json::Value as J;

use mare_shared::flags::{FlagSpec, FlagStore, FlagType, FlagValue};
use mare_shared::lua::{json_to_lua, rt_err};

pub fn install(lua: &Lua, mare: &Table, cli_flags: Option<&J>) -> mlua::Result<()> {
    let store = FlagStore::of(lua);
    store.seed_cli_json(cli_flags);

    let t = lua.create_table()?;
    t.set(
        "register",
        lua.create_function(|lua, (name, opts): (String, Table)| {
            let ty_str: String = opts
                .get("type")
                .map_err(|_| rt_err(format!("mare.flags.register({name}): `opts.type` is required")))?;
            let flag_type = FlagType::parse(&ty_str).map_err(rt_err)?;
            let description: Option<String> = opts.get("description")?;
            let default = match opts.get::<Value>("default")? {
                Value::Nil => None,
                v => Some(FlagValue::from_lua(v, flag_type).map_err(rt_err)?),
            };
            let layer = crate::config_origin::current_layer(lua);
            let source = if layer == "runtime" { "runtime".to_string() } else { format!("config:{layer}") };
            let spec = FlagSpec { name, flag_type, description, default, source: Some(source) };
            if let Some(warn) = FlagStore::of(lua).register(spec).map_err(rt_err)? {
                tracing::warn!("{warn}");
            }
            Ok(())
        })?,
    )?;
    t.set(
        "get",
        lua.create_function(|lua, name: String| match FlagStore::of(lua).get(&name) {
            Some(v) => v.to_lua(lua),
            None => Ok(Value::Nil),
        })?,
    )?;
    t.set("list", lua.create_function(|lua, ()| json_to_lua(lua, &FlagStore::of(lua).list_json()))?)?;
    mare.set("flags", t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn register_get_and_list_respect_cli_values_and_defaults() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        lua.globals().set("mare", mare.clone()).unwrap();
        install(&lua, &mare, Some(&json!({ "flag-a": true, "flag-b": "custom" }))).unwrap();

        lua.load(
            r#"
            mare.flags.register("flag-a", { type = "boolean", description = "Flag A", default = false })
            mare.flags.register("flag-b", { type = "string", default = "fallback" })
            mare.flags.register("flag-c", { type = "string", default = "def-c" })
            assert(mare.flags.get("flag-a") == true)
            assert(mare.flags.get("flag-b") == "custom")
            assert(mare.flags.get("flag-c") == "def-c")
            assert(mare.flags.get("unregistered") == nil)
            local list = mare.flags.list()
            assert(#list == 3)
            assert(list[1].name == "flag-a" and list[1].value == true)
            assert(list[2].name == "flag-b" and list[2].value == "custom")
            assert(list[3].name == "flag-c" and list[3].value == "def-c")
            "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn register_rejects_invalid_inputs() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        lua.globals().set("mare", mare.clone()).unwrap();
        install(&lua, &mare, None).unwrap();

        assert!(lua.load(r#"mare.flags.register("", { type = "boolean" })"#).exec().is_err());
        assert!(lua.load(r#"mare.flags.register("--lead", { type = "boolean" })"#).exec().is_err());
        assert!(lua.load(r#"mare.flags.register("bad", { type = "number" })"#).exec().is_err());
        assert!(lua.load(r#"mare.flags.register("bad", { type = "boolean", default = "x" })"#).exec().is_err());
    }
}
