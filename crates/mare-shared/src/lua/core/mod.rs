//! Per-VM private core handle and typed Rust accessors for core Lua engines.
//!
//! Rust creates one core handle table per VM in the Lua registry (unreachable from Lua because
//! the `debug` library is not loaded) and passes it to every core chunk (`config_*`, `keymap.lua`,
//! `ui_components.lua`, `ui_shell.lua`, `test.lua`). Each engine owns its namespace:
//! `core.config`, `core.keymap`, and `core.ui`.
//!
//! All Rust access to core-Lua internal fields routes through [`config`], [`keymap`], and [`ui`]
//! so no Rust code outside `mare-shared/src/lua/core/` names underscore fields directly.

use mlua::{Lua, Table, Value};

pub mod config;
pub mod keymap;
pub mod ui;

const CORE_HANDLE_KEY: &str = "mare.core_handle";

/// Return the VM's core handle table from the Lua registry, creating it with empty
/// `config`, `keymap`, and `ui` namespaces on first access.
pub fn ensure(lua: &Lua) -> mlua::Result<Table> {
    if let Ok(Value::Table(core)) = lua.named_registry_value::<Value>(CORE_HANDLE_KEY) {
        ensure_namespaces(lua, &core)?;
        return Ok(core);
    }
    let core = lua.create_table()?;
    ensure_namespaces(lua, &core)?;
    lua.set_named_registry_value(CORE_HANDLE_KEY, core.clone())?;
    Ok(core)
}

/// Return the VM's core handle table if it has already been installed.
pub fn get(lua: &Lua) -> mlua::Result<Option<Table>> {
    match lua.named_registry_value::<Value>(CORE_HANDLE_KEY) {
        Ok(Value::Table(core)) => Ok(Some(core)),
        Ok(_) => Ok(None),
        Err(_) => Ok(None),
    }
}

/// Return one named engine namespace table (`"config"`, `"keymap"`, or `"ui"`) on the core handle.
pub fn namespace(lua: &Lua, name: &str) -> mlua::Result<Table> {
    let core = ensure(lua)?;
    ensure_subtable(lua, &core, name)
}

fn ensure_namespaces(lua: &Lua, core: &Table) -> mlua::Result<()> {
    for name in ["config", "keymap", "ui"] {
        let _ = ensure_subtable(lua, core, name)?;
    }
    Ok(())
}

fn ensure_subtable(lua: &Lua, parent: &Table, key: &str) -> mlua::Result<Table> {
    match parent.raw_get::<Value>(key)? {
        Value::Table(t) => Ok(t),
        _ => {
            let t = lua.create_table()?;
            parent.raw_set(key, t.clone())?;
            Ok(t)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_handle_lives_in_registry_with_engine_namespaces() {
        let lua = Lua::new();
        assert!(get(&lua).unwrap().is_none());
        let core = ensure(&lua).unwrap();
        assert!(get(&lua).unwrap().is_some());
        assert!(lua.globals().get::<Value>("core").unwrap().is_nil());
        for ns in ["config", "keymap", "ui"] {
            assert!(core.get::<Table>(ns).is_ok(), "missing namespace {ns}");
        }
        // Repeated ensure returns the same table identity.
        let again = ensure(&lua).unwrap();
        assert_eq!(core.to_pointer(), again.to_pointer());
    }

    #[test]
    fn keymap_apply_config_keys_hook_is_noop_until_registered() {
        let lua = Lua::new();
        let keys = lua.create_table().unwrap();
        keys.set("<C-o>", "transcript").unwrap();
        let sources = lua.create_table().unwrap();
        // No-op when keymap engine has not registered the hook yet.
        keymap::apply_config_keys(&lua, Value::Table(keys.clone()), Value::Table(sources.clone()), false).unwrap();

        let called = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = called.clone();
        let hook = lua
            .create_function(move |_, (k, _s, staged): (Table, Table, bool)| {
                assert_eq!(k.get::<String>("<C-o>")?, "transcript");
                assert!(staged);
                flag.set(true);
                Ok(())
            })
            .unwrap();
        keymap::handle(&lua).unwrap().set("apply_config_keys", hook).unwrap();
        keymap::apply_config_keys(&lua, Value::Table(keys), Value::Table(sources), true).unwrap();
        assert!(called.get());
    }
}
