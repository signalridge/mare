//! Core Lua keymap engine loader (`keymap.lua`) and `core.keymap` host callbacks.

use mare_shared::lua::core;
use mlua::{Lua, Table};

/// Populate `core.keymap` host callbacks and evaluate `preset/keymap.lua` with `(mare, core)`.
pub fn install(lua: &Lua, mare: &Table, _ui_table: &Table) -> mlua::Result<()> {
    let core_handle = core::ensure(lua)?;
    let km_core = core::keymap::handle(lua)?;
    km_core.set(
        "current_owner",
        lua.create_function(|lua, ()| match mare_kernel::current_owner_info(lua) {
            Some((plugin, origin)) => Ok((Some(plugin), Some(origin))),
            None => Ok((None::<String>, None::<String>)),
        })?,
    )?;
    km_core.set(
        "attach_disposer",
        lua.create_function(|lua, dispose: mlua::Function| mare_kernel::attach_owner_disposer(lua, dispose))?,
    )?;
    lua.load(crate::keymap_chunk().as_ref()).set_name("@preset/keymap.lua").call::<()>((mare.clone(), core_handle))?;
    Ok(())
}
